//! GQ on hyperedges: `MATCH HYPEREDGE` (anchors, roles, `EXACT`, `OPTIONAL`,
//! time, properties, views), `CREATE HYPEREDGE`, `SET`, `REMOVE`, `DELETE`,
//! `DETACH DELETE`, `RETRACT`, `MERGE HYPEREDGE`, `participants()` and
//! `EXPLAIN`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use grafeno::{
    Account, AccountKind, Act, Grafeno, Object, Privilege, Rights, RoleDef, Timestamp, View,
};
use grafeno_query::{Limits, Params, QueryResult, Value, execute, execute_with};
use serde_json::{Value as Json, json};

const NS: &str = "team";

fn run(db: &Grafeno, query: &str) -> QueryResult {
    let result =
        execute(db, NS, query, &Params::new()).unwrap_or_else(|e| panic!("{query}\n→ {e}"));
    common::check_split(db, NS, query, &Params::new());
    result
}

fn rows(db: &Grafeno, query: &str) -> Json {
    run(db, query).to_json()["rows"].clone()
}

fn fails(db: &Grafeno, query: &str) -> String {
    match execute(db, NS, query, &Params::new()) {
        Ok(_) => panic!("{query} should fail"),
        Err(error) => error.message,
    }
}

/// Three agents and two tasks; a decision about T1 (planner proposes,
/// builder implements, checker reviews) and a review of T2 where the
/// checker is both author and reviewer, with the builder.
fn team() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (:Agent {name: 'planner'}), (:Agent {name: 'builder'}),
                (:Agent {name: 'checker'}), (:Agent {name: 'idle'}),
                (:Task {name: 'T1'}), (:Task {name: 'T2'})",
    );
    let created = run(
        &db,
        "MATCH (p:Agent {name: 'planner'}), (b:Agent {name: 'builder'}),
               (c:Agent {name: 'checker'}), (t:Task {name: 'T1'})
         CREATE HYPEREDGE h:decision {summary: 'single writer', status: 'open'}
                [(p) AS proposer, (b) AS implementer, (c) AS reviewer, (t) AS subject]
         RETURN h.summary, h.@label, type(h)",
    );
    assert_eq!(created.stats.hyperedges_created, 1);
    assert_eq!(
        created.to_json()["rows"],
        json!([["single writer", "decision", "decision"]])
    );
    run(
        &db,
        "MATCH (b:Agent {name: 'builder'}), (c:Agent {name: 'checker'}), (t:Task {name: 'T2'})
         CREATE HYPEREDGE :review [(c) AS author, (c) AS reviewer, (b), (t) AS subject]",
    );
    db
}

#[test]
fn match_binds_participants_by_role() {
    let db = team();
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision [(a:Agent) AS proposer, (t:Task)]
             RETURN h.summary, a.name, t.name"
        ),
        json!([["single writer", "planner", "T1"]])
    );
    // Without a role a member is any participant: one row per distinct node.
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision [(a:Agent)] RETURN a.name ORDER BY a.name"
        ),
        json!([["builder"], ["checker"], ["planner"]])
    );
    // A node in two roles is one row, not two.
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:review [(a:Agent)] RETURN a.name ORDER BY a.name"
        ),
        json!([["builder"], ["checker"]])
    );
    // The same variable in two roles: one node playing both.
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h [(a) AS author, (a) AS reviewer] RETURN a.name, type(h)"
        ),
        json!([["checker", "review"]])
    );
    // Two members never take one participant.
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision [(a) AS proposer, (b) AS proposer] RETURN count(*)"
        ),
        json!([[0]])
    );
    // Any of several labels; no participants named.
    assert_eq!(
        rows(&db, "MATCH HYPEREDGE h:decision|review RETURN count(h)"),
        json!([[2]])
    );
    assert_eq!(rows(&db, "MATCH HYPEREDGE h RETURN count(h)"), json!([[2]]));
    assert_eq!(
        rows(&db, "MATCH HYPEREDGE h:nothing RETURN count(h)"),
        json!([[0]])
    );
}

#[test]
fn a_bound_node_anchors_the_match() {
    let db = team();
    assert_eq!(
        rows(
            &db,
            "MATCH (c:Agent {name: 'checker'})
             MATCH HYPEREDGE h [(c) AS reviewer]
             RETURN type(h) ORDER BY type(h)"
        ),
        json!([["decision"], ["review"]])
    );
    // Two bound nodes: the hyperedges they share.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Agent {name: 'planner'}), (b:Agent {name: 'builder'})
             MATCH HYPEREDGE h [(p), (b)] RETURN type(h)"
        ),
        json!([["decision"]])
    );
    // Counting per agent, idle ones too.
    assert_eq!(
        rows(
            &db,
            "MATCH (a:Agent)
             OPTIONAL MATCH HYPEREDGE h [(a)]
             RETURN a.name, count(h) ORDER BY a.name"
        ),
        json!([["builder", 2], ["checker", 2], ["idle", 0], ["planner", 1]])
    );
    // Walking on from a participant with ordinary patterns.
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision [(t:Task) AS subject]
             OPTIONAL MATCH (t)-[r]->() RETURN t.name, count(r)"
        ),
        json!([["T1", 0]])
    );
    let plan = run(
        &db,
        "EXPLAIN MATCH (c:Agent {name: 'checker'}) MATCH HYPEREDGE h:decision [(c), (t)] RETURN h",
    );
    let text = plan.to_json().to_string();
    assert!(
        text.contains("hyperedges:decision of c (incidence index)"),
        "{text}"
    );
    let plan = run(&db, "EXPLAIN MATCH HYPEREDGE h:review RETURN h")
        .to_json()
        .to_string();
    assert!(plan.contains("label index"), "{plan}");
}

#[test]
fn exact_compares_the_whole_set_of_nodes() {
    let db = team();
    assert_eq!(
        rows(
            &db,
            "MATCH (c:Agent {name: 'checker'}), (b:Agent {name: 'builder'}), (t:Task {name: 'T2'})
             MATCH HYPEREDGE h [(c), (b), (t)] EXACT RETURN type(h)"
        ),
        json!([["review"]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (c:Agent {name: 'checker'}), (b:Agent {name: 'builder'})
             MATCH HYPEREDGE h [(c), (b)] EXACT RETURN count(h)"
        ),
        json!([[0]])
    );
    assert_eq!(
        rows(&db, "MATCH HYPEREDGE h [(), (), ()] EXACT RETURN type(h)"),
        json!([["review"]])
    );
}

#[test]
fn properties_ids_and_participants() {
    let db = team();
    let id = rows(&db, "MATCH HYPEREDGE h:decision RETURN h.@id")[0][0]
        .as_str()
        .unwrap()
        .to_owned();
    let mut params = Params::new();
    params.insert("id".into(), Value::Str(id.clone()));
    let found = execute(
        &db,
        NS,
        "MATCH HYPEREDGE h {@id: $id} RETURN id(h), h.status, keys(h), properties(h).summary",
        &params,
    )
    .unwrap();
    assert_eq!(
        found.to_json()["rows"],
        json!([[id, "open", ["status", "summary"], "single writer"]])
    );
    assert_eq!(
        rows(&db, "MATCH HYPEREDGE h {status: 'closed'} RETURN count(h)"),
        json!([[0]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision
             UNWIND participants(h) AS p
             RETURN p.node.name, p.role ORDER BY p.role"
        ),
        json!([
            ["builder", "implementer"],
            ["planner", "proposer"],
            ["checker", "reviewer"],
            ["T1", "subject"]
        ])
    );
    // The record, as JSON, with its participants.
    let record = &rows(&db, "MATCH HYPEREDGE h:review RETURN h")[0][0];
    assert_eq!(record["label"], "review");
    assert_eq!(record["participants"].as_array().unwrap().len(), 4);
    assert!(fails(&db, "MATCH HYPEREDGE h RETURN h.@participants").contains("participants(h)"));
    assert!(fails(&db, "MATCH (a:Agent) RETURN participants(a)").contains("hyperedge"));
}

#[test]
fn time_and_retraction() {
    let db = team();
    let before = Timestamp::now();
    run(
        &db,
        "MATCH HYPEREDGE h:decision SET h.@valid_to = '2025-01-01'",
    );
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision VALID AT '2024-06-01' RETURN count(h)"
        ),
        json!([[1]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision VALID AT '2026-06-01' RETURN count(h)"
        ),
        json!([[0]])
    );
    let retracted = run(&db, "MATCH HYPEREDGE h:review RETRACT h");
    assert_eq!(retracted.stats.hyperedges_retracted, 1);
    assert_eq!(
        rows(&db, "MATCH HYPEREDGE h:review RETURN count(h)"),
        json!([[0]])
    );
    let mut params = Params::new();
    params.insert("at".into(), Value::Time(before));
    let then = execute(
        &db,
        NS,
        "MATCH HYPEREDGE h:review KNOWN AT $at RETURN count(h)",
        &params,
    )
    .unwrap();
    assert_eq!(then.to_json()["rows"], json!([[1]]));
}

#[test]
fn set_remove_and_delete() {
    let db = team();
    let set = run(
        &db,
        "MATCH HYPEREDGE h:decision SET h.status = 'done', h += {votes: 3} REMOVE h.summary RETURN h.status, h.votes, h.summary",
    );
    assert_eq!(set.to_json()["rows"], json!([["done", 3, null]]));
    run(&db, "MATCH HYPEREDGE h:decision SET h = {status: 'final'}");
    assert_eq!(
        rows(&db, "MATCH HYPEREDGE h:decision RETURN keys(h)"),
        json!([[["status"]]])
    );
    assert!(fails(&db, "MATCH HYPEREDGE h:decision SET h.@label = 'x'").contains("@valid_to"));

    // A node in a hyperedge: DELETE refuses, DETACH DELETE takes it out.
    assert!(fails(&db, "MATCH (a:Agent {name: 'planner'}) DELETE a").contains("hyperedge"));
    let detached = run(&db, "MATCH (a:Agent {name: 'planner'}) DETACH DELETE a");
    assert_eq!(detached.stats.nodes_deleted, 1);
    assert_eq!(detached.stats.hyperedges_deleted, 0);
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision UNWIND participants(h) AS p RETURN count(p)"
        ),
        json!([[3]])
    );
    let deleted = run(&db, "MATCH HYPEREDGE h:decision DELETE h");
    assert_eq!(deleted.stats.hyperedges_deleted, 1);
    assert_eq!(
        rows(&db, "MATCH HYPEREDGE h:decision RETURN count(h)"),
        json!([[0]])
    );
    // Detaching the last but one node deletes the hyperedge.
    run(&db, "MATCH (b:Agent {name: 'builder'}) DETACH DELETE b");
    let gone = run(&db, "MATCH (t:Task {name: 'T2'}) DETACH DELETE t");
    assert_eq!(gone.stats.hyperedges_deleted, 1);
    assert_eq!(rows(&db, "MATCH HYPEREDGE h RETURN count(h)"), json!([[0]]));
    // Nodes in no hyperedge are deleted by DELETE as before.
    run(&db, "MATCH (a:Agent {name: 'idle'}) DELETE a");
}

#[test]
fn writes_and_reads_in_one_statement() {
    let db = team();
    // A statement sees the hyperedge it created.
    assert_eq!(
        rows(
            &db,
            "MATCH (i:Agent {name: 'idle'}), (t:Task {name: 'T1'})
             CREATE HYPEREDGE :watch [(i) AS watcher, (t)]
             WITH i
             MATCH HYPEREDGE h [(i) AS watcher]
             RETURN type(h)"
        ),
        json!([["watch"]])
    );
    // One per row.
    let made = run(
        &db,
        "MATCH (a:Agent), (t:Task {name: 'T2'}) WHERE a.name <> 'idle'
         CREATE HYPEREDGE :touch [(a), (t)]",
    );
    assert_eq!(made.stats.hyperedges_created, 3);
}

#[test]
fn syntax_and_errors() {
    let db = team();
    // `hyperedge` stays a usable name.
    assert_eq!(
        rows(
            &db,
            "MATCH (hyperedge:Task {name: 'T1'}) RETURN hyperedge.name"
        ),
        json!([["T1"]])
    );
    assert!(fails(&db, "MATCH HYPEREDGE h [(h)] RETURN h").contains("cannot also name"));
    assert!(fails(&db, "MATCH HYPEREDGE h EXACT RETURN h").contains("EXACT needs"));
    assert!(
        fails(
            &db,
            "MATCH (a:Agent {name: 'idle'}) CREATE HYPEREDGE h [(a), (b)]"
        )
        .contains("one label")
    );
    assert!(
        fails(
            &db,
            "MATCH (a:Agent {name: 'idle'}) CREATE HYPEREDGE h:x [(a), (b)]"
        )
        .contains("not bound")
    );
    assert!(
        fails(
            &db,
            "MATCH (a:Agent {name: 'idle'}) CREATE HYPEREDGE h:x [(a:Agent), (a)]"
        )
        .contains("variables alone")
    );
    // The engine's own checks: two distinct nodes at least.
    assert!(
        fails(
            &db,
            "MATCH (a:Agent {name: 'idle'}) CREATE HYPEREDGE :x [(a) AS one, (a) AS two]"
        )
        .contains("two distinct nodes")
    );
    assert!(!fails(&db, "MATCH (a:Agent) MATCH HYPEREDGE h [(a.name)] RETURN h").is_empty());
    // A member bound to something else than a node.
    assert!(fails(&db, "WITH 1 AS a MATCH HYPEREDGE h [(a)] RETURN h").contains("not a node"));
}

fn view_hiding(label: &str) -> Arc<View> {
    let namespace = || Object::Namespace {
        namespace: NS.into(),
    };
    let mut privileges: Vec<Privilege> = [Act::Read, Act::Create, Act::Update, Act::Delete]
        .into_iter()
        .map(|act| Privilege::grant(act, namespace()))
        .collect();
    privileges.push(Privilege::deny(
        Act::Read,
        Object::Label {
            namespace: NS.into(),
            label: label.into(),
            edges: false,
        },
    ));
    let role = RoleDef {
        name: "r".into(),
        privileges,
        grants: Vec::new(),
        roles: Vec::new(),
        policies: Vec::new(),
        description: None,
        created_at: Timestamp::now(),
    };
    let account = Account {
        name: "me".into(),
        kind: AccountKind::Service,
        password_hash: None,
        grants: Vec::new(),
        roles: vec!["r".into()],
        superuser: false,
        own_only: false,
        disabled: false,
        expires_at: None,
        attributes: Default::default(),
        description: None,
        created_at: Timestamp::now(),
    };
    let rights = Rights::compute(&account, &HashMap::from([("r".to_owned(), role)]));
    Arc::new(View::new(Arc::new(rights), Some(&account)))
}

#[test]
fn views_hide_hyperedges_with_hidden_participants() {
    let db = team();
    let limits = Limits {
        view: Some(view_hiding("Task")),
        ..Limits::none()
    };
    let read = |query: &str| {
        execute_with(&db, NS, query, &Params::new(), &limits)
            .unwrap()
            .to_json()["rows"]
            .clone()
    };
    // Every hyperedge has a task: none is visible.
    assert_eq!(read("MATCH HYPEREDGE h RETURN count(h)"), json!([[0]]));
    assert_eq!(
        read("MATCH (a:Agent {name: 'checker'}) OPTIONAL MATCH HYPEREDGE h [(a)] RETURN count(h)"),
        json!([[0]])
    );
    let limits = Limits {
        view: Some(view_hiding("Secret")),
        ..Limits::none()
    };
    let visible = execute_with(
        &db,
        NS,
        "MATCH HYPEREDGE h RETURN count(h)",
        &Params::new(),
        &limits,
    )
    .unwrap();
    assert_eq!(visible.to_json()["rows"], json!([[2]]));
}

#[test]
fn subscriptions_follow_hyperedges() {
    let db = team();
    db.enable_events(true);
    run(&db, "CREATE EVENT SUBSCRIPTION groups ON HYPEREDGES");
    let shown = rows(&db, "SHOW EVENT SUBSCRIPTIONS");
    assert!(shown.to_string().contains("hyperedges"), "{shown}");
    let start = db.begin_read().unwrap().changes(NS, 0, 1000).unwrap().last;
    run(&db, "MATCH HYPEREDGE h:decision SET h.status = 'done'");
    let events = db
        .begin_read()
        .unwrap()
        .events(NS, "groups", start, 100)
        .unwrap();
    assert_eq!(events.changes.len(), 1);
    assert_eq!(
        events.changes[0].values.as_ref().unwrap().changed,
        vec!["status".to_owned()]
    );
}

const DECIDE: &str =
    "MATCH (p:Agent {name: 'planner'}), (b:Agent {name: 'builder'}), (t:Task {name: 'T2'})
     MERGE HYPEREDGE h:decision {key: 'D-1'} [(p) AS proposer, (b) AS implementer, (t) AS subject]
     ON CREATE SET h.status = 'proposed'
     ON MATCH SET h.seen = coalesce(h.seen, 0) + 1
     RETURN h.status, h.seen";

fn count_of(db: &Grafeno, label: &str) -> i64 {
    let rows = rows(db, &format!("MATCH HYPEREDGE h:{label} RETURN count(h)"));
    rows[0][0].as_i64().unwrap()
}

#[test]
fn merge_creates_once_then_matches() {
    let db = team();
    let first = run(&db, DECIDE);
    assert_eq!(first.stats.hyperedges_created, 1);
    assert_eq!(first.to_json()["rows"], json!([["proposed", null]]));
    let again = run(&db, DECIDE);
    assert_eq!(again.stats.hyperedges_created, 0);
    assert_eq!(again.to_json()["rows"], json!([["proposed", 1]]));

    // The participants in another order, written another way: the same one.
    let reordered = run(
        &db,
        "MATCH (t:Task {name: 'T2'}), (b:Agent {name: 'builder'}), (p:Agent {name: 'planner'})
         MERGE HYPEREDGE h:decision {key: 'D-1'} [(t) AS subject, (b) AS implementer, (p) AS proposer]
         ON MATCH SET h.seen = h.seen + 1
         RETURN h.seen",
    );
    assert_eq!(reordered.stats.hyperedges_created, 0);
    assert_eq!(reordered.to_json()["rows"], json!([[2]]));
    // Two hyperedges of that label: team()'s and this one.
    assert_eq!(count_of(&db, "decision"), 2);

    // A node in two roles: the review of T2 (checker as author and
    // reviewer), named in another order.
    let review = run(
        &db,
        "MATCH (b:Agent {name: 'builder'}), (c:Agent {name: 'checker'}), (t:Task {name: 'T2'})
         MERGE HYPEREDGE r:review [(t) AS subject, (c) AS reviewer, (b), (c) AS author]
         RETURN count(r)",
    );
    assert_eq!(review.stats.hyperedges_created, 0);
    assert_eq!(count_of(&db, "review"), 1);

    // Within one statement, a later row finds what an earlier one made.
    let twice = run(
        &db,
        "UNWIND [1, 2, 3] AS i
         MATCH (p:Agent {name: 'planner'}), (c:Agent {name: 'checker'})
         MERGE HYPEREDGE h:pairing {key: 'P'} [(p), (c)]
         RETURN count(*)",
    );
    assert_eq!(twice.stats.hyperedges_created, 1);
    assert_eq!(twice.to_json()["rows"], json!([[3]]));
}

#[test]
fn merge_does_not_take_a_different_hyperedge_for_it() {
    let db = team();
    run(&db, DECIDE);
    let created = |query: &str| run(&db, query).stats.hyperedges_created;
    let bound = "MATCH (p:Agent {name: 'planner'}), (b:Agent {name: 'builder'}),
                       (c:Agent {name: 'checker'}), (t:Task {name: 'T2'})";
    // Another identity property: another event.
    assert_eq!(
        created(&format!(
            "{bound} MERGE HYPEREDGE h:decision {{key: 'D-2'}} [(p) AS proposer, (b) AS implementer, (t) AS subject]"
        )),
        1
    );
    // Fewer participants, more, or another role: not the same either.
    for members in [
        "[(p) AS proposer, (b) AS implementer]",
        "[(p) AS proposer, (b) AS implementer, (t) AS subject, (c) AS reviewer]",
        "[(p) AS proposer, (b) AS reviewer, (t) AS subject]",
        "[(p) AS proposer, (b), (t) AS subject]",
    ] {
        assert_eq!(
            created(&format!(
                "{bound} MERGE HYPEREDGE h:decision {{key: 'D-1'}} {members}"
            )),
            1,
            "{members}"
        );
    }
    // Another label.
    assert_eq!(
        created(&format!(
            "{bound} MERGE HYPEREDGE h:choice {{key: 'D-1'}} [(p) AS proposer, (b) AS implementer, (t) AS subject]"
        )),
        1
    );
    // A property left out of the pattern does not matter.
    let loose = run(
        &db,
        &format!(
            "{bound} MERGE HYPEREDGE h:decision [(p) AS proposer, (b) AS implementer, (t) AS subject] RETURN h.key"
        ),
    );
    // Without the identity property both decisions match, and each is a row.
    assert_eq!(loose.stats.hyperedges_created, 0);
    assert_eq!(loose.to_json()["rows"], json!([["D-1"], ["D-2"]]));
}

#[test]
fn merge_counts_only_what_holds_now() {
    let db = team();
    run(&db, DECIDE);
    run(&db, "MATCH HYPEREDGE h:decision {key: 'D-1'} RETRACT h");
    let after_retract = run(&db, DECIDE);
    assert_eq!(after_retract.stats.hyperedges_created, 1);
    // An invalidated one does not count either.
    db.write(|tx| {
        let id = tx
            .hyperedge_ids_with_label(NS, "decision", None, 100)?
            .into_iter()
            .rev()
            .find(|id| {
                tx.hyperedge(NS, *id)
                    .unwrap()
                    .is_some_and(|edge| !edge.is_retracted() && edge.props.contains_key("key"))
            })
            .expect("the merged one");
        tx.invalidate_hyperedge(NS, id, Timestamp::now())
    })
    .unwrap();
    let after_invalidate = run(&db, DECIDE);
    assert_eq!(after_invalidate.stats.hyperedges_created, 1);
}

#[test]
fn merge_updates_every_match_in_id_order() {
    let db = team();
    for _ in 0..2 {
        run(
            &db,
            "MATCH (p:Agent {name: 'planner'}), (c:Agent {name: 'checker'})
             CREATE HYPEREDGE :meeting {room: 'A'} [(p), (c)]",
        );
    }
    let merged = run(
        &db,
        "MATCH (p:Agent {name: 'planner'}), (c:Agent {name: 'checker'})
         MERGE HYPEREDGE h:meeting {room: 'A'} [(c), (p)]
         ON MATCH SET h.checked = true
         RETURN h.@id",
    );
    assert_eq!(merged.stats.hyperedges_created, 0);
    let ids: Vec<String> = merged.to_json()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[0].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids[0] < ids[1], "{ids:?}");
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:meeting WHERE h.checked RETURN count(h)"
        ),
        json!([[2]])
    );
}

#[test]
fn merge_syntax_and_errors() {
    let db = team();
    let bound = "MATCH (p:Agent {name: 'planner'}), (c:Agent {name: 'checker'})";
    for (query, expected) in [
        (
            format!("{bound} MERGE HYPEREDGE h [(p), (c)]"),
            "needs one label",
        ),
        (
            format!("{bound} MERGE HYPEREDGE h:a|b [(p), (c)]"),
            "needs one label",
        ),
        (
            format!("{bound} MERGE HYPEREDGE h:x [(p), (c)] EXACT"),
            "leave out EXACT",
        ),
        (
            format!("{bound} MERGE HYPEREDGE h:x [(p:Agent), (c)]"),
            "already bound",
        ),
        (
            format!("{bound} MERGE HYPEREDGE h:x {{@id: 'x'}} [(p), (c)]"),
            "not pseudo-properties",
        ),
        (
            format!("{bound} MERGE HYPEREDGE h:x {{k: null}} [(p), (c)]"),
            "null `k`",
        ),
        (
            "MATCH (p:Agent {name: 'planner'}) MERGE HYPEREDGE h:x [(p), (q)]".to_owned(),
            "`q` is not bound",
        ),
        (
            format!("{bound} MERGE HYPEREDGE h:x [(p), (p)]"),
            "takes part twice",
        ),
        (
            format!("{bound} MATCH HYPEREDGE h:decision MERGE HYPEREDGE h:x [(p), (c)]"),
            "already bound",
        ),
    ] {
        let message = fails(&db, &query);
        assert!(message.contains(expected), "{query}\n→ {message}");
    }
    let plan = run(
        &db,
        &format!("EXPLAIN {bound} MERGE HYPEREDGE h:x [(p), (c)]"),
    );
    assert!(
        plan.to_json()
            .to_string()
            .contains("match or create a hyperedge :x"),
        "{}",
        plan.to_json()
    );
}

#[test]
fn concurrent_merges_make_one_hyperedge() {
    let db = Arc::new(team());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                for _ in 0..5 {
                    execute(&db, NS, DECIDE, &Params::new()).unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:decision {key: 'D-1'} RETURN count(h), h.seen"
        ),
        json!([[1, 39]])
    );
}

#[test]
fn merges_reach_replicas_once() {
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    let start = leader.begin_read().unwrap().log_position().unwrap().last;
    run(
        &leader,
        "CREATE (:Agent {name: 'planner'}), (:Agent {name: 'builder'}), (:Task {name: 'T2'})",
    );
    run(&leader, DECIDE);
    run(&leader, DECIDE);
    let entries = leader
        .begin_read()
        .unwrap()
        .log_entries(start, usize::MAX)
        .unwrap();
    let member = Grafeno::in_memory().unwrap();
    member.apply_entries(&entries).unwrap();
    let query = "MATCH HYPEREDGE h:decision RETURN count(h), h.status, h.seen";
    assert_eq!(rows(&member, query), json!([[1, "proposed", 1]]));
    assert_eq!(rows(&member, query), rows(&leader, query));
}

/// Forty executions among the agents, with ids, projects and statuses.
fn executions() -> Grafeno {
    let db = team();
    run(
        &db,
        "UNWIND range(0, 39) AS i
         MATCH (p:Agent {name: 'planner'}), (b:Agent {name: 'builder'})
         CREATE HYPEREDGE :execution {execution_id: 'E-' + toString(i),
                                      project: 'P-' + toString(i % 4),
                                      status: CASE WHEN i % 2 = 0 THEN 'approved' ELSE 'open' END}
                [(p) AS runner, (b) AS reviewer]",
    );
    db
}

const QUERIES: [&str; 5] = [
    "MATCH HYPEREDGE h:execution WHERE h.execution_id = 'E-7' RETURN h.execution_id",
    "MATCH HYPEREDGE h:execution {execution_id: 'E-8'} RETURN h.project",
    "MATCH HYPEREDGE h:execution WHERE h.project = 'P-1' AND h.status = 'open' RETURN h.execution_id ORDER BY h.execution_id",
    "MATCH HYPEREDGE h:execution WHERE h.project = 'P-2' RETURN count(h)",
    "UNWIND ['E-1', 'E-30', 'none'] AS id MATCH HYPEREDGE h:execution WHERE h.execution_id = id RETURN id, h.project",
];

#[test]
fn an_index_on_hyperedges_finds_what_a_scan_finds() {
    let db = executions();
    let before: Vec<Json> = QUERIES.iter().map(|query| rows(&db, query)).collect();
    let anchored = "MATCH (p:Agent {name: 'planner'}) MATCH HYPEREDGE h:execution [(p)] WHERE h.execution_id = 'E-7' RETURN count(h)";
    let anchored_unindexed = profile_reads(&db, anchored);
    let created = run(
        &db,
        "CREATE INDEX FOR HYPEREDGE h:execution ON (h.execution_id)",
    );
    assert_eq!(created.to_json()["rows"][0][4], 40, "{}", created.to_json());
    run(
        &db,
        "CREATE INDEX by_project IF NOT EXISTS FOR HYPEREDGE h:execution ON (h.project, h.status)",
    );
    for (query, expected) in QUERIES.iter().zip(&before) {
        assert_eq!(&rows(&db, query), expected, "{query}");
    }
    // The plans start from the indexes.
    let plan = |query: &str| run(&db, &format!("EXPLAIN {query}")).to_json().to_string();
    assert!(
        plan(QUERIES[0]).contains("by index execution_execution_id_hyperedge"),
        "{}",
        plan(QUERIES[0])
    );
    assert!(
        plan(QUERIES[2]).contains("by index by_project"),
        "{}",
        plan(QUERIES[2])
    );
    // Its first property alone cannot: a hyperedge without a status is
    // not in it.
    assert!(
        !plan(QUERIES[3]).contains("by index by_project"),
        "{}",
        plan(QUERIES[3])
    );
    // Fewer reads than the label scan.
    let reads = |query: &str| -> u64 {
        run(&db, &format!("PROFILE {query}"))
            .profile
            .unwrap()
            .iter()
            .filter_map(|step| step.reads)
            .sum()
    };
    assert!(reads(QUERIES[0]) < 10, "{}", reads(QUERIES[0]));
    // A bound participant anchors first; when the index finds fewer (the
    // planner is in all 40, the key in one), the index is read instead.
    assert!(
        plan(anchored).contains("incidence index"),
        "{}",
        plan(anchored)
    );
    assert_eq!(rows(&db, anchored), json!([[1]]));
    // The 40 incidence keys are still read, to compare; the 40 records
    // and their participants are not.
    let anchored_reads = reads(anchored);
    assert!(
        anchored_reads * 2 < anchored_unindexed,
        "{anchored_reads} reads with the index, {anchored_unindexed} without"
    );
    // The other way round, the incidence list stays: one with the idle
    // agent, none with the key's project for it.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Agent {name: 'builder'}) MATCH HYPEREDGE h:execution [(p)] WHERE h.project = 'P-1' RETURN count(h)"
        ),
        json!([[10]])
    );

    // It is listed, on hyperedges.
    let shown = run(&db, "SHOW INDEXES").to_json();
    assert!(shown.to_string().contains("\"hyperedges\""), "{shown}");
    // Writes keep it: a changed value is found under its new one.
    run(
        &db,
        "MATCH HYPEREDGE h:execution {execution_id: 'E-7'} SET h.execution_id = 'E-7b'",
    );
    assert_eq!(rows(&db, QUERIES[0]), json!([]));
    assert_eq!(
        rows(
            &db,
            "MATCH HYPEREDGE h:execution WHERE h.execution_id = 'E-7b' RETURN count(h)"
        ),
        json!([[1]])
    );
    run(&db, "DROP INDEX by_project");
    assert_eq!(rows(&db, QUERIES[3]), before[3]);
}

#[test]
fn constraints_on_hyperedges() {
    let db = executions();
    run(
        &db,
        "CREATE CONSTRAINT FOR HYPEREDGE h:execution REQUIRE h.execution_id IS UNIQUE",
    );
    let error = fails(
        &db,
        "MATCH (p:Agent {name: 'planner'}), (c:Agent {name: 'checker'})
         CREATE HYPEREDGE :execution {execution_id: 'E-3'} [(p), (c)]",
    );
    assert!(error.contains("must be unique"), "{error}");
    // With the constraint, MERGE on the key cannot be raced into two.
    let merged = run(
        &db,
        "MATCH (p:Agent {name: 'planner'}), (b:Agent {name: 'builder'})
         MERGE HYPEREDGE h:execution {execution_id: 'E-3'} [(p) AS runner, (b) AS reviewer]
         RETURN h.project",
    );
    assert_eq!(merged.stats.hyperedges_created, 0);
    run(
        &db,
        "CREATE CONSTRAINT run_key FOR HYPEREDGE h:run REQUIRE (h.a, h.b) IS KEY",
    );
    let error = fails(
        &db,
        "MATCH (p:Agent {name: 'planner'}), (c:Agent {name: 'checker'})
         CREATE HYPEREDGE :run {a: 1} [(p), (c)]",
    );
    assert!(error.contains("has no b"), "{error}");
    let shown = run(&db, "SHOW CONSTRAINTS").to_json().to_string();
    assert!(
        shown.contains("run_key") && shown.contains("\"hyperedges\""),
        "{shown}"
    );
    for (query, expected) in [
        (
            "CREATE CONSTRAINT FOR HYPEREDGE h:run REQUIRE (h.a, h.b) IS NOT NULL",
            "one property of one label",
        ),
        (
            "CREATE FUZZY INDEX FOR HYPEREDGE h:run ON (h.a)",
            "by value",
        ),
        ("CREATE INDEX FOR HYPEREDGE h:run ON (x.a)", "expected `h`"),
    ] {
        let error = fails(&db, query);
        assert!(error.contains(expected), "{query}\n→ {error}");
    }
}

/// What `EXISTS { … }` and `COUNT { … }` over hyperedges would ask, as GQ
/// already says it: a `CALL { … }` subquery per row, or OPTIONAL MATCH.
#[test]
fn counting_and_testing_hyperedges_per_row() {
    let db = team();
    // How many decisions each agent proposed (COUNT { … }).
    assert_eq!(
        rows(
            &db,
            "MATCH (a:Agent)
             CALL { WITH a MATCH HYPEREDGE h:decision [(a) AS proposer] RETURN count(h) AS proposed }
             RETURN a.name, proposed ORDER BY a.name"
        ),
        json!([["builder", 0], ["checker", 0], ["idle", 0], ["planner", 1]])
    );
    // The agents in no hyperedge at all (NOT EXISTS { … }).
    assert_eq!(
        rows(
            &db,
            "MATCH (a:Agent)
             OPTIONAL MATCH HYPEREDGE h [(a)]
             WITH a, count(h) AS taking_part WHERE taking_part = 0
             RETURN a.name"
        ),
        json!([["idle"]])
    );
    // Agents who reviewed something and also took part in a review of T2,
    // as a filter in WHERE through the subquery's result.
    assert_eq!(
        rows(
            &db,
            "MATCH (a:Agent)
             CALL { WITH a MATCH HYPEREDGE h [(a) AS reviewer] RETURN count(h) AS reviews }
             WITH a, reviews WHERE reviews > 0
             RETURN a.name ORDER BY a.name"
        ),
        json!([["checker"]])
    );
}

/// A view that reads and writes the namespace but may not read hyperedges
/// (or edges) labeled `label`.
fn view_hiding_hyperedges(label: &str) -> Arc<View> {
    let namespace = || Object::Namespace {
        namespace: NS.into(),
    };
    let mut privileges: Vec<Privilege> = [Act::Read, Act::Create, Act::Update, Act::Delete]
        .into_iter()
        .map(|act| Privilege::grant(act, namespace()))
        .collect();
    privileges.push(Privilege::deny(
        Act::Read,
        Object::Label {
            namespace: NS.into(),
            label: label.into(),
            edges: true,
        },
    ));
    let role = RoleDef {
        name: "r".into(),
        privileges,
        grants: Vec::new(),
        roles: Vec::new(),
        policies: Vec::new(),
        description: None,
        created_at: Timestamp::now(),
    };
    let account = Account {
        name: "me".into(),
        kind: AccountKind::Service,
        password_hash: None,
        grants: Vec::new(),
        roles: vec!["r".into()],
        superuser: false,
        own_only: false,
        disabled: false,
        expires_at: None,
        attributes: Default::default(),
        description: None,
        created_at: Timestamp::now(),
    };
    let rights = Rights::compute(&account, &HashMap::from([("r".to_owned(), role)]));
    Arc::new(View::new(Arc::new(rights), Some(&account)))
}

#[test]
fn merge_does_not_duplicate_what_the_caller_cannot_see() {
    let db = team();
    run(&db, DECIDE);
    let hidden = Limits {
        view: Some(view_hiding_hyperedges("decision")),
        ..Limits::none()
    };
    // The caller sees no decision, but one matches: nothing is created.
    let seen = execute_with(
        &db,
        NS,
        "MATCH HYPEREDGE h:decision RETURN count(h)",
        &Params::new(),
        &hidden,
    )
    .unwrap();
    assert_eq!(seen.to_json()["rows"], json!([[0]]));
    let error = execute_with(&db, NS, DECIDE, &Params::new(), &hidden).unwrap_err();
    assert!(error.message.contains("may not see"), "{}", error.message);
    // Nor does it name the hyperedge.
    assert!(!error.message.contains("D-1"), "{}", error.message);
    assert_eq!(count_of(&db, "decision"), 2);
    // Another identity is another event, and the caller may create it.
    let other = execute_with(
        &db,
        NS,
        "MATCH (p:Agent {name: 'planner'}), (b:Agent {name: 'builder'}), (t:Task {name: 'T2'})
         MERGE HYPEREDGE h:decision {key: 'D-9'} [(p) AS proposer, (b) AS implementer, (t) AS subject]
         RETURN h.key",
        &Params::new(),
        &hidden,
    )
    .unwrap();
    assert_eq!(other.stats.hyperedges_created, 1);
}

#[test]
fn property_rules_and_history_on_hyperedges() {
    let db = team();
    run(
        &db,
        "CREATE CONSTRAINT FOR HYPEREDGE h:decision REQUIRE h.status IS NOT NULL",
    );
    run(
        &db,
        "CREATE CONSTRAINT FOR HYPEREDGE h:decision REQUIRE h.votes IS :: INTEGER",
    );
    run(
        &db,
        "CREATE CONSTRAINT FOR HYPEREDGE h:decision REQUIRE h.status IS FUNCTIONAL",
    );
    let shown = run(&db, "SHOW CONSTRAINTS").to_json().to_string();
    assert!(shown.contains("decision_status_not_null"), "{shown}");
    assert!(shown.contains("\"hyperedges\""), "{shown}");
    let bound = "MATCH (p:Agent {name: 'planner'}), (c:Agent {name: 'checker'})";
    // Without the property, or of another type: refused.
    let error = fails(
        &db,
        &format!("{bound} CREATE HYPEREDGE :decision {{votes: 1}} [(p), (c)]"),
    );
    assert!(error.contains("has no status"), "{error}");
    let error = fails(
        &db,
        &format!("{bound} CREATE HYPEREDGE :decision {{status: 'x', votes: 'many'}} [(p), (c)]"),
    );
    assert!(error.contains("not of type INTEGER"), "{error}");
    // A statement is checked at its end: MERGE may set it on creation.
    let merged = run(
        &db,
        &format!(
            "{bound} MERGE HYPEREDGE h:decision {{key: 'D-7'}} [(p), (c)]
             ON CREATE SET h.status = 'proposed' RETURN h.status"
        ),
    );
    assert_eq!(merged.stats.hyperedges_created, 1);
    // A functional property keeps what it was.
    run(
        &db,
        "MATCH HYPEREDGE h:decision {key: 'D-7'} SET h.status = 'approved'",
    );
    let history = rows(
        &db,
        "MATCH HYPEREDGE h:decision {key: 'D-7'} RETURN [e IN history(h, 'status') | e.value]",
    );
    assert_eq!(history, json!([[["proposed"]]]));
    // Removing it is refused, as on nodes.
    let error = fails(
        &db,
        "MATCH HYPEREDGE h:decision {key: 'D-7'} REMOVE h.status",
    );
    assert!(error.contains("has no status"), "{error}");
    // A rule the data already breaks is not declared.
    let error = fails(
        &db,
        "CREATE CONSTRAINT FOR HYPEREDGE h:review REQUIRE h.score IS NOT NULL",
    );
    assert!(error.contains("cannot declare"), "{error}");
}

/// The reads `PROFILE` counts for a query.
fn profile_reads(db: &Grafeno, query: &str) -> u64 {
    run(db, &format!("PROFILE {query}"))
        .profile
        .unwrap()
        .iter()
        .filter_map(|step| step.reads)
        .sum()
}

#[test]
fn match_hyperedge_splits_across_members() {
    let db = executions();
    // A first MATCH HYPEREDGE splits; each part's share of the
    // hyperedges, put end to end, gives one server's rows.
    for query in [
        "MATCH HYPEREDGE h:execution RETURN h.execution_id ORDER BY h.execution_id",
        "MATCH HYPEREDGE h:execution [(a) AS runner] RETURN a.name, count(h)",
        "MATCH HYPEREDGE h:execution WHERE h.status = 'open' RETURN h.project, count(*) ORDER BY h.project",
        "MATCH HYPEREDGE h:execution MATCH (t:Task) RETURN count(*)",
        "MATCH HYPEREDGE h RETURN h.@label, count(h) ORDER BY h.@label",
    ] {
        assert!(
            common::check_split(&db, NS, query, &Params::new()),
            "{query} did not split"
        );
    }
    // With an index on the property, too.
    run(&db, "CREATE INDEX FOR HYPEREDGE h:execution ON (h.project)");
    assert!(common::check_split(
        &db,
        NS,
        "MATCH HYPEREDGE h:execution WHERE h.project = 'P-2' RETURN h.execution_id ORDER BY h.execution_id",
        &Params::new()
    ));
    // A MATCH HYPEREDGE after a MATCH is cut by the MATCH, not again.
    assert!(common::check_split(
        &db,
        NS,
        "MATCH (a:Agent) MATCH HYPEREDGE h [(a)] RETURN a.name, count(h) ORDER BY a.name",
        &Params::new()
    ));
    // Writes do not split.
    assert!(!common::check_split(
        &db,
        NS,
        "MATCH HYPEREDGE h:execution SET h.seen = true",
        &Params::new()
    ));
}
