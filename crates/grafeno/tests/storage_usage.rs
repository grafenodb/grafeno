//! `Grafeno::storage_usage` and `ReadTx::namespace_usage`: what a file and
//! its namespaces take, from redb's statistics and from the records.

use grafeno::{Grafeno, NewEdge, NewNode};

#[test]
fn tables_file_and_namespaces_add_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("usage.redb");
    let db = Grafeno::open(&path).unwrap();
    db.write(|tx| {
        let mut previous = None;
        for i in 0..2_000i64 {
            let id = tx.create_node(
                "a",
                NewNode::new("Item")
                    .prop("i", i)
                    .text(format!("item number {i}")),
            )?;
            if let Some(previous) = previous {
                tx.create_edge("a", NewEdge::new(previous, "next", id))?;
            }
            previous = Some(id);
        }
        tx.create_node("b", NewNode::new("Other"))?;
        Ok(())
    })
    .unwrap();
    let usage = db.storage_usage().unwrap();
    let nodes = usage.tables.iter().find(|t| t.name == "nodes").unwrap();
    assert_eq!(nodes.entries, 2_001);
    assert!(nodes.stored_bytes > 0);
    assert_eq!(
        usage.stored_bytes,
        usage.tables.iter().map(|t| t.stored_bytes).sum::<u64>()
    );
    let (file, allocated, free) = (
        usage.file_bytes.unwrap(),
        usage.allocated_bytes.unwrap(),
        usage.free_bytes.unwrap(),
    );
    assert_eq!(file, std::fs::metadata(&path).unwrap().len());
    assert_eq!(free, file - allocated);
    // Largest first.
    assert!(
        usage
            .tables
            .windows(2)
            .all(|w| w[0].stored_bytes >= w[1].stored_bytes)
    );

    let read = db.begin_read().unwrap();
    let a = read.namespace_usage("a").unwrap();
    assert_eq!((a.nodes, a.edges, a.texts), (2_000, 1_999, 2_000));
    assert!(a.node_bytes > 2_000 * 100 && a.text_bytes > 0 && a.changes > 0);
    let b = read.namespace_usage("b").unwrap();
    assert_eq!((b.nodes, b.edges), (1, 0));
    drop(read);

    // Deleting gives pages back to the file's free space.
    db.write(|tx| {
        for node in tx.nodes_page("a", None, None, 10_000)? {
            tx.delete_node("a", node.id)?;
        }
        Ok(())
    })
    .unwrap();
    let after = db.storage_usage().unwrap();
    assert!(after.stored_bytes < usage.stored_bytes);
}

#[test]
fn in_memory_has_no_file() {
    let db = Grafeno::in_memory().unwrap();
    let usage = db.storage_usage().unwrap();
    assert!(usage.file_bytes.is_none() && usage.free_bytes.is_none());
    assert!(!usage.tables.is_empty());
}
