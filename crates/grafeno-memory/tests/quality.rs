//! The quality of knowledge (phase 12): on a topic with incomplete,
//! contradictory and out-of-date information, a reflection tells the three
//! apart; it names the sources behind what it lists, counts records by the
//! evidence behind them, keeps what was stated with no source apart, and
//! finds what an expected shape asks for and a memory lacks.

use std::collections::BTreeMap;

use grafeno::{Grafeno, NewEdge, NewNode, NodeId, Timestamp};
use grafeno_memory::knowledge::{CONTRADICTS, DECISION, EVIDENCE, SUPPORTED_BY};
use grafeno_memory::{ReflectOptions, SHAPE, Scope, Shape, reflect, shapes};
use serde_json::json;

const NS: &str = "agent";

struct Topic {
    db: Grafeno,
    /// A decision with its reason and its evidence.
    complete: NodeId,
    /// A decision with neither: incomplete.
    incomplete: NodeId,
    /// Two notes recorded as saying opposite things.
    said: NodeId,
    seen: NodeId,
    /// A fact that ended.
    ended: grafeno::EdgeId,
    /// The evidence behind the complete decision.
    log: NodeId,
    /// Stated, with no source.
    claim: NodeId,
}

fn topic() -> Topic {
    let db = Grafeno::in_memory().unwrap();
    let ids = db
        .write(|tx| {
            let mut node = |label: &str, text: &str, props: serde_json::Value| {
                let mut new = NewNode::new(label).text(text.to_owned());
                if let serde_json::Value::Object(props) = props {
                    for (key, value) in props {
                        new = new.prop(key, value);
                    }
                }
                tx.create_node(NS, new)
            };
            let log = node(EVIDENCE, "invoice job log: retries took 40 s", json!({}))?;
            let complete = node(
                DECISION,
                "invoice retries back off for a minute",
                json!({ "reason": "the bank limits calls", "origin": "verified" }),
            )?;
            let incomplete = node(DECISION, "invoice files are kept for a year", json!({}))?;
            let said = node(
                "Note",
                "invoice runs never failed",
                json!({ "origin": "stated" }),
            )?;
            let seen = node("Note", "invoice runs failed twice in May", json!({}))?;
            let claim = node(
                "Note",
                "invoice totals are rounded per line",
                json!({ "origin": "stated" }),
            )?;
            let bank = node("Entity", "the invoice bank", json!({}))?;
            tx.create_edge(NS, NewEdge::new(complete, SUPPORTED_BY, log))?;
            tx.create_edge(NS, NewEdge::new(seen, CONTRADICTS, said))?;
            let ended = tx.create_edge(
                NS,
                NewEdge::new(complete, "uses", bank)
                    .valid_from(Timestamp::from_civil(2024, 1, 1).unwrap())
                    .valid_to(Timestamp::from_civil(2025, 1, 1).unwrap()),
            )?;
            // What a Decision is expected to have, declared once.
            tx.create_node(
                NS,
                NewNode::new(SHAPE)
                    .prop("kind", DECISION)
                    .prop("relations", json!([SUPPORTED_BY]))
                    .prop("properties", json!(["reason"])),
            )?;
            Ok((complete, incomplete, said, seen, ended, log, claim))
        })
        .unwrap();
    let (complete, incomplete, said, seen, ended, log, claim) = ids;
    Topic {
        db,
        complete,
        incomplete,
        said,
        seen,
        ended,
        log,
        claim,
    }
}

fn ids(items: &[grafeno_memory::Item]) -> Vec<String> {
    items.iter().map(|item| item.id.clone()).collect()
}

#[test]
fn incomplete_contradictory_and_out_of_date_are_told_apart() {
    let t = topic();
    let read = t.db.begin_read().unwrap();
    let reflection = reflect(
        &read,
        NS,
        &Scope::Topic("invoice".into()),
        &ReflectOptions::default(),
    )
    .unwrap();
    assert!(!reflection.nothing);
    // The shape node is a declaration, not a memory.
    assert_eq!(reflection.known.memories_by_kind.get("Shape"), None);

    // Incomplete: the decision with neither reason nor evidence, and only
    // it.
    let gap = reflection
        .gaps
        .iter()
        .find(|item| item.id == t.incomplete.to_string())
        .expect("the incomplete decision is a gap");
    assert!(
        gap.why.contains("supported_by") && gap.why.contains("reason"),
        "{}",
        gap.why
    );
    assert!(!ids(&reflection.gaps).contains(&t.complete.to_string()));

    // Contradictory: the two notes, as a contradiction, not settled.
    let clash = &reflection.conflicts[0];
    assert_eq!(clash.kind, "contradiction");
    assert!(!clash.settled);
    assert_eq!(clash.new_fact.id, t.seen.to_string());
    assert_eq!(clash.old_fact.id, t.said.to_string());

    // Out of date: the fact that ended, and nothing else of these.
    assert!(ids(&reflection.stale).contains(&t.ended.to_string()));
    for id in [t.incomplete, t.said, t.seen] {
        assert!(!ids(&reflection.stale).contains(&id.to_string()));
    }

    // Stated with no source: its own list, apart from the uncertain.
    assert!(ids(&reflection.unsupported).contains(&t.claim.to_string()));
    assert!(!ids(&reflection.uncertain).contains(&t.claim.to_string()));

    // The sources behind what is listed: the verified decision's evidence.
    assert_eq!(
        reflection.sources.get(&t.complete.to_string()),
        Some(&vec![t.log.to_string()])
    );

    // Records counted once each by the evidence behind them.
    let by = &reflection.known.by_evidence;
    assert_eq!(by.get("conflicting"), Some(&2));
    assert_eq!(by.get("verified"), Some(&1));
    assert!(by.get("stated, no source").is_some_and(|n| *n >= 1));
    let total: usize = by.values().sum();
    assert_eq!(total, reflection.known.memories + reflection.known.facts);

    // What to do next names each, and nothing is claimed beyond what is
    // stored.
    let next: Vec<&str> = reflection
        .next
        .iter()
        .map(|step| step.what.as_str())
        .collect();
    assert!(
        next.iter()
            .any(|what| what.contains("stated with no source"))
    );
    assert!(next.iter().any(|what| what.contains("expected shapes")));
    assert!(
        reflection
            .limits
            .notes
            .iter()
            .any(|note| note.contains("not probabilities"))
    );
}

#[test]
fn shapes_are_declared_or_given_and_a_given_one_wins() {
    let t = topic();
    let read = t.db.begin_read().unwrap();
    let declared = shapes(&read, NS).unwrap();
    assert_eq!(declared[DECISION].relations, [SUPPORTED_BY]);
    assert_eq!(declared[DECISION].properties, ["reason"]);

    // Given in the call, a shape replaces the declared one for its kind:
    // here a decision needs only a reason, so the incomplete one lacks
    // just that, and a Note needs a `checked_by`.
    let mut given = BTreeMap::new();
    given.insert(
        DECISION.to_owned(),
        Shape {
            relations: Vec::new(),
            properties: vec!["reason".into()],
        },
    );
    given.insert(
        "Note".to_owned(),
        Shape {
            relations: vec!["checked_by".into()],
            properties: Vec::new(),
        },
    );
    let reflection = reflect(
        &read,
        NS,
        &Scope::Topic("invoice".into()),
        &ReflectOptions {
            shapes: given,
            ..ReflectOptions::default()
        },
    )
    .unwrap();
    let why = |id: NodeId| {
        reflection
            .gaps
            .iter()
            .find(|item| item.id == id.to_string())
            .map(|item| item.why.clone())
    };
    let decision = why(t.incomplete).unwrap();
    assert!(decision.contains("reason") && !decision.contains("supported_by"));
    assert!(why(t.claim).unwrap().contains("checked_by"));
}
