//! Full-text index storage (docs/roadmap/STORAGE_AND_INDEXING_OPTIMIZATION.md,
//! phase 9): the same documents and full-text index in the entries format
//! (one key per term, field and node) and in the postings format (one entry
//! per term and node, fields in a varint value): the index's size, its
//! build, what a write costs, how long searches take, and that both answer
//! alike.
//!
//! `cargo run --release -p grafeno --example fulltext_storage` (`DOCS=n`,
//! `CSV=path`)

use std::time::Instant;

use grafeno::{FulltextFormat, Grafeno, IndexDef, NewNode, NodeId, OpenOptions, Props};
use serde_json::json;

fn count() -> usize {
    std::env::var("DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000)
}

/// A vocabulary of made-up words, frequent ones first.
fn vocabulary() -> Vec<String> {
    let syllables = [
        "ma", "re", "to", "si", "la", "co", "nu", "pe", "da", "vi", "gra", "fo", "ten", "cia",
        "mor", "lu", "ca", "tri", "so", "ne",
    ];
    let mut words = Vec::new();
    for a in syllables {
        for b in syllables {
            for c in ["", "s", "do", "ria", "les"] {
                words.push(format!("{a}{b}{c}"));
            }
        }
    }
    words
}

/// Word `k` of document `i`: skewed towards the first words, as text is.
fn word(words: &[String], i: usize, k: usize) -> &str {
    let x = (i.wrapping_mul(2_654_435_761) ^ k.wrapping_mul(40_503)) % 1_000_003;
    let skewed = (x * x / 1_000_003) % words.len();
    &words[skewed]
}

fn doc(words: &[String], i: usize) -> NewNode {
    let title: Vec<&str> = (0..5).map(|k| word(words, i, k + 1_000)).collect();
    let body: Vec<&str> = (0..(40 + i % 60)).map(|k| word(words, i, k)).collect();
    NewNode::new("Doc")
        .prop("title", title.join(" "))
        .prop("body", body.join(" "))
}

fn index() -> IndexDef {
    let mut def = IndexDef::composite("Doc", &["title", "body"]);
    def.name = "words".into();
    def.fulltext = Some(grafeno::Fulltext {
        analyzer: grafeno::Analyzer::Spanish,
        weights: vec![3.0, 1.0],
    });
    def
}

fn p50(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

struct Row {
    format: FulltextFormat,
    entries: u64,
    stored_mib: f64,
    fragmented_mib: f64,
    build_s: f64,
    write_us: f64,
    search_ms: f64,
    phrase_ms: f64,
    fuzzy_ms: f64,
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
    let n = count();
    let words = vocabulary();
    let queries: Vec<String> = (0..200)
        .map(|q| format!("{} {}", word(&words, q, 3), word(&words, q, 9)))
        .collect();
    let phrases: Vec<String> = (0..100)
        .map(|q| {
            format!(
                "\"{} {}\"",
                word(&words, q * 31, 4),
                word(&words, q * 31, 5)
            )
        })
        .collect();
    let fuzzy: Vec<String> = (0..100)
        .map(|q| format!("{}~1", word(&words, q, 7)))
        .collect();
    let mut rows = Vec::new();
    let mut answers = Vec::new();
    let ids: Vec<NodeId> = (0..n).map(|_| NodeId::new()).collect();
    for format in [FulltextFormat::Entries, FulltextFormat::Postings] {
        let path = std::env::temp_dir().join(format!(
            "grafeno-fulltext-storage-{}-{}.redb",
            std::process::id(),
            format.name()
        ));
        let _ = std::fs::remove_file(&path);
        let db = Grafeno::open_with(
            &path,
            &OpenOptions {
                fulltext: format,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        for chunk in (0..n).collect::<Vec<_>>().chunks(10_000) {
            db.write(|tx| {
                for &i in chunk {
                    tx.upsert_node("docs", ids[i], doc(&words, i))?;
                }
                Ok(())
            })
            .unwrap();
        }
        let started = Instant::now();
        let entries = db.write(|tx| tx.create_index("docs", index())).unwrap();
        let build_s = started.elapsed().as_secs_f64();
        // Writes: 2,000 bodies changed, 100 to a transaction.
        let started = Instant::now();
        for chunk in (0..2_000).collect::<Vec<usize>>().chunks(100) {
            db.write(|tx| {
                for &k in chunk {
                    let i = (k * 7_919) % n;
                    let mut props = Props::new();
                    props.insert("body".into(), json!(doc(&words, i + n).props["body"]));
                    tx.set_node_props("docs", ids[i], props)?;
                }
                Ok(())
            })
            .unwrap();
        }
        let write_us = started.elapsed().as_secs_f64() * 1e6 / 2_000.0;
        let usage = db.storage_usage().unwrap();
        let table = if format == FulltextFormat::Postings {
            "fulltext_postings"
        } else {
            "index_entries"
        };
        let found = usage.tables.iter().find(|t| t.name == table).unwrap();
        let read = db.begin_read().unwrap();
        let time = |list: &[String], out: &mut Vec<Vec<(NodeId, u32)>>| {
            let mut samples = Vec::new();
            for query in list {
                let started = Instant::now();
                let hits = read
                    .fulltext_search("docs", "words", query, 20, false)
                    .unwrap();
                samples.push(started.elapsed().as_secs_f64() * 1e3);
                out.push(hits.iter().map(|h| (h.node, h.score.to_bits())).collect());
            }
            p50(samples)
        };
        let mut got = Vec::new();
        // Once to warm the cache, then measured.
        time(&queries, &mut Vec::new());
        let search_ms = time(&queries, &mut got);
        let phrase_ms = time(&phrases, &mut got);
        let fuzzy_ms = time(&fuzzy, &mut got);
        assert!(read.check("docs").unwrap().is_ok());
        answers.push(got);
        rows.push(Row {
            format,
            entries,
            stored_mib: mib(found.stored_bytes),
            fragmented_mib: mib(found.fragmented_bytes),
            build_s,
            write_us,
            search_ms,
            phrase_ms,
            fuzzy_ms,
        });
        drop(read);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
    assert_eq!(answers[0], answers[1], "both formats answer alike");
    println!(
        "{:<9} {:>10} {:>11} {:>11} {:>8} {:>9} {:>10} {:>10} {:>9}",
        "format",
        "entries",
        "stored MiB",
        "fragm. MiB",
        "build s",
        "write µs",
        "words ms",
        "phrase ms",
        "fuzzy ms"
    );
    let mut csv = String::from(
        "format,documents,entries,stored_mib,fragmented_mib,build_s,write_us,words_p50_ms,phrase_p50_ms,fuzzy_p50_ms\n",
    );
    for row in &rows {
        println!(
            "{:<9} {:>10} {:>11.1} {:>11.1} {:>8.2} {:>9.1} {:>10.3} {:>10.3} {:>9.3}",
            row.format.name(),
            row.entries,
            row.stored_mib,
            row.fragmented_mib,
            row.build_s,
            row.write_us,
            row.search_ms,
            row.phrase_ms,
            row.fuzzy_ms
        );
        csv.push_str(&format!(
            "{},{n},{},{:.2},{:.2},{:.3},{:.2},{:.4},{:.4},{:.4}\n",
            row.format.name(),
            row.entries,
            row.stored_mib,
            row.fragmented_mib,
            row.build_s,
            row.write_us,
            row.search_ms,
            row.phrase_ms,
            row.fuzzy_ms
        ));
    }
    println!("same nodes, order and scores in both: yes");
    if let Ok(path) = std::env::var("CSV") {
        std::fs::write(path, csv).unwrap();
    }
}
