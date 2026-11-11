//! The cost of event values (16.10): the same writes with events off, on
//! with no subscription, on with one narrow subscription, and on with one
//! that follows everything and asks for 384-dimension vectors.
//!
//! `cargo run --release -p grafeno --example events_bench [nodes] [case] [plain]`

use std::time::Instant;

use grafeno::{EventTarget, Grafeno, NewNode, NodeId, Props, Subscription};
use serde_json::json;

const DIMENSION: usize = 384;

fn vector(seed: usize) -> Vec<f32> {
    (0..DIMENSION)
        .map(|i| (((seed * 31 + i * 17) % 101) as f32 / 101.0) - 0.5)
        .collect()
}

/// Creates the nodes, then changes each one's city and vector: seconds.
fn load(db: &Grafeno, nodes: usize, vectors: bool) -> f64 {
    let started = Instant::now();
    let mut ids: Vec<NodeId> = Vec::with_capacity(nodes);
    for chunk in 0..nodes.div_ceil(1_000) {
        db.write(|tx| {
            for k in chunk * 1_000..((chunk + 1) * 1_000).min(nodes) {
                let label = if k % 10 == 0 { "Person" } else { "Note" };
                let id = tx.create_node(
                    "bench",
                    NewNode::new(label)
                        .prop("n", k as i64)
                        .prop("city", "Lima")
                        .text(format!("note {k} about topic {}", k % 50)),
                )?;
                if vectors {
                    tx.set_embedding("bench", id, &vector(k))?;
                }
                ids.push(id);
            }
            Ok(())
        })
        .unwrap();
    }
    for (chunk, part) in ids.chunks(1_000).enumerate() {
        db.write(|tx| {
            for (k, id) in part.iter().enumerate() {
                let mut props = Props::new();
                props.insert("city".into(), json!("Cusco"));
                tx.set_node_props("bench", *id, props)?;
                if vectors {
                    tx.set_embedding("bench", *id, &vector(chunk * 1_000 + k + 7))?;
                }
            }
            Ok(())
        })
        .unwrap();
    }
    started.elapsed().as_secs_f64()
}

fn fresh(events: bool, subscriptions: Vec<Subscription>) -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    db.enable_events(events);
    db.write(|tx| {
        for subscription in subscriptions {
            tx.create_subscription("bench", subscription)?;
        }
        Ok(())
    })
    .unwrap();
    db
}

fn log_bytes(db: &Grafeno) -> usize {
    let read = db.begin_read().unwrap();
    let (mut since, mut bytes) = (0, 0usize);
    loop {
        let page = read.events_or_changes("bench", since);
        if page.0 == 0 {
            break;
        }
        bytes += page.1;
        since = page.2;
    }
    bytes
}

trait Pages {
    fn events_or_changes(&self, ns: &str, since: u64) -> (usize, usize, u64);
}

impl Pages for grafeno::ReadTx {
    /// Entries, their JSON size with values, and where to continue.
    fn events_or_changes(&self, ns: &str, since: u64) -> (usize, usize, u64) {
        let names: Vec<String> = self
            .subscriptions(ns)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        let page = match names.first() {
            Some(name) => self.events(ns, name, since, 1_000).unwrap(),
            None => self.changes(ns, since, 1_000).unwrap(),
        };
        let bytes = page
            .changes
            .iter()
            .map(|c| serde_json::to_vec(c).unwrap().len())
            .sum();
        let next = page.next(since);
        let count = if next == since {
            0
        } else {
            page.changes.len().max(1)
        };
        (count, bytes, next)
    }
}

fn main() {
    let nodes: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20_000);
    // `plain`: no vectors (their index dominates the time otherwise).
    let vectors = std::env::args().nth(3).as_deref() != Some("plain");
    println!(
        "{nodes} nodes with text{}, then each one's city{} changed; transactions of 1 000",
        if vectors {
            format!(" and a {DIMENSION}-dimension vector")
        } else {
            String::new()
        },
        if vectors { " and vector" } else { "" },
    );
    let narrow = Subscription {
        labels: vec!["Person".into()],
        keys: vec!["city".into()],
        ..Subscription::new("moves", vec![EventTarget::Properties])
    };
    let everything = Subscription {
        vector_values: true,
        ..Subscription::new(
            "everything",
            vec![
                EventTarget::Nodes,
                EventTarget::Properties,
                EventTarget::Text,
                EventTarget::Vectors,
            ],
        )
    };
    let cases = [
        ("events off", fresh(false, vec![])),
        ("events on, no subscription", fresh(true, vec![])),
        (
            "one subscription (:Person, city)",
            fresh(true, vec![narrow]),
        ),
        (
            "one subscription, everything WITH VECTORS",
            fresh(true, vec![everything]),
        ),
    ];
    // `only`: run one case (each in its own process, for a fair order).
    let only: Option<usize> = std::env::args().nth(2).and_then(|a| a.parse().ok());
    let mut base = None;
    for (index, (name, db)) in cases.into_iter().enumerate() {
        if only.is_some_and(|only| only != index) {
            continue;
        }
        let seconds = load(&db, nodes, vectors);
        let base = *base.get_or_insert(seconds);
        let bytes = log_bytes(&db);
        println!(
            "| {name} | {seconds:.2} s | {:.0} writes/s | {:+.1}% | {:.1} MiB of events |",
            (nodes * 2) as f64 / seconds,
            (seconds / base - 1.0) * 100.0,
            bytes as f64 / (1024.0 * 1024.0)
        );
    }
}
