//! Merging safely: identity rules (strong keys, labels), contradictions
//! reported, merges recorded with who and when, and undone exactly.

use grafeno::{
    ConflictKind, Direction, EdgeFilter, Error, Grafeno, IndexDef, MergeOptions, NewEdge, NewNode,
    NodeId, Timestamp, Value,
};

const NS: &str = "merge";

fn person(name: &str) -> NewNode {
    NewNode::new("Person").prop("name", name)
}

fn kinds(
    db: &Grafeno,
    a: NodeId,
    b: NodeId,
    options: &MergeOptions,
) -> Vec<(ConflictKind, String)> {
    db.write(|tx| tx.merge_conflicts(NS, a, b, options))
        .unwrap()
        .into_iter()
        .map(|conflict| (conflict.kind, conflict.key))
        .collect()
}

#[test]
fn identity_and_contradictions_are_checked_before_merging() {
    let db = Grafeno::in_memory().unwrap();
    let ids = db
        .write(|tx| {
            Ok([
                // Namesakes: the same name, two emails.
                tx.create_node(NS, person("Ana García").prop("email", "ana@a.com"))?,
                tx.create_node(NS, person("Ana Garcia").prop("email", "ana.garcia@b.org"))?,
                // The same email, written differently: no conflict.
                tx.create_node(NS, person("A. García").prop("email", " ANA@a.com "))?,
                // A company with a person's name.
                tx.create_node(NS, NewNode::new("Company").prop("name", "Ana García"))?,
                // Two birth dates.
                tx.create_node(NS, person("Luis").prop("born_on", "1990-02-01"))?,
                tx.create_node(NS, person("Luis Pérez").prop("born_on", "1991-07-12"))?,
                // A label more is not a conflict.
                tx.create_node(
                    NS,
                    person("Luis")
                        .label("Engineer")
                        .prop("born_on", "1990-02-01"),
                )?,
                // An identifier under a unique constraint.
                tx.create_node(NS, person("Eva").prop("passport", "X1"))?,
                tx.create_node(NS, person("Eva").prop("passport", "Y2"))?,
            ])
        })
        .unwrap();
    db.write(|tx| {
        tx.create_index(
            NS,
            IndexDef {
                name: IndexDef::default_name("Person", "passport", true),
                label: "Person".into(),
                key: "passport".into(),
                unique: true,
                fuzzy: false,
                ..IndexDef::default()
            },
        )
        .map(|_| ())
    })
    .unwrap();
    let options = MergeOptions::default();
    assert_eq!(
        kinds(&db, ids[0], ids[1], &options),
        [(ConflictKind::StrongKey, "email".to_owned())]
    );
    assert!(
        kinds(&db, ids[0], ids[2], &options).is_empty(),
        "same email"
    );
    assert_eq!(
        kinds(&db, ids[0], ids[3], &options),
        [(ConflictKind::Labels, "labels".to_owned())]
    );
    assert_eq!(
        kinds(&db, ids[4], ids[5], &options),
        [(ConflictKind::Property, "born_on".to_owned())]
    );
    assert!(kinds(&db, ids[4], ids[6], &options).is_empty());
    assert_eq!(
        kinds(&db, ids[7], ids[8], &options),
        [(ConflictKind::StrongKey, "passport".to_owned())],
        "unique constraints make strong keys"
    );

    // A refused merge changes nothing.
    let refused = db
        .write(|tx| tx.merge_nodes(NS, ids[0], ids[1]))
        .unwrap_err();
    assert!(
        matches!(&refused, Error::Conflict(message) if message.contains("different email")),
        "{refused}"
    );
    assert!(db.begin_read().unwrap().node(NS, ids[1]).unwrap().is_some());
    // The same entity merges.
    let report = db.write(|tx| tx.merge_nodes(NS, ids[0], ids[2])).unwrap();
    assert!(report.conflicts.is_empty());
}

#[test]
fn functional_edges_conflict_when_they_point_elsewhere() {
    let db = Grafeno::in_memory().unwrap();
    let (a, b, c, lima, quito) = db
        .write(|tx| {
            let lima = tx.create_node(NS, NewNode::new("City").prop("name", "Lima"))?;
            let quito = tx.create_node(NS, NewNode::new("City").prop("name", "Quito"))?;
            let a = tx.create_node(NS, person("Bob"))?;
            let b = tx.create_node(NS, person("Robert"))?;
            let c = tx.create_node(NS, person("Bobby"))?;
            tx.create_edge(NS, NewEdge::new(a, "born_in", lima))?;
            tx.create_edge(NS, NewEdge::new(b, "born_in", quito))?;
            tx.create_edge(NS, NewEdge::new(c, "born_in", lima))?;
            // Where one lives is not functional here, and may differ.
            tx.create_edge(NS, NewEdge::new(a, "lived_in", quito))?;
            tx.create_edge(NS, NewEdge::new(c, "lived_in", lima))?;
            Ok((a, b, c, lima, quito))
        })
        .unwrap();
    let options = MergeOptions {
        functional: vec!["born_in".into()],
        ..MergeOptions::default()
    };
    let conflicts = db
        .write(|tx| tx.merge_conflicts(NS, a, b, &options))
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].kind, ConflictKind::Edge);
    assert_eq!(conflicts[0].kept, Value::from(vec![lima.to_string()]));
    assert_eq!(conflicts[0].merged, Value::from(vec![quito.to_string()]));
    assert!(kinds(&db, a, c, &options).is_empty());
    assert!(
        kinds(&db, a, b, &MergeOptions::default()).is_empty(),
        "not declared functional"
    );
    // An ended fact is not a current one.
    let edge = db
        .begin_read()
        .unwrap()
        .neighbors(NS, b, Direction::Out, &EdgeFilter::any().label("born_in"))
        .unwrap()[0]
        .edge
        .id;
    db.write(|tx| tx.invalidate_edge(NS, edge, Timestamp::from_secs(1)))
        .unwrap();
    assert!(kinds(&db, a, b, &options).is_empty());
}

#[test]
fn merges_are_recorded_and_undone_exactly() {
    let db = Grafeno::in_memory().unwrap();
    let (keep, dup, acme, note) = db
        .write(|tx| {
            let keep = tx.create_node(
                NS,
                person("Ana García")
                    .prop("city", "Lima")
                    .text("Ana writes Rust")
                    .embedding(vec![1.0, 0.0]),
            )?;
            let dup = tx.create_node(
                NS,
                person("ana garcia lopez")
                    .label("Engineer")
                    .prop("city", "Cusco")
                    .prop("age", 34)
                    .text("Ana lives in Lima")
                    .embedding(vec![0.0, 1.0]),
            )?;
            let acme = tx.create_node(NS, NewNode::new("Company").prop("name", "Acme"))?;
            let note = tx.create_node(NS, NewNode::new("Note").prop("title", "Meeting"))?;
            tx.create_edge(NS, NewEdge::new(keep, "works_at", acme))?;
            tx.create_edge(NS, NewEdge::new(dup, "works_at", acme))?;
            tx.create_edge(NS, NewEdge::new(note, "mentions", dup))?;
            tx.create_edge(NS, NewEdge::new(dup, "same_as", keep))?;
            tx.set_importance(NS, dup, 0.9)?;
            Ok((keep, dup, acme, note))
        })
        .unwrap();
    let before = {
        let read = db.begin_read().unwrap();
        (
            read.node(NS, keep).unwrap().unwrap(),
            read.node(NS, dup).unwrap().unwrap(),
            read.neighbor_ids(NS, dup, Direction::Both, &EdgeFilter::any())
                .unwrap()
                .len(),
            read.stats(NS).unwrap(),
        )
    };

    let force = MergeOptions {
        force: true,
        ..MergeOptions::default()
    };
    let report = db
        .write(|tx| {
            tx.act_as(Some(grafeno::Actor::new("curator")));
            tx.merge_nodes_with(NS, keep, dup, &force)
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let records = read.merges(NS, Some(dup)).unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(
        (record.id, record.kept, record.merged),
        (report.id, keep, dup)
    );
    assert_eq!(record.by.as_deref(), Some("curator"));
    assert!(record.forced);
    assert_eq!(record.conflicts[0].key, "city");
    assert_eq!(record.merged_before.node, before.1);
    assert_eq!(
        record.merged_before.text.as_deref(),
        Some("Ana lives in Lima")
    );
    assert_eq!((record.moved.len(), record.dropped.len()), (1, 2));
    assert!(read.merges(NS, Some(acme)).unwrap().is_empty());
    drop(read);

    let undone = db.write(|tx| tx.undo_merge(NS, report.id, false)).unwrap();
    assert!(undone.undone_at.is_some());
    let read = db.begin_read().unwrap();
    assert_eq!(read.node(NS, keep).unwrap().unwrap(), before.0);
    assert_eq!(read.node(NS, dup).unwrap().unwrap(), before.1);
    assert_eq!(
        read.text(NS, dup).unwrap().as_deref(),
        Some("Ana lives in Lima")
    );
    assert_eq!(
        read.text(NS, keep).unwrap().as_deref(),
        Some("Ana writes Rust")
    );
    assert_eq!(read.embedding(NS, dup).unwrap(), Some(vec![0.0, 1.0]));
    assert_eq!(read.search_text(NS, "lives", 5).unwrap()[0].node, dup);
    assert_eq!(
        read.find_by_name(NS, "ana garcia lopez").unwrap(),
        vec![dup]
    );
    assert_eq!(
        read.neighbor_ids(NS, dup, Direction::Both, &EdgeFilter::any())
            .unwrap()
            .len(),
        before.2,
        "every edge is back"
    );
    let incoming = read
        .neighbor_ids(NS, dup, Direction::In, &EdgeFilter::any())
        .unwrap();
    assert!(incoming.iter().any(|(_, other)| *other == note));
    assert_eq!(read.memory(NS, &before.1).unwrap().importance, 0.9);
    let stats = read.stats(NS).unwrap();
    assert_eq!((stats.nodes, stats.edges), (before.3.nodes, before.3.edges));
    assert_eq!(stats.node_labels, before.3.node_labels);
    let check = read.check(NS).unwrap();
    assert!(check.is_ok(), "{:?}", check.problems);
    drop(read);
    assert!(
        matches!(
            db.write(|tx| tx.undo_merge(NS, report.id, false)),
            Err(Error::Conflict(_))
        ),
        "once"
    );

    // Merged again, then changed: undoing would lose the change.
    let again = db
        .write(|tx| tx.merge_nodes_with(NS, keep, dup, &force))
        .unwrap();
    db.write(|tx| {
        let mut props = grafeno::Props::new();
        props.insert("role".into(), Value::from("lead"));
        tx.set_node_props(NS, keep, props)
    })
    .unwrap();
    assert!(matches!(
        db.write(|tx| tx.undo_merge(NS, again.id, false)),
        Err(Error::Conflict(message)) if message.contains("changed after the merge")
    ));
    db.write(|tx| tx.undo_merge(NS, again.id, true)).unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.node(NS, dup).unwrap().is_some());
    assert_eq!(read.merges(NS, None).unwrap().len(), 2);
    assert!(read.check(NS).unwrap().is_ok());
}
