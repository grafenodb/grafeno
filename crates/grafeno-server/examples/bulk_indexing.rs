//! Automatic indexing at bulk speed (docs/roadmap/STORAGE_AND_INDEXING_OPTIMIZATION.md,
//! phase 7): a bulk load into an automatic namespace against the same load
//! into a manual one followed by the `CREATE INDEX`es the policy would have
//! made, and against record-by-record automatic indexing (what a load did
//! in phase 6). For an import and for `CALL { … } IN TRANSACTIONS`, on
//! two shapes of data: rows of scalar properties and documents with a long
//! text (a full-text index).
//!
//! `cargo run --release -p grafeno-server --example bulk_indexing`
//! (`ROWS=n`, `DOCS=n`, `CSV=path`)

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use grafeno::{Grafeno, ImportOptions, IndexingMode, NewNode, NodeId, OpenOptions};
use serde_json::{Value, json};

fn count(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn fresh(name: &str, indexing: IndexingMode) -> (Grafeno, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "grafeno-bulk-indexing-{}-{name}.redb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = Grafeno::open_with(
        &path,
        &OpenOptions {
            indexing,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    (db, path)
}

const WORDS: [&str; 12] = [
    "memoria",
    "grafo",
    "índice",
    "consulta",
    "agente",
    "registro",
    "texto",
    "vector",
    "búsqueda",
    "relación",
    "catálogo",
    "entidad",
];

/// The properties of record `i` of `shape`.
fn props(shape: &str, i: usize) -> Vec<(&'static str, Value)> {
    let countries = ["PE", "AR", "CL", "MX", "ES", "US", "BR", "CO"];
    match shape {
        "rows" => vec![
            ("number", json!(i)),
            ("country", json!(countries[i % countries.len()])),
            (
                "placed",
                json!(format!(
                    "2026-{:02}-{:02}T{:02}:00:00Z",
                    i % 12 + 1,
                    i % 28 + 1,
                    i % 24
                )),
            ),
            ("total", json!(((i * 7919) % 100_000) as f64 / 100.0)),
            ("paid", json!(i % 3 != 0)),
            ("customer", json!(format!("K{}", i % 20_000))),
        ],
        _ => vec![
            ("code", json!(format!("D{i}"))),
            ("year", json!(1990 + i % 35)),
            (
                "body",
                json!(
                    (0..60)
                        .map(|k| WORDS[(i * 7 + k * 5) % WORDS.len()])
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            ),
        ],
    }
}

fn label(shape: &str) -> &'static str {
    if shape == "rows" { "Order" } else { "Doc" }
}

/// An export of `n` records of `shape` in namespace `bulk`.
fn export(shape: &str, n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    writeln!(
        out,
        r#"{{"type":"header","format":"grafeno-export","version":2,"exported_at":0}}"#
    )
    .unwrap();
    for i in 0..n {
        let props: serde_json::Map<String, Value> = props(shape, i)
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        let line = json!({
            "type": "node", "ns": "bulk",
            "node": {"id": NodeId::new(), "labels": [label(shape)], "props": props,
                     "sources": [], "created_at": 0, "updated_at": 0}
        });
        writeln!(out, "{line}").unwrap();
    }
    out
}

/// The GQ that creates the same records in transactions of 20,000.
fn gq_load(shape: &str, n: usize) -> String {
    let body = match shape {
        "rows" => {
            "CREATE (:Order {number: i, country: ['PE','AR','CL','MX','ES','US','BR','CO'][i % 8], \
                   placed: '2026-01-01T00:00:00Z', total: toFloat(i % 100000) / 100.0, paid: i % 3 <> 0, \
                   customer: 'K' + toString(i % 20000)})"
        }
        _ => {
            "CREATE (:Doc {code: 'D' + toString(i), year: 1990 + i % 35, \
              body: reduce(s = '', k IN range(0, 59) | s + ['memoria','grafo','índice','consulta','agente','registro','texto','vector','búsqueda','relación','catálogo','entidad'][(i * 7 + k * 5) % 12] + ' ')})"
        }
    };
    format!(
        "UNWIND range(0, {}) AS i CALL {{ WITH i {body} }} IN TRANSACTIONS OF 20000 ROWS",
        n - 1
    )
}

/// The indexes an automatic namespace made, as `CREATE INDEX`es of the
/// same shape.
fn same_indexes(db: &Grafeno) -> Vec<(String, grafeno::IndexDef)> {
    db.begin_read()
        .unwrap()
        .index_defs("bulk")
        .unwrap()
        .into_iter()
        .map(|mut def| {
            def.origin = grafeno::IndexOrigin::Manual;
            def.name = def.name.trim_start_matches("auto_").to_owned();
            ("bulk".to_owned(), def)
        })
        .collect()
}

fn entries(db: &Grafeno) -> u64 {
    db.begin_read()
        .unwrap()
        .indexes("bulk")
        .unwrap()
        .iter()
        .map(|info| info.entries)
        .sum()
}

struct Row {
    shape: String,
    records: usize,
    path: &'static str,
    how: &'static str,
    load_s: f64,
    index_s: f64,
    indexes: usize,
    entries: u64,
}

fn main() {
    let mut rows: Vec<Row> = Vec::new();
    for (shape, n) in [
        ("rows", count("ROWS", 300_000)),
        ("docs", count("DOCS", 50_000)),
    ] {
        let data = export(shape, n);
        // Automatic, held back (phase 7): the reference set of indexes.
        let (auto, auto_path) = fresh("auto", IndexingMode::Automatic);
        let started = Instant::now();
        auto.import_with(data.as_slice(), &ImportOptions::bulk())
            .unwrap();
        let load_s = started.elapsed().as_secs_f64();
        let wanted = same_indexes(&auto);
        rows.push(Row {
            shape: shape.into(),
            records: n,
            path: "import",
            how: "automatic, after the load",
            load_s,
            index_s: 0.0,
            indexes: wanted.len(),
            entries: entries(&auto),
        });
        drop(auto);
        let _ = std::fs::remove_file(auto_path);

        // Manual, then CREATE INDEX.
        let (manual, manual_path) = fresh("manual", IndexingMode::Manual);
        let started = Instant::now();
        manual
            .import_with(data.as_slice(), &ImportOptions::bulk())
            .unwrap();
        let load_s = started.elapsed().as_secs_f64();
        let started = Instant::now();
        manual
            .write(|tx| {
                for (ns, def) in wanted.clone() {
                    tx.create_index(&ns, def)?;
                }
                Ok(())
            })
            .unwrap();
        rows.push(Row {
            shape: shape.into(),
            records: n,
            path: "import",
            how: "manual, then CREATE INDEX",
            load_s,
            index_s: started.elapsed().as_secs_f64(),
            indexes: wanted.len(),
            entries: entries(&manual),
        });
        drop(manual);
        let _ = std::fs::remove_file(manual_path);

        // Automatic, record by record (phase 6's loads): chunks of 20,000.
        let (each, each_path) = fresh("each", IndexingMode::Automatic);
        let started = Instant::now();
        for chunk in (0..n).collect::<Vec<_>>().chunks(20_000) {
            each.write(|tx| {
                for &i in chunk {
                    let mut node = NewNode::new(label(shape));
                    for (k, v) in props(shape, i) {
                        node = node.prop(k, v);
                    }
                    tx.create_node("bulk", node)?;
                }
                Ok(())
            })
            .unwrap();
        }
        rows.push(Row {
            shape: shape.into(),
            records: n,
            path: "writes",
            how: "automatic, record by record",
            load_s: started.elapsed().as_secs_f64(),
            index_s: 0.0,
            indexes: each.begin_read().unwrap().index_defs("bulk").unwrap().len(),
            entries: entries(&each),
        });
        drop(each);
        let _ = std::fs::remove_file(each_path);

        // CALL { CREATE } IN TRANSACTIONS.
        let query = gq_load(shape, n);
        for (how, mode) in [
            ("automatic, after the load", IndexingMode::Automatic),
            ("manual, then CREATE INDEX", IndexingMode::Manual),
        ] {
            let (db, path) = fresh("gq", mode);
            let started = Instant::now();
            grafeno_query::execute(&db, "bulk", &query, &grafeno_query::Params::new()).unwrap();
            let load_s = started.elapsed().as_secs_f64();
            let started = Instant::now();
            if mode == IndexingMode::Manual {
                db.write(|tx| {
                    for (ns, def) in wanted.clone() {
                        tx.create_index(&ns, def)?;
                    }
                    Ok(())
                })
                .unwrap();
            }
            rows.push(Row {
                shape: shape.into(),
                records: n,
                path: "in transactions",
                how,
                load_s,
                index_s: started.elapsed().as_secs_f64(),
                indexes: db.begin_read().unwrap().index_defs("bulk").unwrap().len(),
                entries: entries(&db),
            });
            drop(db);
            let _ = std::fs::remove_file(path);
        }
    }
    println!(
        "{:<6} {:>8} {:<16} {:<28} {:>8} {:>8} {:>8} {:>8} {:>10}",
        "shape", "records", "path", "how", "load s", "index s", "total s", "indexes", "entries"
    );
    let mut csv = String::from("shape,records,path,how,load_s,index_s,total_s,indexes,entries\n");
    for row in &rows {
        let total = row.load_s + row.index_s;
        println!(
            "{:<6} {:>8} {:<16} {:<28} {:>8.2} {:>8.2} {:>8.2} {:>8} {:>10}",
            row.shape,
            row.records,
            row.path,
            row.how,
            row.load_s,
            row.index_s,
            total,
            row.indexes,
            row.entries
        );
        csv.push_str(&format!(
            "{},{},{},\"{}\",{:.3},{:.3},{:.3},{},{}\n",
            row.shape,
            row.records,
            row.path,
            row.how,
            row.load_s,
            row.index_s,
            total,
            row.indexes,
            row.entries
        ));
    }
    if let Ok(path) = std::env::var("CSV") {
        std::fs::write(path, csv).unwrap();
    }
}
