//! The group-commit writer alone under the write mix of
//! `grafeno-server/examples/many_connections.rs` (a new node, or a property
//! set on one of 100,000 indexed nodes), from many threads at once, on a
//! file with immediate durability: what the writer itself takes, without
//! HTTP (docs/roadmap/STORAGE_AND_INDEXING_OPTIMIZATION.md, phase 3.4).
//!
//! `cargo run --release -p grafeno-server --example writer_mix -- 50 200 800`

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use grafeno::{Grafeno, IndexDef, NewEdge, NewNode, NodeId, Props, Writer, WriterOptions};
use serde_json::json;

const PEOPLE: usize = 100_000;

fn main() {
    let runs: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let runs = if runs.is_empty() {
        vec![50, 200, 800]
    } else {
        runs
    };
    let path = std::env::temp_dir().join(format!("grafeno-writer-mix-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let db = Arc::new(Grafeno::open(&path).unwrap());
    let ids: Arc<Vec<NodeId>> = Arc::new(
        db.write(|tx| {
            let mut ids = Vec::with_capacity(PEOPLE);
            for i in 0..PEOPLE {
                ids.push(
                    tx.create_node(
                        "load",
                        NewNode::new("Person")
                            .prop("code", format!("C{i}"))
                            .prop("age", (i % 90) as i64),
                    )?,
                );
            }
            for i in 0..PEOPLE {
                for k in 1..=5 {
                    tx.create_edge(
                        "load",
                        NewEdge::new(ids[i], "knows", ids[(i * 7 + k * 101) % PEOPLE]),
                    )?;
                }
            }
            tx.create_index("load", IndexDef::new("Person", "code"))?;
            Ok(ids)
        })
        .unwrap(),
    );
    let writer = Arc::new(Writer::new(db.clone(), WriterOptions::default()));
    println!("| writers | writes/s | p50 ms | p99 ms | writes per commit |");
    println!("| ---: | ---: | ---: | ---: | ---: |");
    for threads in runs {
        let before = writer.metrics();
        let stop = Arc::new(AtomicBool::new(false));
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (writer, stop, ids) = (writer.clone(), stop.clone(), ids.clone());
                std::thread::spawn(move || {
                    let mut times = Vec::new();
                    let mut n = t as u64;
                    while !stop.load(Ordering::Relaxed) {
                        n = n
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        let person = ids[((n >> 13) % PEOPLE as u64) as usize];
                        let started = Instant::now();
                        if n % 2 == 0 {
                            writer
                                .write(move |tx| {
                                    tx.create_node(
                                        "load",
                                        NewNode::new("Visit").prop("person", person.to_string()),
                                    )
                                })
                                .unwrap();
                        } else {
                            writer
                                .write(move |tx| {
                                    let mut props = Props::new();
                                    props.insert("seen".into(), json!(n % 1000));
                                    tx.set_node_props("load", person, props)
                                })
                                .unwrap();
                        }
                        times.push(started.elapsed().as_secs_f64() * 1000.0);
                    }
                    times
                })
            })
            .collect();
        let started = Instant::now();
        std::thread::sleep(Duration::from_secs(8));
        stop.store(true, Ordering::Relaxed);
        let mut times: Vec<f64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        let seconds = started.elapsed().as_secs_f64();
        times.sort_by(f64::total_cmp);
        let at = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
        let after = writer.metrics();
        let groups = after.groups - before.groups;
        let writes = after.writes - before.writes;
        println!(
            "| {threads} | {:.0} | {:.1} | {:.1} | {:.0} |",
            times.len() as f64 / seconds,
            at(0.5),
            at(0.99),
            writes as f64 / groups.max(1) as f64
        );
    }
    drop(writer);
    drop(db);
    let _ = std::fs::remove_file(&path);
}
