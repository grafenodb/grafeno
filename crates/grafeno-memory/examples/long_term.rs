//! Long-term memory: thousands of conversations over two years, and
//! questions about decisions made in the first months, some of them
//! replaced a year later (`supersedes`). Each memory is as old as it is, so
//! recency ranks the recent chatter up and the old decisions down: the
//! question is whether they still come back.
//!
//! `cargo run --release -p grafeno-memory --example long_term -- [copies]`
//!
//! `copies` (1 by default) repeats the conversations that many times, each
//! copy a little different, to see how the answers hold as the memory
//! grows. Measured for each way of recalling: the old decision in first
//! place, in the first three and in the context an agent gets (600
//! tokens); for replaced decisions, the new one ahead of the old; and the
//! time a recall takes.

#[path = "common/recall_fixture.rs"]
mod fixture;

use std::collections::HashMap;
use std::time::Instant;

use grafeno::{Grafeno, NodeId, SearchMode};
use grafeno_memory::{ContextOptions, Ranking, RecallQuery, context_for, recall};

const NS: &str = "long";

pub struct Way {
    pub name: &'static str,
    pub mode: SearchMode,
    /// Importance and recency weigh in (the default).
    pub ranking: bool,
    /// Only decisions (the question is about one).
    pub decisions_only: bool,
}

pub const WAYS: [Way; 5] = [
    Way {
        name: "BM25",
        mode: SearchMode::Text,
        ranking: true,
        decisions_only: false,
    },
    Way {
        name: "HNSW",
        mode: SearchMode::Vector,
        ranking: true,
        decisions_only: false,
    },
    Way {
        name: "hybrid + graph (default)",
        mode: SearchMode::Hybrid,
        ranking: true,
        decisions_only: false,
    },
    Way {
        name: "hybrid + graph, relevance only",
        mode: SearchMode::Hybrid,
        ranking: false,
        decisions_only: false,
    },
    Way {
        name: "hybrid + graph, decisions only",
        mode: SearchMode::Hybrid,
        ranking: true,
        decisions_only: true,
    },
];

/// `embedding` a little changed (each number by up to ±0.01, from a fixed
/// seed), normalized again: a copy is another conversation, not the same
/// vector twice.
fn varied(embedding: &[f32], copy: usize) -> Vec<f32> {
    let mut state = 0x2545_f491_u32.wrapping_mul(copy as u32 + 1);
    let mut v: Vec<f32> = embedding
        .iter()
        .map(|x| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            x + (state as f32 / u32::MAX as f32 - 0.5) * 0.02
        })
        .collect();
    let norm = v
        .iter()
        .map(|x| x * x)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

/// The memory: `copies` of the conversations, and the decisions.
pub fn build(
    copies: usize,
) -> (
    Grafeno,
    HashMap<String, NodeId>,
    Vec<fixture::Question>,
    usize,
) {
    let (conversations, decisions, questions) = fixture::long_term();
    let db = Grafeno::in_memory().unwrap();
    let mut all = Vec::new();
    for copy in 0..copies {
        for c in &conversations {
            all.push(fixture::Memory {
                key: format!("{}-{copy}", c.key),
                label: c.label.clone(),
                text: if copy == 0 {
                    c.text.clone()
                } else {
                    format!("{} (#{copy})", c.text)
                },
                links: Vec::new(),
                embedding: if copy == 0 {
                    c.embedding.clone()
                } else {
                    varied(&c.embedding, copy)
                },
                at: c.at.clone(),
            });
        }
    }
    let count = all.len();
    all.extend(decisions);
    let ids = fixture::load(&db, NS, &all);
    (db, ids, questions, count)
}

/// Gives every decision an importance of 0.8, keeping its age.
pub fn mark_decisions_important(db: &Grafeno, ids: &HashMap<String, NodeId>) {
    db.write(|tx| {
        for (key, id) in ids.iter().filter(|(k, _)| k.starts_with("dec-")) {
            let _ = key;
            tx.set_importance(NS, *id, 0.8)?;
        }
        Ok(())
    })
    .unwrap();
}

pub struct Outcome {
    pub first: usize,
    pub in_three: usize,
    pub in_context: usize,
    /// Replaced decisions: the new one ranks ahead of the old.
    pub newer_first: usize,
    pub ms: f64,
}

pub fn measure(
    db: &Grafeno,
    ids: &HashMap<String, NodeId>,
    questions: &[fixture::Question],
    way: &Way,
) -> Outcome {
    let read = db.begin_read().unwrap();
    let mut outcome = Outcome {
        first: 0,
        in_three: 0,
        in_context: 0,
        newer_first: 0,
        ms: 0.0,
    };
    let started = Instant::now();
    for q in questions {
        let query = RecallQuery {
            text: Some(q.text.clone()),
            embedding: Some(q.embedding.clone()),
            limit: 10,
            mode: Some(way.mode),
            ranking: way.ranking.then(Ranking::default),
            labels: if way.decisions_only {
                vec!["Decision".into()]
            } else {
                Vec::new()
            },
            ..RecallQuery::default()
        };
        let found = recall(&read, NS, &query).unwrap();
        let answer = ids[&q.answer[0]];
        let at = found.iter().position(|r| r.node.id == answer);
        outcome.first += usize::from(at == Some(0));
        outcome.in_three += usize::from(at.is_some_and(|at| at < 3));
        if let Some(old) = &q.replaced {
            let old = found.iter().position(|r| r.node.id == ids[old]);
            outcome.newer_first += usize::from(match (at, old) {
                (Some(new), Some(old)) => new < old,
                (Some(_), None) => true,
                _ => false,
            });
        }
    }
    outcome.ms = started.elapsed().as_secs_f64() * 1000.0 / questions.len() as f64;
    for q in questions {
        let query = RecallQuery {
            text: Some(q.text.clone()),
            embedding: Some(q.embedding.clone()),
            mode: Some(way.mode),
            ranking: way.ranking.then(Ranking::default),
            labels: if way.decisions_only {
                vec!["Decision".into()]
            } else {
                Vec::new()
            },
            ..RecallQuery::default()
        };
        let context = context_for(&read, NS, &query, &ContextOptions::with_budget(600)).unwrap();
        let answer = ids[&q.answer[0]];
        outcome.in_context += usize::from(context.items.iter().any(|i| i.node == answer));
    }
    outcome
}

fn main() {
    let copies = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1usize);
    let started = Instant::now();
    let (db, ids, questions, conversations) = build(copies);
    let replaced = questions.iter().filter(|q| q.replaced.is_some()).count();
    println!(
        "{conversations} conversations and {} decisions loaded in {:.1} s; {} questions ({replaced} about a replaced decision)\n",
        ids.len() - conversations,
        started.elapsed().as_secs_f64(),
        questions.len()
    );
    println!(
        "| Way | First | In 3 | In the context (600 tokens) | New ahead of replaced | ms a recall |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    let n = questions.len() as f64;
    let row = |name: &str, o: Outcome| {
        println!(
            "| {name} | {:.0}% | {:.0}% | {:.0}% | {:.0}% | {:.2} |",
            100.0 * o.first as f64 / n,
            100.0 * o.in_three as f64 / n,
            100.0 * o.in_context as f64 / n,
            100.0 * o.newer_first as f64 / replaced.max(1) as f64,
            o.ms
        );
    };
    for way in &WAYS {
        row(way.name, measure(&db, &ids, &questions, way));
    }
    // Decisions remembered as important (0.8), as an agent is told to.
    mark_decisions_important(&db, &ids);
    row(
        "hybrid + graph, decisions important",
        measure(&db, &ids, &questions, &WAYS[2]),
    );
}
