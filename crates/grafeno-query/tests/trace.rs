//! Phase 16.9: `TRACE` records how a statement walked the graph, in
//! order, bounded, and only when asked.

use grafeno::Grafeno;
use grafeno_query::{Params, execute};
use serde_json::{Value as Json, json};

const NS: &str = "t";

fn run(db: &Grafeno, query: &str) -> Json {
    execute(db, NS, query, &Params::new())
        .unwrap_or_else(|e| panic!("{query}\n→ {e}"))
        .to_json()
}

fn people() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (ana:Person {name: 'Ana', age: 40}), (bo:Person {name: 'Bo', age: 20}),
                (cy:Person {name: 'Cy', age: 35}), (acme:Company {name: 'Acme'}),
                (ana)-[:knows]->(bo), (ana)-[:knows]->(cy), (ana)-[:works_at]->(acme)",
    );
    run(&db, "CREATE INDEX FOR (p:Person) ON (p.name)");
    db
}

fn id_of(db: &Grafeno, name: &str) -> Json {
    run(db, &format!("MATCH (n {{name: '{name}'}}) RETURN n.@id"))["rows"][0][0].clone()
}

fn kinds(events: &Json) -> Vec<&str> {
    events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect()
}

#[test]
fn a_walk_in_order() {
    let db = people();
    let result = run(
        &db,
        "TRACE MATCH (p:Person {name: 'Ana'})-[:knows]->(f:Person) WHERE f.age > 30 RETURN f.name",
    );
    assert_eq!(result["rows"], json!([["Cy"]]));
    let trace = &result["trace"];
    assert_eq!(trace["format"], 1);
    assert_eq!(trace["cut"], false);
    assert_eq!(
        trace["steps"],
        json!([{"step": 1, "clause": "MATCH"}, {"step": 2, "clause": "RETURN"}])
    );
    let events = &trace["events"];
    let (ana, bo, cy) = (id_of(&db, "Ana"), id_of(&db, "Bo"), id_of(&db, "Cy"));
    // The anchor, found through the index; two edges followed, both ends
    // kept by the pattern; Bo dropped by the condition; one row.
    assert_eq!(events[0]["kind"], "anchor");
    assert_eq!(events[0]["node"], ana);
    assert_eq!(events[0]["via"], "index Person_name");
    assert_eq!(events[0]["step"], 1);
    let expanded: Vec<&Json> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "expand")
        .collect();
    assert_eq!(expanded.len(), 2, "works_at is not followed: {events}");
    assert!(
        expanded
            .iter()
            .all(|e| e["from"] == ana && e["edge"].is_string())
    );
    let dropped: Vec<&Json> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "drop")
        .collect();
    assert_eq!(dropped.len(), 1, "{events}");
    assert_eq!(dropped[0]["reason"], "condition");
    assert_eq!(dropped[0]["condition"], "f.age > 30");
    assert!(dropped[0]["nodes"].as_array().unwrap().contains(&bo));
    let rows: Vec<&Json> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "row")
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["index"], 0);
    // Events are numbered in order, the row last.
    for (at, event) in events.as_array().unwrap().iter().enumerate() {
        assert_eq!(event["seq"], at);
    }
    assert_eq!(*kinds(events).last().unwrap(), "row");
    let _ = cy;
}

#[test]
fn drops_say_why() {
    let db = people();
    // A label scan: the company is not even a candidate; the pattern's
    // property drops Bo and Cy at the anchor.
    let result = run(&db, "TRACE MATCH (p:Person {age: 40}) RETURN p.name");
    let events = &result["trace"]["events"];
    let anchors = kinds(events).iter().filter(|k| **k == "anchor").count();
    let drops: Vec<&Json> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "drop")
        .collect();
    assert_eq!(anchors, 1);
    assert_eq!(drops.len(), 2);
    assert!(drops.iter().all(|d| d["reason"] == "pattern"));
    assert!(events[0]["via"].as_str().unwrap().starts_with("label"));
    // A target node that does not fit the pattern.
    let result = run(
        &db,
        "TRACE MATCH (:Person {name: 'Ana'})-->(c:Company) RETURN c.name",
    );
    let events = &result["trace"]["events"];
    let dropped = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "drop" && e["reason"] == "pattern")
        .count();
    assert_eq!(
        dropped, 2,
        "the two people Ana knows are not companies: {events}"
    );
    // Variable-length steps give their edges.
    let result = run(
        &db,
        "TRACE MATCH (:Person {name: 'Ana'})-[:knows*1..2]->(f) RETURN f.name",
    );
    assert!(
        result["trace"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "expand" && e["edges"].is_array())
    );
}

#[test]
fn bounded_and_only_when_asked() {
    let db = Grafeno::in_memory().unwrap();
    run(&db, "UNWIND range(1, 12000) AS i CREATE (:N {i: i})");
    let result = run(&db, "TRACE MATCH (n:N) RETURN count(n)");
    assert_eq!(result["rows"], json!([[12000]]), "the query still finishes");
    assert_eq!(result["trace"]["cut"], true);
    assert_eq!(
        result["trace"]["events"].as_array().unwrap().len(),
        grafeno_query::TRACE_EVENTS
    );
    let plain = run(&db, "MATCH (n:N) RETURN count(n)");
    assert!(plain.get("trace").is_none());
    // Writes are traced too, and still write.
    let written = run(&db, "TRACE MATCH (n:N {i: 1}) SET n.seen = true RETURN n.i");
    assert_eq!(written["rows"], json!([[1]]));
    assert_eq!(
        run(&db, "MATCH (n:N {seen: true}) RETURN count(n)")["rows"],
        json!([[1]])
    );
    let error = execute(
        &db,
        NS,
        "TRACE CREATE INDEX FOR (n:N) ON (n.i)",
        &Params::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("TRACE run queries"), "{error}");
}
