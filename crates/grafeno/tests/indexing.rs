//! Indexing policies per namespace (phase 6 of the storage roadmap): an
//! automatic namespace indexes properties as they appear, a manual one
//! does not, a hybrid one follows its rules first; a namespace of an
//! earlier release stays manual until a migration gives it a policy and
//! the indexes it wants.

use grafeno::{
    Grafeno, IndexKind, IndexOrigin, IndexRange, IndexingMode, IndexingPolicy, MigrationOptions,
    MigrationState, NewNode, OpenOptions, RunControl, StepKind,
};
use serde_json::json;

fn open(path: &std::path::Path, indexing: IndexingMode) -> Grafeno {
    Grafeno::open_with(
        path,
        &OpenOptions {
            indexing,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn automatic() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    db.set_default_indexing(IndexingMode::Automatic);
    db
}

fn names(db: &Grafeno, ns: &str) -> Vec<(String, IndexOrigin)> {
    let mut out: Vec<(String, IndexOrigin)> = db
        .begin_read()
        .unwrap()
        .index_defs(ns)
        .unwrap()
        .into_iter()
        .map(|def| (def.name, def.origin))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn item(i: i64) -> NewNode {
    NewNode::new("Item")
        .prop("code", format!("C{i}"))
        .prop("price", i as f64 / 4.0)
        .prop("active", i % 2 == 0)
        .prop("tags", json!(["a", "b"]))
        .prop("meta", json!({"x": 1}))
        .prop("notes", "x".repeat(300))
}

#[test]
fn an_automatic_namespace_indexes_properties_as_they_appear() {
    let db = automatic();
    db.write(|tx| {
        for i in 0..50 {
            tx.create_node("shop", item(i))?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(
        names(&db, "shop"),
        vec![
            ("auto_Item_active".to_owned(), IndexOrigin::Auto),
            ("auto_Item_code".to_owned(), IndexOrigin::Auto),
            ("auto_Item_notes_text".to_owned(), IndexOrigin::Auto),
            ("auto_Item_price".to_owned(), IndexOrigin::Auto),
        ]
    );
    let read = db.begin_read().unwrap();
    // Every node is in them, as if CREATE INDEX had come first.
    let found = read
        .index_lookup("shop", "auto_Item_code", &IndexRange::Equal(json!("C7")), 5)
        .unwrap();
    assert_eq!(found.len(), 1);
    let cheap = read
        .index_lookup(
            "shop",
            "auto_Item_price",
            &IndexRange::Between {
                lower: std::ops::Bound::Included(json!(0.0)),
                upper: std::ops::Bound::Included(json!(2.0)),
            },
            100,
        )
        .unwrap();
    assert_eq!(cheap.len(), 9);
    assert!(read.check("shop").unwrap().is_ok());
    let info = read
        .indexes("shop")
        .unwrap()
        .into_iter()
        .find(|i| i.def.name == "auto_Item_code")
        .unwrap();
    assert_eq!((info.entries, info.uses), (50, 1));
    // A new label decides again; a known pair does not.
    drop(read);
    db.write(|tx| tx.create_node("shop", NewNode::new("Tag").prop("code", "t")))
        .unwrap();
    assert!(names(&db, "shop").iter().any(|(n, _)| n == "auto_Tag_code"));
}

#[test]
fn a_manual_namespace_and_the_library_default_index_nothing() {
    let db = Grafeno::in_memory().unwrap();
    assert_eq!(db.default_indexing(), IndexingMode::Manual);
    db.write(|tx| tx.create_node("plain", item(1))).unwrap();
    assert!(names(&db, "plain").is_empty());
    let db = automatic();
    db.write(|tx| {
        tx.create_namespace("quiet", None)?;
        tx.set_namespace_indexing("quiet", IndexingPolicy::with_mode(IndexingMode::Manual))?;
        tx.create_node("quiet", item(1))
    })
    .unwrap();
    assert!(names(&db, "quiet").is_empty());
}

#[test]
fn create_index_takes_the_place_of_an_automatic_one() {
    let db = automatic();
    db.write(|tx| tx.create_node("shop", item(1))).unwrap();
    let held = db
        .write(|tx| tx.create_index("shop", grafeno::IndexDef::new("Item", "code")))
        .unwrap();
    assert_eq!(held, 1);
    let made = names(&db, "shop");
    assert!(
        made.contains(&("Item_code".to_owned(), IndexOrigin::Manual)),
        "{made:?}"
    );
    assert!(!made.iter().any(|(n, _)| n == "auto_Item_code"));
    assert!(db.begin_read().unwrap().check("shop").unwrap().is_ok());
}

#[test]
fn a_dropped_automatic_index_is_not_made_again() {
    let db = automatic();
    db.write(|tx| tx.create_node("shop", item(1))).unwrap();
    db.write(|tx| tx.drop_index("shop", "auto_Item_code").map(|_| ()))
        .unwrap();
    db.write(|tx| tx.create_node("shop", item(2))).unwrap();
    assert!(
        !names(&db, "shop")
            .iter()
            .any(|(n, _)| n == "auto_Item_code")
    );
}

#[test]
fn a_hybrid_policy_follows_its_rules_first() {
    let db = automatic();
    let policy: IndexingPolicy = serde_json::from_value(json!({
        "mode": "hybrid",
        "exclude": ["notes"],
        "labels": {"Item": {"properties": {"code": ["exact", "fulltext"]}, "exclude": ["price"]}},
        "properties": {"active": ["none"]},
        "max_per_label": 3
    }))
    .unwrap();
    db.write(|tx| {
        tx.create_namespace("shop", None)?;
        tx.set_namespace_indexing("shop", policy)?;
        tx.create_node("shop", item(1))?;
        tx.create_node(
            "shop",
            NewNode::new("Item").prop("a", 1).prop("b", 2).prop("c", 3),
        )
    })
    .unwrap();
    let made: Vec<String> = names(&db, "shop").into_iter().map(|(n, _)| n).collect();
    // code: both kinds; price excluded for Item, notes everywhere, active
    // by rule; then a, b until the limit of 3.
    assert_eq!(
        made,
        ["auto_Item_a", "auto_Item_code", "auto_Item_code_text"]
    );
}

#[test]
fn turning_a_namespace_automatic_leaves_its_data_to_the_migration() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        for i in 0..20 {
            tx.create_node("old", item(i))?;
        }
        Ok(())
    })
    .unwrap();
    db.write(|tx| {
        tx.set_namespace_indexing("old", IndexingPolicy::with_mode(IndexingMode::Automatic))
    })
    .unwrap();
    // Properties already there are not indexed behind a write.
    db.write(|tx| tx.create_node("old", item(99).prop("fresh", "new")))
        .unwrap();
    let made: Vec<String> = names(&db, "old").into_iter().map(|(n, _)| n).collect();
    assert_eq!(made, ["auto_Item_fresh"]);
    // The migration plans and creates the rest.
    let plan = db
        .plan_migration(&MigrationOptions {
            namespaces: vec!["old".into()],
            indexing: Some(IndexingPolicy::with_mode(IndexingMode::Automatic)),
            verify: true,
            ..MigrationOptions::default()
        })
        .unwrap();
    assert_eq!(plan.steps[0].kind, StepKind::Index);
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
    let made: Vec<String> = names(&db, "old").into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        made,
        [
            "auto_Item_active",
            "auto_Item_code",
            "auto_Item_fresh",
            "auto_Item_notes_text",
            "auto_Item_price"
        ]
    );
    let read = db.begin_read().unwrap();
    assert!(read.check("old").unwrap().is_ok());
    let found = read
        .index_lookup("old", "auto_Item_code", &IndexRange::Equal(json!("C3")), 5)
        .unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn a_namespace_of_an_earlier_release_stays_manual() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.redb");
    drop(open(&path, IndexingMode::Manual));
    {
        let db = open(&path, IndexingMode::Manual);
        db.write(|tx| tx.create_node("legacy", item(1))).unwrap();
    }
    // What an earlier release wrote: a namespace without a policy.
    {
        use redb::{ReadableTable, TableDefinition};
        const NAMESPACES: TableDefinition<&str, &[u8]> = TableDefinition::new("namespaces");
        let raw = redb::Database::open(&path).unwrap();
        let tx = raw.begin_write().unwrap();
        {
            let mut table = tx.open_table(NAMESPACES).unwrap();
            let value = table.get("legacy").unwrap().unwrap().value().to_vec();
            let mut meta: serde_json::Value = serde_json::from_slice(&value).unwrap();
            assert!(meta.as_object_mut().unwrap().remove("indexing").is_some());
            table
                .insert("legacy", serde_json::to_vec(&meta).unwrap().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
    }
    let db = open(&path, IndexingMode::Automatic);
    db.write(|tx| tx.create_node("legacy", item(2).prop("fresh", 1)))
        .unwrap();
    assert!(names(&db, "legacy").is_empty());
    let info = db
        .begin_read()
        .unwrap()
        .namespace("legacy")
        .unwrap()
        .unwrap();
    assert!(info.indexing.is_none());
    assert!(
        db.pending_work()
            .unwrap()
            .iter()
            .any(|p| p.kind == "indexing")
    );
    // A namespace made now follows the database's mode.
    db.write(|tx| tx.create_node("new", item(1))).unwrap();
    assert!(!names(&db, "new").is_empty());
}

#[test]
fn a_write_declares_the_indexes_it_names() {
    let db = Grafeno::in_memory().unwrap();
    let indexes = [(
        "title".to_owned(),
        vec![IndexKind::Exact, IndexKind::Fulltext],
    )]
    .into_iter()
    .collect();
    db.write(|tx| {
        tx.declare_property_indexes("docs", &["Doc".to_owned()], &indexes)?;
        tx.create_node(
            "docs",
            NewNode::new("Doc").prop("title", "Spring catalogue"),
        )
    })
    .unwrap();
    assert_eq!(
        names(&db, "docs"),
        vec![
            ("Doc_title".to_owned(), IndexOrigin::Manual),
            ("Doc_title_text".to_owned(), IndexOrigin::Manual),
        ]
    );
    // Declared again, nothing changes.
    db.write(|tx| tx.declare_property_indexes("docs", &["Doc".to_owned()], &indexes))
        .unwrap();
    assert_eq!(names(&db, "docs").len(), 2);
}
