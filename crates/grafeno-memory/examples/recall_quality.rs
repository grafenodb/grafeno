//! Retrieval quality on questions an agent asks of its memory: words (the
//! question shares the answer's words), paraphrase, the other language
//! (Spanish and English), and multi-hop (the answer is one relation away
//! from what the question names). Compares BM25 alone, BM25 with Spanish
//! stemming, the vector index alone (HNSW), the two fused, and the fusion
//! with graph expansion.
//!
//! `cargo run --release -p grafeno-memory --example recall_quality`
//!
//! The memories, questions and embeddings are fixtures (`data/recall`,
//! made by `tools/fixtures/make-recall-fixtures.mjs` with a small
//! multilingual model): the numbers are the same on every run.

#[path = "common/recall_fixture.rs"]
mod fixture;

use std::collections::HashMap;
use std::time::Instant;

use grafeno::{Grafeno, Language, NodeId, SearchMode};
use grafeno_memory::{Expansion, RecallQuery, recall};

/// One way of retrieving.
pub struct Way {
    pub name: &'static str,
    pub ns: &'static str,
    pub mode: SearchMode,
    pub expand: bool,
}

pub const WAYS: [Way; 6] = [
    Way {
        name: "BM25",
        ns: "plain",
        mode: SearchMode::Text,
        expand: false,
    },
    Way {
        name: "BM25, stemmed (es)",
        ns: "es",
        mode: SearchMode::Text,
        expand: false,
    },
    Way {
        name: "HNSW",
        ns: "plain",
        mode: SearchMode::Vector,
        expand: false,
    },
    Way {
        name: "hybrid",
        ns: "plain",
        mode: SearchMode::Hybrid,
        expand: false,
    },
    Way {
        name: "hybrid + graph",
        ns: "plain",
        mode: SearchMode::Hybrid,
        expand: true,
    },
    Way {
        name: "hybrid + graph, stemmed",
        ns: "es",
        mode: SearchMode::Hybrid,
        expand: true,
    },
];

/// Where the first answer came, per question (`None`: not in the first 10).
pub fn ranks(
    db: &Grafeno,
    way: &Way,
    ids: &HashMap<String, NodeId>,
    questions: &[fixture::Question],
) -> (Vec<Option<usize>>, f64) {
    let read = db.begin_read().unwrap();
    let started = Instant::now();
    let ranks = questions
        .iter()
        .map(|q| {
            let query = RecallQuery {
                text: Some(q.text.clone()),
                embedding: Some(q.embedding.clone()),
                limit: 10,
                mode: Some(way.mode),
                expand: way.expand.then(|| Expansion {
                    depth: 1,
                    ..Expansion::default()
                }),
                ranking: None,
                ..RecallQuery::default()
            };
            let answers: Vec<NodeId> = q.answer.iter().map(|k| ids[k]).collect();
            recall(&read, way.ns, &query)
                .unwrap()
                .iter()
                .position(|r| answers.contains(&r.node.id))
        })
        .collect();
    let per_query = started.elapsed().as_secs_f64() * 1000.0 / questions.len() as f64;
    (ranks, per_query)
}

/// Both namespaces, the plain one and the one in Spanish, with the corpus.
pub fn build() -> (Grafeno, HashMap<String, NodeId>, Vec<fixture::Question>) {
    let (memories, questions) = fixture::corpus();
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_namespace("es", None)?;
        tx.set_namespace_language("es", Some(Language::Spanish))?;
        Ok(())
    })
    .unwrap();
    let ids = fixture::load(&db, "plain", &memories);
    let es = fixture::load(&db, "es", &memories);
    // The same keys in both: answers are found by key, so map the Spanish
    // namespace's nodes under the same keys with a prefix.
    let mut all = ids;
    for (key, id) in es {
        all.insert(format!("es:{key}"), id);
    }
    (db, all, questions)
}

/// The keys of `way`'s namespace.
pub fn keys_for(way: &Way, all: &HashMap<String, NodeId>) -> HashMap<String, NodeId> {
    all.iter()
        .filter_map(|(k, id)| match (way.ns, k.strip_prefix("es:")) {
            ("es", Some(key)) => Some((key.to_owned(), *id)),
            ("plain", None) => Some((k.clone(), *id)),
            _ => None,
        })
        .collect()
}

fn share(ranks: &[Option<usize>], within: usize) -> f64 {
    let hits = ranks
        .iter()
        .filter(|r| r.is_some_and(|r| r < within))
        .count();
    100.0 * hits as f64 / ranks.len().max(1) as f64
}

fn main() {
    let (db, all, questions) = build();
    let kinds = ["words", "paraphrase", "crosslingual", "multihop"];
    println!(
        "{} memories, {} questions\n",
        all.len() / 2,
        questions.len()
    );
    println!("| Way | Questions | First | In 3 | In 10 | ms a question |");
    println!("| --- | --- | ---: | ---: | ---: | ---: |");
    for way in &WAYS {
        let ids = keys_for(way, &all);
        let (ranks, ms) = ranks(&db, way, &ids, &questions);
        for kind in std::iter::once("all").chain(kinds) {
            let selected: Vec<Option<usize>> = questions
                .iter()
                .zip(&ranks)
                .filter(|(q, _)| kind == "all" || q.kind == kind)
                .map(|(_, r)| *r)
                .collect();
            println!(
                "| {} | {kind} ({}) | {:.0}% | {:.0}% | {:.0}% | {} |",
                way.name,
                selected.len(),
                share(&selected, 1),
                share(&selected, 3),
                share(&selected, 10),
                if kind == "all" {
                    format!("{ms:.2}")
                } else {
                    String::new()
                },
            );
        }
    }
}
