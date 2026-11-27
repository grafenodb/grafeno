//! What a write costs, by part (docs/roadmap/STORAGE_AND_INDEXING_OPTIMIZATION.md,
//! phase 3): the same writes with and without each kind of index, a
//! thousand to a transaction on a file with eventual durability, so that
//! the time is the engine's work and not the disk's.
//!
//! `cargo run --release -p grafeno --example write_cost` (`CSV=path`
//! writes the rows)

use std::io::Write as _;
use std::time::Instant;

use grafeno::{Durability, Grafeno, IndexDef, NewNode, NodeId, Props};
use serde_json::json;

const NODES: usize = 20_000;
const PER_TX: usize = 1_000;

fn text(i: usize) -> String {
    let words = [
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
    ];
    (0..40)
        .map(|k| words[(i * 7 + k * 3) % words.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

fn fresh(name: &str) -> (Grafeno, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "grafeno-write-cost-{}-{name}.redb",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let db = Grafeno::open(&path).unwrap();
    db.set_durability(Durability::Eventual);
    (db, path)
}

/// µs per write of `f(i)` over `NODES` writes, `PER_TX` a transaction.
fn time(
    db: &Grafeno,
    mut f: impl FnMut(&mut grafeno::WriteTx, usize) -> grafeno::Result<()>,
) -> f64 {
    let started = Instant::now();
    for chunk in 0..NODES / PER_TX {
        db.write(|tx| {
            for i in chunk * PER_TX..(chunk + 1) * PER_TX {
                f(tx, i)?;
            }
            Ok(())
        })
        .unwrap();
    }
    started.elapsed().as_secs_f64() * 1e6 / NODES as f64
}

fn main() {
    let setups: [(&str, &dyn Fn(&Grafeno)); 5] = [
        ("no index", &|_| {}),
        ("a property index", &|db| {
            db.write(|tx| tx.create_index("w", IndexDef::new("Doc", "code")))
                .unwrap();
        }),
        ("five property indexes", &|db| {
            db.write(|tx| {
                for key in ["code", "a", "b", "c", "d"] {
                    tx.create_index("w", IndexDef::new("Doc", key))?;
                }
                Ok(())
            })
            .unwrap();
        }),
        ("a full-text index", &|db| {
            grafeno_free_fulltext(db);
        }),
        ("full-text and a property index", &|db| {
            grafeno_free_fulltext(db);
            db.write(|tx| tx.create_index("w", IndexDef::new("Doc", "code")))
                .unwrap();
        }),
    ];
    let mut csv = String::from("indexes,operation,us_per_write\n");
    println!(
        "| indexes | create a node | set an unindexed property | set an indexed property | set the embedding |"
    );
    println!("| --- | ---: | ---: | ---: | ---: |");
    for (name, setup) in setups {
        let (db, path) = fresh(&name.replace(' ', "-"));
        setup(&db);
        let mut ids: Vec<NodeId> = Vec::with_capacity(NODES);
        let create = time(&db, |tx, i| {
            ids.push(
                tx.create_node(
                    "w",
                    NewNode::new("Doc")
                        .prop("code", format!("D{i}"))
                        .prop("a", i as i64)
                        .prop("b", (i % 7) as i64)
                        .prop("c", i % 2 == 0)
                        .prop("d", format!("v{}", i % 100))
                        .prop("body", text(i)),
                )?,
            );
            Ok(())
        });
        let unindexed = time(&db, |tx, i| {
            let mut props = Props::new();
            props.insert("seen".into(), json!(i));
            tx.set_node_props("w", ids[i], props)
        });
        let indexed = time(&db, |tx, i| {
            let mut props = Props::new();
            props.insert("code".into(), json!(format!("E{i}")));
            tx.set_node_props("w", ids[i], props)
        });
        let embedding = time(&db, |tx, i| {
            let v: Vec<f32> = (0..32).map(|d| ((i * 31 + d) as f32 * 0.1).sin()).collect();
            tx.set_embedding("w", ids[i], &v)
        });
        println!("| {name} | {create:.1} | {unindexed:.1} | {indexed:.1} | {embedding:.1} |");
        std::io::stdout().flush().unwrap();
        for (op, us) in [
            ("create a node", create),
            ("set an unindexed property", unindexed),
            ("set an indexed property", indexed),
            ("set the embedding", embedding),
        ] {
            csv.push_str(&format!("\"{name}\",\"{op}\",{us:.2}\n"));
        }
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
    // Building indexes over existing nodes: time, and what the index table
    // takes and leaves fragmented.
    println!(
        "
| index built over {NODES} nodes | seconds | entries | stored MiB | fragmented MiB |"
    );
    println!("| --- | ---: | ---: | ---: | ---: |");
    for (name, fulltext) in [("a property index", false), ("a full-text index", true)] {
        let (db, path) = fresh(&format!("build-{fulltext}"));
        time(&db, |tx, i| {
            tx.create_node(
                "w",
                NewNode::new("Doc")
                    .prop("code", format!("D{}", (i * 7919) % NODES))
                    .prop("body", text(i)),
            )?;
            Ok(())
        });
        let started = Instant::now();
        if fulltext {
            grafeno_free_fulltext(&db);
        } else {
            db.write(|tx| tx.create_index("w", IndexDef::new("Doc", "code")))
                .unwrap();
        }
        let seconds = started.elapsed().as_secs_f64();
        let usage = db.storage_usage().unwrap();
        let table = usage
            .tables
            .iter()
            .find(|t| t.name == "index_entries")
            .unwrap();
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        println!(
            "| {name} | {seconds:.2} | {} | {:.1} | {:.1} |",
            table.entries,
            mib(table.stored_bytes),
            mib(table.fragmented_bytes)
        );
        csv.push_str(&format!(
            "\"build {name}\",\"seconds\",{seconds:.3}
\"build {name}\",\"fragmented MiB\",{:.2}
",
            mib(table.fragmented_bytes)
        ));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(file) = std::env::var("CSV") {
        std::fs::write(file, csv).unwrap();
    }
}

/// A full-text index on the body, as `CREATE FULLTEXT INDEX` makes it.
fn grafeno_free_fulltext(db: &Grafeno) {
    let def: IndexDef = serde_json::from_value(json!({
        "name": "doc_body",
        "label": "Doc",
        "key": "body",
        "fulltext": {"analyzer": "spanish"}
    }))
    .unwrap();
    db.write(|tx| tx.create_index("w", def.clone())).unwrap();
}
