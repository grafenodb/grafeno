//! Knowledge over time (phase 10): a decision that changed several times,
//! with its history and the reasons of each change; two states compared;
//! a change of state told apart from a contradiction.

use grafeno::{
    Constraint, Grafeno, NewEdge, NewNode, NodeId, OnConflict, Scope as Within, Timestamp,
};
use grafeno_memory::history::REASON;
use grafeno_memory::knowledge::{CONTRADICTS, DECISION, EVIDENCE, SUPERSEDES, SUPPORTED_BY};
use grafeno_memory::{ReflectOptions, Scope, compare, history, reflect};

const NS: &str = "agent";

fn day(year: i64, month: i64, day: i64) -> Timestamp {
    Timestamp::from_civil(year, month, day).unwrap()
}

/// Three versions of the decision on where invoices are stored, each
/// replacing the one before with a reason and, for the last, a measurement
/// as evidence.
fn decisions(db: &Grafeno) -> [NodeId; 4] {
    db.write(|tx| {
        let mut decision =
            |text: &str| tx.create_node(NS, NewNode::new(DECISION).text(text.to_owned()));
        let first = decision("Store invoices in one file per month")?;
        let second = decision("Store invoices in the ledger database")?;
        let third = decision("Store invoices in the ledger database, one table per year")?;
        let measure = tx.create_node(
            NS,
            NewNode::new(EVIDENCE).text("Queries over one table took 4 s at 30 million rows"),
        )?;
        tx.create_edge(
            NS,
            NewEdge::new(second, SUPERSEDES, first)
                .prop(REASON, "monthly files could not be searched"),
        )?;
        tx.create_edge(
            NS,
            NewEdge::new(third, SUPERSEDES, second)
                .prop(REASON, "one table grew too slow")
                .source(measure),
        )?;
        tx.create_edge(NS, NewEdge::new(third, SUPPORTED_BY, measure))?;
        Ok([first, second, third, measure])
    })
    .unwrap()
}

#[test]
fn a_decision_that_changed_keeps_its_history_and_reasons() {
    let db = Grafeno::in_memory().unwrap();
    let [first, second, third, measure] = decisions(&db);
    let read = db.begin_read().unwrap();
    // From any version, the same line.
    for from in [first, second, third] {
        let line = history(&read, NS, from).unwrap();
        assert_eq!(line.current, third);
        let ids: Vec<NodeId> = line.versions.iter().map(|v| v.id).collect();
        assert_eq!(ids, [first, second, third]);
        assert_eq!(line.versions[0].reason, None);
        assert_eq!(
            line.versions[1].reason.as_deref(),
            Some("monthly files could not be searched")
        );
        assert_eq!(
            line.versions[2].reason.as_deref(),
            Some("one table grew too slow")
        );
        assert_eq!(line.versions[2].evidence, vec![measure]);
        assert!(line.versions[1].supersedes_fact.is_some());
        assert!(line.branches.is_empty());
    }
    // A memory nothing replaced is a line of one.
    let line = history(&read, NS, measure).unwrap();
    assert_eq!(line.current, measure);
    assert_eq!(line.versions.len(), 1);
}

#[test]
fn two_states_are_compared_and_a_change_told_from_an_end() {
    let db = Grafeno::in_memory().unwrap();
    let (billing, files, ledger, cache, ana) = db
        .write(|tx| {
            let mut node = |label: &str, name: &str| {
                tx.create_node(NS, NewNode::new(label).prop("name", name.to_owned()))
            };
            let billing = node("Service", "billing")?;
            let files = node("Store", "monthly files")?;
            let ledger = node("Database", "ledger")?;
            let cache = node("Cache", "invoice cache")?;
            let ana = node("Person", "Ana")?;
            // The store changed: files until March, the ledger since.
            tx.create_edge(
                NS,
                NewEdge::new(billing, "stores_in", files)
                    .valid_from(day(2025, 1, 1))
                    .valid_to(day(2026, 3, 1)),
            )?;
            tx.create_edge(
                NS,
                NewEdge::new(billing, "stores_in", ledger).valid_from(day(2026, 3, 1)),
            )?;
            // The cache ended, and nothing took its place.
            tx.create_edge(
                NS,
                NewEdge::new(billing, "uses", cache)
                    .valid_from(day(2025, 1, 1))
                    .valid_to(day(2026, 2, 1)),
            )?;
            // Ana began maintaining it; nobody did before.
            tx.create_edge(
                NS,
                NewEdge::new(ana, "maintains", billing).valid_from(day(2026, 4, 1)),
            )?;
            // A fact true at both times.
            tx.create_edge(NS, NewEdge::new(billing, "part_of", ledger))?;
            Ok((billing, files, ledger, cache, ana))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let comparison = compare(&read, NS, &[billing], day(2025, 6, 1), day(2026, 6, 1)).unwrap();
    assert_eq!(comparison.kept, 1);
    assert_eq!(comparison.changed.len(), 1);
    let change = &comparison.changed[0];
    assert_eq!(change.before[0].to, files);
    assert_eq!(change.after[0].to, ledger);
    assert_eq!(comparison.ended.len(), 1);
    assert_eq!(comparison.ended[0].to, cache);
    assert_eq!(comparison.began.len(), 1);
    assert_eq!(comparison.began[0].from, ana);
    // The same time twice: nothing changed.
    let same = compare(&read, NS, &[billing], day(2026, 6, 1), day(2026, 6, 1)).unwrap();
    assert!(same.changed.is_empty() && same.ended.is_empty() && same.began.is_empty());
    assert_eq!(same.kept, 3);
}

#[test]
fn reflection_tells_a_change_of_state_from_a_contradiction() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        // One host per service: a newer host ends the older one.
        tx.create_constraint(
            NS,
            Constraint::Edge {
                name: "one_host".into(),
                label: "runs_on".into(),
                scope: Within::Source,
                on_conflict: OnConflict::Invalidate,
            },
        )?;
        // One owner per service, and a clash waits for someone.
        tx.create_constraint(
            NS,
            Constraint::Edge {
                name: "one_owner".into(),
                label: "owned_by".into(),
                scope: Within::Source,
                on_conflict: OnConflict::Flag,
            },
        )?;
        Ok(())
    })
    .unwrap();
    let ids = db
        .write(|tx| {
            let mut node = |label: &str, name: &str| {
                tx.create_node(NS, NewNode::new(label).prop("name", name.to_owned()))
            };
            let billing = node("Service", "billing")?;
            let old_host = node("Host", "old host")?;
            let new_host = node("Host", "new host")?;
            let ana = node("Person", "Ana")?;
            let bo = node("Person", "Bo")?;
            let said = tx.create_node(NS, NewNode::new("Note").text("billing never went down"))?;
            let seen = tx.create_node(NS, NewNode::new("Note").text("billing went down twice"))?;
            tx.create_edge(NS, NewEdge::new(billing, "runs_on", old_host))?;
            tx.create_edge(NS, NewEdge::new(billing, "runs_on", new_host))?;
            tx.create_edge(NS, NewEdge::new(billing, "owned_by", ana))?;
            tx.create_edge(NS, NewEdge::new(billing, "owned_by", bo))?;
            tx.create_edge(NS, NewEdge::new(said, "about", billing))?;
            tx.create_edge(NS, NewEdge::new(seen, "about", billing))?;
            tx.create_edge(NS, NewEdge::new(seen, CONTRADICTS, said))?;
            Ok(billing)
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let reflection = reflect(&read, NS, &Scope::Node(ids), &ReflectOptions::default()).unwrap();
    let kinds: Vec<(&str, &str)> = reflection
        .conflicts
        .iter()
        .map(|clash| (clash.kind, clash.action.as_str()))
        .collect();
    assert!(kinds.contains(&("change", "invalidated")), "{kinds:?}");
    assert!(kinds.contains(&("contradiction", "flagged")), "{kinds:?}");
    assert!(kinds.contains(&("contradiction", "recorded")), "{kinds:?}");
    // Unsettled first.
    assert!(!reflection.conflicts[0].settled);
    assert!(
        reflection
            .limits
            .notes
            .iter()
            .any(|note| note.contains("different sources"))
    );
}
