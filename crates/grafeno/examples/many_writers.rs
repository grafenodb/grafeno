//! Many agents writing at once: each one saves memories as fast as it can
//! (a node with a text, a 384-number embedding, and an edge to its
//! project), through the group-commit writer with immediate durability.
//! Compared: every agent on one database file (one writer), and the agents
//! spread over 2 and 4 files, each with its own writer, as namespaces kept
//! in separate files would be.
//!
//! `cargo run --release -p grafeno --example many_writers -- [seconds]`
//!
//! Each step runs `seconds` (5 by default). Reported: memories written a
//! second, and how long a write waited (p50, p99), per number of agents.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use grafeno::{Grafeno, NewEdge, NewNode, NodeId, Writer, WriterOptions};

const DIMENSIONS: usize = 384;

fn open(name: &str) -> Arc<Grafeno> {
    let path = std::env::temp_dir().join(format!(
        "grafeno-many-writers-{name}-{}.redb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    Arc::new(Grafeno::open(&path).unwrap())
}

/// A unit vector from a seed.
fn embedding(seed: u64) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut v: Vec<f32> = (0..DIMENSIONS)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

struct Step {
    written: usize,
    latencies: Vec<f64>,
    seconds: f64,
}

/// `agents` threads writing for `seconds` on `files` databases (agent i
/// writes to file i mod files).
fn run(agents: usize, files: usize, seconds: f64) -> Step {
    let mut writers = Vec::new();
    let mut projects = Vec::new();
    for file in 0..files {
        let db = open(&format!("{agents}-{files}-{file}"));
        let project = db
            .write(|tx| tx.create_node("agents", NewNode::new("Project").prop("name", "p")))
            .unwrap();
        writers.push(Arc::new(Writer::new(db, WriterOptions::default())));
        projects.push(project);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let handles: Vec<_> = (0..agents)
        .map(|agent| {
            let writer = writers[agent % files].clone();
            let project: NodeId = projects[agent % files];
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut latencies = Vec::new();
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let seed = (agent as u64) << 32 | n;
                    let vector = embedding(seed);
                    let text = format!(
                        "Agent {agent} noted decision {n}: the release moves to Tuesday after the review of the billing changes."
                    );
                    let begun = Instant::now();
                    writer
                        .write(move |tx| {
                            let id = tx.create_node(
                                "agents",
                                NewNode::new("Memory")
                                    .text(text.as_str())
                                    .embedding(vector.clone()),
                            )?;
                            tx.create_edge("agents", NewEdge::new(id, "part_of", project))?;
                            Ok(())
                        })
                        .unwrap();
                    latencies.push(begun.elapsed().as_secs_f64() * 1000.0);
                    n += 1;
                }
                latencies
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs_f64(seconds));
    stop.store(true, Ordering::Relaxed);
    let mut latencies: Vec<f64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let elapsed = started.elapsed().as_secs_f64();
    latencies.sort_by(f64::total_cmp);
    Step {
        written: latencies.len(),
        latencies,
        seconds: elapsed,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let seconds = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(5.0);
    println!(
        "{} CPUs; {seconds} s a step, immediate durability\n",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    println!("| Agents | Files (writers) | Memories/s | p50 ms | p99 ms |");
    println!("| ---: | ---: | ---: | ---: | ---: |");
    for agents in [1, 8, 32, 128] {
        for files in [1, 2, 4] {
            if files > agents {
                continue;
            }
            let step = run(agents, files, seconds);
            println!(
                "| {agents} | {files} | {:.0} | {:.1} | {:.1} |",
                step.written as f64 / step.seconds,
                percentile(&step.latencies, 0.5),
                percentile(&step.latencies, 0.99),
            );
        }
    }
}
