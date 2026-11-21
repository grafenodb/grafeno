//! Phase 12.5: searches walk the vector graph with int8 copies of the
//! vectors and rescore the best candidates exactly. Recall stays high, the
//! scores returned are exact, and the copies follow every change.

use std::collections::HashSet;

use grafeno::{Grafeno, NewNode, NodeId};

const NS: &str = "q";
const DIM: usize = 64;

fn vectors(count: usize) -> Vec<Vec<f32>> {
    let mut state = 88172645463325252u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % 2_000_000) as f32 / 1_000_000.0 - 1.0
    };
    let centers: Vec<Vec<f32>> = (0..16)
        .map(|_| (0..DIM).map(|_| next()).collect())
        .collect();
    (0..count)
        .map(|i| {
            let v: Vec<f32> = centers[i % 16].iter().map(|c| c + 0.4 * next()).collect();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / norm).collect()
        })
        .collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn filled(count: usize) -> (Grafeno, Vec<NodeId>, Vec<Vec<f32>>) {
    let db = Grafeno::in_memory().unwrap();
    let all = vectors(count);
    let ids = db
        .write(|tx| {
            all.iter()
                .map(|v| tx.create_node(NS, NewNode::new("Doc").embedding(v.clone())))
                .collect::<grafeno::Result<Vec<_>>>()
        })
        .unwrap();
    (db, ids, all)
}

#[test]
fn recall_stays_high_and_scores_are_exact() {
    let (db, ids, all) = filled(3_000);
    let read = db.begin_read().unwrap();
    let mut found = 0;
    for query in all.iter().step_by(30) {
        let mut exact: Vec<(f32, usize)> = all
            .iter()
            .enumerate()
            .map(|(i, v)| (dot(query, v), i))
            .collect();
        exact.sort_by(|a, b| b.0.total_cmp(&a.0));
        let truth: HashSet<NodeId> = exact[..10].iter().map(|(_, i)| ids[*i]).collect();
        let hits = read.search_vector_with(NS, query, 10, 64).unwrap();
        assert_eq!(hits.len(), 10);
        for hit in &hits {
            let at = ids.iter().position(|id| *id == hit.node).unwrap();
            // Rescored with the exact vector, not the int8 copy.
            assert!((hit.score - dot(query, &all[at])).abs() < 1e-5);
        }
        assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));
        found += hits.iter().filter(|hit| truth.contains(&hit.node)).count();
    }
    let recall = found as f64 / (100 * 10) as f64;
    assert!(recall >= 0.95, "recall@10 {recall}");
    // Estimated from 200 of its own vectors: 50 left the estimate close
    // enough to 0.95 to fail now and then.
    assert!(read.vector_recall(NS, 200, 64).unwrap().unwrap() >= 0.95);
}

#[test]
fn the_int8_copies_follow_every_change() {
    let (db, ids, all) = filled(500);
    // A removed vector is never found; a replaced one is found by its new value.
    db.write(|tx| {
        tx.remove_embedding(NS, ids[0])?;
        tx.set_embedding(NS, ids[1], &all[2])
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let hits = read.search_vector(NS, &all[0], 5).unwrap();
    assert!(hits.iter().all(|hit| hit.node != ids[0]));
    let hits = read.search_vector(NS, &all[2], 2).unwrap();
    assert!(hits.iter().any(|hit| hit.node == ids[1]));
    assert!(read.check(NS).unwrap().is_ok());
    drop(read);
    // Compacting rebuilds the graph and keeps the copies.
    db.write(|tx| tx.compact_vectors(NS).map(|_| ())).unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(read.search_vector(NS, &all[3], 1).unwrap()[0].node, ids[3]);
    drop(read);
    // A backup carries them.
    let path = std::env::temp_dir().join(format!("grafeno-q8-{}.redb", NodeId::new()));
    db.begin_read().unwrap().backup(&path).unwrap();
    {
        let copy = Grafeno::open(&path).unwrap();
        let read = copy.begin_read().unwrap();
        assert_eq!(read.search_vector(NS, &all[4], 1).unwrap()[0].node, ids[4]);
    }
    std::fs::remove_file(&path).unwrap();
    // Dropping the namespace removes them with the vectors.
    db.write(|tx| tx.drop_namespace(NS).map(|_| ())).unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.search_vector(NS, &all[4], 1).unwrap().is_empty());
}
