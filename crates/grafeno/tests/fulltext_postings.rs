//! Full-text index storage (phase 9 of the storage roadmap): the postings
//! format keeps one entry per term and node and answers every query with
//! the same nodes, order and scores as the entries format; a file holding
//! it is layout 21, and rebuilding its indexes in the entries format gives
//! it its earlier layout back.

use grafeno::{
    Compression, FulltextFormat, Grafeno, IndexDef, MigrationOptions, MigrationState, NewNode,
    NodeId, OpenOptions, RunControl,
};
use serde_json::json;

const NS: &str = "docs";

fn open(path: &std::path::Path, fulltext: FulltextFormat) -> Grafeno {
    Grafeno::open_with(
        path,
        &OpenOptions {
            fulltext,
            // Compression marks the file 20 of its own: off, to see 21 alone.
            compression: Compression::Off,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn index() -> IndexDef {
    let mut def = IndexDef::composite("Doc", &["title", "body", "tags"]);
    def.name = "words".into();
    def.fulltext = Some(grafeno::Fulltext {
        analyzer: grafeno::Analyzer::Spanish,
        weights: vec![3.0, 1.0, 0.5],
    });
    def
}

const WORDS: [&str; 14] = [
    "grafo",
    "grafos",
    "memoria",
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
    "camino",
];

/// 600 documents, then some changed and some deleted.
fn load(db: &Grafeno) -> Vec<NodeId> {
    let ids = db
        .write(|tx| {
            tx.create_index(NS, index())?;
            (0..600)
                .map(|i| {
                    let body: Vec<&str> = (0..(20 + i % 50))
                        .map(|k| WORDS[(i * 7 + k * 3 + k * k) % WORDS.len()])
                        .collect();
                    tx.create_node(
                        NS,
                        NewNode::new("Doc")
                            .prop(
                                "title",
                                format!("{} {}", WORDS[i % 14], WORDS[(i / 14) % 14]),
                            )
                            .prop("body", body.join(" "))
                            .prop("tags", json!([WORDS[i % 5], WORDS[(i + 3) % 9]])),
                    )
                })
                .collect::<grafeno::Result<Vec<_>>>()
        })
        .unwrap();
    db.write(|tx| {
        for (k, id) in ids.iter().enumerate().step_by(37) {
            let mut props = grafeno::Props::new();
            props.insert(
                "body".into(),
                json!(format!("camino nuevo {k} grafo memoria")),
            );
            tx.set_node_props(NS, *id, props)?;
        }
        for id in ids.iter().skip(5).step_by(53) {
            tx.delete_node(NS, *id)?;
        }
        Ok(())
    })
    .unwrap();
    ids
}

const QUERIES: [&str; 11] = [
    "grafo",
    "memoria consulta",
    "\"grafo memoria\"",
    "\"camino grafo\"~3",
    "+agente -texto",
    "vector AND relación",
    "catálogo OR entidad",
    "graf*",
    "memorai~2",
    "title:grafo",
    "tags:\"consulta\"",
];

/// Every query's hits: node, score's bits, and fragments.
fn answers(db: &Grafeno) -> Vec<Vec<(NodeId, u32, usize)>> {
    let read = db.begin_read().unwrap();
    QUERIES
        .iter()
        .map(|query| {
            read.fulltext_search(NS, "words", query, 1_000, true)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.node, hit.score.to_bits(), hit.fragments.len()))
                .collect()
        })
        .collect()
}

fn migrate(db: &Grafeno, fulltext: FulltextFormat) {
    let plan = db
        .plan_migration(&MigrationOptions {
            reindex: true,
            verify: true,
            fulltext: Some(fulltext),
            ..MigrationOptions::default()
        })
        .unwrap();
    db.start_migration(&plan).unwrap();
    let done = db
        .run_migration(
            &plan.id,
            &RunControl {
                exclusive: true,
                ..RunControl::default()
            },
        )
        .unwrap();
    assert_eq!(done.state, MigrationState::Completed, "{done:?}");
}

#[test]
fn both_formats_answer_alike() {
    let dir = tempfile::tempdir().unwrap();
    let entries = open(&dir.path().join("entries.redb"), FulltextFormat::Entries);
    let postings = open(&dir.path().join("postings.redb"), FulltextFormat::Postings);
    // The same ids in both, so that ties order alike.
    let ids = load(&entries);
    let export = {
        let mut out = Vec::new();
        entries
            .begin_read()
            .unwrap()
            .export(&[NS.to_owned()], &mut out)
            .unwrap();
        out
    };
    postings.import(export.as_slice()).unwrap();
    assert!(!ids.is_empty());
    let (a, b) = (answers(&entries), answers(&postings));
    for (query, (a, b)) in QUERIES.iter().zip(a.iter().zip(&b)) {
        assert!(!a.is_empty(), "{query} finds something");
        assert_eq!(a, b, "{query}");
    }
    // Both check out; the entry counts say what each holds.
    for db in [&entries, &postings] {
        let read = db.begin_read().unwrap();
        assert!(
            read.check(NS).unwrap().is_ok(),
            "{:?}",
            read.check(NS).unwrap()
        );
    }
    assert_eq!(entries.layout().unwrap(), grafeno::BASE_LAYOUT);
    assert_eq!(postings.layout().unwrap(), grafeno::POSTINGS_LAYOUT);
    let held = |db: &Grafeno| db.begin_read().unwrap().indexes(NS).unwrap()[0].entries;
    // One entry per term and node against one per term, field and node.
    assert!(
        held(&postings) < held(&entries),
        "{} {}",
        held(&postings),
        held(&entries)
    );
}

#[test]
fn rebuilding_converts_both_ways_and_the_layout_follows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("convert.redb");
    let before;
    {
        // A file of an earlier release: entries.
        let db = open(&path, FulltextFormat::Entries);
        load(&db);
        before = answers(&db);
        assert_eq!(db.layout().unwrap(), grafeno::BASE_LAYOUT);
    }
    // This release keeps the existing index as it is until it is rebuilt.
    let db = open(&path, FulltextFormat::Postings);
    assert_eq!(answers(&db), before);
    assert_eq!(db.layout().unwrap(), grafeno::BASE_LAYOUT);
    // Writes keep it in its format.
    db.write(|tx| tx.create_node(NS, NewNode::new("Doc").prop("title", "grafo extra")))
        .unwrap();
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    let with_extra = answers(&db);
    migrate(&db, FulltextFormat::Postings);
    assert_eq!(answers(&db), with_extra);
    assert_eq!(db.layout().unwrap(), grafeno::POSTINGS_LAYOUT);
    // A backup keeps the postings.
    let copy = dir.path().join("copy.redb");
    db.begin_read().unwrap().backup(&copy).unwrap();
    {
        let restored = open(&copy, FulltextFormat::Postings);
        assert_eq!(answers(&restored), with_extra);
        assert!(restored.begin_read().unwrap().check(NS).unwrap().is_ok());
    }
    // Back to entries: the earlier layout, and no postings table.
    migrate(&db, FulltextFormat::Entries);
    assert_eq!(answers(&db), with_extra);
    assert_eq!(db.layout().unwrap(), grafeno::BASE_LAYOUT);
    let usage = db.storage_usage().unwrap();
    assert!(usage.tables.iter().all(|t| t.name != "fulltext_postings"));
}

#[test]
fn dropping_takes_the_postings_away() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("drop.redb"), FulltextFormat::Postings);
    load(&db);
    db.write(|tx| tx.create_node("other", NewNode::new("Doc").prop("title", "grafo")))
        .unwrap();
    db.write(|tx| tx.create_index("other", index()).map(|_| ()))
        .unwrap();
    let rows = |db: &Grafeno| {
        db.storage_usage()
            .unwrap()
            .tables
            .iter()
            .find(|t| t.name == "fulltext_postings")
            .map_or(0, |t| t.entries)
    };
    let all = rows(&db);
    db.write(|tx| tx.drop_index(NS, "words").map(|_| ()))
        .unwrap();
    let left = rows(&db);
    assert!(left > 0 && left < all, "{left} of {all}");
    db.write(|tx| tx.drop_namespace("other").map(|_| ()))
        .unwrap();
    assert_eq!(rows(&db), 0);
}

#[test]
fn postings_and_envelopes_leave_their_layouts_in_either_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("both.redb");
    let db = Grafeno::open_with(
        &path,
        &OpenOptions {
            copy_before_upgrade: false,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    load(&db);
    db.train_dictionaries(&grafeno::TrainOptions::default())
        .unwrap();
    db.write(|tx| tx.create_node(NS, NewNode::new("Doc").prop("title", "grafo")))
        .unwrap();
    // Postings, then envelopes: 21.
    assert_eq!(db.layout().unwrap(), grafeno::POSTINGS_LAYOUT);
    let before = answers(&db);
    // Back to JSON first: still 21, for the postings.
    db.set_compression(Compression::Off);
    let plan = db
        .plan_migration(&MigrationOptions {
            encoding: Some(grafeno::Encoding::Json),
            ..MigrationOptions::default()
        })
        .unwrap();
    db.start_migration(&plan).unwrap();
    db.run_migration(&plan.id, &RunControl::default()).unwrap();
    assert_eq!(db.layout().unwrap(), grafeno::POSTINGS_LAYOUT);
    // Then entries: the layout of a file with neither.
    migrate(&db, FulltextFormat::Entries);
    assert_eq!(db.layout().unwrap(), grafeno::BASE_LAYOUT);
    assert_eq!(answers(&db), before);
}
