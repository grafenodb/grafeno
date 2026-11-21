//! GQ on hyperedges: `MATCH HYPEREDGE` (anchors, roles, `EXACT`, `OPTIONAL`,
//! time, properties, views), `CREATE HYPEREDGE`, `SET`, `REMOVE`, `DELETE`,
//! `DETACH DELETE`, `RETRACT`, `participants()` and `EXPLAIN`.

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
