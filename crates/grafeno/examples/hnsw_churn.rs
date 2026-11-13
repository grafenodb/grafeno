//! Measures the vector index as it changes: recall@10 against exact search
//! after deleting 10%, 30% and 50% of the vectors, after inserting them
//! back, and after rounds of updates (churn).
//!
//! ```text
//! cargo run --release -p grafeno --example hnsw_churn -- [vectors] [dimension] [queries]
//! ```
//!
//! Defaults: 20 000 vectors of dimension 128 in 64 clusters, 200 queries.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use grafeno::{Grafeno, NewNode, NodeId};

const NS: &str = "churn";
const CLUSTERS: usize = 64;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }

    fn normal(&mut self) -> f32 {
        (0..4).map(|_| self.unit()).sum::<f32>() * 0.5
    }
}

fn normalized(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    vector.iter_mut().for_each(|x| *x /= norm);
    vector
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// recall@10 at each `ef`, against the exact neighbors among `live`.
fn recall(
    db: &Grafeno,
    live: &HashMap<NodeId, Vec<f32>>,
    queries: &[Vec<f32>],
    efs: &[usize],
) -> Vec<f64> {
    let exact: Vec<HashSet<NodeId>> = queries
        .iter()
        .map(|query| {
            let mut scored: Vec<(f32, NodeId)> =
                live.iter().map(|(id, v)| (dot(query, v), *id)).collect();
            scored.sort_by(|a, b| b.0.total_cmp(&a.0));
            scored.into_iter().take(10).map(|(_, id)| id).collect()
        })
        .collect();
    let read = db.begin_read().unwrap();
    efs.iter()
        .map(|&ef| {
            let per: Vec<usize> = queries
                .iter()
                .zip(&exact)
                .map(|(query, truth)| {
                    read.search_vector_with(NS, query, 10, ef)
                        .unwrap()
                        .iter()
                        .filter(|hit| truth.contains(&hit.node))
                        .count()
                })
                .collect();
            if std::env::var_os("CHURN_DEBUG").is_some() && ef == 64 {
                let lost = per.iter().filter(|n| **n < 5).count();
                let hits0: Vec<usize> = queries
                    .iter()
                    .map(|q| read.search_vector_with(NS, q, 10, ef).unwrap().len())
                    .collect();
                eprintln!(
                    "  queries with recall < 0.5: {lost}; queries with < 10 results: {}",
                    hits0.iter().filter(|n| **n < 10).count()
                );
            }
            let found: usize = per.iter().sum();
            found as f64 / (queries.len() * 10) as f64
        })
        .collect()
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|a| a.parse().expect("numbers"))
        .collect();
    let count = args.first().copied().unwrap_or(20_000);
    let dimension = args.get(1).copied().unwrap_or(128);
    let queries = args.get(2).copied().unwrap_or(200);
    let efs = [16, 32, 64];

    let mut rng = Rng(0xC0FFEE);
    let centers: Vec<Vec<f32>> = (0..CLUSTERS)
        .map(|_| (0..dimension).map(|_| rng.unit()).collect())
        .collect();
    let point = |rng: &mut Rng| {
        let center = &centers[(rng.next() % CLUSTERS as u64) as usize];
        normalized(center.iter().map(|c| c + 0.35 * rng.normal()).collect())
    };
    let tests: Vec<Vec<f32>> = (0..queries).map(|_| point(&mut rng)).collect();

    let db = Grafeno::in_memory().unwrap();
    let mut live: HashMap<NodeId, Vec<f32>> = HashMap::new();
    let mut ids: Vec<NodeId> = Vec::new();
    let started = Instant::now();
    for _ in 0..count.div_ceil(1_000) {
        let batch: Vec<Vec<f32>> = (0..1_000.min(count - ids.len()))
            .map(|_| point(&mut rng))
            .collect();
        let created = db
            .write(|tx| {
                batch
                    .iter()
                    .map(|v| tx.create_node(NS, NewNode::new("Point").embedding(v.clone())))
                    .collect::<grafeno::Result<Vec<_>>>()
            })
            .unwrap();
        for (id, v) in created.iter().zip(batch) {
            live.insert(*id, v);
        }
        ids.extend(created);
    }
    println!(
        "{count} vectors of dimension {dimension}, built in {:.1} s\n",
        started.elapsed().as_secs_f64()
    );
    println!(
        "| State | Vectors | Removed since built | recall@10 ef 16 | ef 32 | ef 64 | Dangling links | Unreachable |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    let report = |state: &str, live: &HashMap<NodeId, Vec<f32>>| {
        let values = recall(&db, live, &tests, &efs);
        let checked = db.begin_read().unwrap().check(NS).unwrap();
        if !checked.problems.is_empty() {
            eprintln!("  problems: {:?}", checked.problems);
        }
        let dangling = checked.dangling_links;
        let unreachable = checked.unreachable_vectors;
        let health = db.begin_read().unwrap().vector_health(NS).unwrap();
        let removed = health.first().map_or(0, |h| h.removed);
        println!(
            "| {state} | {} | {removed} | {:.3} | {:.3} | {:.3} | {dangling} | {unreachable} |",
            live.len(),
            values[0],
            values[1],
            values[2]
        );
    };
    report("built", &live);

    for share in [10, 30, 50] {
        // Delete a random share of the vectors (the nodes stay).
        let mut order = ids.clone();
        for i in (1..order.len()).rev() {
            order.swap(i, (rng.next() % (i as u64 + 1)) as usize);
        }
        let gone: Vec<NodeId> = order[..ids.len() * share / 100].to_vec();
        let started = Instant::now();
        for chunk in gone.chunks(1_000) {
            db.write(|tx| {
                for id in chunk {
                    tx.remove_embedding(NS, *id)?;
                }
                Ok(())
            })
            .unwrap();
        }
        let saved: Vec<(NodeId, Vec<f32>)> = gone
            .iter()
            .map(|id| (*id, live.remove(id).unwrap()))
            .collect();
        let took = started.elapsed().as_secs_f64();
        report(&format!("{share}% deleted ({took:.1} s)"), &live);
        let as_new = std::env::var_os("CHURN_AS_NEW").is_some();
        for chunk in saved.chunks(1_000) {
            let created = db
                .write(|tx| {
                    let mut created = Vec::new();
                    for (id, v) in chunk {
                        if as_new {
                            created.push(
                                tx.create_node(NS, NewNode::new("Point").embedding(v.clone()))?,
                            );
                        } else {
                            tx.set_embedding(NS, *id, v)?;
                            created.push(*id);
                        }
                    }
                    Ok(created)
                })
                .unwrap();
            for (id, (_, v)) in created.iter().zip(chunk) {
                live.insert(*id, v.clone());
            }
            if as_new {
                ids.extend(created);
            }
        }
        report(&format!("{share}% inserted back"), &live);
    }

    // Churn: a fifth of the vectors move elsewhere, five times.
    for round in 1..=5 {
        let moved: Vec<NodeId> = (0..ids.len() / 5)
            .map(|_| ids[(rng.next() % ids.len() as u64) as usize])
            .collect();
        for chunk in moved.chunks(1_000) {
            let fresh: Vec<Vec<f32>> = chunk.iter().map(|_| point(&mut rng)).collect();
            db.write(|tx| {
                for (id, v) in chunk.iter().zip(&fresh) {
                    tx.set_embedding(NS, *id, v)?;
                }
                Ok(())
            })
            .unwrap();
            for (id, v) in chunk.iter().zip(fresh) {
                live.insert(*id, v);
            }
        }
        report(&format!("churn round {round} (20% updated)"), &live);
    }

    // What automatic compaction does once removals pass its threshold.
    let health = db.begin_read().unwrap().vector_health(NS).unwrap();
    eprintln!("health: {}", serde_json::to_string(&health).unwrap());
    let started = Instant::now();
    db.write(|tx| tx.compact_vectors(NS)).unwrap();
    let took = started.elapsed().as_secs_f64();
    report(&format!("compacted ({took:.1} s)"), &live);
}
