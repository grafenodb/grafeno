//! Event subscriptions (16.10): off by default; when on, the change log
//! entries a subscription asks for carry the values, and nothing else does.

use grafeno::{Delivery, EventTarget, Grafeno, NewEdge, NewNode, Props, Subscription};
use serde_json::json;

const NS: &str = "e";

fn props(value: serde_json::Value) -> Props {
    value.as_object().unwrap().clone()
}

#[test]
fn off_by_default() {
    let db = Grafeno::in_memory().unwrap();
    assert!(!db.events_enabled());
    let refused = db
        .write(|tx| tx.create_subscription(NS, Subscription::new("all", vec![EventTarget::Nodes])));
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains("[events] enabled")
    );
    db.write(|tx| tx.create_node(NS, NewNode::new("A").prop("x", 1)))
        .unwrap();
    let page = db.begin_read().unwrap().changes(NS, 0, 10).unwrap();
    assert_eq!(page.changes.len(), 1);
    assert!(page.changes[0].values.is_none());
}

#[test]
fn values_for_what_subscriptions_ask() {
    let db = Grafeno::in_memory().unwrap();
    db.enable_events(true);
    db.write(|tx| {
        tx.create_subscription(
            NS,
            Subscription {
                labels: vec!["Person".into()],
                keys: vec!["city".into()],
                ..Subscription::new("moves", vec![EventTarget::Properties])
            },
        )?;
        tx.create_subscription(
            NS,
            Subscription {
                vector_values: true,
                delivery: Delivery::Webhook {
                    url: "http://127.0.0.1:9/hook".into(),
                    secret: None,
                },
                ..Subscription::new(
                    "everything",
                    vec![
                        EventTarget::Nodes,
                        EventTarget::Edges,
                        EventTarget::Text,
                        EventTarget::Vectors,
                    ],
                )
            },
        )
    })
    .unwrap();
    let (ana, acme) = db
        .write(|tx| {
            let ana = tx.create_node(
                NS,
                NewNode::new("Person")
                    .prop("name", "Ana")
                    .prop("city", "Lima"),
            )?;
            let acme = tx.create_node(NS, NewNode::new("Company").prop("name", "Acme"))?;
            tx.create_edge(NS, NewEdge::new(ana, "works_at", acme))?;
            Ok((ana, acme))
        })
        .unwrap();
    // A move, and in the same commit a second change folded into it.
    db.write(|tx| {
        tx.set_node_props(NS, ana, props(json!({"city": "Cusco"})))?;
        tx.set_node_props(NS, ana, props(json!({"age": 40})))?;
        tx.set_text(NS, ana, "Ana moved to Cusco")?;
        tx.set_embedding(NS, ana, &[0.6, 0.8])
    })
    .unwrap();
    // A change the first subscription does not follow (not city).
    db.write(|tx| tx.set_node_props(NS, ana, props(json!({"age": 41}))))
        .unwrap();
    db.write(|tx| tx.delete_node(NS, acme)).unwrap();

    let read = db.begin_read().unwrap();
    let moves = read.events(NS, "moves", 0, 100).unwrap();
    assert_eq!(
        moves.changes.len(),
        2,
        "the creation and the move: {moves:?}"
    );
    let moved = &moves.changes[1].values.as_ref().unwrap();
    assert_eq!(moved.before.as_ref().unwrap()["city"], "Lima");
    assert_eq!(moved.after.as_ref().unwrap()["city"], "Cusco");
    assert_eq!(moved.changed, vec!["age".to_owned(), "city".to_owned()]);
    assert_eq!(
        moved.text.as_deref(),
        Some("Ana moved to Cusco"),
        "the other subscription asked"
    );

    let all = read.events(NS, "everything", 0, 100).unwrap();
    let kinds: Vec<(bool, String)> = all
        .changes
        .iter()
        .map(|c| (c.edge.is_some(), format!("{:?}", c.op)))
        .collect();
    assert!(kinds.contains(&(true, "Created".into())));
    let vector = all
        .changes
        .iter()
        .find_map(|c| c.values.as_ref().and_then(|v| v.vector.clone()))
        .expect("the vector's values");
    assert!((vector[0] - 0.6).abs() < 1e-6);
    let deleted = all
        .changes
        .iter()
        .find(|c| c.node == Some(acme) && format!("{:?}", c.op) == "Deleted")
        .unwrap();
    let values = deleted.values.as_ref().unwrap();
    assert_eq!(values.before.as_ref().unwrap()["name"], "Acme");
    assert!(values.after.is_none());
    // The change log itself shows no values.
    assert!(
        read.changes(NS, 0, 100)
            .unwrap()
            .changes
            .iter()
            .all(|c| c.values.is_none())
    );
    assert!(read.events(NS, "nothing", 0, 10).is_err());
}

#[test]
fn replicated_with_their_values() {
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    leader.enable_events(true);
    leader
        .write(|tx| {
            tx.create_subscription(NS, Subscription::new("nodes", vec![EventTarget::Nodes]))
        })
        .unwrap();
    leader
        .write(|tx| tx.create_node(NS, NewNode::new("A").prop("x", 1)))
        .unwrap();
    // A follower whose events are off still keeps what the leader recorded.
    let follower = Grafeno::in_memory().unwrap();
    follower
        .apply_entries(
            &leader
                .begin_read()
                .unwrap()
                .log_entries(0, usize::MAX)
                .unwrap(),
        )
        .unwrap();
    let events = follower
        .begin_read()
        .unwrap()
        .events(NS, "nodes", 0, 10)
        .unwrap();
    assert_eq!(events.changes.len(), 1);
    assert_eq!(
        events.changes[0]
            .values
            .as_ref()
            .unwrap()
            .after
            .as_ref()
            .unwrap()["x"],
        1
    );
}
