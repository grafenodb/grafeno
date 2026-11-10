//! What the indexes and rules of 16.5 cost and save on a large graph:
//! loading with and without a composite key, an edge index and property
//! rules, then lookups through a composite index (all of it, and a prefix)
//! and an edge index against the scans they replace.
//!
//! `cargo run --release -p grafeno-query --example schema_bench [people]`

use std::time::{Duration, Instant};

use grafeno::{Constraint, Grafeno, IndexDef, NewEdge, NewNode, NodeId, PropertyType};
use grafeno_query::{Params, execute_read};

const NS: &str = "bench";

fn schema(db: &Grafeno) {
    db.write(|tx| {
        tx.create_index(
            NS,
            IndexDef {
                unique: true,
                required: true,
                ..IndexDef::composite("Person", &["last", "first", "uid"])
            },
        )?;
        tx.create_index(NS, IndexDef::on_edges("knows", "since"))?;
        tx.create_constraint(
            NS,
            Constraint::Exists {
                name: "since_required".into(),
                label: "knows".into(),
                key: "since".into(),
                edges: true,
            },
        )?;
        tx.create_constraint(
            NS,
            Constraint::Typed {
                name: "age_integer".into(),
                label: "Person".into(),
                key: "age".into(),
                of: PropertyType::Integer,
                edges: false,
            },
        )
    })
    .unwrap();
}

/// Loads `people` people, each knowing the next two; the time it took.
fn load(db: &Grafeno, people: usize) -> Duration {
    let started = Instant::now();
    let mut ids: Vec<NodeId> = Vec::with_capacity(people);
    for chunk in 0..people.div_ceil(5_000) {
        let from = chunk * 5_000;
        let to = (from + 5_000).min(people);
        let made = db
            .write(|tx| {
                (from..to)
                    .map(|i| {
                        tx.create_node(
                            NS,
                            NewNode::new("Person")
                                .prop("last", format!("L{}", i % 1_000))
                                .prop("first", format!("F{}", i % 997))
                                .prop("uid", i as i64)
                                .prop("age", (i % 90) as i64),
                        )
                    })
                    .collect::<grafeno::Result<Vec<_>>>()
            })
            .unwrap();
        ids.extend(made);
    }
    for chunk in ids.chunks(5_000).enumerate() {
        let (at, slice) = chunk;
        db.write(|tx| {
            for (k, from) in slice.iter().enumerate() {
                let i = at * 5_000 + k;
                for step in [1, 2] {
                    if let Some(to) = ids.get(i + step) {
                        tx.create_edge(
                            NS,
                            NewEdge::new(*from, "knows", *to).prop("since", (i % 50_000) as i64),
                        )?;
                    }
                }
            }
            Ok(())
        })
        .unwrap();
    }
    started.elapsed()
}

fn time(db: &Grafeno, query: &str, runs: u32) -> (Duration, String) {
    let read = db.begin_read().unwrap();
    let mut last = String::new();
    let started = Instant::now();
    for _ in 0..runs {
        let result = execute_read(&read, NS, query, &Params::new()).unwrap();
        last = result.to_json()["rows"].to_string();
    }
    (started.elapsed() / runs, last)
}

fn main() {
    let people: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(200_000);
    let plain = Grafeno::in_memory().unwrap();
    let plain_load = load(&plain, people);
    let ruled = Grafeno::in_memory().unwrap();
    schema(&ruled);
    let ruled_load = load(&ruled, people);
    println!(
        "load {people} people and {} edges: {:.2} s plain, {:.2} s with a 3-property key, an edge index and 2 rules",
        people * 2 - 3,
        plain_load.as_secs_f64(),
        ruled_load.as_secs_f64()
    );
    for (what, query, runs) in [
        (
            "whole key",
            "MATCH (p:Person {last: 'L7', first: 'F7', uid: 7}) RETURN p.uid",
            200,
        ),
        (
            "prefix (last)",
            "MATCH (p:Person {last: 'L7'}) RETURN count(p)",
            20,
        ),
        (
            "prefix and range",
            "MATCH (p:Person) WHERE p.last = 'L7' AND p.first >= 'F1' AND p.first < 'F2' RETURN count(p)",
            20,
        ),
        (
            "edge range",
            "MATCH (a:Person)-[k:knows]->(b) WHERE k.since >= 100 AND k.since < 110 RETURN count(b)",
            20,
        ),
    ] {
        let (scan, scanned) = time(&plain, query, runs.min(3));
        let (seek, sought) = time(&ruled, query, runs);
        assert_eq!(scanned, sought, "{query}");
        println!(
            "{what:>17}: {:>9.3} ms by scan, {:>7.3} ms by index ({sought})",
            scan.as_secs_f64() * 1000.0,
            seek.as_secs_f64() * 1000.0
        );
    }
    let read = ruled.begin_read().unwrap();
    assert!(read.check(NS).unwrap().is_ok());
}
