//! Keeping indexes as records change: an index whose properties did not
//! change keeps its entries untouched, the indexes read once per
//! transaction follow every change to their definitions, and a built
//! index is the same whether its entries were inserted in record order or
//! key order.

use grafeno::{Grafeno, IndexDef, NewNode, Props};
use serde_json::json;

fn fulltext(name: &str, label: &str, key: &str) -> IndexDef {
    serde_json::from_value(json!({
        "name": name, "label": label, "key": key, "fulltext": {"analyzer": "english"}
    }))
    .unwrap()
}

#[test]
fn an_unrelated_change_leaves_the_indexes_as_they_were() {
    let db = Grafeno::in_memory().unwrap();
    let id = db
        .write(|tx| {
            tx.create_index("n", fulltext("body_text", "Doc", "body"))?;
            tx.create_index("n", IndexDef::new("Doc", "code"))?;
            tx.create_node(
                "n",
                NewNode::new("Doc")
                    .prop("code", "D1")
                    .prop("body", "graphs remember what agents learn"),
            )
        })
        .unwrap();
    let entries = |db: &Grafeno| {
        let usage = db.storage_usage().unwrap();
        usage
            .tables
            .iter()
            .find(|t| t.name == "index_entries")
            .unwrap()
            .entries
    };
    let search = |db: &Grafeno, q: &str| {
        db.begin_read()
            .unwrap()
            .fulltext_search("n", "body_text", q, 10, false)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.node, hit.score))
            .collect::<Vec<_>>()
    };
    let before = (entries(&db), search(&db, "agents"));
    // Unrelated properties, an embedding: the same entries and scores.
    db.write(|tx| {
        let mut props = Props::new();
        props.insert("seen".into(), json!(3));
        tx.set_node_props("n", id, props)?;
        tx.set_embedding("n", id, &[1.0, 0.0, 0.5])
    })
    .unwrap();
    assert_eq!((entries(&db), search(&db, "agents")), before);
    // The indexed text changes: its entries do.
    db.write(|tx| {
        let mut props = Props::new();
        props.insert("body".into(), json!("palaces keep memories"));
        tx.set_node_props("n", id, props)
    })
    .unwrap();
    assert!(search(&db, "agents").is_empty());
    assert_eq!(search(&db, "palaces").len(), 1);
    // A label it covers goes: the entries go with it.
    db.write(|tx| {
        tx.add_label("n", id, "Archived")?;
        tx.remove_label("n", id, "Doc")
    })
    .unwrap();
    assert!(search(&db, "palaces").is_empty());
    assert!(db.begin_read().unwrap().check("n").unwrap().is_ok());
}

#[test]
fn indexes_read_once_follow_changes_in_the_same_transaction() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        // Read (and kept) before the index exists…
        tx.create_node("n", NewNode::new("P").prop("k", 1))?;
        tx.create_index("n", IndexDef::new("P", "k"))?;
        // …then a write after it is created must index.
        tx.create_node("n", NewNode::new("P").prop("k", 2))?;
        tx.drop_index("n", "P_k")?;
        tx.create_node("n", NewNode::new("P").prop("k", 3))?;
        tx.create_index("n", IndexDef::new("P", "k"))?;
        tx.create_node("n", NewNode::new("P").prop("k", 4))?;
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    for k in 1..=4 {
        let found = read
            .index_lookup("n", "P_k", &grafeno::IndexRange::Equal(json!(k)), 10)
            .unwrap();
        assert_eq!(found.len(), 1, "k = {k}");
    }
    assert!(read.check("n").unwrap().is_ok());
}

#[test]
fn building_an_index_holds_every_entry_once() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        for i in 0..3_000i64 {
            tx.create_node(
                "n",
                NewNode::new("Doc")
                    .prop("code", format!("C{}", (i * 7919) % 3_000))
                    .prop("body", format!("word{} word{} shared", i % 50, i % 7)),
            )?;
        }
        tx.create_index("n", IndexDef::new("Doc", "code"))?;
        tx.create_index("n", fulltext("body_text", "Doc", "body"))?;
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.check("n").unwrap().is_ok());
    let hits = read
        .fulltext_search("n", "body_text", "shared", 5_000, false)
        .unwrap();
    assert_eq!(hits.len(), 3_000);
    let one = read
        .index_lookup(
            "n",
            "Doc_code",
            &grafeno::IndexRange::Equal(json!("C42")),
            10,
        )
        .unwrap();
    assert_eq!(one.len(), 1);
}

#[test]
fn rebuilding_indexes_writes_nothing_to_the_change_log() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_index("n", IndexDef::new("P", "k"))?;
        for i in 0..200i64 {
            tx.create_node("n", NewNode::new("P").prop("k", i))?;
        }
        Ok(())
    })
    .unwrap();
    let logged = |db: &Grafeno| {
        db.begin_read()
            .unwrap()
            .changes("n", 0, 100_000)
            .unwrap()
            .changes
            .len()
    };
    let before = logged(&db);
    db.write(|tx| tx.rebuild_indexes("n")).unwrap();
    assert_eq!(logged(&db), before);
    assert!(db.begin_read().unwrap().check("n").unwrap().is_ok());
}
