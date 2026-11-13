//! The palace as a view (phase 11): routes with a goal, a review that finds
//! facts recorded twice and memories related to nothing else of their room,
//! and a node's context rebuilt.

use grafeno::{Grafeno, NewEdge, NewNode, NodeId, Timestamp};
use grafeno_memory::knowledge::SUPERSEDES;
use grafeno_memory::palace::ENTRANCE;
use grafeno_memory::{
    NewRoom, Proposal, ReviewOptions, RouteQuery, RouteStep, anchor, create_palace, create_room,
    find_locus, reconstruct, review, route,
};

const NS: &str = "agent";

fn note(db: &Grafeno, text: &str) -> NodeId {
    db.write(|tx| tx.create_node(NS, NewNode::new("Note").text(text.to_owned())))
        .unwrap()
}

#[test]
fn a_route_follows_a_line_of_versions_to_its_start() {
    let db = Grafeno::in_memory().unwrap();
    let [v1, v2, v3] = [
        "one file a month",
        "the ledger",
        "the ledger, a table a year",
    ]
    .map(|text| note(&db, text));
    db.write(|tx| {
        tx.create_edge(NS, NewEdge::new(v2, SUPERSEDES, v1))?;
        tx.create_edge(NS, NewEdge::new(v3, SUPERSEDES, v2))?;
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    // From the current one back: one route, the longest.
    let back = route(
        &read,
        NS,
        &RouteQuery::new(v3, vec![RouteStep::new(SUPERSEDES).repeat()]),
    )
    .unwrap();
    assert_eq!(back.routes.len(), 1);
    assert_eq!(back.routes[0].nodes, [v3, v2, v1]);
    assert!(!back.cut);
    // From the first one forwards, against the arrows.
    let forward = route(
        &read,
        NS,
        &RouteQuery::new(v1, vec![RouteStep::new(SUPERSEDES).backward().repeat()]),
    )
    .unwrap();
    assert_eq!(forward.routes[0].nodes, [v1, v2, v3]);
    assert!(forward.routes[0].hops.iter().all(|hop| !hop.forward));
    // Fewer hops than the line: cut, and said.
    let mut short = RouteQuery::new(v3, vec![RouteStep::new(SUPERSEDES).repeat()]);
    short.max_hops = 1;
    let short = route(&read, NS, &short).unwrap();
    assert_eq!(short.routes[0].nodes, [v3, v2]);
    assert!(short.cut);
}

#[test]
fn a_route_follows_only_facts_true_then() {
    let db = Grafeno::in_memory().unwrap();
    let [store, ana, bo] = ["the ledger store", "Ana", "Bo"].map(|text| note(&db, text));
    let day = |y, m, d| Timestamp::from_civil(y, m, d).unwrap();
    db.write(|tx| {
        tx.create_edge(
            NS,
            NewEdge::new(bo, "maintains", store)
                .valid_from(day(2025, 1, 1))
                .valid_to(day(2026, 3, 1)),
        )?;
        tx.create_edge(
            NS,
            NewEdge::new(ana, "maintains", store).valid_from(day(2026, 3, 1)),
        )?;
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let who = |at: Timestamp| -> Vec<NodeId> {
        let mut query = RouteQuery::new(store, vec![RouteStep::new("maintains").backward()]);
        query.valid_at = Some(at);
        route(&read, NS, &query)
            .unwrap()
            .routes
            .iter()
            .map(|route| *route.nodes.last().unwrap())
            .collect()
    };
    assert_eq!(who(day(2025, 6, 1)), [bo]);
    assert_eq!(who(day(2026, 6, 1)), [ana]);
}

#[test]
fn a_review_finds_facts_recorded_twice_and_loose_memories() {
    let db = Grafeno::in_memory().unwrap();
    let (palace, joined, piled) = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Work", None)?.id;
            let joined = create_room(tx, NS, palace, NewRoom::new("Billing"))?.id;
            let piled = create_room(tx, NS, palace, NewRoom::new("Odds and ends"))?.id;
            Ok((palace, joined, piled))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let joined_at = find_locus(&read, NS, joined, ENTRANCE).unwrap().unwrap();
    let piled_at = find_locus(&read, NS, piled, ENTRANCE).unwrap().unwrap();
    drop(read);
    let [a, b, c, loose] =
        ["invoices", "the ledger", "the queue", "the lunch"].map(|text| note(&db, text));
    let [x, y, z] = ["a", "b", "c"].map(|text| note(&db, text));
    let (kept, twice) = db
        .write(|tx| {
            let kept = tx.create_edge(NS, NewEdge::new(a, "stored_in", b))?;
            let twice = tx.create_edge(NS, NewEdge::new(a, "stored_in", b))?;
            tx.create_edge(NS, NewEdge::new(b, "fed_by", c))?;
            for memory in [a, b, c, loose] {
                anchor(tx, NS, memory, joined_at, None)?;
            }
            for memory in [x, y, z] {
                anchor(tx, NS, memory, piled_at, None)?;
            }
            Ok((kept, twice))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let proposals = review(&read, NS, palace, &ReviewOptions::default())
        .unwrap()
        .proposals;
    assert!(
        proposals.iter().any(|p| matches!(p,
            Proposal::RedundantFacts { kept: k, duplicates, room, .. }
                if *k == kept && *duplicates == vec![twice] && *room == joined)),
        "{proposals:#?}"
    );
    assert!(proposals.iter().any(|p| matches!(p,
        Proposal::LooseMemory { memory, room, .. } if *memory == loose && *room == joined)));
    // Only the lunch is loose in Billing.
    assert_eq!(
        proposals
            .iter()
            .filter(|p| matches!(p, Proposal::LooseMemory { room, .. } if *room == joined))
            .count(),
        1
    );
    assert!(proposals.iter().any(|p| matches!(p,
        Proposal::UnconnectedRoom { room, memories: 3, .. } if *room == piled)));
}

#[test]
fn a_context_is_rebuilt_and_a_palace_is_refused() {
    let db = Grafeno::in_memory().unwrap();
    let (palace, room) = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Work", None)?.id;
            let room = create_room(tx, NS, palace, NewRoom::new("Billing"))?.id;
            Ok((palace, room))
        })
        .unwrap();
    let entrance = find_locus(&db.begin_read().unwrap(), NS, room, ENTRANCE)
        .unwrap()
        .unwrap();
    let [old, new] = ["one file a month", "the ledger"].map(|text| note(&db, text));
    db.write(|tx| {
        tx.create_edge(NS, NewEdge::new(new, SUPERSEDES, old))?;
        anchor(tx, NS, new, entrance, None)?;
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let rebuilt = reconstruct(&read, NS, new, 30).unwrap();
    assert_eq!(rebuilt.places.len(), 1);
    assert_eq!(rebuilt.places[0].room, Some(room));
    assert_eq!(rebuilt.facts.len(), 1);
    assert_eq!(rebuilt.history.unwrap().versions.len(), 2);
    assert!(!rebuilt.timeline.is_empty());
    // A room is rebuilt too, with its reflection.
    assert_eq!(
        reconstruct(&read, NS, room, 30)
            .unwrap()
            .reflection
            .known
            .memories,
        1
    );
    assert!(reconstruct(&read, NS, palace, 30).is_err());
}
