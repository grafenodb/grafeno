//! Finding hyperedges by a property: `MATCH HYPEREDGE … WHERE h.key = …`
//! on 100,000 hyperedges of one label, without and with a property index,
//! beside the same events kept as nodes with an index (what a graph without
//! hyperedges does). Each query runs `SAMPLES` times with a different value;
//! latencies are p50, p95 and p99, reads come from `PROFILE`, and the index
//! cost is the time to create it and to write 1,000 more hyperedges with it.
//!
//! `cargo run --release -p grafeno-query --example hyperedge_index_bench -- [hyperedges]`

use std::time::{Duration, Instant};

use grafeno::{Durability, Grafeno, NewHyperEdge, NewNode, NodeId};
use grafeno_query::{Params, Value, execute};

const NS: &str = "bench";
const SAMPLES: usize = 300;
/// Without an index a query scans the label: fewer samples.
const SCAN_SAMPLES: usize = 50;
const BATCH: usize = 1_000;
const AGENTS: usize = 1_000;
const PROJECTS: usize = 100;

/// The value of `$v` for a sample.
type ValueFor<'a> = &'a dyn Fn(usize) -> Value;

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn percentiles(mut samples: Vec<Duration>) -> (f64, f64, f64) {
    samples.sort();
    let at = |q: f64| micros(samples[((samples.len() - 1) as f64 * q) as usize]);
    (at(0.50), at(0.95), at(0.99))
}

/// Runs `query` once per sample, with `$v` set by `value(i)`; returns the
/// percentiles, the rows of the last run and the reads `PROFILE` counts.
fn measure(
    db: &Grafeno,
    query: &str,
    value: impl Fn(usize) -> Value,
    samples_wanted: usize,
) -> ((f64, f64, f64), usize, u64) {
    let mut samples = Vec::with_capacity(samples_wanted);
    let mut rows = 0;
    for i in 0..samples_wanted {
        let mut params = Params::new();
        params.insert("v".into(), value(i));
        let started = Instant::now();
        let result = execute(db, NS, query, &params).unwrap();
        samples.push(started.elapsed());
        rows = result.rows.len();
    }
    let mut params = Params::new();
    params.insert("v".into(), value(0));
    let profile = execute(db, NS, &format!("PROFILE {query}"), &params).unwrap();
    let reads = profile
        .profile
        .unwrap()
        .iter()
        .filter_map(|step| step.reads)
        .sum();
    (percentiles(samples), rows, reads)
}

fn props(i: usize) -> [(&'static str, grafeno::Value); 3] {
    [
        ("execution_id", format!("E-{i}").into()),
        ("project_id", format!("P-{}", i % PROJECTS).into()),
        ("status", ["approved", "rejected", "open"][i % 3].into()),
    ]
}

fn write_hyperedges(db: &Grafeno, agents: &[NodeId], from: usize, to: usize) {
    for start in (from..to).step_by(BATCH) {
        db.write(|tx| {
            for i in start..(start + BATCH).min(to) {
                let mut new = NewHyperEdge::new("execution")
                    .member(agents[i % AGENTS], "runner")
                    .member(agents[(i * 7 + 1) % AGENTS], "reviewer")
                    .member(agents[(i * 13 + 2) % AGENTS], "subject");
                for (key, value) in props(i) {
                    new = new.prop(key, value);
                }
                tx.create_hyperedge(NS, new)?;
            }
            Ok(())
        })
        .unwrap();
    }
}

fn write_event_nodes(db: &Grafeno, count: usize) {
    for start in (0..count).step_by(BATCH) {
        db.write(|tx| {
            for i in start..(start + BATCH).min(count) {
                let mut node = NewNode::new("Execution");
                for (key, value) in props(i) {
                    node = node.prop(key, value);
                }
                tx.create_node(NS, node)?;
            }
            Ok(())
        })
        .unwrap();
    }
}

fn gql(db: &Grafeno, query: &str) -> Duration {
    let started = Instant::now();
    execute(db, NS, query, &Params::new()).unwrap_or_else(|e| panic!("{query}: {e}"));
    started.elapsed()
}

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(100_000);
    let db = Grafeno::in_memory().unwrap();
    db.set_durability(Durability::Eventual);
    let agents: Vec<NodeId> = db
        .write(|tx| {
            (0..AGENTS)
                .map(|i| tx.create_node(NS, NewNode::new("Agent").prop("n", i as i64)))
                .collect()
        })
        .unwrap();
    write_hyperedges(&db, &agents, 0, count);
    write_event_nodes(&db, count);

    let by_id = |i: usize| Value::Str(format!("E-{}", (i * 7919) % count));
    let by_project = |i: usize| Value::Str(format!("P-{}", i % PROJECTS));
    let queries: [(&str, &str, ValueFor); 4] = [
        (
            "hyperedge by execution_id (1 row)",
            "MATCH HYPEREDGE h:execution WHERE h.execution_id = $v RETURN h.@id",
            &by_id,
        ),
        (
            "hyperedge by project_id and status (~0.3%)",
            "MATCH HYPEREDGE h:execution WHERE h.project_id = $v AND h.status = 'approved' RETURN h.@id",
            &by_project,
        ),
        (
            "event node by execution_id (1 row)",
            "MATCH (e:Execution) WHERE e.execution_id = $v RETURN e.@id",
            &by_id,
        ),
        (
            "event node by project_id and status (~0.3%)",
            "MATCH (e:Execution) WHERE e.project_id = $v AND e.status = 'approved' RETURN e.@id",
            &by_project,
        ),
    ];
    let before: Vec<_> = queries
        .iter()
        .map(|(_, query, value)| measure(&db, query, value, SCAN_SAMPLES))
        .collect();

    // Indexes: the hyperedges' (when the engine has them) and the nodes'.
    let hyper_index = [
        "CREATE INDEX FOR HYPEREDGE h:execution ON (h.execution_id)",
        "CREATE INDEX FOR HYPEREDGE h:execution ON (h.project_id, h.status)",
    ];
    let mut hyper_indexed = true;
    let mut index_build = Duration::ZERO;
    for query in hyper_index {
        let started = Instant::now();
        match execute(&db, NS, query, &Params::new()) {
            Ok(_) => index_build += started.elapsed(),
            Err(error) => {
                println!("(no hyperedge index: {error})");
                hyper_indexed = false;
                break;
            }
        }
    }
    gql(&db, "CREATE INDEX FOR (e:Execution) ON (e.execution_id)");
    gql(
        &db,
        "CREATE INDEX FOR (e:Execution) ON (e.project_id, e.status)",
    );
    let after: Vec<_> = queries
        .iter()
        .map(|(_, query, value)| measure(&db, query, value, SAMPLES))
        .collect();

    println!(
        "{count} hyperedges of 3 participants and {count} event nodes; {SCAN_SAMPLES} samples per query without indexes, {SAMPLES} with them\n"
    );
    println!(
        "| Query | Without index p50 / p95 / p99 (µs) | reads | With index p50 / p95 / p99 (µs) | reads | rows |"
    );
    println!("| --- | --- | --- | --- | --- | --- |");
    for (((name, _, _), (b, _, b_reads)), (a, rows, a_reads)) in
        queries.iter().zip(&before).zip(&after)
    {
        println!(
            "| {name} | {:.0} / {:.0} / {:.0} | {b_reads} | {:.0} / {:.0} / {:.0} | {a_reads} | {rows} |",
            b.0, b.1, b.2, a.0, a.1, a.2
        );
    }

    if hyper_indexed {
        // Writing with the two indexes, against writing without them.
        let fresh = Grafeno::in_memory().unwrap();
        fresh.set_durability(Durability::Eventual);
        let agents2: Vec<NodeId> = fresh
            .write(|tx| {
                (0..AGENTS)
                    .map(|i| tx.create_node(NS, NewNode::new("Agent").prop("n", i as i64)))
                    .collect()
            })
            .unwrap();
        let started = Instant::now();
        write_hyperedges(&fresh, &agents2, 0, 10 * BATCH);
        let plain = started.elapsed();
        for query in hyper_index {
            gql(&fresh, query);
        }
        let started = Instant::now();
        write_hyperedges(&fresh, &agents2, 10 * BATCH, 20 * BATCH);
        let indexed = started.elapsed();
        let rate = |d: Duration| (10 * BATCH) as f64 / d.as_secs_f64();
        println!(
            "\nBuilding both hyperedge indexes over {count}: {:.0} ms. Writing 10,000 hyperedges: {:.0}/s without the indexes, {:.0}/s with them.",
            index_build.as_secs_f64() * 1e3,
            rate(plain),
            rate(indexed)
        );
    }
}
