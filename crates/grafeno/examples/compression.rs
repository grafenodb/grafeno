//! How a database's stored values compress (docs/roadmap/
//! STORAGE_AND_INDEXING_OPTIMIZATION.md, phase 5): for the node, edge and
//! change log values of a file, their size and the time to compress and
//! decompress each one with Zstandard at levels 1 and 3, without and with
//! a dictionary trained on 2% of them. Values already in an envelope are
//! measured decoded.
//!
//! ```text
//! cargo run --release -p grafeno --example compression -- <file.redb>
//! ```
//!
//! The file must not be open elsewhere; it is only read.

use std::time::Instant;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

const NODES: TableDefinition<(&str, u128), &[u8]> = TableDefinition::new("nodes");
const EDGES: TableDefinition<(&str, u128), &[u8]> = TableDefinition::new("edges");
const CHANGES: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("changes");

/// A value as stored, out of its envelope if it has one (uncompressed
/// envelopes only: run this on a file written with compression off).
fn decoded(bytes: &[u8]) -> Vec<u8> {
    if bytes.first() == Some(&0x1E) && bytes.get(2) == Some(&0) {
        let mut at = 3;
        while bytes[at] & 0x80 != 0 {
            at += 1;
        }
        return bytes[at + 1..].to_vec();
    }
    bytes.to_vec()
}

fn values(db: &redb::Database, which: &str) -> Vec<Vec<u8>> {
    let read = db.begin_read().unwrap();
    let mut out = Vec::new();
    match which {
        "nodes" | "edges" => {
            let table = read
                .open_table(if which == "nodes" { NODES } else { EDGES })
                .unwrap();
            for item in table.iter().unwrap() {
                out.push(decoded(item.unwrap().1.value()));
            }
        }
        _ => {
            let table = read.open_table(CHANGES).unwrap();
            for item in table.iter().unwrap() {
                out.push(decoded(item.unwrap().1.value()));
            }
        }
    }
    out
}

fn run(
    name: &str,
    values: &[Vec<u8>],
    compress: &dyn Fn(&[u8]) -> Vec<u8>,
    decompress: &dyn Fn(&[u8], usize) -> Vec<u8>,
) {
    let raw: usize = values.iter().map(Vec::len).sum();
    let started = Instant::now();
    let packed: Vec<Vec<u8>> = values.iter().map(|v| compress(v)).collect();
    let compress_s = started.elapsed().as_secs_f64();
    // Kept only when it saves 12% (the engine's rule), else stored as is.
    let kept: usize = values
        .iter()
        .zip(&packed)
        .map(|(v, p)| {
            if (p.len() + 8) * 100 <= v.len() * 88 {
                p.len() + 8
            } else {
                v.len()
            }
        })
        .sum();
    let started = Instant::now();
    for (v, p) in values.iter().zip(&packed) {
        assert_eq!(&decompress(p, v.len()), v);
    }
    let decompress_s = started.elapsed().as_secs_f64();
    let n = values.len().max(1) as f64;
    println!(
        "| {name} | {:.1} | {:.1} | {:.1}% | {:.2} | {:.2} |",
        raw as f64 / 1048576.0,
        kept as f64 / 1048576.0,
        100.0 * kept as f64 / raw.max(1) as f64,
        compress_s * 1e6 / n,
        decompress_s * 1e6 / n
    );
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("give a database file: compression <file.redb>");
    let db = redb::Database::open(&path).unwrap();
    for which in ["nodes", "edges", "changes"] {
        let values = values(&db, which);
        if values.is_empty() {
            continue;
        }
        let average = values.iter().map(Vec::len).sum::<usize>() / values.len();
        println!(
            "\n{which}: {} values, {average} bytes on average\n",
            values.len()
        );
        println!("| codec | MiB | stored MiB | of the original | compress µs | decompress µs |");
        println!("| --- | ---: | ---: | ---: | ---: | ---: |");
        for level in [1, 3] {
            run(
                &format!("Zstandard {level}"),
                &values,
                &|v| zstd::bulk::compress(v, level).unwrap(),
                &|p, n| zstd::bulk::decompress(p, n).unwrap(),
            );
        }
        let sample: Vec<&[u8]> = values.iter().step_by(50).map(Vec::as_slice).collect();
        let started = Instant::now();
        let dictionary = zstd::dict::from_samples(&sample, 64 * 1024).unwrap();
        let trained_s = started.elapsed().as_secs_f64();
        for level in [1, 3] {
            let encoder = zstd::dict::EncoderDictionary::copy(&dictionary, level);
            let decoder = zstd::dict::DecoderDictionary::copy(&dictionary);
            run(
                &format!("Zstandard {level}, dictionary"),
                &values,
                &|v| {
                    zstd::bulk::Compressor::with_prepared_dictionary(&encoder)
                        .unwrap()
                        .compress(v)
                        .unwrap()
                },
                &|p, n| {
                    zstd::bulk::Decompressor::with_prepared_dictionary(&decoder)
                        .unwrap()
                        .decompress(p, n)
                        .unwrap()
                },
            );
        }
        println!(
            "\nThe dictionary: {} KiB, trained in {trained_s:.2} s on {} values.",
            dictionary.len() / 1024,
            sample.len()
        );
    }
}
