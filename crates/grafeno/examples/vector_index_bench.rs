//! Named vector indexes (16.7): recall@10 and latency for each similarity,
//! and for filters of several selectivities applied during the walk,
//! against filtering the plain search's results afterwards.
//!
//! ```text
//! cargo run --release -p grafeno --example vector_index_bench -- [vectors] [dimension] [queries]
//! ```
//!
//! Defaults: 20 000 vectors of dimension 128 in 64 clusters (as
//! `hnsw_churn` measures), 200 queries, `ef` 64. Labels `Below50`,
//! `Below10` and `Below1` mark a half, a tenth and a hundredth of the
//! nodes (by position modulo 100): the filters keep one of them.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use grafeno::{
    Grafeno, IndexDef, NewNode, NodeId, Similarity, VectorFilter, VectorSearch, VectorSpec,
};
use serde_json::json;

const NS: &str = "bench";
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

fn distance(similarity: Similarity, a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    match similarity {
        Similarity::Cosine => {
            let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            1.0 - dot / (na * nb)
        }
        Similarity::Dot => -dot,
        Similarity::Euclidean => a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum(),
    }
}

fn percentile(times: &mut [Duration], at: f64) -> f64 {
    times.sort();
    let index = ((times.len() as f64 * at) as usize).min(times.len() - 1);
    times[index].as_secs_f64() * 1000.0
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let count = args.first().copied().unwrap_or(20_000);
    let dimension = args.get(1).copied().unwrap_or(128);
    let queries_count = args.get(2).copied().unwrap_or(200);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let centers: Vec<Vec<f32>> = (0..CLUSTERS)
        .map(|_| (0..dimension).map(|_| rng.unit() * 3.0).collect())
        .collect();
    let point = |rng: &mut Rng| -> Vec<f32> {
        let center = &centers[(rng.next() % CLUSTERS as u64) as usize];
        center.iter().map(|c| c + rng.normal()).collect()
    };
    let vectors: Vec<Vec<f32>> = (0..count).map(|_| point(&mut rng)).collect();
    let queries: Vec<Vec<f32>> = (0..queries_count).map(|_| point(&mut rng)).collect();

    let db = Grafeno::in_memory().unwrap();
    let ids: Vec<NodeId> = db
        .write(|tx| {
            vectors
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let mut node = NewNode::new("Item").prop("v", json!(v));
                    for below in [50, 10, 1] {
                        if i % 100 < below {
                            node = node.label(format!("Below{below}"));
                        }
                    }
                    tx.create_node(NS, node)
                })
                .collect()
        })
        .unwrap();
    for similarity in [Similarity::Cosine, Similarity::Dot, Similarity::Euclidean] {
        let started = Instant::now();
        db.write(|tx| {
            tx.create_index(
                NS,
                IndexDef {
                    name: similarity.name().into(),
                    vector: Some(VectorSpec {
                        similarity,
                        ..VectorSpec::new(dimension)
                    }),
                    ..IndexDef::new("Item", "v")
                },
            )
        })
        .unwrap();
        println!(
            "{:>9}: built in {:.1} s",
            similarity.name(),
            started.elapsed().as_secs_f64()
        );
    }
    let read = db.begin_read().unwrap();
    let exact = |similarity: Similarity, query: &[f32], below: usize| {
        let mut scored: Vec<(f32, usize)> = vectors
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 100 < below)
            .map(|(i, v)| (distance(similarity, query, v), i))
            .collect();
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored
            .into_iter()
            .take(10)
            .map(|(_, i)| ids[i])
            .collect::<HashSet<NodeId>>()
    };
    println!(
        "
| similarity | filter keeps | during the walk: recall@10, p50, p99 | after the walk: recall@10 |"
    );
    println!("| --- | --- | --- | --- |");
    for similarity in [Similarity::Cosine, Similarity::Dot, Similarity::Euclidean] {
        for (share, below) in [
            ("all", 100),
            ("half", 50),
            ("a tenth", 10),
            ("a hundredth", 1),
        ] {
            let label = format!("Below{below}");
            let filter = (below < 100).then(|| VectorFilter {
                labels: vec![label.clone()],
                props: grafeno::Props::new(),
            });
            let (mut found, mut after, mut total, mut times) = (0usize, 0usize, 0usize, Vec::new());
            for query in &queries {
                let truth = exact(similarity, query, below);
                let started = Instant::now();
                let hits = read
                    .vector_search(
                        NS,
                        query,
                        &VectorSearch {
                            index: Some(similarity.name().into()),
                            limit: 10,
                            filter: filter.clone(),
                            ..VectorSearch::default()
                        },
                    )
                    .unwrap();
                times.push(started.elapsed());
                found += hits.iter().filter(|hit| truth.contains(&hit.node)).count();
                // Filtering afterwards: the ten nearest of all, then the
                // ones with the label.
                let plain = read
                    .vector_search(
                        NS,
                        query,
                        &VectorSearch {
                            index: Some(similarity.name().into()),
                            limit: 10,
                            ..VectorSearch::default()
                        },
                    )
                    .unwrap();
                after += plain
                    .iter()
                    .filter(|hit| {
                        truth.contains(&hit.node)
                            && read.node(NS, hit.node).unwrap().unwrap().has_label(&label)
                            || below == 100 && truth.contains(&hit.node)
                    })
                    .count();
                total += truth.len();
            }
            println!(
                "| {} | {share} | {:.3}, {:.2} ms, {:.2} ms | {:.3} |",
                similarity.name(),
                found as f64 / total.max(1) as f64,
                percentile(&mut times, 0.5),
                percentile(&mut times, 0.99),
                after as f64 / total.max(1) as f64,
            );
        }
    }
}
