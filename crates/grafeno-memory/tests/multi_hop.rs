//! The multi-hop example as a regression test: the decision Ana's team made
//! is recalled only through the graph, and the hub everyone is linked to
//! does not bury it.

#[path = "../examples/multi_hop.rs"]
mod example;

use example::{ask, memory, three_hops};
use grafeno_memory::{Expansion, Signal};

#[test]
fn the_decision_comes_back_only_through_the_graph() {
    let memory = memory().unwrap();
    let read = memory.db.begin_read().unwrap();

    let alone = ask(&read, None).unwrap();
    assert!(alone.iter().all(|r| r.node.id != memory.decision));

    let found = ask(&read, Some(three_hops())).unwrap();
    let rank = found
        .iter()
        .position(|r| r.node.id == memory.decision)
        .expect("the decision is recalled");
    let decision = &found[rank];
    assert!(
        decision
            .signals
            .iter()
            .all(|s| matches!(s, Signal::Graph { .. } | Signal::Memory { .. })),
        "only the graph found it: {:?}",
        decision.signals
    );
    assert!(
        decision
            .signals
            .iter()
            .any(|s| matches!(s, Signal::Graph { hops: 3, .. })),
        "Ana, three hops away, adds to it"
    );
    let other = found
        .iter()
        .position(|r| r.node.id == memory.other_decision)
        .unwrap_or(usize::MAX);
    assert!(
        rank < other,
        "Ana's team's decision ranks above the other team's"
    );
    assert!(
        found[..rank]
            .iter()
            .all(|r| !r.node.explicit_name().unwrap_or("").starts_with("Employee")),
        "no one linked only through the hub ranks above it"
    );
}

#[test]
fn hubs_undamped_bury_the_decision() {
    let memory = memory().unwrap();
    let read = memory.db.begin_read().unwrap();
    let undamped = Expansion {
        hubs: 0.0,
        fanout: 0,
        ..three_hops()
    };
    let found = ask(&read, Some(undamped)).unwrap();
    assert!(found.iter().all(|r| r.node.id != memory.decision));
    let acme = found.iter().position(|r| r.node.id == memory.acme).unwrap();

    let damped = ask(&read, Some(three_hops())).unwrap();
    let damped_acme = damped
        .iter()
        .position(|r| r.node.id == memory.acme)
        .unwrap();
    assert!(damped_acme > acme, "the hub drops: {acme} → {damped_acme}");
}

#[test]
fn fanout_keeps_the_search_matches_and_caps_the_rest() {
    let memory = memory().unwrap();
    let read = memory.db.begin_read().unwrap();
    let capped = Expansion {
        fanout: 3,
        ..three_hops()
    };
    let found = ask(&read, Some(capped)).unwrap();
    // Acme's 203 neighbors are cut to 3; the chain from Ana to the
    // decision has no node with more than 3 edges.
    let employees = found
        .iter()
        .filter(|r| r.node.explicit_name().unwrap_or("").starts_with("Employee"))
        .count();
    assert!(employees <= 3, "{employees} employees");
    assert!(found.iter().any(|r| r.node.id == memory.decision));
}
