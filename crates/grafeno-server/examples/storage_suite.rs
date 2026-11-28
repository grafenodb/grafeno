//! The storage and indexing baseline (docs/roadmap/STORAGE_AND_INDEXING_OPTIMIZATION.md,
//! phase 1): six datasets loaded into files the way an application would,
//! then for each what it takes on disk (tables, file, after `vacuum`), how
//! long loading took, and the latency of writes and of every kind of
//! read; plus the results of fixed queries, written as fixtures that later
//! phases compare against.
//!
//! ```text
//! cargo run --release -p grafeno-server --example storage_suite
//! ```
//!
//! `SCALE=0.1` shrinks every dataset (tests, quick runs); `ONLY=catalog,
//! documents,rows,graph,hyperedges,memory` picks datasets; `CSV=path`
//! writes the measurements; `FIXTURES=dir` writes each dataset's results
//! as `<dir>/<dataset>.json`, and `COMPARE=dir` compares them with those
//! files instead and fails on a difference. Ids are fresh in every run, so
//! fixtures hold stable properties (codes, numbers) and BM25 scores; vector
//! results are compared as recall against an exact search, since the HNSW
//! graph's levels are drawn at random.
//!
//! Each dataset is compressed once loaded (phase 5): dictionaries trained
//! and every value rewritten, as `grafeno storage apply --encoding
//! compressed` does, before its sizes and latencies are measured.
//! `COMPRESSION=off` writes and measures values as they are, as before.
//! `FULLTEXT=entries` keeps full-text indexes in the format before phase 9.
//! `INDEXING=automatic` makes every namespace automatic (phase 6), to
//! measure what automatic indexes cost loads, writes and space.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use grafeno::Grafeno;
use grafeno_query::{Limits, Params, Value as GqValue};
use serde_json::{Value, json};

fn compression() -> grafeno::Compression {
    match std::env::var("COMPRESSION").as_deref() {
        Ok("off") => grafeno::Compression::Off,
        _ => grafeno::Compression::Auto,
    }
}

/// `FULLTEXT=entries` builds full-text indexes as before phase 9.
fn fulltext() -> grafeno::FulltextFormat {
    match std::env::var("FULLTEXT").as_deref() {
        Ok("entries") => grafeno::FulltextFormat::Entries,
        _ => grafeno::FulltextFormat::Postings,
    }
}

/// `INDEXING=automatic` loads every dataset into automatic namespaces
/// (phase 6): their properties get indexes as they appear.
fn indexing() -> grafeno::IndexingMode {
    match std::env::var("INDEXING").as_deref() {
        Ok("automatic") => grafeno::IndexingMode::Automatic,
        _ => grafeno::IndexingMode::Manual,
    }
}

fn open_db(path: &Path) -> Grafeno {
    Grafeno::open_with(
        path,
        &grafeno::OpenOptions {
            compression: compression(),
            indexing: indexing(),
            fulltext: fulltext(),
            ..grafeno::OpenOptions::default()
        },
    )
    .unwrap()
}

/// With compression on: trains the dictionaries and rewrites what is
/// stored with them; returns the seconds it took.
fn compress_stored(db: &Grafeno) -> f64 {
    if compression() == grafeno::Compression::Off {
        return 0.0;
    }
    let started = Instant::now();
    let plan = db
        .plan_migration(&grafeno::MigrationOptions {
            encoding: Some(grafeno::Encoding::Compressed),
            ..grafeno::MigrationOptions::default()
        })
        .unwrap();
    db.start_migration(&plan).unwrap();
    let done = db
        .run_migration(
            &plan.id,
            &grafeno::RunControl {
                exclusive: true,
                ..grafeno::RunControl::default()
            },
        )
        .unwrap();
    assert_eq!(done.state, grafeno::MigrationState::Completed, "{done:?}");
    started.elapsed().as_secs_f64()
}

fn scale() -> f64 {
    std::env::var("SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0)
}

fn n(base: usize) -> usize {
    ((base as f64 * scale()) as usize).max(10)
}

fn gq(db: &Grafeno, ns: &str, query: &str) -> grafeno_query::QueryResult {
    grafeno_query::execute_with(db, ns, query, &Params::new(), &Limits::none())
        .unwrap_or_else(|e| panic!("{query}\n→ {e}"))
}

fn gq_with(db: &Grafeno, ns: &str, query: &str, params: &Params) -> grafeno_query::QueryResult {
    grafeno_query::execute_with(db, ns, query, params, &Limits::none())
        .unwrap_or_else(|e| panic!("{query}\n→ {e}"))
}

/// Latencies in ms of `runs` calls of `f(i)`: p50, p95, p99.
fn latency(runs: usize, mut f: impl FnMut(usize)) -> (f64, f64, f64) {
    let mut times: Vec<f64> = (0..runs)
        .map(|i| {
            let started = Instant::now();
            f(i);
            started.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    times.sort_by(f64::total_cmp);
    let at = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    (at(0.5), at(0.95), at(0.99))
}

/// A deterministic vector of `dims` values for item `i` (clustered: items
/// with the same `i % 200` are close).
fn vector(i: usize, dims: usize) -> Vec<f32> {
    let cluster = i % 200;
    (0..dims)
        .map(|d| {
            let base = ((cluster * 31 + d * 7) as f32 * 0.37).sin();
            let noise = ((i * 13 + d * 3) as f32 * 1.91).cos() * 0.15;
            base + noise
        })
        .collect()
}

/// One row of the measurements.
#[derive(Default)]
struct Row {
    dataset: String,
    records: usize,
    load_s: f64,
    /// Training dictionaries and rewriting the values with them.
    compress_s: f64,
    file_mib: f64,
    stored_mib: f64,
    fragmented_mib: f64,
    vacuumed_mib: f64,
    largest: String,
    measures: Vec<(String, (f64, f64, f64))>,
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Sizes of the file, then of a vacuumed copy.
fn sizes(db: &Grafeno, path: &Path, row: &mut Row) {
    row.compress_s = compress_stored(db);
    let usage = db.storage_usage().unwrap();
    row.file_mib = mib(usage.file_bytes.unwrap_or(0));
    row.stored_mib = mib(usage.stored_bytes);
    row.fragmented_mib = mib(usage.fragmented_bytes);
    row.largest = usage
        .tables
        .iter()
        .take(3)
        .map(|t| format!("{} {:.1}", t.name, mib(t.stored_bytes)))
        .collect::<Vec<_>>()
        .join("; ");
    let copy = path.with_extension("vacuum.redb");
    let _ = std::fs::remove_file(&copy);
    db.begin_read().unwrap().backup(&copy).unwrap();
    let done = Grafeno::vacuum(&copy).unwrap();
    row.vacuumed_mib = mib(done.after);
    let _ = std::fs::remove_file(&copy);
}

/// Single interactive writes (one transaction each, immediate durability).
fn writes(db: &Grafeno, ns: &str, label: &str, row: &mut Row) {
    let measured = latency(200, |i| {
        db.write(|tx| tx.create_node(ns, grafeno::NewNode::new(label).prop("written", i as i64)))
            .unwrap();
    });
    row.measures.push(("write a node".into(), measured));
}

fn rows_of(result: &grafeno_query::QueryResult) -> Value {
    result.to_json()["rows"].clone()
}

type Fixtures = BTreeMap<String, Value>;

fn catalog(path: &Path, fixtures: &mut Fixtures) -> Row {
    let db = open_db(path);
    let ns = "catalog";
    let items = n(100_000);
    let dims = 128;
    let mut row = Row {
        dataset: "catalog".into(),
        records: items,
        ..Row::default()
    };
    let started = Instant::now();
    gq(&db, ns, "CREATE INDEX FOR (e:Entity) ON (e.code)");
    gq(
        &db,
        ns,
        "CREATE FULLTEXT INDEX entity_text FOR (e:Entity) ON EACH [e.title, e.definition] OPTIONS {analyzer: 'spanish'}",
    );
    gq(
        &db,
        ns,
        &format!(
            "CREATE VECTOR INDEX entity_vectors FOR (e:Entity) ON (e.@embedding) OPTIONS {{dimension: {dims}}}"
        ),
    );
    let words = [
        "inflamación",
        "crónica",
        "tejido",
        "conectivo",
        "fiebre",
        "fatiga",
        "dolor",
        "articular",
        "renal",
        "cardíaco",
        "pulmonar",
        "hepático",
        "infección",
        "bacteriana",
        "viral",
        "congénito",
    ];
    for chunk in (0..items).collect::<Vec<_>>().chunks(5_000) {
        let rows: Vec<GqValue> = chunk
            .iter()
            .map(|&i| {
                let w = |k: usize| words[(i * 7 + k * 5) % words.len()];
                let mut map = BTreeMap::new();
                map.insert("code".into(), GqValue::Str(format!("C{i}")));
                map.insert("title".into(), GqValue::Str(format!("Trastorno {} {} del grupo {}", w(1), w(2), i % 997)));
                map.insert("definition".into(), GqValue::Str(format!(
                    "Afección {} {} caracterizada por {} {}, con {} y {} persistentes; categoría {} del capítulo {}.",
                    w(3), w(4), w(5), w(6), w(7), w(8), i % 4_000, i % 26
                )));
                map.insert("chapter".into(), GqValue::Int((i % 26) as i64));
                map.insert("embedding".into(), GqValue::List(vector(i, dims).into_iter().map(|v| GqValue::Float(v as f64)).collect()));
                GqValue::Map(map)
            })
            .collect();
        let mut params = Params::new();
        params.insert("rows".into(), GqValue::List(rows));
        gq_with(
            &db,
            ns,
            "UNWIND $rows AS r CREATE (e:Entity {code: r.code, title: r.title, definition: r.definition, chapter: r.chapter}) SET e.@embedding = r.embedding",
            &params,
        );
    }
    gq(
        &db,
        ns,
        "MATCH (e:Entity) WHERE e.chapter > 0 CALL { WITH e MATCH (p:Entity {code: 'C' + toString(e.chapter - 1)}) CREATE (e)-[:child_of]->(p) } IN TRANSACTIONS OF 5000 ROWS",
    );
    // A vector index on `@embedding` reads the node embeddings: nothing to build.
    row.load_s = started.elapsed().as_secs_f64();
    sizes(&db, path, &mut row);
    let read = || db.begin_read().unwrap();
    let mut params = Params::new();
    row.measures.push((
        "lookup by code (index)".into(),
        latency(2_000, |i| {
            params.insert("c".into(), GqValue::Str(format!("C{}", (i * 7919) % items)));
            grafeno_query::execute_read(
                &read(),
                ns,
                "MATCH (e:Entity {code: $c}) RETURN e.title",
                &params,
            )
            .unwrap();
        }),
    ));
    let queries = [
        "inflamación crónica",
        "dolor articular",
        "fiebre~1 renal",
        "\"tejido conectivo\"",
        "cardíaco pulmonar",
    ];
    row.measures.push((
        "full-text search, 10".into(),
        latency(500, |i| {
            params.insert("q".into(), GqValue::Str(queries[i % queries.len()].into()));
            grafeno_query::execute_read(&read(), ns, "CALL db.searchIndex('entity_text', $q, 10) YIELD node, score RETURN node.code, score", &params).unwrap();
        }),
    ));
    let probes: Vec<Vec<f32>> = (0..50).map(|k| vector(k * 37 + 5, dims)).collect();
    row.measures.push((
        "vector search, 10".into(),
        latency(500, |i| {
            params.insert("v".into(), GqValue::List(probes[i % probes.len()].iter().map(|v| GqValue::Float(*v as f64)).collect()));
            grafeno_query::execute_read(&read(), ns, "CALL db.searchVector($v, {limit: 10, index: 'entity_vectors'}) YIELD node RETURN node.code", &params).unwrap();
        }),
    ));
    row.measures.push((
        "two hops up the hierarchy".into(),
        latency(1_000, |i| {
            params.insert("c".into(), GqValue::Str(format!("C{}", (i * 7919) % items)));
            grafeno_query::execute_read(
                &read(),
                ns,
                "MATCH (e:Entity {code: $c})-[:child_of*1..2]->(p) RETURN count(p)",
                &params,
            )
            .unwrap();
        }),
    ));
    writes(&db, ns, "Entity", &mut row);
    // Fixtures: full-text results with scores, lookups, a hierarchy count,
    // and recall@10 of the vector index against an exact search.
    let mut fixture: BTreeMap<String, Value> = BTreeMap::new();
    for q in queries {
        let mut p = Params::new();
        p.insert("q".into(), GqValue::Str(q.into()));
        let found = grafeno_query::execute_read(
            &read(),
            ns,
            "CALL db.searchIndex('entity_text', $q, 10) YIELD node, score RETURN node.code, score",
            &p,
        )
        .unwrap();
        fixture.insert(format!("search {q}"), rows_of(&found));
    }
    fixture.insert(
        "chapters".into(),
        rows_of(&gq(
            &db,
            ns,
            "MATCH (e:Entity) RETURN e.chapter, count(*) ORDER BY e.chapter LIMIT 5",
        )),
    );
    fixture.insert(
        "children of C1".into(),
        rows_of(&gq(
            &db,
            ns,
            "MATCH (e:Entity)-[:child_of]->(:Entity {code: 'C1'}) RETURN count(e)",
        )),
    );
    let normalize = |v: &[f32]| {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect::<Vec<f32>>()
    };
    let all: Vec<Vec<f32>> = (0..items).map(|i| normalize(&vector(i, dims))).collect();
    let mut hits = 0;
    for probe in &probes {
        let q = normalize(probe);
        let mut exact: Vec<(f32, usize)> = all
            .iter()
            .enumerate()
            .map(|(i, v)| (v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>(), i))
            .collect();
        exact.sort_by(|a, b| b.0.total_cmp(&a.0));
        let truth: Vec<String> = exact
            .iter()
            .take(10)
            .map(|(_, i)| format!("C{i}"))
            .collect();
        let mut p = Params::new();
        p.insert(
            "v".into(),
            GqValue::List(probe.iter().map(|v| GqValue::Float(*v as f64)).collect()),
        );
        let found = grafeno_query::execute_read(&read(), ns, "CALL db.searchVector($v, {limit: 10, index: 'entity_vectors'}) YIELD node RETURN node.code", &p).unwrap();
        for r in rows_of(&found).as_array().unwrap() {
            if truth.iter().any(|t| r[0] == *t) {
                hits += 1;
            }
        }
    }
    let recall = hits as f64 / (probes.len() * 10) as f64;
    fixture.insert(
        "vector recall@10 (at least)".into(),
        json!((recall * 100.0).floor() / 100.0),
    );
    fixtures.insert("catalog".into(), json!(fixture));
    row
}

fn documents(path: &Path, fixtures: &mut Fixtures) -> Row {
    let db = open_db(path);
    let ns = "docs";
    let count = n(20_000);
    let mut row = Row {
        dataset: "documents".into(),
        records: count,
        ..Row::default()
    };
    let words: Vec<String> = (0..3_000).map(|k| format!("term{k}")).collect();
    let started = Instant::now();
    gq(&db, ns, "CREATE INDEX FOR (d:Document) ON (d.number)");
    gq(
        &db,
        ns,
        "CREATE FULLTEXT INDEX doc_text FOR (d:Document) ON EACH [d.title, d.body] OPTIONS {analyzer: 'english', weights: {title: 2}}",
    );
    for chunk in (0..count).collect::<Vec<_>>().chunks(1_000) {
        db.write(|tx| {
            for &i in chunk {
                // About 2,000 characters from a skewed vocabulary.
                let body: Vec<&str> = (0..250)
                    .map(|k| {
                        words[((i * 31 + k * k * 7) % 3_000).min((i + k * 13) % 3_000)].as_str()
                    })
                    .collect();
                let body = body.join(" ");
                tx.create_node(
                    ns,
                    grafeno::NewNode::new("Document")
                        .prop("number", i as i64)
                        .prop("title", format!("Report {} on {}", i, words[i % 3_000]))
                        .prop("body", body.clone())
                        .prop(
                            "created",
                            format!("2026-{:02}-{:02}", i % 12 + 1, i % 28 + 1),
                        )
                        .text(body),
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    row.load_s = started.elapsed().as_secs_f64();
    sizes(&db, path, &mut row);
    let read = || db.begin_read().unwrap();
    let mut params = Params::new();
    row.measures.push((
        "full-text search, 10".into(),
        latency(500, |i| {
            params.insert("q".into(), GqValue::Str(format!("{} {}", words[i % 50], words[(i * 7) % 300])));
            grafeno_query::execute_read(&read(), ns, "CALL db.searchIndex('doc_text', $q, 10) YIELD node, score RETURN node.number, score", &params).unwrap();
        }),
    ));
    row.measures.push((
        "node text search (BM25), 10".into(),
        latency(500, |i| {
            params.insert(
                "q".into(),
                GqValue::Str(format!("{} {}", words[i % 50], words[(i * 7) % 300])),
            );
            grafeno_query::execute_read(
                &read(),
                ns,
                "CALL db.search($q, 10) YIELD node, score RETURN node.number, score",
                &params,
            )
            .unwrap();
        }),
    ));
    row.measures.push((
        "read a document by number".into(),
        latency(2_000, |i| {
            params.insert("n".into(), GqValue::Int(((i * 7919) % count) as i64));
            grafeno_query::execute_read(
                &read(),
                ns,
                "MATCH (d:Document {number: $n}) RETURN d.body",
                &params,
            )
            .unwrap();
        }),
    ));
    writes(&db, ns, "Note", &mut row);
    let mut fixture: BTreeMap<String, Value> = BTreeMap::new();
    for q in ["term1 term2", "term10 term300", "\"term3 term4\"", "term7*"] {
        let mut p = Params::new();
        p.insert("q".into(), GqValue::Str(q.into()));
        let found = grafeno_query::execute_read(
            &read(),
            ns,
            "CALL db.searchIndex('doc_text', $q, 10) YIELD node, score RETURN node.number, score",
            &p,
        )
        .unwrap();
        fixture.insert(format!("search {q}"), rows_of(&found));
        let found = grafeno_query::execute_read(
            &read(),
            ns,
            "CALL db.search($q, 10) YIELD node, score RETURN node.number, score",
            &p,
        )
        .unwrap();
        fixture.insert(format!("text {q}"), rows_of(&found));
    }
    fixtures.insert("documents".into(), json!(fixture));
    row
}

fn rows_dataset(path: &Path, fixtures: &mut Fixtures) -> Row {
    let db = open_db(path);
    let ns = "rows";
    let count = n(300_000);
    let mut row = Row {
        dataset: "rows".into(),
        records: count,
        ..Row::default()
    };
    let countries = ["PE", "AR", "CL", "MX", "ES", "US", "BR", "CO"];
    let started = Instant::now();
    gq(&db, ns, "CREATE INDEX FOR (o:Order) ON (o.number)");
    gq(
        &db,
        ns,
        "CREATE INDEX FOR (o:Order) ON (o.country, o.placed)",
    );
    for chunk in (0..count).collect::<Vec<_>>().chunks(10_000) {
        db.write(|tx| {
            for &i in chunk {
                tx.create_node(
                    ns,
                    grafeno::NewNode::new("Order")
                        .prop("number", i as i64)
                        .prop("country", countries[i % countries.len()])
                        .prop(
                            "placed",
                            format!(
                                "2026-{:02}-{:02}T{:02}:00:00Z",
                                i % 12 + 1,
                                i % 28 + 1,
                                i % 24
                            ),
                        )
                        .prop("total", ((i * 7919) % 100_000) as f64 / 100.0)
                        .prop("paid", i % 3 != 0)
                        .prop("customer", format!("K{}", i % 20_000)),
                )?;
            }
            Ok(())
        })
        .unwrap();
    }
    row.load_s = started.elapsed().as_secs_f64();
    sizes(&db, path, &mut row);
    let read = || db.begin_read().unwrap();
    let mut params = Params::new();
    row.measures.push((
        "an order by number".into(),
        latency(2_000, |i| {
            params.insert("n".into(), GqValue::Int(((i * 7919) % count) as i64));
            grafeno_query::execute_read(
                &read(),
                ns,
                "MATCH (o:Order {number: $n}) RETURN o.total",
                &params,
            )
            .unwrap();
        }),
    ));
    row.measures.push((
        "a country's latest 20".into(),
        latency(500, |i| {
            params.insert(
                "c".into(),
                GqValue::Str(countries[i % countries.len()].into()),
            );
            grafeno_query::execute_read(
                &read(),
                ns,
                "MATCH (o:Order {country: $c}) RETURN o.number ORDER BY o.placed DESC LIMIT 20",
                &params,
            )
            .unwrap();
        }),
    ));
    row.measures.push((
        "unindexed filter, count".into(),
        latency(5, |_| {
            gq(
                &db,
                ns,
                "MATCH (o:Order) WHERE o.customer = 'K77' RETURN count(o)",
            );
        }),
    ));
    writes(&db, ns, "Order", &mut row);
    let mut fixture: BTreeMap<String, Value> = BTreeMap::new();
    fixture.insert("PE latest".into(), rows_of(&gq(&db, ns, "MATCH (o:Order {country: 'PE'}) RETURN o.number, o.placed ORDER BY o.placed DESC, o.number LIMIT 10")));
    fixture.insert("totals".into(), rows_of(&gq(&db, ns, "MATCH (o:Order) WHERE o.number < 1000 RETURN o.country, count(*), sum(o.total) ORDER BY o.country")));
    fixture.insert(
        "customer K77".into(),
        rows_of(&gq(
            &db,
            ns,
            "MATCH (o:Order) WHERE o.customer = 'K77' RETURN count(o)",
        )),
    );
    fixtures.insert("rows".into(), json!(fixture));
    row
}

fn graph(path: &Path, fixtures: &mut Fixtures) -> Row {
    let db = open_db(path);
    let ns = "graph";
    let nodes = n(50_000);
    let mut row = Row {
        dataset: "graph".into(),
        records: nodes * 11,
        ..Row::default()
    };
    let started = Instant::now();
    let ids: Vec<grafeno::NodeId> = db
        .write(|tx| {
            (0..nodes)
                .map(|i| tx.create_node(ns, grafeno::NewNode::new("Person").prop("i", i as i64)))
                .collect()
        })
        .unwrap();
    for chunk in (0..nodes).collect::<Vec<_>>().chunks(5_000) {
        db.write(|tx| {
            for &i in chunk {
                for k in 1..=10 {
                    let mut edge =
                        grafeno::NewEdge::new(ids[i], "knows", ids[(i * 7 + k * 101) % nodes])
                            .prop("weight", k as i64);
                    if k % 3 == 0 {
                        edge.valid_from = Some(grafeno::Timestamp::from_micros(
                            1_700_000_000_000_000 + (i as i64) * 1_000_000,
                        ));
                    }
                    tx.create_edge(ns, edge)?;
                }
            }
            Ok(())
        })
        .unwrap();
    }
    row.load_s = started.elapsed().as_secs_f64();
    sizes(&db, path, &mut row);
    let read = || db.begin_read().unwrap();
    row.measures.push((
        "neighbors of a node".into(),
        latency(2_000, |i| {
            read()
                .neighbors(
                    ns,
                    ids[(i * 7919) % nodes],
                    grafeno::Direction::Both,
                    &grafeno::EdgeFilter::any(),
                )
                .unwrap();
        }),
    ));
    let mut params = Params::new();
    row.measures.push((
        "two hops, count".into(),
        latency(500, |i| {
            params.insert("i".into(), GqValue::Int(((i * 7919) % nodes) as i64));
            grafeno_query::execute_read(
                &read(),
                ns,
                "MATCH (p:Person {i: $i})-[:knows]->()-[:knows]->(f) RETURN count(f)",
                &params,
            )
            .unwrap();
        }),
    ));
    writes(&db, ns, "Person", &mut row);
    let mut fixture: BTreeMap<String, Value> = BTreeMap::new();
    fixture.insert("degrees".into(), rows_of(&gq(&db, ns, "MATCH (p:Person) WHERE p.i < 20 RETURN p.i, COUNT { (p)-[:knows]-() } AS d ORDER BY p.i")));
    fixture.insert(
        "two hops of 3".into(),
        rows_of(&gq(
            &db,
            ns,
            "MATCH (p:Person {i: 3})-[:knows]->()-[:knows]->(f) RETURN count(f), count(DISTINCT f)",
        )),
    );
    fixtures.insert("graph".into(), json!(fixture));
    row
}

fn hyperedges(path: &Path, fixtures: &mut Fixtures) -> Row {
    let db = open_db(path);
    let ns = "hyper";
    let nodes = n(20_000);
    let count = n(50_000);
    let mut row = Row {
        dataset: "hyperedges".into(),
        records: nodes + count,
        ..Row::default()
    };
    let started = Instant::now();
    gq(
        &db,
        ns,
        &format!(
            "UNWIND range(0, {}) AS i CREATE (:Agent {{i: i}})",
            nodes - 1
        ),
    );
    gq(&db, ns, "CREATE INDEX FOR (a:Agent) ON (a.i)");
    gq(&db, ns, "CREATE INDEX FOR HYPEREDGE h:execution ON (h.run)");
    gq(
        &db,
        ns,
        &format!(
            "UNWIND range(0, {}) AS k CALL {{ WITH k MATCH (a:Agent {{i: k % {nodes}}}), (b:Agent {{i: (k * 7) % {nodes}}}), (c:Agent {{i: (k * 13 + 1) % {nodes}}}) CREATE HYPEREDGE h:execution {{run: k}} [(a) AS planner, (b) AS worker, (c) AS reviewer] }} IN TRANSACTIONS OF 2000 ROWS",
            count - 1
        ),
    );
    row.load_s = started.elapsed().as_secs_f64();
    sizes(&db, path, &mut row);
    let read = || db.begin_read().unwrap();
    let mut params = Params::new();
    row.measures.push((
        "a hyperedge by property".into(),
        latency(1_000, |i| {
            params.insert("r".into(), GqValue::Int(((i * 7919) % count) as i64));
            grafeno_query::execute_read(
                &read(),
                ns,
                "MATCH HYPEREDGE h:execution {run: $r} RETURN h.run",
                &params,
            )
            .unwrap();
        }),
    ));
    writes(&db, ns, "Agent", &mut row);
    let mut fixture: BTreeMap<String, Value> = BTreeMap::new();
    fixture.insert(
        "run 42".into(),
        rows_of(&gq(
            &db,
            ns,
            "MATCH HYPEREDGE h:execution {run: 42} [(p)] RETURN h.run, p.i ORDER BY p.i",
        )),
    );
    fixtures.insert("hyperedges".into(), json!(fixture));
    row
}

fn memory(path: &Path, fixtures: &mut Fixtures) -> Row {
    let db = std::sync::Arc::new(open_db(path));
    let ns = "agent";
    let count = n(10_000);
    let mut row = Row {
        dataset: "memory".into(),
        records: count,
        ..Row::default()
    };
    let server = grafeno_server::mcp::McpServer::shared(db.clone(), ns.into());
    let call = |tool: &str, args: Value| -> Value {
        let out = server.call(tool, args).unwrap();
        assert_eq!(out["isError"], false, "{tool}: {out}");
        out["structuredContent"].clone()
    };
    let topics = [
        "storage",
        "indexing",
        "compression",
        "migration",
        "clusters",
        "search",
        "agents",
        "palaces",
        "security",
        "releases",
    ];
    let started = Instant::now();
    call("palace_create", json!({"name": "Engine"}));
    for topic in topics {
        call(
            "palace_create_room",
            json!({"palace": "Engine", "name": topic}),
        );
    }
    let mut ids = Vec::new();
    for i in 0..count {
        let topic = topics[i % topics.len()];
        let made = call(
            "remember",
            json!({
                "text": format!("Decision {i} about {topic}: keep {topic} simple and measure it before changing it ({}).", i % 97),
                "labels": [if i % 3 == 0 { "Decision" } else { "Fact" }],
                "importance": (i % 10) as f64 / 10.0,
            }),
        );
        let id = made["id"].as_str().unwrap().to_owned();
        call(
            "palace_anchor",
            json!({"memory": id, "palace": "Engine", "room": topic}),
        );
        if let Some(previous) = ids.last() {
            call(
                "relate",
                json!({"from": id, "label": "follows", "to": previous}),
            );
        }
        ids.push(id);
    }
    row.load_s = started.elapsed().as_secs_f64();
    sizes(&db, path, &mut row);
    row.measures.push((
        "context of a question".into(),
        latency(200, |i| {
            call(
                "context",
                json!({"query": format!("what did we decide about {}?", topics[i % topics.len()])}),
            );
        }),
    ));
    row.measures.push((
        "enter a room".into(),
        latency(100, |i| {
            call(
                "palace_enter",
                json!({"palace": "Engine", "room": topics[i % topics.len()]}),
            );
        }),
    ));
    let measured = latency(200, |i| {
        call("remember", json!({"text": format!("Measured note {i}")}));
    });
    row.measures.push(("remember".into(), measured));
    let mut fixture: BTreeMap<String, Value> = BTreeMap::new();
    for topic in ["storage", "palaces"] {
        let found = call(
            "recall",
            json!({"query": format!("decisions about {topic}"), "limit": 5}),
        );
        // As a set: recall weighs recency too, so the order of near ties
        // may move with the seconds between runs.
        let mut texts: Vec<String> = found["results"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|r| r["text"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        texts.sort();
        fixture.insert(format!("recall {topic}"), json!(texts));
    }
    fixture.insert(
        "anchored".into(),
        rows_of(&gq(&db, ns, "MATCH (m)-[:anchored_at]->() RETURN count(m)")),
    );
    fixtures.insert("memory".into(), json!(fixture));
    row
}

/// Equal, numbers within a relative 1e-9: a fixture file's numbers come
/// back from JSON with their last digit rounded, while a score is an `f32`
/// (any real change moves it by far more).
fn close(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| close(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| close(v, w)))
        }
        _ => a == b,
    }
}

fn main() {
    let only: Option<Vec<String>> = std::env::var("ONLY")
        .ok()
        .map(|s| s.split(',').map(str::to_owned).collect());
    let wanted = |name: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == name));
    let dir = tempfile::tempdir().unwrap();
    let mut fixtures = Fixtures::new();
    let mut rows = Vec::new();
    type Build = fn(&Path, &mut Fixtures) -> Row;
    let datasets: [(&str, Build); 6] = [
        ("catalog", catalog),
        ("documents", documents),
        ("rows", rows_dataset),
        ("graph", graph),
        ("hyperedges", hyperedges),
        ("memory", memory),
    ];
    println!(
        "| dataset | records | load s | compress s | file MiB | stored MiB | fragmented MiB | after vacuum MiB | largest tables (MiB) |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |");
    for (name, build) in datasets {
        if !wanted(name) {
            continue;
        }
        let path: PathBuf = dir.path().join(format!("{name}.redb"));
        let row = build(&path, &mut fixtures);
        println!(
            "| {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {} |",
            row.dataset,
            row.records,
            row.load_s,
            row.compress_s,
            row.file_mib,
            row.stored_mib,
            row.fragmented_mib,
            row.vacuumed_mib,
            row.largest
        );
        std::io::stdout().flush().unwrap();
        rows.push(row);
    }
    println!("\n| dataset | operation | p50 ms | p95 ms | p99 ms |");
    println!("| --- | --- | ---: | ---: | ---: |");
    for row in &rows {
        for (what, (p50, p95, p99)) in &row.measures {
            println!(
                "| {} | {what} | {p50:.3} | {p95:.3} | {p99:.3} |",
                row.dataset
            );
        }
    }
    if let Ok(file) = std::env::var("CSV") {
        let mut csv = String::from(
            "dataset,records,load_s,compress_s,file_mib,stored_mib,fragmented_mib,vacuumed_mib,operation,p50_ms,p95_ms,p99_ms\n",
        );
        for row in &rows {
            for (what, (p50, p95, p99)) in &row.measures {
                csv.push_str(&format!(
                    "{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},\"{what}\",{p50:.4},{p95:.4},{p99:.4}\n",
                    row.dataset,
                    row.records,
                    row.load_s,
                    row.compress_s,
                    row.file_mib,
                    row.stored_mib,
                    row.fragmented_mib,
                    row.vacuumed_mib
                ));
            }
        }
        std::fs::write(file, csv).unwrap();
    }
    if let Ok(out) = std::env::var("FIXTURES") {
        std::fs::create_dir_all(&out).unwrap();
        for (name, fixture) in &fixtures {
            let text = serde_json::to_string_pretty(fixture).unwrap() + "\n";
            std::fs::write(Path::new(&out).join(format!("{name}.json")), text).unwrap();
        }
    }
    if let Ok(dir) = std::env::var("COMPARE") {
        let mut differ = 0;
        for (name, fixture) in &fixtures {
            let saved: Value = serde_json::from_slice(
                &std::fs::read(Path::new(&dir).join(format!("{name}.json"))).unwrap(),
            )
            .unwrap();
            for (key, value) in fixture.as_object().unwrap() {
                let recall = key.starts_with("vector recall");
                let same = if recall {
                    value.as_f64() >= saved[key].as_f64()
                } else {
                    close(&saved[key], value)
                };
                if !same {
                    differ += 1;
                    eprintln!("{name} / {key}: {} now, {} saved", value, saved[key]);
                }
            }
        }
        assert_eq!(differ, 0, "{differ} results differ from the fixtures");
        println!("\nEvery result matches the fixtures in {dir}.");
    }
}
