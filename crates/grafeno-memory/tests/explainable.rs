//! Explainable retrieval (phase 9): a question that needs four memories
//! joined, walks narrowed by relation and by time, how each memory was
//! found kept apart from what supports it, the next part of a walk, and
//! related memories packed together.

use grafeno::{Grafeno, NewEdge, NewNode, NodeId, Timestamp};
use grafeno_memory::knowledge::{CONTRADICTS, DEPENDS_ON, EVIDENCE, SUPPORTED_BY};
use grafeno_memory::{
    ContextOptions, NewRoom, RecallQuery, WalkQuery, anchor, context_for, create_palace,
    create_room, find_locus, walk,
};

const NS: &str = "agent";

/// An incident anchored in the room `Incidents`, and the chain behind it:
/// incident -hit-> service -depends_on-> store <-maintains- ana (since
/// 2026-03-01), who took over from bo (2025-01-01 to 2026-03-01). A log
/// supports the incident, a hypothesis contradicts it, and a note about the
/// service is related by `about`.
struct Fixture {
    db: Grafeno,
    incident: NodeId,
    service: NodeId,
    store: NodeId,
    ana: NodeId,
    bo: NodeId,
    log: NodeId,
    hypothesis: NodeId,
    note: NodeId,
}

fn day(year: i64, month: i64, day: i64) -> Timestamp {
    Timestamp::from_civil(year, month, day).unwrap()
}

fn fixture() -> Fixture {
    let db = Grafeno::in_memory().unwrap();
    let room = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Operations", None)?.id;
            Ok(create_room(tx, NS, palace, NewRoom::new("Incidents").topic("incidents"))?.id)
        })
        .unwrap();
    let entrance = find_locus(&db.begin_read().unwrap(), NS, room, "entrance")
        .unwrap()
        .unwrap();
    let ids = db
        .write(|tx| {
            let mut node = |label: &str, text: &str| {
                tx.create_node(NS, NewNode::new(label).text(text.to_owned()))
            };
            let incident = node("Experience", "The billing outage stopped the night jobs")?;
            let service = node("Service", "The invoice service sends the bills")?;
            let store = node("Database", "The ledger store keeps the bills")?;
            let ana = node("Person", "Ana keeps stores running")?;
            let bo = node("Person", "Bo kept stores running")?;
            let log = node(EVIDENCE, "The job log of that night")?;
            let hypothesis = node("Hypothesis", "Nothing stopped: the jobs were late")?;
            let note = node("Note", "A note on how the invoice service retries")?;
            tx.create_edge(NS, NewEdge::new(incident, "hit", service))?;
            tx.create_edge(NS, NewEdge::new(service, DEPENDS_ON, store))?;
            tx.create_edge(
                NS,
                NewEdge::new(ana, "maintains", store).valid_from(day(2026, 3, 1)),
            )?;
            tx.create_edge(
                NS,
                NewEdge::new(bo, "maintains", store)
                    .valid_from(day(2025, 1, 1))
                    .valid_to(day(2026, 3, 1)),
            )?;
            tx.create_edge(NS, NewEdge::new(incident, SUPPORTED_BY, log))?;
            tx.create_edge(NS, NewEdge::new(hypothesis, CONTRADICTS, incident))?;
            tx.create_edge(NS, NewEdge::new(note, "about", service))?;
            anchor(tx, NS, incident, entrance, None)?;
            Ok([incident, service, store, ana, bo, log, hypothesis, note])
        })
        .unwrap();
    let [incident, service, store, ana, bo, log, hypothesis, note] = ids;
    Fixture {
        db,
        incident,
        service,
        store,
        ana,
        bo,
        log,
        hypothesis,
        note,
    }
}

fn chain_walk(f: &Fixture, at: Timestamp) -> grafeno_memory::Walk {
    let query = WalkQuery::from(f.incident)
        .hops(3)
        .relation("hit")
        .relation(DEPENDS_ON)
        .relation("maintains")
        .valid_at(at)
        .budget(2_000);
    walk(&f.db.begin_read().unwrap(), NS, &query).unwrap()
}

#[test]
fn four_memories_are_joined_and_each_says_how_it_was_found() {
    let f = fixture();
    let found = chain_walk(&f, day(2026, 6, 1));
    let ids: Vec<NodeId> = found.memories.iter().map(|m| m.id).collect();
    for needed in [f.incident, f.service, f.store, f.ana] {
        assert!(ids.contains(&needed), "{ids:?}");
    }
    // Only the relations asked for are followed, and only facts true then.
    for left in [f.bo, f.log, f.hypothesis, f.note] {
        assert!(!ids.contains(&left), "{ids:?}");
    }
    let ana = found.memories.iter().find(|m| m.id == f.ana).unwrap();
    assert_eq!(ana.hops, 3);
    assert_eq!(ana.origin, f.incident);
    let path: Vec<&str> = ana.via.iter().map(|hop| hop.label.as_str()).collect();
    assert_eq!(path, ["hit", DEPENDS_ON, "maintains"]);
    assert!(ana.found_by.contains("relation"), "{}", ana.found_by);
    for memory in &found.memories {
        assert!(!memory.found_by.is_empty());
    }

    // The context packs the four, and says its count is an estimate.
    let context = found.context.unwrap();
    for needed in [f.incident, f.service, f.store, f.ana] {
        assert!(context.items.iter().any(|item| item.node == needed));
    }
    assert!(context.tokens_estimated);
    assert!(context.tokens > 0);
}

#[test]
fn a_walk_at_an_earlier_time_follows_the_facts_true_then() {
    let f = fixture();
    let ids: Vec<NodeId> = chain_walk(&f, day(2025, 6, 1))
        .memories
        .iter()
        .map(|m| m.id)
        .collect();
    assert!(ids.contains(&f.bo), "{ids:?}");
    assert!(!ids.contains(&f.ana), "{ids:?}");
}

#[test]
fn what_supports_a_memory_is_kept_apart_from_how_it_was_found() {
    let f = fixture();
    let found = walk(
        &f.db.begin_read().unwrap(),
        NS,
        &WalkQuery::from(f.incident).hops(1),
    )
    .unwrap();
    let incident = found.memories.iter().find(|m| m.id == f.incident).unwrap();
    assert_eq!(incident.justification.supported_by, vec![f.log]);
    assert_eq!(incident.justification.contradicted_by, vec![f.hypothesis]);
    assert!(!incident.justification.archived);

    let json = serde_json::to_value(incident).unwrap();
    assert!(json["found_by"].is_string());
    let justification = json["justification"].as_object().unwrap();
    // The factual side names records; the algorithm's side stays out of it.
    for key in ["found_by", "via", "relevance", "hops", "strength"] {
        assert!(!justification.contains_key(key), "{key}");
    }
}

#[test]
fn skip_gives_the_next_part_of_a_walk() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();
    let ids = |query: WalkQuery| -> Vec<NodeId> {
        walk(&read, NS, &query)
            .unwrap()
            .memories
            .iter()
            .map(|m| m.id)
            .collect()
    };
    let whole = ids(WalkQuery::from(f.incident).hops(3).limit(6));
    let first = ids(WalkQuery::from(f.incident).hops(3).limit(3));
    let next = ids(WalkQuery::from(f.incident).hops(3).limit(3).skip(3));
    assert_eq!(whole.len(), 6);
    assert_eq!([first, next].concat(), whole);
}

#[test]
fn related_memories_are_packed_together() {
    let db = Grafeno::in_memory().unwrap();
    let [a1, a2, b1, b2] = db
        .write(|tx| {
            let mut node =
                |text: &str| tx.create_node(NS, NewNode::new("Note").text(text.to_owned()));
            let a1 = node("zeta zeta zeta zeta first pair")?;
            let b1 = node("zeta zeta zeta second pair")?;
            let a2 = node("zeta zeta first pair again")?;
            let b2 = node("zeta second pair again")?;
            tx.create_edge(NS, NewEdge::new(a1, "related_to", a2))?;
            tx.create_edge(NS, NewEdge::new(b1, "related_to", b2))?;
            Ok([a1, a2, b1, b2])
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let query = RecallQuery::new().text("zeta").limit(10).expand(None);
    let mut options = ContextOptions::with_budget(2_000);
    options.group = true;
    let context = context_for(&read, NS, &query, &options).unwrap();
    let at = |id: NodeId| {
        context
            .items
            .iter()
            .position(|item| item.node == id)
            .unwrap()
    };
    assert_eq!(at(a1).abs_diff(at(a2)), 1);
    assert_eq!(at(b1).abs_diff(at(b2)), 1);
}
