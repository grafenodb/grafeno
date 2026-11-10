//! Null as a stored value (16.2): kept by every write that sets
//! properties, removed only by name, never in an index, never a
//! uniqueness conflict, and carried by exports and replication.

use grafeno::{Grafeno, IndexDef, IndexRange, NewEdge, NewNode, Props};
use serde_json::{Value, json};

const NS: &str = "n";

fn props(pairs: &[(&str, Value)]) -> Props {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[test]
fn nulls_are_kept_and_removed_by_name() {
    let db = Grafeno::in_memory().unwrap();
    let (a, e) = db
        .write(|tx| {
            let a = tx.create_node(NS, NewNode::new("P").prop("x", 1).prop("y", Value::Null))?;
            let b = tx.create_node(NS, NewNode::new("P"))?;
            let e = tx.create_edge(NS, NewEdge::new(a, "r", b).prop("w", 1))?;
            tx.set_node_props(NS, a, props(&[("x", Value::Null), ("z", json!(2))]))?;
            tx.set_edge_props(NS, e, props(&[("w", Value::Null)]))?;
            Ok((a, e))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let node = read.node(NS, a).unwrap().unwrap();
    assert_eq!(
        node.props,
        props(&[("x", Value::Null), ("y", Value::Null), ("z", json!(2))])
    );
    assert_eq!(read.edge(NS, e).unwrap().unwrap().props["w"], Value::Null);
    drop(read);

    db.write(|tx| {
        tx.remove_node_props(NS, a, &["x", "y", "missing"])?;
        tx.remove_edge_props(NS, e, &["w"])
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(
        read.node(NS, a).unwrap().unwrap().props,
        props(&[("z", json!(2))])
    );
    assert!(read.edge(NS, e).unwrap().unwrap().props.is_empty());
    assert!(read.check(NS).unwrap().is_ok());
}

#[test]
fn indexes_hold_no_nulls() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_index(NS, IndexDef::new("P", "x"))?;
        let mut unique = IndexDef::new("P", "email");
        unique.unique = true;
        unique.name = IndexDef::default_name("P", "email", true);
        tx.create_index(NS, unique)?;
        // Several nulls under a unique index are no conflict.
        for i in 0..3 {
            tx.create_node(
                NS,
                NewNode::new("P")
                    .prop("x", Value::Null)
                    .prop("email", Value::Null)
                    .prop("i", i),
            )?;
        }
        tx.create_node(NS, NewNode::new("P").prop("x", 5).prop("email", "a@b"))?;
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let found = read
        .index_lookup(NS, "P_x", &IndexRange::Equal(json!(5)), 100)
        .unwrap();
    assert_eq!(found.len(), 1);
    let info = read.indexes(NS).unwrap();
    let entries: u64 = info.iter().find(|i| i.def.name == "P_x").unwrap().entries;
    assert_eq!(entries, 1, "the nulls are not in the index");
    let report = read.check(NS).unwrap();
    assert!(report.is_ok(), "{report:?}");
    drop(read);
    // A non-null duplicate is still refused.
    let duplicate = db.write(|tx| {
        tx.create_node(NS, NewNode::new("P").prop("email", "a@b"))
            .map(|_| ())
    });
    assert!(duplicate.is_err());
}

#[test]
fn exports_carry_nulls() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_node(NS, NewNode::new("P").prop("x", Value::Null).prop("y", 1))
            .map(|_| ())
    })
    .unwrap();
    let mut out = Vec::new();
    db.begin_read()
        .unwrap()
        .export(&[NS.to_owned()], &mut out)
        .unwrap();
    let copy = Grafeno::in_memory().unwrap();
    copy.import(out.as_slice()).unwrap();
    let nodes = copy.begin_read().unwrap().nodes(NS).unwrap();
    assert_eq!(
        nodes[0].props,
        props(&[("x", Value::Null), ("y", json!(1))])
    );
}
