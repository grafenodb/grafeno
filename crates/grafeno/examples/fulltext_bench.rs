//! Full-text indexes (16.6) against the node text search they sit beside:
//! precision at 10 on a labelled, synthetic set, and time per query.
//!
//! Each document belongs to one of 400 topics, each of eight words drawn
//! from a pool of 800 (so a word belongs to several topics): its title
//! holds one word of the topic and a common word, its body a few more
//! among common words and many words of other topics, every word in a random form (`graph`, `graphs`, `graphing`). A
//! query is two words of a topic in their plain form; the relevant
//! documents are that topic's. The node text holds title and body; the
//! full-text index has them as two fields, the title weighing three, with
//! the English analyzer (stems), and searches the words as `+a +b` (both
//! required) and as plain words.
//!
//! `cargo run --release -p grafeno --example fulltext_bench [documents]`

use std::time::Instant;

use grafeno::{Analyzer, Fulltext, Grafeno, IndexDef, NewNode, NodeId};

const NS: &str = "bench";
const TOPICS: usize = 400;
/// Words the topics draw from: each word belongs to several topics.
const POOL: usize = 800;
const WORDS_PER_TOPIC: usize = 8;

struct Random(u64);

impl Random {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// A made-up word: consonant-vowel syllables, never an English word that
/// a stemmer would treat differently.
fn word(seed: usize) -> String {
    const C: &[u8] = b"bdfgklmnprtvz";
    const V: &[u8] = b"aeiou";
    let mut out = String::new();
    let mut s = seed * 7919 + 13;
    for _ in 0..3 {
        out.push(C[s % C.len()] as char);
        s /= C.len();
        out.push(V[s % V.len()] as char);
        s /= V.len();
        s = s * 31 + 7;
    }
    out.push(C[seed % C.len()] as char);
    out
}

fn inflect(word: &str, random: &mut Random) -> String {
    match random.next(3) {
        0 => word.to_owned(),
        1 => format!("{word}s"),
        _ => format!("{word}ing"),
    }
}

fn main() {
    let documents: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(20_000);
    let vocabulary: Vec<Vec<String>> = (0..TOPICS)
        .map(|topic| {
            let mut picked = Random(topic as u64 * 2_654_435_761 + 1);
            (0..WORDS_PER_TOPIC)
                .map(|_| word(picked.next(POOL)))
                .collect()
        })
        .collect();
    let common: Vec<String> = (0..300).map(|i| word(10_000 + i)).collect();
    let mut random = Random(88172645463325252);
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_index(
            NS,
            IndexDef {
                name: "docs".into(),
                more_keys: vec!["body".into()],
                fulltext: Some(Fulltext {
                    analyzer: Analyzer::English,
                    weights: vec![3.0, 1.0],
                }),
                ..IndexDef::new("Doc", "title")
            },
        )
    })
    .unwrap();
    let mut topic_of: std::collections::HashMap<NodeId, usize> = Default::default();
    let started = Instant::now();
    for chunk in 0..documents.div_ceil(2_000) {
        let made = db
            .write(|tx| {
                let mut made = Vec::new();
                for _ in 0..2_000.min(documents - chunk * 2_000) {
                    let topic = random.next(TOPICS);
                    let words = &vocabulary[topic];
                    let title = format!(
                        "{} {}",
                        inflect(&words[random.next(WORDS_PER_TOPIC)], &mut random),
                        common[random.next(common.len())]
                    );
                    let mut body = Vec::new();
                    for _ in 0..30 {
                        let roll = random.next(20);
                        body.push(if roll < 3 {
                            inflect(&words[random.next(WORDS_PER_TOPIC)], &mut random)
                        } else if roll < 8 {
                            // A word of another topic.
                            let other = &vocabulary[random.next(TOPICS)];
                            inflect(&other[random.next(WORDS_PER_TOPIC)], &mut random)
                        } else {
                            common[random.next(common.len())].clone()
                        });
                    }
                    let body = body.join(" ");
                    let id = tx.create_node(
                        NS,
                        NewNode::new("Doc")
                            .prop("title", title.clone())
                            .prop("body", body.clone())
                            .text(format!("{title}. {body}")),
                    )?;
                    made.push((id, topic));
                }
                Ok(made)
            })
            .unwrap();
        topic_of.extend(made);
    }
    println!(
        "{documents} documents in {TOPICS} topics, loaded with both indexes in {:.1} s",
        started.elapsed().as_secs_f64()
    );
    let read = db.begin_read().unwrap();
    let queries: Vec<(usize, String, String)> = (0..200)
        .map(|_| {
            let topic = random.next(TOPICS);
            let a = random.next(WORDS_PER_TOPIC);
            let b = (a + 1 + random.next(WORDS_PER_TOPIC - 1)) % WORDS_PER_TOPIC;
            (
                topic,
                vocabulary[topic][a].clone(),
                vocabulary[topic][b].clone(),
            )
        })
        .collect();
    let precision = |found: &[NodeId], topic: usize| -> f64 {
        found
            .iter()
            .take(10)
            .filter(|id| topic_of.get(id) == Some(&topic))
            .count() as f64
            / 10.0
    };
    type Search<'a> = Box<dyn Fn(&str, &str) -> Vec<NodeId> + 'a>;
    let ways: Vec<(&str, Search)> = vec![
        (
            "node text (words)",
            Box::new(|a, b| {
                read.search_text(NS, &format!("{a} {b}"), 10)
                    .unwrap()
                    .into_iter()
                    .map(|hit| hit.node)
                    .collect()
            }),
        ),
        (
            "full-text, words",
            Box::new(|a, b| {
                read.fulltext_search(NS, "docs", &format!("{a} {b}"), 10, false)
                    .unwrap()
                    .into_iter()
                    .map(|hit| hit.node)
                    .collect()
            }),
        ),
        (
            "full-text, +a +b",
            Box::new(|a, b| {
                read.fulltext_search(NS, "docs", &format!("+{a} +{b}"), 10, false)
                    .unwrap()
                    .into_iter()
                    .map(|hit| hit.node)
                    .collect()
            }),
        ),
        (
            "full-text, fragments",
            Box::new(|a, b| {
                read.fulltext_search(NS, "docs", &format!("{a} {b}"), 10, true)
                    .unwrap()
                    .into_iter()
                    .map(|hit| hit.node)
                    .collect()
            }),
        ),
    ];
    for (name, search) in &ways {
        let started = Instant::now();
        let mut total = 0.0;
        for (topic, a, b) in &queries {
            total += precision(&search(a, b), *topic);
        }
        let per_query = started.elapsed().as_secs_f64() * 1000.0 / queries.len() as f64;
        println!(
            "{name:>22}: precision at 10 {:.3}, {per_query:.2} ms a query",
            total / queries.len() as f64
        );
    }
}
