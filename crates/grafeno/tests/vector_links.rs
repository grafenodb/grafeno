//! Vector links by number (phase 10 of the storage roadmap): a space whose
//! neighbor lists are varints of numbers holds the same graph as one whose
//! lists are ids (same neighbors, same order), answers searches alike, and
//! survives removals, reinserts, reopening and backups; the file is
//! layout 22 while a space keeps numbers, and compacting every space into
//! ids gives it its earlier layout back.

use grafeno::{
    Compression, FulltextFormat, Grafeno, IndexDef, LinkFormat, MigrationOptions, MigrationState,
    NewNode, NodeId, OpenOptions, RunControl, VectorSearch, VectorSpec,
};
use serde_json::json;

const NS: &str = "vec";

fn open(path: &std::path::Path, links: LinkFormat) -> Grafeno {
    Grafeno::open_with(
        path,
        &OpenOptions {
            vector_links: links,
            // Only the links decide the layout here.
            compression: Compression::Off,
            fulltext: FulltextFormat::Entries,
            copy_before_upgrade: false,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

/// A vector of 12 values from `i`, spread.
fn vector(i: usize) -> Vec<f32> {
    (0..12)
        .map(|k| (((i * 7 + 3) * (k + 1)) % 97) as f32 / 97.0 - 0.5)
        .collect()
}

/// 1,500 nodes with embeddings and a property vector indexed on its own,
/// then some removed and some given new vectors.
fn load(db: &Grafeno, ids: &[NodeId]) {
    db.write(|tx| {
        let mut def = IndexDef::new("Item", "v");
        def.name = "by_v".into();
        def.vector = Some(VectorSpec::new(12));
        tx.create_index(NS, def)?;
        for (i, id) in ids.iter().enumerate() {
            tx.upsert_node(
                NS,
                *id,
                NewNode::new("Item").prop("v", json!(vector(i + 5_000))),
            )?;
            tx.set_embedding(NS, *id, &vector(i))?;
        }
        Ok(())
    })
    .unwrap();
    db.write(|tx| {
        for (i, id) in ids.iter().enumerate() {
            if i % 41 == 0 {
                tx.remove_embedding(NS, *id)?;
            } else if i % 53 == 0 {
                tx.set_embedding(NS, *id, &vector(i + 10_000))?;
            }
        }
        Ok(())
    })
    .unwrap();
}

/// Every node's lists on layers 0 and 1, of the embeddings and the index.
fn graph(db: &Grafeno, ids: &[NodeId]) -> Vec<Vec<NodeId>> {
    let read = db.begin_read().unwrap();
    let mut out = Vec::new();
    for id in ids {
        for layer in 0..2 {
            for index in [None, Some("by_v")] {
                out.push(read.vector_neighbors(NS, index, *id, layer).unwrap());
            }
        }
    }
    out
}

fn searches(db: &Grafeno) -> Vec<Vec<(NodeId, u32)>> {
    let read = db.begin_read().unwrap();
    (0..60)
        .flat_map(|q| {
            let query = vector(q * 13 + 7);
            [None, Some("by_v".to_owned())].map(|index| {
                read.vector_search(
                    NS,
                    &query,
                    &VectorSearch {
                        index,
                        limit: 10,
                        ..VectorSearch::default()
                    },
                )
                .unwrap()
                .into_iter()
                .map(|hit| (hit.node, hit.score.to_bits()))
                .collect()
            })
        })
        .collect()
}

fn migrate(db: &Grafeno, links: LinkFormat) -> grafeno::Migration {
    let plan = db
        .plan_migration(&MigrationOptions {
            verify: true,
            vector_links: Some(links),
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
    done
}

#[test]
fn numbers_hold_the_same_graph_as_ids() {
    let dir = tempfile::tempdir().unwrap();
    let ids_path = dir.path().join("ids.redb");
    let numbers_path = dir.path().join("numbers.redb");
    let nodes: Vec<NodeId> = (0..1_500).map(|_| NodeId::new()).collect();
    let by_ids = open(&ids_path, LinkFormat::Ids);
    let by_numbers = open(&numbers_path, LinkFormat::Numbers);
    load(&by_ids, &nodes);
    load(&by_numbers, &nodes);
    let lists = graph(&by_ids, &nodes);
    assert!(lists.iter().filter(|list| !list.is_empty()).count() > 3_000);
    assert_eq!(lists, graph(&by_numbers, &nodes));
    let answers = searches(&by_ids);
    assert_eq!(answers, searches(&by_numbers));
    for db in [&by_ids, &by_numbers] {
        let read = db.begin_read().unwrap();
        let report = read.check(NS).unwrap();
        assert!(report.is_ok(), "{report:?}");
    }
    let (a, b) = (
        by_ids.begin_read().unwrap().check(NS).unwrap(),
        by_numbers.begin_read().unwrap().check(NS).unwrap(),
    );
    assert_eq!(
        (a.dangling_links, a.unreachable_vectors),
        (b.dangling_links, b.unreachable_vectors)
    );
    assert_eq!(by_ids.layout().unwrap(), grafeno::BASE_LAYOUT);
    assert_eq!(by_numbers.layout().unwrap(), grafeno::NUMBERS_LAYOUT);
    // Smaller links.
    let links = |db: &Grafeno| {
        db.storage_usage()
            .unwrap()
            .tables
            .iter()
            .find(|t| t.name == "hnsw_links")
            .map_or(0, |t| t.stored_bytes)
    };
    assert!(
        links(&by_numbers) * 3 < links(&by_ids),
        "{} {}",
        links(&by_numbers),
        links(&by_ids)
    );
    // Reopened, with nothing cached: the same.
    drop(by_numbers);
    let reopened = open(&numbers_path, LinkFormat::Numbers);
    assert_eq!(graph(&reopened, &nodes), lists);
    assert_eq!(searches(&reopened), answers);
    // A backup keeps the numbers.
    let copy = dir.path().join("copy.redb");
    reopened.begin_read().unwrap().backup(&copy).unwrap();
    let restored = open(&copy, LinkFormat::Numbers);
    assert_eq!(searches(&restored), answers);
    assert!(restored.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn compacting_converts_both_ways_and_the_layout_follows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("convert.redb");
    let nodes: Vec<NodeId> = (0..800).map(|_| NodeId::new()).collect();
    // A file of an earlier release: ids.
    {
        let db = open(&path, LinkFormat::Ids);
        load(&db, &nodes);
    }
    let db = open(&path, LinkFormat::Numbers);
    let before = searches(&db);
    // Existing spaces keep ids until compacted; writes go on in ids.
    assert_eq!(db.layout().unwrap(), grafeno::BASE_LAYOUT);
    db.write(|tx| tx.set_embedding(NS, nodes[3], &vector(77_777)))
        .unwrap();
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    let written = searches(&db);
    let done = migrate(&db, LinkFormat::Numbers);
    assert_eq!(done.layout_after, Some(grafeno::NUMBERS_LAYOUT));
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    // Compacting rebuilds the graph: answers may differ from the old one,
    // but they are the ones a compaction in ids gives.
    let numbered = searches(&db);
    assert_eq!(numbered.len(), written.len());
    assert!(before.len() == numbered.len());
    // Back to ids: the earlier layout, no number tables.
    let done = migrate(&db, LinkFormat::Ids);
    assert_eq!(done.layout_after, Some(grafeno::BASE_LAYOUT));
    assert_eq!(searches(&db), numbered);
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    let tables = db.storage_usage().unwrap().tables;
    assert!(
        tables
            .iter()
            .all(|t| t.name != "vector_numbers" && t.name != "vector_ids")
    );
}

#[test]
fn dropping_a_namespace_takes_its_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("drop.redb"), LinkFormat::Numbers);
    let nodes: Vec<NodeId> = (0..300).map(|_| NodeId::new()).collect();
    load(&db, &nodes);
    db.write(|tx| {
        let id = tx.create_node("other", NewNode::new("Item"))?;
        tx.set_embedding("other", id, &vector(1))
    })
    .unwrap();
    let rows = |db: &Grafeno, name: &str| {
        db.storage_usage()
            .unwrap()
            .tables
            .iter()
            .find(|t| t.name == name)
            .map_or(0, |t| t.entries)
    };
    assert!(rows(&db, "vector_ids") > 600);
    db.write(|tx| tx.drop_namespace(NS).map(|_| ())).unwrap();
    assert_eq!(rows(&db, "vector_ids"), 1);
    assert_eq!(rows(&db, "vector_numbers"), 1);
}
