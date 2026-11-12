use std::thread::sleep;
use std::time::Duration;

use grafeno::{
    Direction, EdgeFilter, Error, Grafeno, NewEdge, NewNode, NodeId, Props, Timestamp, Value,
};

const NS: &str = "test";

fn note(db: &Grafeno, title: &str) -> NodeId {
    db.write(|tx| tx.create_node(NS, NewNode::new("Note").prop("title", title)))
        .unwrap()
}

fn link(db: &Grafeno, from: NodeId, to: NodeId) {
    db.write(|tx| tx.create_edge(NS, NewEdge::new(from, "links_to", to)))
        .unwrap();
}

#[test]
fn create_and_read_nodes() {
    let db = Grafeno::in_memory().unwrap();
    let id = db
        .write(|tx| {
            tx.create_node(
                NS,
                NewNode::new("Note")
                    .label("Pinned")
                    .label("Note")
                    .prop("title", "A"),
            )
        })
        .unwrap();

    let read = db.begin_read().unwrap();
    let node = read.node(NS, id).unwrap().unwrap();
    assert_eq!(node.labels, vec!["Note", "Pinned"]);
    assert_eq!(node.prop("title"), Some(&Value::from("A")));
    assert_eq!(node.display_name(), "A");
    assert!(node.has_label("Pinned"));
    assert_eq!(read.nodes_with_label(NS, "Pinned").unwrap(), vec![id]);
    assert!(
        read.node("other", id).unwrap().is_none(),
        "namespaces are isolated"
    );
}

#[test]
fn failed_write_is_rolled_back() {
    let db = Grafeno::in_memory().unwrap();
    let result: Result<(), Error> = db.write(|tx| {
        tx.create_node(NS, NewNode::new("Note"))?;
        Err(Error::Invalid("abort".into()))
    });
    assert!(result.is_err());
    assert!(db.begin_read().unwrap().nodes(NS).unwrap().is_empty());
}

#[test]
fn invalid_input_is_rejected() {
    let db = Grafeno::in_memory().unwrap();
    let a = note(&db, "A");
    let missing = NodeId::new();
    let mut tx = db.begin_write().unwrap();
    assert!(matches!(
        tx.create_node("", NewNode::new("Note")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        tx.create_node(NS, NewNode::new(" x")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        tx.create_edge(NS, NewEdge::new(a, "links_to", missing)),
        Err(Error::NodeNotFound(id)) if id == missing
    ));
    assert!(matches!(
        tx.create_edge(
            NS,
            NewEdge::new(a, "x", a)
                .valid_from(Timestamp::from_secs(5))
                .valid_to(Timestamp::from_secs(1))
        ),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn neighbors_by_direction_and_label() {
    let db = Grafeno::in_memory().unwrap();
    let (a, b, c) = (note(&db, "A"), note(&db, "B"), note(&db, "C"));
    link(&db, a, b);
    db.write(|tx| tx.create_edge(NS, NewEdge::new(c, "mentions", a)))
        .unwrap();

    let read = db.begin_read().unwrap();
    let out = read
        .neighbors(NS, a, Direction::Out, &EdgeFilter::any())
        .unwrap();
    assert_eq!(out.iter().map(|n| n.node).collect::<Vec<_>>(), vec![b]);

    let incoming = read
        .neighbors(NS, a, Direction::In, &EdgeFilter::any())
        .unwrap();
    assert_eq!(incoming.iter().map(|n| n.node).collect::<Vec<_>>(), vec![c]);

    let both = read
        .neighbors(NS, a, Direction::Both, &EdgeFilter::any())
        .unwrap();
    assert_eq!(both.len(), 2);

    let mentions = read
        .neighbors(NS, a, Direction::Both, &EdgeFilter::any().label("mentions"))
        .unwrap();
    assert_eq!(mentions.len(), 1);
    assert_eq!(mentions[0].node, c);
    assert_eq!(mentions[0].edge.label, "mentions");
}

#[test]
fn self_loop_is_reported_once() {
    let db = Grafeno::in_memory().unwrap();
    let a = note(&db, "A");
    link(&db, a, a);
    let read = db.begin_read().unwrap();
    assert_eq!(
        read.neighbors(NS, a, Direction::Both, &EdgeFilter::any())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn props_merge_and_labels() {
    let db = Grafeno::in_memory().unwrap();
    let a = note(&db, "A");
    db.write(|tx| {
        let mut props = Props::new();
        props.insert("title".into(), Value::Null);
        props.insert("score".into(), Value::from(3));
        tx.set_node_props(NS, a, props)?;
        tx.add_label(NS, a, "Archived")?;
        tx.remove_label(NS, a, "Note")
    })
    .unwrap();

    let read = db.begin_read().unwrap();
    let node = read.node(NS, a).unwrap().unwrap();
    // A null is a value: the key stays.
    assert_eq!(node.prop("title"), Some(&Value::Null));
    assert_eq!(node.prop("score"), Some(&Value::from(3)));
    assert_eq!(node.labels, vec!["Archived"]);
    assert!(read.nodes_with_label(NS, "Note").unwrap().is_empty());
    assert_eq!(read.nodes_with_label(NS, "Archived").unwrap(), vec![a]);
}

#[test]
fn deleting_a_node_removes_its_edges() {
    let db = Grafeno::in_memory().unwrap();
    let (a, b, c) = (note(&db, "A"), note(&db, "B"), note(&db, "C"));
    link(&db, a, b);
    link(&db, c, b);
    db.write(|tx| tx.delete_node(NS, b)).unwrap();

    let read = db.begin_read().unwrap();
    assert!(read.node(NS, b).unwrap().is_none());
    assert!(read.edges(NS).unwrap().is_empty());
    assert!(
        read.neighbors(NS, a, Direction::Both, &EdgeFilter::any())
            .unwrap()
            .is_empty()
    );
    assert!(
        read.nodes_with_label(NS, "Note")
            .unwrap()
            .iter()
            .all(|id| *id != b)
    );
}

#[test]
fn valid_time_filters_facts() {
    let db = Grafeno::in_memory().unwrap();
    let ana = db
        .write(|tx| tx.create_node(NS, NewNode::new("Person")))
        .unwrap();
    let acme = db
        .write(|tx| tx.create_node(NS, NewNode::new("Company")))
        .unwrap();
    let edge = db
        .write(|tx| {
            tx.create_edge(
                NS,
                NewEdge::new(ana, "works_at", acme).valid_from(Timestamp::from_secs(100)),
            )
        })
        .unwrap();
    db.write(|tx| tx.invalidate_edge(NS, edge, Timestamp::from_secs(200)))
        .unwrap();

    let read = db.begin_read().unwrap();
    let at = |secs| {
        read.neighbors(
            NS,
            ana,
            Direction::Out,
            &EdgeFilter::any().valid_at(Timestamp::from_secs(secs)),
        )
        .unwrap()
        .len()
    };
    assert_eq!(at(50), 0);
    assert_eq!(at(150), 1);
    assert_eq!(at(200), 0);
    // Without `valid_at`, the fact is still part of current knowledge.
    assert_eq!(
        read.neighbors(NS, ana, Direction::Out, &EdgeFilter::any())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn retracted_facts_stay_in_history() {
    let db = Grafeno::in_memory().unwrap();
    let (a, b) = (note(&db, "A"), note(&db, "B"));
    link(&db, a, b);
    sleep(Duration::from_millis(2));
    let before = Timestamp::now();
    sleep(Duration::from_millis(2));
    let edge = db.begin_read().unwrap().edges(NS).unwrap()[0].id;
    db.write(|tx| tx.retract_edge(NS, edge)).unwrap();

    let read = db.begin_read().unwrap();
    assert!(
        read.neighbors(NS, a, Direction::Out, &EdgeFilter::any())
            .unwrap()
            .is_empty()
    );
    let then = read
        .neighbors(NS, a, Direction::Out, &EdgeFilter::any().known_at(before))
        .unwrap();
    assert_eq!(then.len(), 1);
    assert!(then[0].edge.is_retracted());
}

#[test]
fn k_hop_and_shortest_path() {
    let db = Grafeno::in_memory().unwrap();
    let ids: Vec<NodeId> = ["A", "B", "C", "D"].iter().map(|t| note(&db, t)).collect();
    let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
    link(&db, a, b);
    link(&db, b, c);
    link(&db, c, d);
    link(&db, a, c);

    let read = db.begin_read().unwrap();
    let any = EdgeFilter::any();
    let hops = read.k_hop(NS, a, 2, Direction::Out, &any).unwrap();
    assert_eq!(hops.len(), 3);
    assert_eq!(hops.iter().find(|(n, _)| *n == d).unwrap().1, 2);

    let path = read
        .shortest_path(NS, a, d, 5, Direction::Out, &any)
        .unwrap()
        .unwrap();
    assert_eq!(path.nodes, vec![a, c, d]);
    assert_eq!(path.edges.len(), 2);

    assert!(
        read.shortest_path(NS, d, a, 5, Direction::Out, &any)
            .unwrap()
            .is_none()
    );
    assert!(
        read.shortest_path(NS, d, a, 5, Direction::Both, &any)
            .unwrap()
            .is_some()
    );
    assert!(
        read.shortest_path(NS, a, d, 1, Direction::Out, &any)
            .unwrap()
            .is_none()
    );
}

#[test]
fn subgraph_around_seeds() {
    let db = Grafeno::in_memory().unwrap();
    let ids: Vec<NodeId> = ["A", "B", "C", "D"].iter().map(|t| note(&db, t)).collect();
    link(&db, ids[0], ids[1]);
    link(&db, ids[1], ids[2]);
    link(&db, ids[2], ids[3]);

    let read = db.begin_read().unwrap();
    let sub = read
        .subgraph(NS, &[ids[1]], 1, Direction::Both, &EdgeFilter::any())
        .unwrap();
    let mut nodes: Vec<NodeId> = sub.nodes.iter().map(|n| n.id).collect();
    nodes.sort();
    let mut expected = vec![ids[0], ids[1], ids[2]];
    expected.sort();
    assert_eq!(nodes, expected);
    assert_eq!(sub.edges.len(), 2);
}

#[test]
fn data_survives_reopening() {
    let path = std::env::temp_dir().join(format!("grafeno-test-{}.redb", NodeId::new()));
    let id = {
        let db = Grafeno::open(&path).unwrap();
        db.write(|tx| {
            tx.create_node(
                NS,
                NewNode::new("Note")
                    .prop("title", "persisted")
                    .text("persisted text")
                    .embedding(vec![1.0, 0.0]),
            )
        })
        .unwrap()
    };
    {
        let db = Grafeno::open(&path).unwrap();
        let read = db.begin_read().unwrap();
        let node = read.node(NS, id).unwrap().unwrap();
        assert_eq!(node.prop("title"), Some(&Value::from("persisted")));
        assert_eq!(read.search_text(NS, "persisted", 1).unwrap()[0].node, id);
        assert_eq!(read.search_vector(NS, &[1.0, 0.1], 1).unwrap()[0].node, id);
    }
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn upsert_creates_then_replaces_with_client_ids() {
    let db = Grafeno::in_memory().unwrap();
    let id = NodeId::from_u128(0x0123_4567_89ab_4cde_8f01_2345_6789_abcd);
    let other = note(&db, "Other");
    let created = db
        .write(|tx| {
            let created = tx.upsert_node(
                NS,
                id,
                NewNode::new("Note")
                    .label("Draft")
                    .prop("title", "First")
                    .text("first version")
                    .embedding(vec![1.0, 0.0]),
            )?;
            tx.create_edge(NS, NewEdge::new(id, "links_to", other))?;
            Ok(created)
        })
        .unwrap();
    assert!(created);
    let created_at = db
        .begin_read()
        .unwrap()
        .node(NS, id)
        .unwrap()
        .unwrap()
        .created_at;

    let created = db
        .write(|tx| {
            tx.upsert_node(
                NS,
                id,
                NewNode::new("Note")
                    .prop("title", "Second")
                    .text("second version"),
            )
        })
        .unwrap();
    assert!(!created);

    let read = db.begin_read().unwrap();
    let node = read.node(NS, id).unwrap().unwrap();
    assert_eq!(node.labels, vec!["Note"]);
    assert_eq!(node.display_name(), "Second");
    assert_eq!(node.created_at, created_at);
    assert!(read.nodes_with_label(NS, "Draft").unwrap().is_empty());
    assert!(read.search_text(NS, "first", 5).unwrap().is_empty());
    assert_eq!(read.search_text(NS, "second", 5).unwrap()[0].node, id);
    assert!(
        read.embedding(NS, id).unwrap().is_none(),
        "a missing embedding is removed"
    );
    assert_eq!(
        read.neighbors(NS, id, Direction::Out, &EdgeFilter::any())
            .unwrap()
            .len(),
        1,
        "edges are kept"
    );
}

#[test]
fn delete_edges_from_replaces_derived_links() {
    let db = Grafeno::in_memory().unwrap();
    let (a, b, c) = (note(&db, "A"), note(&db, "B"), note(&db, "C"));
    link(&db, a, b);
    link(&db, a, c);
    link(&db, b, a);
    db.write(|tx| tx.create_edge(NS, NewEdge::new(a, "mentions", b)))
        .unwrap();

    let deleted = db
        .write(|tx| tx.delete_edges_from(NS, a, Some("links_to")))
        .unwrap();
    assert_eq!(deleted, 2);
    let read = db.begin_read().unwrap();
    let out = read
        .neighbors(NS, a, Direction::Out, &EdgeFilter::any())
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].edge.label, "mentions");
    assert_eq!(
        read.neighbors(NS, a, Direction::In, &EdgeFilter::any())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn degree_counts_the_edges_a_filter_keeps_up_to_a_cap() {
    let db = Grafeno::in_memory().unwrap();
    let hub = note(&db, "hub");
    let others: Vec<NodeId> = (0..10).map(|i| note(&db, &format!("n{i}"))).collect();
    for other in &others[..6] {
        link(&db, hub, *other);
    }
    for other in &others[6..] {
        link(&db, *other, hub);
    }
    let retracted = db
        .write(|tx| tx.create_edge(NS, NewEdge::new(hub, "cites", others[0])))
        .unwrap();
    db.write(|tx| tx.retract_edge(NS, retracted)).unwrap();

    let read = db.begin_read().unwrap();
    let any = EdgeFilter::any();
    let degree =
        |direction, filter: &EdgeFilter, cap| read.degree(NS, hub, direction, filter, cap).unwrap();
    assert_eq!(degree(Direction::Out, &any, 100), 6);
    assert_eq!(degree(Direction::In, &any, 100), 4);
    assert_eq!(degree(Direction::Both, &any, 100), 10);
    assert_eq!(degree(Direction::Both, &any, 7), 7);
    assert_eq!(
        degree(Direction::Out, &EdgeFilter::any().label("cites"), 100),
        0
    );
    assert_eq!(
        degree(Direction::Both, &any, 100),
        read.neighbor_ids(NS, hub, Direction::Both, &any)
            .unwrap()
            .len()
    );
}
