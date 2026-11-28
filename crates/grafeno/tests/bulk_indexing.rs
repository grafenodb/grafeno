//! Automatic indexing at bulk speed (phase 7 of the storage roadmap): a
//! bulk load holds its automatic decisions back and builds the indexes
//! after it, one pass each; the export's own indexes too. The result is
//! what record-by-record indexing gives: the same indexes, every record
//! in them.

use grafeno::{Grafeno, ImportOptions, IndexOrigin, IndexRange, IndexingMode, NewNode};
use serde_json::json;

fn automatic() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    db.set_default_indexing(IndexingMode::Automatic);
    db
}

fn indexes(db: &Grafeno, ns: &str) -> Vec<(String, IndexOrigin, u64)> {
    let mut out: Vec<_> = db
        .begin_read()
        .unwrap()
        .indexes(ns)
        .unwrap()
        .into_iter()
        .map(|info| (info.def.name, info.def.origin, info.entries))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// An export of `count` items in `ns`, written by a manual database, with
/// one index of its own on `sku`.
fn export(ns: &str, count: i64) -> Vec<u8> {
    let source = Grafeno::in_memory().unwrap();
    source
        .write(|tx| {
            tx.create_index(ns, grafeno::IndexDef::new("Item", "sku"))?;
            for i in 0..count {
                tx.create_node(
                    ns,
                    NewNode::new("Item")
                        .prop("sku", format!("S{i}"))
                        .prop("code", format!("C{i}"))
                        .prop("price", i as f64 / 4.0)
                        .prop("notes", "word ".repeat(80)),
                )?;
            }
            Ok(())
        })
        .unwrap();
    let mut out = Vec::new();
    source
        .begin_read()
        .unwrap()
        .export(&[ns.to_owned()], &mut out)
        .unwrap();
    out
}

fn expected() -> Vec<(String, IndexOrigin, u64)> {
    vec![
        ("Item_sku".into(), IndexOrigin::Manual, 300),
        ("auto_Item_code".into(), IndexOrigin::Auto, 300),
        ("auto_Item_notes_text".into(), IndexOrigin::Auto, 300),
        ("auto_Item_price".into(), IndexOrigin::Auto, 300),
    ]
}

#[test]
fn an_import_builds_its_indexes_after_the_records() {
    let data = export("shop", 300);
    for options in [ImportOptions::default(), ImportOptions { chunk: Some(70) }] {
        let db = automatic();
        db.import_with(data.as_slice(), &options).unwrap();
        let mut found = indexes(&db, "shop");
        // A full-text index holds one entry per word and record.
        let words = found
            .iter()
            .find(|i| i.0 == "auto_Item_notes_text")
            .unwrap()
            .2;
        assert!(words >= 300, "{words}");
        found.iter_mut().for_each(|i| {
            if i.0 == "auto_Item_notes_text" {
                i.2 = 300;
            }
        });
        assert_eq!(found, expected(), "{options:?}");
        let read = db.begin_read().unwrap();
        assert!(read.check("shop").unwrap().is_ok());
        let hit = read
            .index_lookup(
                "shop",
                "auto_Item_code",
                &IndexRange::Equal(json!("C123")),
                5,
            )
            .unwrap();
        assert_eq!(hit.len(), 1);
        let text = read
            .fulltext_search("shop", "auto_Item_notes_text", "word", 500, false)
            .unwrap();
        assert_eq!(text.len(), 300);
        drop(read);
        // The pairs are decided: a write of a known one makes nothing new,
        // a new one is decided at once.
        db.write(|tx| {
            tx.create_node(
                "shop",
                NewNode::new("Item").prop("code", "X").prop("size", 3),
            )
        })
        .unwrap();
        let names: Vec<String> = indexes(&db, "shop").into_iter().map(|i| i.0).collect();
        assert_eq!(names.len(), 5);
        assert!(names.contains(&"auto_Item_size".to_owned()));
    }
}

#[test]
fn a_failed_bulk_import_leaves_its_pairs_to_later_writes() {
    let mut data = export("shop", 300);
    data.extend_from_slice(b"{\"type\":\"node\",\"ns\":\"shop\",\"node\":not json}\n");
    let db = automatic();
    assert!(
        db.import_with(data.as_slice(), &ImportOptions { chunk: Some(100) })
            .is_err()
    );
    // The chunks before the bad line stay (297 items: the header, the
    // namespace and the index take three lines of the first chunk, the
    // last chunk goes with the bad line); the export's own index is kept
    // over them, and no automatic index was made.
    let found = indexes(&db, "shop");
    assert_eq!(found, vec![("Item_sku".into(), IndexOrigin::Manual, 297)]);
    // The next write that brings a pair decides it, from the records there.
    db.write(|tx| tx.create_node("shop", NewNode::new("Item").prop("code", "X")))
        .unwrap();
    let code = indexes(&db, "shop")
        .into_iter()
        .find(|i| i.0 == "auto_Item_code")
        .unwrap();
    assert_eq!(code.2, 298);
    assert!(db.begin_read().unwrap().check("shop").unwrap().is_ok());
}

#[test]
fn a_deferred_write_decides_once_at_its_end() {
    let db = automatic();
    db.write(|tx| {
        tx.with_deferred_indexes(|tx| {
            for i in 0..200 {
                tx.create_node("docs", NewNode::new("Chunk").prop("n", i).prop("part", "a"))?;
            }
            // Nothing decided yet.
            assert!(tx.index_defs("docs")?.is_empty());
            Ok(())
        })
    })
    .unwrap();
    assert_eq!(
        indexes(&db, "docs"),
        vec![
            ("auto_Chunk_n".into(), IndexOrigin::Auto, 200),
            ("auto_Chunk_part".into(), IndexOrigin::Auto, 200),
        ]
    );
    // In a manual namespace, nothing.
    let manual = Grafeno::in_memory().unwrap();
    manual
        .write(|tx| {
            tx.with_deferred_indexes(|tx| {
                tx.create_node("docs", NewNode::new("Chunk").prop("n", 1))
            })
        })
        .unwrap();
    assert!(indexes(&manual, "docs").is_empty());
}
