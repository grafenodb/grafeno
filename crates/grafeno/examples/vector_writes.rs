//! Writes with vectors on the one writer: what each part of a write costs
//! as the vector graph grows, and agents writing at once through the group
//! commit while others search.
//!
//! `cargo run --release -p grafeno --example vector_writes [sizes…]`
//! (default 10000 100000; `SECONDS` per load step, 3 by default)
//!
//! A database file with immediate durability; vectors of 128 dimensions
//! in 64 clusters, texts of 40 words from a 5 000-word vocabulary.
//!
//! 1. Single writes, each its own transaction, 300 of each kind: an empty
//!    commit, a node, a node with a text, with a vector, with both. The
//!    differences are what the text index and the vector graph add.
//! 2. Open load from 256 client threads, each write a node with a text and
//!    a vector, at rising rates, while 4 threads search the graph
//!    without pause. Latency runs from the scheduled time.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grafeno::{Durability, Error, Grafeno, NewNode, Writer, WriterOptions};

const NS: &str = "agents";
const DIMENSION: usize = 128;
const CLIENTS: usize = 256;
const SEARCHERS: usize = 4;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
    }
}

struct Corpus {
    centers: Vec<Vec<f32>>,
}

impl Corpus {
    fn new() -> Self {
        let mut rng = Rng(0xFACE);
        Self {
            centers: (0..64)
                .map(|_| (0..DIMENSION).map(|_| rng.unit()).collect())
                .collect(),
        }
    }
    fn vector(&self, rng: &mut Rng) -> Vec<f32> {
        let center = &self.centers[(rng.next() % 64) as usize];
        center.iter().map(|c| c + 0.35 * rng.unit()).collect()
    }
    fn text(rng: &mut Rng) -> String {
        (0..40)
            .map(|_| {
                let common = rng.next() % 5_000 + 1;
                format!("w{}", rng.next() % common)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn temp() -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("grafeno-vector-writes-{}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn percentile(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1]
}

fn ms(duration: Duration) -> String {
    let ms = duration.as_secs_f64() * 1e3;
    if ms < 10.0 {
        format!("{ms:.2}")
    } else {
        format!("{ms:.0}")
    }
}

fn until(at: Instant) {
    loop {
        let now = Instant::now();
        if now >= at {
            return;
        }
        let left = at - now;
        if left > Duration::from_millis(2) {
            std::thread::sleep(left - Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
}

/// Fills the graph to `size` vectors (with texts), in transactions of
/// 5 000.
fn fill(db: &Grafeno, corpus: &Corpus, rng: &mut Rng, from: usize, size: usize) -> Duration {
    let started = Instant::now();
    let mut at = from;
    while at < size {
        let n = 5_000.min(size - at);
        let batch: Vec<(Vec<f32>, String)> = (0..n)
            .map(|_| (corpus.vector(rng), Corpus::text(rng)))
            .collect();
        db.write(|tx| {
            for (vector, text) in &batch {
                tx.create_node(
                    NS,
                    NewNode::new("Memory")
                        .text(text.clone())
                        .embedding(vector.clone()),
                )?;
            }
            Ok(())
        })
        .unwrap();
        at += n;
    }
    started.elapsed()
}

fn single_writes(db: &Grafeno, corpus: &Corpus, rng: &mut Rng) {
    println!("| write | own transaction p50 ms | p95 ms | p99 ms | in groups of 200, ms each |");
    println!("| --- | --- | --- | --- | --- |");
    let kinds = [
        "empty commit",
        "node",
        "node + text",
        "node + vector",
        "node + text + vector",
    ];
    for (kind, name) in kinds.iter().enumerate() {
        let mut times = Vec::new();
        for _ in 0..300 {
            let (vector, text) = (corpus.vector(rng), Corpus::text(rng));
            let started = Instant::now();
            db.write(|tx| {
                let mut node = NewNode::new("Memory").prop("kind", kind as i64);
                if kind == 2 || kind == 4 {
                    node = node.text(text.clone());
                }
                if kind == 3 || kind == 4 {
                    node = node.embedding(vector.clone());
                }
                if kind > 0 {
                    tx.create_node(NS, node)?;
                }
                Ok(())
            })
            .unwrap();
            times.push(started.elapsed());
        }
        times.sort();
        // The same writes 200 to a transaction, as the group commit runs
        // them under load: what is left per write once the commit and the
        // pages they share are paid once.
        let batch: Vec<(Vec<f32>, String)> = (0..200)
            .map(|_| (corpus.vector(rng), Corpus::text(rng)))
            .collect();
        let started = Instant::now();
        db.write(|tx| {
            for (vector, text) in &batch {
                let mut node = NewNode::new("Memory").prop("kind", kind as i64);
                if kind == 2 || kind == 4 {
                    node = node.text(text.clone());
                }
                if kind == 3 || kind == 4 {
                    node = node.embedding(vector.clone());
                }
                if kind > 0 {
                    tx.create_node(NS, node)?;
                }
            }
            Ok(())
        })
        .unwrap();
        let grouped = started.elapsed() / 200;
        println!(
            "| {name} | {} | {} | {} | {} |",
            ms(percentile(&times, 0.5)),
            ms(percentile(&times, 0.95)),
            ms(percentile(&times, 0.99)),
            ms(grouped)
        );
    }
}

struct Outcome {
    latencies: Vec<Duration>,
    rejected: usize,
    elapsed: Duration,
}

fn offer(writer: &Arc<Writer>, corpus: &Arc<Corpus>, rate: f64, seconds: f64) -> Outcome {
    let total = (rate * seconds) as usize;
    let start = Instant::now() + Duration::from_millis(50);
    let next = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new((Vec::with_capacity(total), 0usize)));
    std::thread::scope(|scope| {
        for client in 0..CLIENTS.min(total.max(1)) {
            let (writer, corpus, next, results) = (
                writer.clone(),
                corpus.clone(),
                next.clone(),
                results.clone(),
            );
            scope.spawn(move || {
                let mut rng = Rng(0xABCD + client as u64);
                loop {
                    let ticket = next.fetch_add(1, Ordering::Relaxed);
                    if ticket >= total {
                        return;
                    }
                    let (vector, text) = (corpus.vector(&mut rng), Corpus::text(&mut rng));
                    let scheduled = start + Duration::from_secs_f64(ticket as f64 / rate);
                    until(scheduled);
                    let outcome = writer.write(move |tx| {
                        tx.create_node(
                            NS,
                            NewNode::new("Memory")
                                .text(text.clone())
                                .embedding(vector.clone()),
                        )
                    });
                    let latency = scheduled.elapsed();
                    let mut results = results.lock().unwrap();
                    match outcome {
                        Ok(_) => results.0.push(latency),
                        Err(Error::Busy(_)) => results.1 += 1,
                        Err(error) => panic!("{error}"),
                    }
                }
            });
        }
    });
    let (mut latencies, rejected) = std::mem::take(&mut *results.lock().unwrap());
    latencies.sort();
    Outcome {
        latencies,
        rejected,
        elapsed: start.elapsed(),
    }
}

fn load(db: &Arc<Grafeno>, corpus: &Arc<Corpus>, seconds: f64) {
    let writer = Arc::new(Writer::new(db.clone(), WriterOptions::default()));
    println!(
        "| offered/s | done/s | p50 ms | p95 ms | p99 ms | writes/commit | mean commit ms | rejected | search p50 / p99 ms |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for rate in [100.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0] {
        let before = writer.metrics();
        let searching = Arc::new(AtomicBool::new(true));
        let searches: Vec<_> = (0..SEARCHERS)
            .map(|s| {
                let (db, corpus, searching) = (db.clone(), corpus.clone(), searching.clone());
                std::thread::spawn(move || {
                    let mut rng = Rng(0x5EA + s as u64);
                    let mut times = Vec::new();
                    while searching.load(Ordering::Relaxed) {
                        let query = corpus.vector(&mut rng);
                        let started = Instant::now();
                        db.begin_read()
                            .unwrap()
                            .search_vector(NS, &query, 10)
                            .unwrap();
                        times.push(started.elapsed());
                    }
                    times
                })
            })
            .collect();
        let outcome = offer(&writer, corpus, rate, seconds);
        searching.store(false, Ordering::Relaxed);
        let mut search_times: Vec<Duration> = searches
            .into_iter()
            .flat_map(|s| s.join().unwrap())
            .collect();
        search_times.sort();
        let after = writer.metrics();
        let groups = (after.groups - before.groups).max(1);
        let commit = (after.commit_seconds.sum - before.commit_seconds.sum) / groups as f64;
        let l = &outcome.latencies;
        println!(
            "| {rate:.0} | {:.0} | {} | {} | {} | {:.1} | {:.1} | {} | {} / {} |",
            l.len() as f64 / outcome.elapsed.as_secs_f64(),
            ms(percentile(l, 0.5)),
            ms(percentile(l, 0.95)),
            ms(percentile(l, 0.99)),
            (after.writes - before.writes) as f64 / groups as f64,
            commit * 1e3,
            outcome.rejected,
            ms(percentile(&search_times, 0.5)),
            ms(percentile(&search_times, 0.99)),
        );
        if outcome.rejected > 0 || percentile(l, 0.5) > Duration::from_secs(2) {
            break;
        }
    }
}

fn main() {
    let sizes: Vec<usize> = {
        let args: Vec<usize> = std::env::args()
            .skip(1)
            .map(|a| a.parse().expect("sizes"))
            .collect();
        if args.is_empty() {
            vec![10_000, 100_000]
        } else {
            args
        }
    };
    let seconds: f64 = std::env::var("SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);
    let path = temp();
    let db = Arc::new(Grafeno::open(&path).unwrap());
    db.set_durability(Durability::Immediate);
    let corpus = Arc::new(Corpus::new());
    let mut rng = Rng(0xBEEF);
    let mut size = 0;
    for target in sizes {
        // The writes of earlier steps count toward the graph.
        let have = db
            .begin_read()
            .unwrap()
            .vector_health(NS)
            .unwrap()
            .first()
            .map_or(0, |h| h.vectors as usize);
        size = size.max(have);
        let took = fill(&db, &corpus, &mut rng, size, target);
        size = target.max(size);
        println!(
            "\n## {size} vectors (filled in {:.0} s)\n",
            took.as_secs_f64()
        );
        single_writes(&db, &corpus, &mut rng);
        println!();
        load(&db, &corpus, seconds);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
