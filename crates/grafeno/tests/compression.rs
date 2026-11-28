//! Compression of stored values (phase 5 of the storage roadmap): values
//! compress with dictionaries trained per namespace and kind, read back
//! the same, the file becomes layout 20 only once it holds an envelope,
//! and a migration back to JSON gives an earlier release its file back.

use grafeno::{
    Compression, Encoding, Grafeno, IndexDef, MigrationOptions, MigrationState, NewEdge, NewNode,
    OpenOptions, RunControl, TrainOptions,
};

fn catalogue(db: &Grafeno, ns: &str, n: i64) {
    db.write(|tx| {
        tx.create_index(ns, IndexDef::new("Item", "code"))?;
        let mut previous = None;
        for i in 0..n {
            let id = tx.create_node(
                ns,
                NewNode::new("Item")
                    .prop("code", format!("C{i:05}"))
                    .prop("name", format!("item number {i} of the spring catalogue"))
                    .prop("price", (i * 37 % 1000) as f64 / 10.0)
                    .prop("tags", serde_json::json!(["catalogue", "spring", i % 7]))
                    .text(format!(
                        "Item {i} of the spring catalogue, sold in boxes of {} units, made of recycled material.",
                        i % 12 + 1
                    )),
            )?;
            if let Some(previous) = previous {
                tx.create_edge(ns, NewEdge::new(previous, "next", id).prop("weight", i % 5))?;
            }
            previous = Some(id);
        }
        Ok(())
    })
    .unwrap();
}

fn open(path: &std::path::Path, compression: Compression) -> Grafeno {
    Grafeno::open_with(
        path,
        &OpenOptions {
            compression,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn tables(path: &std::path::Path) -> Vec<String> {
    use redb::{ReadableDatabase, TableHandle};
    let db = redb::Database::open(path).unwrap();
    let read = db.begin_read().unwrap();
    read.list_tables()
        .unwrap()
        .map(|t| t.name().to_owned())
        .collect()
}

fn digests(db: &Grafeno, ns: &str) -> grafeno::NamespaceDigest {
    db.begin_read().unwrap().namespace_digest(ns).unwrap()
}

#[test]
fn compression_off_keeps_the_file_as_earlier_releases_write_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("off.redb");
    let db = open(&path, Compression::Off);
    catalogue(&db, "shop", 300);
    assert_eq!(db.layout().unwrap(), grafeno::BASE_LAYOUT);
    drop(db);
    // No table an earlier release would not know.
    let names = tables(&path);
    assert!(
        !names
            .iter()
            .any(|t| t == "dictionaries" || t == "text_values"),
        "{names:?}"
    );
}

#[test]
fn values_compress_with_dictionaries_and_read_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auto.redb");
    let plain_path = dir.path().join("plain.redb");
    let plain = open(&plain_path, Compression::Off);
    catalogue(&plain, "shop", 2_000);
    let db = open(&path, Compression::Auto);
    catalogue(&db, "shop", 2_000);
    // Small values wait for a dictionary: nothing compressed yet.
    assert_eq!(db.layout().unwrap(), grafeno::BASE_LAYOUT);

    let trained = db.train_dictionaries(&TrainOptions::default()).unwrap();
    let kinds: Vec<_> = trained.iter().map(|d| d.kind).collect();
    assert!(kinds.contains(&grafeno::StoredKind::Node), "{kinds:?}");
    assert!(kinds.contains(&grafeno::StoredKind::Edge), "{kinds:?}");
    assert!(kinds.contains(&grafeno::StoredKind::Text), "{kinds:?}");
    // Trained once: nothing more to train. Storing them made it layout 20.
    assert!(
        db.train_dictionaries(&TrainOptions::default())
            .unwrap()
            .is_empty()
    );
    assert_eq!(db.layout().unwrap(), grafeno::ENVELOPE_LAYOUT);

    // What was stored before rewrites with them.
    let plan = db
        .plan_migration(&MigrationOptions {
            encoding: Some(Encoding::Compressed),
            verify: true,
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

    // The same records, texts and search results as the uncompressed file.
    let (a, b) = (db.begin_read().unwrap(), plain.begin_read().unwrap());
    assert!(
        a.check("shop").unwrap().is_ok(),
        "{:?}",
        a.check("shop").unwrap().problems
    );
    let ids = |r: &grafeno::ReadTx| {
        r.nodes_page("shop", None, None, 5_000)
            .unwrap()
            .into_iter()
            .map(|n| (n.props.get("code").cloned(), n.labels))
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&a), ids(&b));
    let first = a.nodes_page("shop", None, None, 1).unwrap()[0].id;
    assert!(a.text("shop", first).unwrap().unwrap().contains("recycled"));
    let hits = |r: &grafeno::ReadTx| {
        r.search_text("shop", "boxes recycled 7", 5)
            .unwrap()
            .into_iter()
            .map(|h| h.score)
            .collect::<Vec<_>>()
    };
    assert_eq!(hits(&a), hits(&b));

    // Smaller: nodes, edges and texts.
    let (ua, ub) = (
        a.namespace_usage("shop").unwrap(),
        b.namespace_usage("shop").unwrap(),
    );
    assert!(
        ua.node_bytes * 2 < ub.node_bytes,
        "{} against {}",
        ua.node_bytes,
        ub.node_bytes
    );
    assert!(
        ua.edge_bytes * 10 < ub.edge_bytes * 8,
        "{} against {}",
        ua.edge_bytes,
        ub.edge_bytes
    );
    assert!(
        ua.text_bytes * 2 < ub.text_bytes,
        "{} against {}",
        ua.text_bytes,
        ub.text_bytes
    );

    // Kept across opening the file again.
    drop((a, b));
    drop(db);
    let db = open(&path, Compression::Auto);
    assert_eq!(db.dictionaries().unwrap().len(), trained.len());
    let read = db.begin_read().unwrap();
    assert!(read.check("shop").unwrap().is_ok());
    assert!(
        read.text("shop", first)
            .unwrap()
            .unwrap()
            .contains("recycled")
    );
}

#[test]
fn back_to_json_gives_the_file_its_earlier_layout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("back.redb");
    let db = open(&path, Compression::Auto);
    catalogue(&db, "shop", 1_500);
    db.train_dictionaries(&TrainOptions::default()).unwrap();
    catalogue(&db, "more", 1_500);
    let before = (digests(&db, "shop"), digests(&db, "more"));
    assert_eq!(db.layout().unwrap(), grafeno::ENVELOPE_LAYOUT);
    drop(db);

    let db = open(&path, Compression::Off);
    let plan = db
        .plan_migration(&MigrationOptions {
            encoding: Some(Encoding::Json),
            verify: true,
            batch_size: 500,
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
    assert_eq!(done.layout_after, Some(grafeno::BASE_LAYOUT), "{done:?}");
    assert_eq!((digests(&db, "shop"), digests(&db, "more")), before);
    let read = db.begin_read().unwrap();
    assert!(read.check("shop").unwrap().is_ok() && read.check("more").unwrap().is_ok());
    drop(read);
    drop(db);
    let names = tables(&path);
    assert!(
        !names
            .iter()
            .any(|t| t == "dictionaries" || t == "text_values"),
        "{names:?}"
    );
    assert_eq!(
        Grafeno::file_layout(&path).unwrap(),
        Some(grafeno::BASE_LAYOUT)
    );
}

#[test]
fn an_interrupted_compression_resumes_to_the_same_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cut.redb");
    let db = open(&path, Compression::Off);
    catalogue(&db, "shop", 1_200);
    let before = digests(&db, "shop");
    drop(db);
    let db = open(&path, Compression::Auto);
    let options = MigrationOptions {
        encoding: Some(Encoding::Compressed),
        reindex: true,
        verify: true,
        batch_size: 250,
        ..MigrationOptions::default()
    };
    let plan = db.plan_migration(&options).unwrap();
    db.start_migration(&plan).unwrap();
    let mut state = MigrationState::Paused;
    while state == MigrationState::Paused {
        state = db
            .run_migration(
                &plan.id,
                &RunControl {
                    max_batches: Some(2),
                    exclusive: true,
                    ..RunControl::default()
                },
            )
            .unwrap()
            .state;
    }
    assert_eq!(state, MigrationState::Completed);
    assert_eq!(digests(&db, "shop"), before);
    assert!(db.begin_read().unwrap().check("shop").unwrap().is_ok());
    // Nothing left to rewrite.
    let next = db.plan_migration(&options).unwrap();
    let reencode = next
        .steps
        .iter()
        .find(|s| s.kind == grafeno::StepKind::Reencode && s.namespace == "shop")
        .unwrap();
    assert_eq!(reencode.estimate, 0, "{next:?}");
}

#[test]
fn a_backup_of_a_compressed_file_restores() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("a.redb"), Compression::Auto);
    catalogue(&db, "shop", 1_200);
    db.train_dictionaries(&TrainOptions::default()).unwrap();
    db.write(|tx| {
        tx.create_node(
            "shop",
            NewNode::new("Item")
                .prop("code", "late")
                .text("a late arrival of the spring catalogue"),
        )
    })
    .unwrap();
    let copy = dir.path().join("copy.redb");
    db.begin_read().unwrap().backup(&copy).unwrap();
    let other = Grafeno::in_memory().unwrap();
    other.restore_backup(&copy).unwrap();
    assert_eq!(digests(&other, "shop"), digests(&db, "shop"));
    assert!(other.begin_read().unwrap().check("shop").unwrap().is_ok());
    // The restored database writes with the backup's dictionaries.
    assert_eq!(other.dictionaries().unwrap(), db.dictionaries().unwrap());
}
