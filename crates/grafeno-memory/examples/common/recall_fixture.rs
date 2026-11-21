//! The retrieval fixtures of `data/recall` (made by
//! `tools/fixtures/make-recall-fixtures.mjs`): memories and questions with
//! their embeddings, loaded into a namespace. Shared by
//! `examples/recall_quality.rs`, `examples/long_term.rs` and their tests.

#![allow(dead_code)]

use std::collections::HashMap;

use grafeno::{Grafeno, NewEdge, NewNode, NodeId};
use serde_json::Value;

/// A memory to store.
pub struct Memory {
    pub key: String,
    pub label: String,
    pub text: String,
    /// `(relation, other key)`.
    pub links: Vec<(String, String)>,
    pub embedding: Vec<f32>,
    /// When it was written (long term), as ISO 8601.
    pub at: Option<String>,
}

/// A question, and the keys of the memories that answer it.
pub struct Question {
    pub text: String,
    pub kind: String,
    pub answer: Vec<String>,
    /// A memory the answer replaced (long term).
    pub replaced: Option<String>,
    pub embedding: Vec<f32>,
}

fn read(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("recall")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// Base64 (standard alphabet, padded) to bytes.
fn base64(text: &str) -> Vec<u8> {
    let value = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => 0,
        }
    };
    let bytes = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, c)| n | (value(*c) << (18 - 6 * i)));
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

fn vectors(value: &Value) -> Vec<Vec<f32>> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            let scale = v["s"].as_f64().unwrap() as f32;
            base64(v["e"].as_str().unwrap())
                .into_iter()
                .map(|b| f32::from(b as i8) * scale)
                .collect()
        })
        .collect()
}

fn memories(items: &Value, embeddings: Vec<Vec<f32>>, label: Option<&str>) -> Vec<Memory> {
    items
        .as_array()
        .unwrap()
        .iter()
        .zip(embeddings)
        .map(|(m, embedding)| {
            let mut links: Vec<(String, String)> = m["links"]
                .as_array()
                .map(|links| {
                    links
                        .iter()
                        .map(|l| {
                            (
                                l[0].as_str().unwrap().to_owned(),
                                l[1].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Some(old) = m["supersedes"].as_str() {
                links.push(("supersedes".into(), old.to_owned()));
            }
            Memory {
                key: m["key"].as_str().unwrap().to_owned(),
                label: m["label"].as_str().or(label).unwrap_or("Memory").to_owned(),
                text: m["text"].as_str().unwrap().to_owned(),
                links,
                embedding,
                at: m["at"].as_str().map(str::to_owned),
            }
        })
        .collect()
}

fn questions(items: &Value, embeddings: Vec<Vec<f32>>) -> Vec<Question> {
    items
        .as_array()
        .unwrap()
        .iter()
        .zip(embeddings)
        .map(|(q, embedding)| Question {
            text: q["q"].as_str().unwrap().to_owned(),
            kind: q["kind"].as_str().unwrap().to_owned(),
            answer: q["answer"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_owned())
                .collect(),
            replaced: q["replaced"].as_str().map(str::to_owned),
            embedding,
        })
        .collect()
}

/// Memories of six projects and questions asked of them.
pub fn corpus() -> (Vec<Memory>, Vec<Question>) {
    let data = read("corpus.json");
    let v = read("corpus.vectors.json");
    (
        memories(&data["memories"], vectors(&v["memories"]), None),
        questions(&data["questions"], vectors(&v["questions"])),
    )
}

/// Thousands of conversations and the decisions among them, with questions
/// about the decisions.
pub fn long_term() -> (Vec<Memory>, Vec<Memory>, Vec<Question>) {
    let data = read("longterm.json");
    let v = read("longterm.vectors.json");
    (
        memories(
            &data["conversations"],
            vectors(&v["conversations"]),
            Some("Episode"),
        ),
        memories(
            &data["decisions"],
            vectors(&v["decisions"]),
            Some("Decision"),
        ),
        questions(&data["questions"], vectors(&v["questions"])),
    )
}

/// Stores `memories` in `ns` (in batches, then their relations); returns
/// each key's node.
pub fn load(db: &Grafeno, ns: &str, memories: &[Memory]) -> HashMap<String, NodeId> {
    let mut ids = HashMap::new();
    for batch in memories.chunks(500) {
        let made = db
            .write(|tx| {
                batch
                    .iter()
                    .map(|m| {
                        tx.create_node(
                            ns,
                            NewNode::new(m.label.as_str())
                                .prop("key", m.key.as_str())
                                .text(m.text.as_str())
                                .embedding(m.embedding.clone()),
                        )
                    })
                    .collect::<grafeno::Result<Vec<NodeId>>>()
            })
            .unwrap();
        for (m, id) in batch.iter().zip(made) {
            ids.insert(m.key.clone(), id);
        }
    }
    // Each memory as old as it is: its last access is when it was written.
    db.write(|tx| {
        for m in memories.iter().filter(|m| m.at.is_some()) {
            let at = grafeno::Timestamp::parse(m.at.as_deref().unwrap())?;
            let state = grafeno::MemoryState {
                importance: grafeno::DEFAULT_IMPORTANCE,
                accesses: 0,
                last_access: at,
            };
            tx.restore_memory(ns, ids[&m.key], state)?;
        }
        for m in memories {
            for (relation, other) in &m.links {
                tx.create_edge(ns, NewEdge::new(ids[&m.key], relation.as_str(), ids[other]))?;
            }
        }
        Ok(())
    })
    .unwrap();
    ids
}
