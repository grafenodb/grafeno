//! Hubs in recall's graph expansion: how often a node linked to everything
//! reaches the top 10, and what damping costs.
//!
//! 20 000 notes of 12 words from a 3 000-word vocabulary, each linked to
//! 2 of 200 topics and all to one user node. 300 queries of 2 words taken
//! from random notes, recall of 10, depth 1 and 2.
//!
//! Run with `cargo run --release -p grafeno-memory --example hub_bench`.

use std::time::Instant;

use grafeno::{Grafeno, NewEdge, NewNode, NodeId};
use grafeno_memory::{Expansion, RecallQuery, recall};

const NS: &str = "bench";

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
}

fn word(i: usize) -> String {
    let letters = b"abcdefghijklmnopqrstuvwxyz";
    let mut out = String::from("w");
    let mut n = i + 1;
    while n > 0 {
        out.push(letters[n % 26] as char);
        n /= 26;
    }
    out
}

fn main() -> grafeno::Result<()> {
    let mut rng = Rng(0x5EED);
    let db = Grafeno::in_memory()?;
    let mut texts = Vec::new();
    let user = db.write(|tx| {
        let user = tx.create_node(NS, NewNode::new("Person").prop("name", "user"))?;
        let topics: Vec<NodeId> = (0..200)
            .map(|t| tx.create_node(NS, NewNode::new("Topic").prop("name", format!("topic {t}"))))
            .collect::<grafeno::Result<_>>()?;
        for _ in 0..20_000 {
            // Skewed: low word numbers are common.
            let text: Vec<String> = (0..12)
                .map(|_| {
                    let common = rng.below(3_000) + 1;
                    word(rng.below(common))
                })
                .collect();
            let text = text.join(" ");
            let note = tx.create_node(NS, NewNode::new("Note").text(text.clone()))?;
            texts.push(text);
            tx.create_edge(NS, NewEdge::new(user, "wrote", note))?;
            for _ in 0..2 {
                tx.create_edge(NS, NewEdge::new(note, "about", topics[rng.below(200)]))?;
            }
        }
        Ok(user)
    })?;
    let queries: Vec<String> = (0..300)
        .map(|_| {
            let note = rng.below(texts.len());
            let words: Vec<&str> = texts[note].split(' ').collect();
            let (a, b) = (rng.below(12), rng.below(12));
            format!("{} {}", words[a], words[b])
        })
        .collect();

    let read = db.begin_read()?;
    println!(
        "| depth | hubs | fanout | user in top 10 | topics in top 10 (mean) | p50 ms | p99 ms |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- |");
    for depth in [1, 2] {
        for (hubs, fanout) in [(0.0, 0), (1.0, 0), (1.0, 100)] {
            let expansion = Expansion {
                depth,
                hubs,
                fanout,
                ..Expansion::default()
            };
            let mut with_user = 0;
            let mut topics = 0;
            let mut times = Vec::new();
            for query in &queries {
                let start = Instant::now();
                let found = recall(
                    &read,
                    NS,
                    &RecallQuery::new()
                        .text(query.as_str())
                        .expand(Some(expansion.clone())),
                )?;
                times.push(start.elapsed().as_secs_f64() * 1e3);
                with_user += found.iter().any(|r| r.node.id == user) as usize;
                topics += found.iter().filter(|r| r.node.has_label("Topic")).count();
            }
            times.sort_by(f64::total_cmp);
            println!(
                "| {depth} | {hubs} | {fanout} | {:.0}% | {:.1} | {:.1} | {:.1} |",
                100.0 * with_user as f64 / queries.len() as f64,
                topics as f64 / queries.len() as f64,
                times[times.len() / 2],
                times[times.len() * 99 / 100],
            );
        }
    }
    Ok(())
}
