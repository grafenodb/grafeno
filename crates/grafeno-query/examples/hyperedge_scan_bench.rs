//! What reading hyperedges without an index costs, beside nodes: reading
//! each of 100,000 by id, paging through them, and a GQ query that scans a
//! label (`MATCH HYPEREDGE … WHERE`), against the same scan over as many
//! nodes; then a bound participant in 300 hyperedges with a key, without
//! and with an index on the key. Each figure is the best of `RUNS` runs.
//!
//! `cargo run --release -p grafeno-query --example hyperedge_scan_bench -- [count]`

use std::time::{Duration, Instant};

use grafeno::{Durability, Grafeno, HyperEdgeId, NewHyperEdge, NewNode, NodeId};
use grafeno_query::{Params, execute};

const NS: &str = "bench";
const RUNS: usize = 5;
const BATCH: usize = 1_000;

fn best(mut f: impl FnMut()) -> Duration {
    (0..RUNS)
        .map(|_| {
            let started = Instant::now();
            f();
            started.elapsed()
        })
        .min()
        .unwrap()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
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
            (0..1_000)
                .map(|i| tx.create_node(NS, NewNode::new("Agent").prop("n", i as i64)))
                .collect()
        })
        .unwrap();
    let mut ids: Vec<HyperEdgeId> = Vec::with_capacity(count);
    for start in (0..count).step_by(BATCH) {
        let made = db
            .write(|tx| {
                (start..(start + BATCH).min(count))
                    .map(|i| {
                        tx.create_hyperedge(
                            NS,
                            NewHyperEdge::new("execution")
                                .member(agents[i % 1_000], "runner")
                                .member(agents[(i * 7 + 1) % 1_000], "reviewer")
                                .member(agents[(i * 13 + 2) % 1_000], "subject")
                                .prop("execution_id", format!("E-{i}")),
                        )
                    })
                    .collect::<grafeno::Result<Vec<_>>>()
            })
            .unwrap();
        ids.extend(made);
        db.write(|tx| {
            for i in start..(start + BATCH).min(count) {
                tx.create_node(
                    NS,
                    NewNode::new("Execution").prop("execution_id", format!("E-{i}")),
                )?;
            }
            Ok(())
        })
        .unwrap();
    }

    let read = db.begin_read().unwrap();
    let by_id = best(|| {
        for id in &ids {
            std::hint::black_box(read.hyperedge(NS, *id).unwrap());
        }
    });
    let paged = best(|| {
        let mut after = None;
        loop {
            let page = read.hyperedges_page(NS, after, 1_000).unwrap();
            let Some(last) = page.last() else { break };
            after = Some(last.id);
            std::hint::black_box(&page);
        }
    });
    drop(read);
    let gq = |query: &str| {
        best(|| {
            execute(&db, NS, query, &Params::new()).unwrap();
        })
    };
    let hyper_scan = gq("MATCH HYPEREDGE h:execution WHERE h.execution_id = 'E-77' RETURN h.@id");
    let node_scan = gq("MATCH (e:Execution) WHERE e.execution_id = 'E-77' RETURN e.@id");
    // The agent is found by scanning its label (1,000 nodes): most of the
    // time either way.
    let busy = "MATCH (a:Agent {n: 77}) MATCH HYPEREDGE h:execution [(a)] WHERE h.execution_id = 'E-77' RETURN h.@id";
    let busy_unindexed = gq(busy);
    execute(
        &db,
        NS,
        "CREATE INDEX FOR HYPEREDGE h:execution ON (h.execution_id)",
        &Params::new(),
    )
    .unwrap();
    let busy_indexed = gq(busy);
    println!("{count} hyperedges of 3 participants and {count} nodes; best of {RUNS}\n");
    println!("| Read | Time | Per record |");
    println!("| --- | --- | --- |");
    let per = |d: Duration| d.as_secs_f64() * 1e6 / count as f64;
    println!(
        "| Every hyperedge by id | {:.0} ms | {:.2} µs |",
        ms(by_id),
        per(by_id)
    );
    println!(
        "| Every hyperedge in pages of 1,000 | {:.0} ms | {:.2} µs |",
        ms(paged),
        per(paged)
    );
    println!(
        "| GQ scan of the label, hyperedges | {:.0} ms | {:.2} µs |",
        ms(hyper_scan),
        per(hyper_scan)
    );
    println!(
        "| GQ scan of the label, nodes | {:.0} ms | {:.2} µs |",
        ms(node_scan),
        per(node_scan)
    );
    println!(
        "
A participant in 300 hyperedges, by key: {:.2} ms by its incidence list, {:.3} ms with an index on the key.",
        ms(busy_unindexed),
        ms(busy_indexed)
    );
}
