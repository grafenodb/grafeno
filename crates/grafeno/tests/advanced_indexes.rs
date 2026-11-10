//! Phase 16.5 in the engine: composite, edge and multi-label indexes and
//! property rules hold on every write path (the API, replicated entries,
//! exports and imports, a rebuild), and lookups find what they should.

use grafeno::{Constraint, Grafeno, IndexDef, IndexRange, NewEdge, NewNode, NodeId, PropertyType};
use serde_json::json;

const NS: &str = "x";

fn person(last: &str, first: &str, n: i64) -> NewNode {
    NewNode::new("Person")
        .prop("last", last)
        .prop("first", first)
        .prop("n", n)
}

fn key() -> IndexDef {
    IndexDef {
        unique: true,
        required: true,
        ..IndexDef::composite("Person", &["last", "first"])
    }
}

fn schema(db: &Grafeno) {
    db.write(|tx| {
        tx.create_index(NS, key())?;
        tx.create_index(NS, IndexDef::on_edges("knows", "since"))?;
        tx.create_index(
            NS,
            IndexDef {
                name: "names".into(),
                more_labels: vec!["Company".into()],
                ..IndexDef::new("Person", "n")
            },
        )?;
        tx.create_constraint(
            NS,
            Constraint::Exists {
                name: "since_required".into(),
                label: "knows".into(),
                key: "since".into(),
                edges: true,
            },
        )?;
        tx.create_constraint(
            NS,
            Constraint::Typed {
                name: "n_integer".into(),
                label: "Person".into(),
                key: "n".into(),
                of: PropertyType::Integer,
                edges: false,
            },
        )
    })
    .unwrap();
}

fn data(db: &Grafeno) -> Vec<NodeId> {
    db.write(|tx| {
        let mut ids = Vec::new();
        for n in 0..20 {
            ids.push(tx.create_node(NS, person(&format!("L{}", n % 4), &format!("F{n}"), n))?);
        }
        for pair in ids.windows(2) {
            tx.create_edge(
                NS,
                NewEdge::new(pair[0], "knows", pair[1]).prop("since", 2000),
            )?;
        }
        tx.create_edge(
            NS,
            NewEdge::new(ids[0], "knows", ids[5]).prop("since", 1990),
        )?;
        tx.create_node(NS, NewNode::new("Company").prop("n", 7))?;
        Ok(ids)
    })
    .unwrap()
}

#[test]
fn rules_hold_on_plain_api_writes() {
    let db = Grafeno::in_memory().unwrap();
    schema(&db);
    let ids = data(&db);
    for broken in [
        person("L1", "F1", 99),                    // the key again
        NewNode::new("Person").prop("last", "L9"), // no first
        person("L9", "F9", 1).prop("first", json!(null)),
        person("L9", "F9", 1).prop("n", "one"),
    ] {
        assert!(
            db.write(|tx| tx.create_node(NS, broken.clone())).is_err(),
            "{broken:?}"
        );
    }
    let mut props = grafeno::Props::new();
    props.insert("first".into(), json!(null));
    assert!(
        db.write(|tx| tx.set_node_props(NS, ids[3], props.clone()))
            .is_err()
    );
    assert!(
        db.write(|tx| tx.remove_node_props(NS, ids[3], &["last"]))
            .is_err()
    );
    // An edge without its required property, or with it as null.
    assert!(
        db.write(|tx| tx.create_edge(NS, NewEdge::new(ids[1], "knows", ids[9])))
            .is_err()
    );
    // Deferred: the record may break the rule until the check.
    db.write(|tx| {
        tx.defer_checks();
        let id = tx.create_node(NS, NewNode::new("Person").prop("last", "L9"))?;
        let mut first = grafeno::Props::new();
        first.insert("first".into(), json!("F9"));
        tx.set_node_props(NS, id, first)?;
        tx.check_deferred()
    })
    .unwrap();
    let failed = db.write(|tx| {
        tx.defer_checks();
        tx.create_node(NS, NewNode::new("Person").prop("last", "L8"))?;
        tx.check_deferred()
    });
    assert!(failed.is_err());

    // Lookups.
    let read = db.begin_read().unwrap();
    let found = read
        .index_seek_nodes(NS, &key().name, &[json!("L1")], None, usize::MAX)
        .unwrap();
    assert_eq!(found.len(), 5);
    let one = read
        .index_seek_nodes(
            NS,
            &key().name,
            &[json!("L1")],
            Some(&IndexRange::Equal(json!("F5"))),
            usize::MAX,
        )
        .unwrap();
    assert_eq!(one, vec![ids[5]]);
    let edges = read
        .index_seek_edges(
            NS,
            "knows_since",
            &[],
            Some(&IndexRange::Equal(json!(1990))),
            usize::MAX,
        )
        .unwrap();
    assert_eq!(edges.len(), 1);
    let seven = read
        .index_seek_nodes(
            NS,
            "names",
            &[],
            Some(&IndexRange::Equal(json!(7))),
            usize::MAX,
        )
        .unwrap();
    assert_eq!(seven.len(), 2, "a person and a company");
    assert!(
        read.index_seek_nodes(NS, "knows_since", &[], None, 1)
            .is_err()
    );
    assert!(read.check(NS).unwrap().is_ok());
}

#[test]
fn replicated_entries_and_exports_carry_them() {
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    schema(&leader);
    data(&leader);
    leader
        .write(|tx| {
            let edges = tx.index_seek_edges(
                NS,
                "knows_since",
                &[],
                Some(&IndexRange::Equal(json!(1990))),
                1,
            )?;
            let mut props = grafeno::Props::new();
            props.insert("since".into(), json!(1991));
            tx.set_edge_props(NS, edges[0], props)
        })
        .unwrap();

    // A follower applying the entries ends with the same indexes and rules.
    let follower = Grafeno::in_memory().unwrap();
    let entries = leader
        .begin_read()
        .unwrap()
        .log_entries(0, usize::MAX)
        .unwrap();
    follower.apply_entries(&entries).unwrap();
    let (a, b) = (leader.begin_read().unwrap(), follower.begin_read().unwrap());
    assert_eq!(a.indexes(NS).unwrap(), b.indexes(NS).unwrap());
    assert_eq!(a.constraints(NS).unwrap(), b.constraints(NS).unwrap());
    assert!(b.check(NS).unwrap().is_ok());
    let moved = b
        .index_seek_edges(
            NS,
            "knows_since",
            &[],
            Some(&IndexRange::Equal(json!(1991))),
            usize::MAX,
        )
        .unwrap();
    assert_eq!(moved.len(), 1);
    // And it enforces them on its own writes.
    assert!(
        follower
            .write(|tx| tx.create_node(NS, person("L0", "F0", 1)))
            .is_err()
    );

    // An export brings the indexes, keys included, to a new database.
    let mut dump = Vec::new();
    a.export(&[NS.to_owned()], &mut dump).unwrap();
    let copy = Grafeno::in_memory().unwrap();
    copy.import(dump.as_slice()).unwrap();
    let read = copy.begin_read().unwrap();
    assert_eq!(read.indexes(NS).unwrap(), a.indexes(NS).unwrap());
    assert!(read.check(NS).unwrap().is_ok());
}
