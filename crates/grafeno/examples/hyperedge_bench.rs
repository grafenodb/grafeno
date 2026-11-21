//! Hyperedges against binary edges: creating them (3, 10, 50 and 100
//! participants), reading one by id, a node's hyperedges, a label's in
//! pages, by participants, replacing participants and deleting, and the
//! bytes each table takes once the file is closed.
//!
//! Every phase runs in batches of `BATCH` operations per transaction with
//! eventual durability (no fsync per commit), so the numbers measure the
//! engine and redb; reads are single operations on one snapshot, timed one
//! by one (p50, p99).
//!
//! `cargo run --release -p grafeno --example hyperedge_bench -- [nodes] [hyperedges]`

use std::time::{Duration, Instant};

use grafeno::{
    Durability, Grafeno, HyperEdgeFilter, HyperEdgeId, NewEdge, NewHyperEdge, NewNode, NodeId,
    Participant, ParticipantMatch,
};
use redb::{ReadableDatabase, ReadableTableMetadata, TableHandle};

const NS: &str = "bench";
const BATCH: usize = 500;
const SAMPLES: usize = 2_000;

/// Deterministic pseudo-random numbers (xorshift64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// `k` distinct indexes below `n`.
    fn distinct(&mut self, k: usize, n: usize) -> Vec<usize> {
        let mut picked = std::collections::BTreeSet::new();
        while picked.len() < k {
            picked.insert(self.below(n));
        }
        picked.into_iter().collect()
    }
}

fn per_second(count: usize, took: Duration) -> f64 {
    count as f64 / took.as_secs_f64()
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// p50 and p99 of `samples`, in microseconds.
fn percentiles(mut samples: Vec<Duration>) -> (f64, f64) {
    samples.sort();
    let at = |q: f64| micros(samples[((samples.len() - 1) as f64 * q) as usize]);
    (at(0.50), at(0.99))
}

fn time_each(count: usize, mut f: impl FnMut(usize)) -> (f64, f64) {
    let samples = (0..count)
        .map(|i| {
            let started = Instant::now();
            f(i);
            started.elapsed()
        })
        .collect();
    percentiles(samples)
}

/// Bytes stored by the tables whose name starts with one of `prefixes`,
/// with redb's B-tree metadata.
fn table_bytes(path: &std::path::Path, prefixes: &[&str]) -> Vec<(String, u64, u64)> {
    let db = redb::Database::open(path).unwrap();
    let tx = db.begin_read().unwrap();
    let mut out = Vec::new();
    let mut handles: Vec<_> = tx.list_tables().unwrap().collect();
    handles.sort_by(|a, b| a.name().cmp(b.name()));
    for handle in handles {
        let name = handle.name().to_owned();
        if !prefixes.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let table = tx.open_untyped_table(handle).unwrap();
        let stats = table.stats().unwrap();
        out.push((
            name,
            table.len().unwrap(),
            stats.stored_bytes() + stats.metadata_bytes(),
        ));
    }
    out
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let node_count = args.first().copied().unwrap_or(10_000);
    let count = args.get(1).copied().unwrap_or(5_000);
    let mut rng = Rng(88172645463325252);
    println!("{node_count} nodes, {count} hyperedges per size, batches of {BATCH}");

    // Binary edges, as the baseline.
    {
        let path = std::env::temp_dir().join(format!("grafeno-hyper-edges-{}.redb", NodeId::new()));
        let db = Grafeno::open(&path).unwrap();
        db.set_durability(Durability::Eventual);
        let ids = make_nodes(&db, node_count);
        let started = Instant::now();
        for chunk in 0..count.div_ceil(BATCH) {
            let size = BATCH.min(count - chunk * BATCH);
            db.write(|tx| {
                for _ in 0..size {
                    let (a, b) = (ids[rng.below(node_count)], ids[rng.below(node_count)]);
                    tx.create_edge(NS, NewEdge::new(a, "link", b))?;
                }
                Ok(())
            })
            .unwrap();
        }
        let took = started.elapsed();
        db.flush().unwrap();
        drop(db);
        let bytes: u64 = table_bytes(&path, &["edges", "adjacency_"])
            .iter()
            .map(|t| t.2)
            .sum();
        println!(
            "\n| binary edges | {:.0}/s | {:.1} µs each | edges + adjacency {:.1} B each |",
            per_second(count, took),
            micros(took) / count as f64,
            bytes as f64 / count as f64
        );
        std::fs::remove_file(&path).unwrap();
    }

    println!(
        "\n| participants | create /s | create µs | by id p50/p99 µs | node's p50/p99 µs | \
         label page of 100 p50/p99 µs | contains 2 p50/p99 µs | exact p50/p99 µs | \
         replace one /s | delete /s |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut sizes = Vec::new();
    for k in [3usize, 10, 50, 100] {
        let path = std::env::temp_dir().join(format!("grafeno-hyper-{k}-{}.redb", NodeId::new()));
        let db = Grafeno::open(&path).unwrap();
        db.set_durability(Durability::Eventual);
        let ids = make_nodes(&db, node_count);
        let mut members: Vec<Vec<NodeId>> = Vec::with_capacity(count);
        let mut made: Vec<HyperEdgeId> = Vec::with_capacity(count);
        let started = Instant::now();
        for chunk in 0..count.div_ceil(BATCH) {
            let size = BATCH.min(count - chunk * BATCH);
            let picks: Vec<Vec<NodeId>> = (0..size)
                .map(|_| {
                    rng.distinct(k, node_count)
                        .into_iter()
                        .map(|i| ids[i])
                        .collect()
                })
                .collect();
            let new_ids = db
                .write(|tx| {
                    picks
                        .iter()
                        .enumerate()
                        .map(|(i, nodes)| {
                            let mut new =
                                NewHyperEdge::new(if i % 2 == 0 { "even" } else { "odd" });
                            for (j, node) in nodes.iter().enumerate() {
                                new = new.member(*node, if j == 0 { "lead" } else { "member" });
                            }
                            tx.create_hyperedge(NS, new)
                        })
                        .collect::<grafeno::Result<Vec<_>>>()
                })
                .unwrap();
            made.extend(new_ids);
            members.extend(picks);
        }
        let create = started.elapsed();
        db.flush().unwrap();

        let read = db.begin_read().unwrap();
        let any = HyperEdgeFilter::any();
        let by_id = time_each(SAMPLES, |i| {
            let id = made[(i * 7919) % made.len()];
            assert!(read.hyperedge(NS, id).unwrap().is_some());
        });
        let mut found = 0usize;
        let by_node = time_each(SAMPLES, |i| {
            found += read
                .node_hyperedges(NS, ids[(i * 104_729) % node_count], &any)
                .unwrap()
                .len();
        });
        let mut after: Option<HyperEdgeId> = None;
        let by_label = time_each(SAMPLES.min(count / 200).max(1), |_| {
            let page = read
                .hyperedges_with_label(NS, "even", &any, after, 100)
                .unwrap();
            after = page.last().map(|e| e.id);
        });
        let contains = time_each(SAMPLES, |i| {
            let m = &members[(i * 31) % members.len()];
            let hits = read
                .hyperedges_by_participants(NS, &m[..2], ParticipantMatch::Contains, &any)
                .unwrap();
            assert!(!hits.is_empty());
        });
        let exact = time_each(SAMPLES, |i| {
            let m = &members[(i * 31) % members.len()];
            let hits = read
                .hyperedges_by_participants(NS, m, ParticipantMatch::Exact, &any)
                .unwrap();
            assert!(!hits.is_empty());
        });
        drop(read);

        // Replace one participant of each hyperedge in the first half.
        let half = count / 2;
        let started = Instant::now();
        for chunk in 0..half.div_ceil(BATCH) {
            let range = chunk * BATCH..(chunk * BATCH + BATCH).min(half);
            let swaps: Vec<(HyperEdgeId, Vec<Participant>)> = range
                .map(|i| {
                    let mut list: Vec<Participant> = members[i]
                        .iter()
                        .enumerate()
                        .map(|(j, n)| {
                            Participant::with_role(*n, if j == 0 { "lead" } else { "member" })
                        })
                        .collect();
                    let fresh = loop {
                        let candidate = ids[rng.below(node_count)];
                        if !members[i].contains(&candidate) {
                            break candidate;
                        }
                    };
                    list.last_mut().unwrap().node = fresh;
                    (made[i], list)
                })
                .collect();
            db.write(|tx| {
                for (id, list) in swaps {
                    tx.set_hyperedge_participants(NS, id, list)?;
                }
                Ok(())
            })
            .unwrap();
        }
        let replace = started.elapsed();
        db.flush().unwrap();
        drop(db);
        let tables = table_bytes(&path, &["hyperedge"]);
        let db = Grafeno::open(&path).unwrap();
        db.set_durability(Durability::Eventual);

        // Delete the second half.
        let started = Instant::now();
        for chunk in made[half..].chunks(BATCH) {
            db.write(|tx| {
                for id in chunk {
                    tx.delete_hyperedge(NS, *id)?;
                }
                Ok(())
            })
            .unwrap();
        }
        let delete = started.elapsed();
        let report = db.begin_read().unwrap().check(NS).unwrap();
        assert!(report.is_ok(), "{:#?}", report.problems);
        assert_eq!(report.hyperedges as usize, half);
        drop(db);

        println!(
            "| {k} | {:.0} | {:.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.0} | {:.0} |",
            per_second(count, create),
            micros(create) / count as f64,
            by_id.0,
            by_id.1,
            by_node.0,
            by_node.1,
            by_label.0,
            by_label.1,
            contains.0,
            contains.1,
            exact.0,
            exact.1,
            per_second(half, replace),
            per_second(count - half, delete),
        );
        let _ = found;
        sizes.push((k, tables));
        std::fs::remove_file(&path).unwrap();
    }

    println!(
        "\nStored bytes with B-tree metadata, after creating {count} and replacing one participant of half:"
    );
    println!("| participants | table | entries | KiB | bytes per hyperedge |");
    println!("| --- | --- | --- | --- | --- |");
    for (k, tables) in sizes {
        for (name, entries, bytes) in tables {
            println!(
                "| {k} | {name} | {entries} | {:.1} | {:.1} |",
                bytes as f64 / 1024.0,
                bytes as f64 / count as f64
            );
        }
    }
}

fn make_nodes(db: &Grafeno, count: usize) -> Vec<NodeId> {
    let mut ids = Vec::with_capacity(count);
    for chunk in 0..count.div_ceil(5_000) {
        let made = db
            .write(|tx| {
                (chunk * 5_000..(chunk * 5_000 + 5_000).min(count))
                    .map(|i| tx.create_node(NS, NewNode::new("Item").prop("i", i as i64)))
                    .collect::<grafeno::Result<Vec<_>>>()
            })
            .unwrap();
        ids.extend(made);
    }
    ids
}
