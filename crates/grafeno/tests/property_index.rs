//! Property indexes: lookups, upkeep on every write, uniqueness, check,
//! rebuild, export and import.

use std::ops::Bound;

use grafeno::{Error, Grafeno, IndexDef, IndexRange, NewNode, NodeId, Props, Value};
use serde_json::json;

const NS: &str = "idx";

fn def(label: &str, key: &str, unique: bool) -> IndexDef {
    IndexDef {
        name: IndexDef::default_name(label, key, unique),
        label: label.into(),
        key: key.into(),
        unique,
        fuzzy: false,
        ..IndexDef::default()
    }
}

fn people(db: &Grafeno) -> Vec<NodeId> {
    db.write(|tx| {
        let mut ids = Vec::new();
        for (name, age) in [
            ("Ana", 34),
            ("Bob", 28),
            ("Carla", 41),
            ("Dan", 19),
            ("Eve", 34),
        ] {
            ids.push(tx.create_node(
                NS,
                NewNode::new("Person").prop("name", name).prop("age", age),
            )?);
        }
        // Same property, other label: not in the index.
        tx.create_node(NS, NewNode::new("Robot").prop("name", "R2").prop("age", 50))?;
        Ok(ids)
    })
    .unwrap()
}

fn lookup(db: &Grafeno, name: &str, range: IndexRange) -> Vec<NodeId> {
    db.begin_read()
        .unwrap()
        .index_lookup(NS, name, &range, usize::MAX)
        .unwrap()
}

#[test]
fn lookups_by_value_range_and_prefix() {
    let db = Grafeno::in_memory().unwrap();
    let ids = people(&db);
    let filled = db
        .write(|tx| tx.create_index(NS, def("Person", "age", false)))
        .unwrap();
    assert_eq!(filled, 5);
    db.write(|tx| tx.create_index(NS, def("Person", "name", false)))
        .unwrap();

    let mut ages = lookup(&db, "Person_age", IndexRange::Equal(json!(34)));
    ages.sort();
    let mut expected = vec![ids[0], ids[4]];
    expected.sort();
    assert_eq!(ages, expected);
    // Ranges come back in value order; 34.0 equals 34.
    assert_eq!(
        lookup(
            &db,
            "Person_age",
            IndexRange::Between {
                lower: Bound::Included(json!(30)),
                upper: Bound::Unbounded
            }
        )
        .len(),
        3
    );
    assert_eq!(
        lookup(
            &db,
            "Person_age",
            IndexRange::Between {
                lower: Bound::Unbounded,
                upper: Bound::Included(json!(28.0))
            }
        ),
        vec![ids[3], ids[1]]
    );
    assert_eq!(
        lookup(&db, "Person_name", IndexRange::Prefix("Ca".into())),
        vec![ids[2]]
    );
    // A string range does not reach numbers, and a mixed range finds nothing.
    assert!(lookup(&db, "Person_age", IndexRange::Equal(json!("34"))).is_empty());
    assert!(
        lookup(
            &db,
            "Person_age",
            IndexRange::Between {
                lower: Bound::Included(json!(1)),
                upper: Bound::Included(json!("z"))
            }
        )
        .is_empty()
    );

    let read = db.begin_read().unwrap();
    let infos = read.indexes(NS).unwrap();
    assert_eq!(
        infos
            .iter()
            .map(|i| (i.def.name.as_str(), i.entries))
            .collect::<Vec<_>>(),
        [("Person_age", 5), ("Person_name", 5)]
    );
    assert!(read.check(NS).unwrap().is_ok());
}

#[test]
fn writes_keep_indexes_current() {
    let db = Grafeno::in_memory().unwrap();
    let ids = people(&db);
    db.write(|tx| tx.create_index(NS, def("Person", "age", false)))
        .unwrap();
    db.write(|tx| {
        let mut props = Props::new();
        props.insert("age".into(), Value::from(35));
        tx.set_node_props(NS, ids[0], props)?;
        tx.remove_label(NS, ids[1], "Person").err(); // the only label: refused
        tx.add_label(NS, ids[1], "Former")?;
        tx.remove_label(NS, ids[1], "Person")?;
        tx.delete_node(NS, ids[2])?;
        tx.create_node(NS, NewNode::new("Person").prop("age", 35))?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        lookup(&db, "Person_age", IndexRange::Equal(json!(34))),
        vec![ids[4]]
    );
    assert_eq!(
        lookup(&db, "Person_age", IndexRange::Equal(json!(35))).len(),
        2
    );
    assert!(
        lookup(&db, "Person_age", IndexRange::Equal(json!(28))).is_empty(),
        "no longer a Person"
    );
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());

    assert!(db.write(|tx| tx.drop_index(NS, "Person_age")).unwrap());
    assert!(!db.write(|tx| tx.drop_index(NS, "Person_age")).unwrap());
    assert!(lookup(&db, "Person_age", IndexRange::Equal(json!(35))).is_empty());
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn unique_indexes_refuse_duplicates() {
    let db = Grafeno::in_memory().unwrap();
    people(&db);
    // Ana and Eve are both 34.
    assert!(
        matches!(db.write(|tx| tx.create_index(NS, def("Person", "age", true))), Err(Error::Invalid(m)) if m.contains("both have 34"))
    );
    db.write(|tx| tx.create_index(NS, def("Person", "name", true)))
        .unwrap();
    let error = db
        .write(|tx| tx.create_node(NS, NewNode::new("Person").prop("name", "Ana")))
        .unwrap_err();
    assert!(error.to_string().contains("must be unique"), "{error}");
    // Another label may reuse the value, and so may the same node.
    db.write(|tx| tx.create_node(NS, NewNode::new("Pet").prop("name", "Ana")))
        .unwrap();
    let ana = lookup(&db, "Person_name_unique", IndexRange::Equal(json!("Ana")))[0];
    db.write(|tx| {
        let mut props = Props::new();
        props.insert("nick".into(), Value::from("A"));
        tx.set_node_props(NS, ana, props)
    })
    .unwrap();
    // Same definition again: kept; a clash: refused.
    assert_eq!(
        db.write(|tx| tx.create_index(NS, def("Person", "name", true)))
            .unwrap(),
        5
    );
    let clash = IndexDef {
        name: "other".into(),
        ..def("Person", "name", true)
    };
    assert!(db.write(|tx| tx.create_index(NS, clash)).is_err());
}

#[test]
fn indexes_survive_rebuild_and_export() {
    let db = Grafeno::in_memory().unwrap();
    people(&db);
    db.write(|tx| tx.create_index(NS, def("Person", "age", false)))
        .unwrap();
    db.write(|tx| tx.rebuild_indexes(NS)).unwrap();
    assert_eq!(
        lookup(&db, "Person_age", IndexRange::Equal(json!(34))).len(),
        2
    );
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());

    let mut dump = Vec::new();
    db.begin_read().unwrap().export(&[], &mut dump).unwrap();
    assert!(String::from_utf8_lossy(&dump).contains(r#""type":"index""#));
    let copy = Grafeno::in_memory().unwrap();
    copy.import(dump.as_slice()).unwrap();
    assert_eq!(
        lookup(&copy, "Person_age", IndexRange::Equal(json!(34))).len(),
        2
    );
    assert!(copy.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn fuzzy_indexes_find_what_looks_alike() {
    use grafeno::textsim::Measure;
    let db = Grafeno::in_memory().unwrap();
    let ids = db
        .write(|tx| {
            let mut ids = Vec::new();
            for name in [
                "John Smith",
                "Jane Smith",
                "Jonathan Smithers",
                "María José García",
                "Pedro López",
            ] {
                ids.push(tx.create_node(NS, NewNode::new("Person").prop("name", name))?);
            }
            ids.push(
                tx.create_node(
                    NS,
                    NewNode::new("Person")
                        .prop("name", "Robert Brown")
                        .prop("aliases", vec!["Bobby Brown"]),
                )?,
            );
            // Another label is not in the index.
            tx.create_node(NS, NewNode::new("Company").prop("name", "John Smith Ltd"))?;
            Ok(ids)
        })
        .unwrap();
    let entries = db
        .write(|tx| tx.create_index(NS, IndexDef::fuzzy("Person", "name")))
        .unwrap();
    assert!(entries > 20, "trigram entries: {entries}");
    db.write(|tx| tx.create_index(NS, IndexDef::fuzzy("Person", "aliases")))
        .unwrap();
    let read = db.begin_read().unwrap();
    let search = |key: &str, text: &str| {
        read.fuzzy_search(NS, "Person", key, text, Measure::Dice { n: 3 }, 3)
            .unwrap()
    };
    // A typo, accents, case, word order.
    assert_eq!(search("name", "jonh smith")[0].node, ids[0]);
    assert_eq!(search("name", "MARIA JOSE GARCIA")[0].node, ids[3]);
    assert_eq!(search("name", "garcia maria jose")[0].node, ids[3]);
    assert!(search("name", "jonh smith")[0].score > 0.5);
    assert_eq!(search("aliases", "bobby brwn")[0].node, ids[5]);
    assert!(search("name", "zzzz").is_empty());
    let winkler = read
        .fuzzy_search(NS, "Person", "name", "jon smith", Measure::JaroWinkler, 2)
        .unwrap();
    assert_eq!(winkler[0].node, ids[0]);
    assert!(
        read.fuzzy_search(NS, "Person", "age", "x", Measure::Exact, 1)
            .is_err(),
        "no fuzzy index there"
    );
    assert!(read.check(NS).unwrap().is_ok());
    drop(read);

    // Entries follow the node.
    db.write(|tx| {
        let mut props = grafeno::Props::new();
        props.insert("name".into(), serde_json::json!("Pedro Lopes"));
        tx.set_node_props(NS, ids[4], props)
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let hits = read
        .fuzzy_search(NS, "Person", "name", "pedro lopes", Measure::Exact, 1)
        .unwrap();
    assert_eq!((hits[0].node, hits[0].score), (ids[4], 1.0));
    assert!(read.check(NS).unwrap().is_ok());
    let mut dump = Vec::new();
    read.export(&[], &mut dump).unwrap();
    drop(read);
    let copy = Grafeno::in_memory().unwrap();
    copy.import(dump.as_slice()).unwrap();
    let read = copy.begin_read().unwrap();
    assert!(read.index_defs(NS).unwrap().iter().any(|def| def.fuzzy));
    assert_eq!(
        read.fuzzy_search(
            NS,
            "Person",
            "name",
            "jonh smith",
            Measure::Dice { n: 3 },
            1
        )
        .unwrap()[0]
            .node,
        ids[0]
    );
    assert!(read.check(NS).unwrap().is_ok());
    drop(read);

    // Never unique; an exact index can sit beside it.
    let unique = IndexDef {
        unique: true,
        ..IndexDef::fuzzy("Person", "name")
    };
    assert!(db.write(|tx| tx.create_index(NS, unique)).is_err());
    db.write(|tx| tx.create_index(NS, IndexDef::new("Person", "name")))
        .unwrap();
}
