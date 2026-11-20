//! A memory another one replaced (`supersedes`, true now) ranks below it,
//! whoever recorded either; once the replacement stops being true, it does
//! not.

use grafeno::{Actor, Grafeno, NewEdge, NewNode, NodeId, Timestamp};
use grafeno_memory::{RecallQuery, Signal, recall};

const NS: &str = "agent";

fn actor(name: &str) -> Option<Actor> {
    Some(Actor {
        name: name.into(),
        own_only: true,
        view: None,
    })
}

/// An old decision, recorded by one agent, and its replacement, recorded
/// by another. The old one matches the question better, word for word.
fn decisions() -> (Grafeno, NodeId, NodeId, grafeno::EdgeId) {
    let db = Grafeno::in_memory().unwrap();
    let old = db
        .write(|tx| {
            tx.act_as(actor("agent-a"));
            tx.create_node(
                NS,
                NewNode::new("Decision").text("The deploy runs on Fridays at noon"),
            )
        })
        .unwrap();
    let (new, edge) = db
        .write(|tx| {
            tx.act_as(actor("agent-b"));
            let new = tx.create_node(
                NS,
                NewNode::new("Decision").text("Deploys moved to Tuesday mornings"),
            )?;
            let edge = tx.create_edge(NS, NewEdge::new(new, "supersedes", old))?;
            Ok((new, edge))
        })
        .unwrap();
    (db, old, new, edge)
}

fn ranked(db: &Grafeno, query: &RecallQuery) -> Vec<NodeId> {
    let read = db.begin_read().unwrap();
    recall(&read, NS, query)
        .unwrap()
        .into_iter()
        .map(|r| r.node.id)
        .collect()
}

#[test]
fn a_replaced_memory_ranks_below_its_replacement() {
    let (db, old, new, _) = decisions();
    let query = RecallQuery::new().text("deploy Fridays noon").limit(5);
    let order = ranked(&db, &query);
    let at = |id| order.iter().position(|x| *x == id);
    assert!(at(new).is_some(), "{order:?}");
    assert!(at(old).is_some(), "still recalled: {order:?}");
    assert!(at(new) < at(old), "{order:?}");

    // It says so.
    let read = db.begin_read().unwrap();
    let results = recall(&read, NS, &query).unwrap();
    let replaced = results.iter().find(|r| r.node.id == old).unwrap();
    assert!(
        replaced
            .signals
            .iter()
            .any(|s| matches!(s, Signal::Superseded { by, .. } if by == &vec![new])),
        "{:?}",
        replaced.signals
    );

    // Weight 1 leaves the ranking as it was: the better match first.
    let as_it_was = RecallQuery {
        superseded_weight: 1.0,
        ..query
    };
    let order = ranked(&db, &as_it_was);
    assert_eq!(order.first(), Some(&old), "{order:?}");
}

#[test]
fn a_replacement_no_longer_true_lowers_nothing() {
    let (db, old, _, edge) = decisions();
    db.write(|tx| {
        tx.act_as(actor("agent-b"));
        tx.invalidate_edge(NS, edge, Timestamp::now())
    })
    .unwrap();
    let order = ranked(
        &db,
        &RecallQuery::new().text("deploy Fridays noon").limit(5),
    );
    assert_eq!(order.first(), Some(&old), "{order:?}");
}
