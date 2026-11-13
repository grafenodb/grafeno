//! Named vector indexes (16.7): several per namespace, each with its
//! similarity and parameters, kept on writes, searched with filters during
//! the walk, by a node's own vector, built in the background, replicated.

use grafeno::{
    Grafeno, IndexDef, NewNode, NodeId, Similarity, VectorFilter, VectorSearch, VectorSpec,
};
use serde_json::json;

const NS: &str = "v";

fn index(name: &str, label: &str, key: &str, spec: VectorSpec) -> IndexDef {
    IndexDef {
        name: name.into(),
        vector: Some(spec),
        ..IndexDef::new(label, key)
    }
}

/// 400 docs on a circle (2 dimensions, angle i degrees), with `bucket` =
/// i % 10, and a vector of 3 dimensions too.
fn docs(db: &Grafeno) -> Vec<NodeId> {
    db.write(|tx| {
        (0..400)
            .map(|i| {
                let angle = (i as f32).to_radians();
                tx.create_node(
                    NS,
                    NewNode::new("Doc")
                        .prop("v2", json!([angle.cos(), angle.sin()]))
                        .prop("v3", json!([angle.cos() * 2.0, angle.sin() * 2.0, 1.0]))
                        .prop("bucket", i % 10)
                        .prop("i", i),
                )
            })
            .collect()
    })
    .unwrap()
}

fn search(db: &Grafeno, query: &[f32], search: VectorSearch) -> Vec<i64> {
    let read = db.begin_read().unwrap();
    read.vector_search(NS, query, &search)
        .unwrap()
        .into_iter()
        .map(|hit| {
            read.node(NS, hit.node).unwrap().unwrap().props["i"]
                .as_i64()
                .unwrap()
        })
        .collect()
}

#[test]
fn several_indexes_with_their_similarity() {
    let db = Grafeno::in_memory().unwrap();
    let ids = docs(&db);
    let filled = db
        .write(|tx| {
            let a = tx.create_index(NS, index("by_angle", "Doc", "v2", VectorSpec::new(2)))?;
            let b = tx.create_index(
                NS,
                index(
                    "by_place",
                    "Doc",
                    "v3",
                    VectorSpec {
                        similarity: Similarity::Euclidean,
                        m: 8,
                        ..VectorSpec::new(3)
                    },
                ),
            )?;
            Ok((a, b))
        })
        .unwrap();
    assert_eq!(filled, (400, 400));
    let near = |name: &str, query: &[f32]| {
        search(
            &db,
            query,
            VectorSearch {
                index: Some(name.into()),
                limit: 3,
                ..VectorSearch::default()
            },
        )
    };
    // 90 degrees: the nearest angles, whatever the index.
    assert_eq!(near("by_angle", &[0.0, 1.0])[0], 90);
    assert_eq!(near("by_place", &[0.0, 2.0, 1.0])[0], 90);
    // Cosine ignores length; Euclidean does not.
    assert_eq!(near("by_angle", &[0.0, 50.0])[0], 90);
    let far = db
        .begin_read()
        .unwrap()
        .vector_search(
            NS,
            &[0.0, 2.0, 1.0],
            &VectorSearch {
                index: Some("by_place".into()),
                limit: 1,
                ..VectorSearch::default()
            },
        )
        .unwrap();
    assert!((far[0].score - 1.0).abs() < 1e-3, "distance 0 scores 1");

    // Kept on writes: a moved vector, a removed one, a wrong dimension.
    db.write(|tx| {
        let mut props = grafeno::Props::new();
        props.insert("v2".into(), json!([0.0, -1.0]));
        tx.set_node_props(NS, ids[0], props)
    })
    .unwrap();
    assert_eq!(near("by_angle", &[0.0, -1.0])[0], 0);
    db.write(|tx| tx.remove_node_props(NS, ids[0], &["v2"]))
        .unwrap();
    assert_ne!(near("by_angle", &[0.0, -1.0])[0], 0);
    let wrong = db.write(|tx| {
        let mut props = grafeno::Props::new();
        props.insert("v2".into(), json!([1.0, 2.0, 3.0]));
        tx.set_node_props(NS, ids[1], props)
    });
    assert!(wrong.unwrap_err().to_string().contains("dimension 2"));
    let read = db.begin_read().unwrap();
    assert!(
        read.check(NS).unwrap().is_ok(),
        "{:?}",
        read.check(NS).unwrap().problems
    );
    let listed = read.indexes(NS).unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|i| i.def.name == "by_angle")
            .unwrap()
            .entries,
        399
    );
    drop(read);

    // A rebuild fills them again; a drop leaves nothing behind.
    db.write(|tx| tx.rebuild_indexes(NS)).unwrap();
    assert_eq!(near("by_place", &[0.0, 2.0, 1.0])[0], 90);
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    db.write(|tx| tx.drop_index(NS, "by_place")).unwrap();
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn filters_apply_during_the_walk() {
    let db = Grafeno::in_memory().unwrap();
    docs(&db);
    db.write(|tx| tx.create_index(NS, index("by_angle", "Doc", "v2", VectorSpec::new(2))))
        .unwrap();
    // One node in ten passes the filter: still ten results, all of them in
    // the bucket, the nearest ones.
    let mut props = grafeno::Props::new();
    props.insert("bucket".into(), json!(7));
    let found = search(
        &db,
        &[1.0, 0.0],
        VectorSearch {
            index: Some("by_angle".into()),
            limit: 10,
            filter: Some(VectorFilter {
                labels: vec!["Doc".into()],
                props,
            }),
            ..VectorSearch::default()
        },
    );
    assert_eq!(found.len(), 10);
    assert!(found.iter().all(|i| i % 10 == 7), "{found:?}");
    // 357 degrees is 3 from the query, 7 degrees 7 away.
    assert_eq!(&found[..2], &[357, 7]);
    // A node's own vector: its neighbors, itself left out.
    let read = db.begin_read().unwrap();
    let node = read
        .nodes_with_label(NS, "Doc")
        .unwrap()
        .into_iter()
        .find(|id| read.node(NS, *id).unwrap().unwrap().props["i"] == 100)
        .unwrap();
    let similar: Vec<i64> = read
        .similar(
            NS,
            node,
            &VectorSearch {
                index: Some("by_angle".into()),
                limit: 2,
                ..VectorSearch::default()
            },
        )
        .unwrap()
        .into_iter()
        .map(|hit| {
            read.node(NS, hit.node).unwrap().unwrap().props["i"]
                .as_i64()
                .unwrap()
        })
        .collect();
    let mut sorted = similar.clone();
    sorted.sort();
    assert_eq!(sorted, vec![99, 101]);
}

#[test]
fn background_builds_and_replicas() {
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    docs(&leader);
    leader
        .write(|tx| {
            tx.create_index(
                NS,
                index(
                    "later",
                    "Doc",
                    "v2",
                    VectorSpec {
                        building: Some(Default::default()),
                        ..VectorSpec::new(2)
                    },
                ),
            )
        })
        .unwrap();
    let read = leader.begin_read().unwrap();
    assert_eq!(read.pending_indexes(NS).unwrap(), vec!["later".to_owned()]);
    assert_eq!(read.index_builds(NS).unwrap()[0].total, 400);
    drop(read);
    // A chunk, a write meanwhile (indexed at once), then the rest.
    let first = leader
        .write(|tx| tx.build_index_step(NS, "later", 150))
        .unwrap()
        .unwrap();
    assert_eq!((first.done, first.finished), (150, false));
    leader
        .write(|tx| {
            tx.create_node(
                NS,
                NewNode::new("Doc")
                    .prop("v2", json!([-1.0, 0.0]))
                    .prop("i", 999),
            )
        })
        .unwrap();
    assert_eq!(leader.build_pending_indexes(NS, 100).unwrap(), 1);
    let read = leader.begin_read().unwrap();
    assert!(read.pending_indexes(NS).unwrap().is_empty());
    assert!(read.check(NS).unwrap().is_ok());
    assert_eq!(read.indexes(NS).unwrap()[0].entries, 401, "every node once");
    // A follower replays the creation, the steps and the write.
    let follower = Grafeno::in_memory().unwrap();
    follower
        .apply_entries(&read.log_entries(0, usize::MAX).unwrap())
        .unwrap();
    let copy = follower.begin_read().unwrap();
    assert_eq!(copy.indexes(NS).unwrap(), read.indexes(NS).unwrap());
    assert!(copy.check(NS).unwrap().is_ok());
    let entries = read.log_entries(0, usize::MAX).unwrap();
    assert!(entries.iter().any(|(_, entry)| entry.format == 7));
}

#[test]
fn health_counts_removals_and_compacting_starts_over() {
    let db = Grafeno::in_memory().unwrap();
    let ids = docs(&db);
    db.write(|tx| {
        for (i, id) in ids.iter().enumerate() {
            let angle = (i as f32).to_radians();
            tx.set_embedding(NS, *id, &[angle.cos(), angle.sin(), 0.5])?;
        }
        tx.create_index(
            NS,
            index(
                "by_place",
                "Doc",
                "v3",
                VectorSpec {
                    similarity: Similarity::Euclidean,
                    m: 8,
                    ..VectorSpec::new(3)
                },
            ),
        )
    })
    .unwrap();
    let health = |db: &Grafeno| db.begin_read().unwrap().vector_health(NS).unwrap();
    let fresh = health(&db);
    assert_eq!(fresh.len(), 2);
    assert_eq!(fresh[0].index, None);
    assert_eq!(fresh[1].index.as_deref(), Some("by_place"));
    assert!(
        fresh
            .iter()
            .all(|h| h.vectors == 400 && h.removed == 0 && h.churn == 0.0)
    );

    // 100 embeddings removed and 50 nodes deleted: both graphs lose them.
    db.write(|tx| {
        for id in &ids[..100] {
            tx.remove_embedding(NS, *id)?;
        }
        for id in &ids[100..150] {
            tx.delete_node(NS, *id)?;
        }
        Ok(())
    })
    .unwrap();
    let worn = health(&db);
    assert_eq!((worn[0].vectors, worn[0].removed), (250, 150));
    assert!(worn[0].repaired > 0);
    assert!((worn[0].churn - 0.6).abs() < 1e-9);
    assert_eq!((worn[1].vectors, worn[1].removed), (350, 50));

    // Compacting the index starts its count over and keeps its similarity.
    let report = db
        .write(|tx| tx.compact_vector_index(NS, "by_place"))
        .unwrap();
    assert_eq!(report.vectors, 350);
    let after = health(&db);
    assert_eq!(
        (after[1].vectors, after[1].removed, after[1].repaired),
        (350, 0, 0)
    );
    assert_eq!(after[0].removed, 150, "the namespace's own graph untouched");
    let hit = db
        .begin_read()
        .unwrap()
        .vector_search(
            NS,
            &[0.0, 2.0, 1.0],
            &VectorSearch {
                index: Some("by_place".into()),
                limit: 1,
                ..VectorSearch::default()
            },
        )
        .unwrap();
    assert!(
        (hit[0].score - 1.0).abs() < 1e-3,
        "still Euclidean: distance 0 scores 1"
    );
    assert!(
        db.write(|tx| tx.compact_vector_index(NS, "missing"))
            .is_err()
    );

    db.write(|tx| tx.compact_vectors(NS)).unwrap();
    assert_eq!(health(&db)[0].removed, 0);
}
