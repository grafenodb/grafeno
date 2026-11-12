//! The vector index as it changes: deletions, the same vectors inserted
//! back under the same ids, and rounds of updates keep search quality, and
//! compacting rebuilds the graph whole.

use grafeno::{Grafeno, NewNode, NodeId};

const NS: &str = "churn";

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
    }
}

#[test]
fn search_quality_survives_deletes_reinserts_and_churn() {
    let mut rng = Rng(0xDEC0DE);
    let centers: Vec<Vec<f32>> = (0..24)
        .map(|_| (0..32).map(|_| rng.unit()).collect())
        .collect();
    let point = |rng: &mut Rng, i: usize| -> Vec<f32> {
        centers[i % 24]
            .iter()
            .map(|c| c + 0.35 * rng.unit())
            .collect()
    };
    let db = Grafeno::in_memory().unwrap();
    let mut ids: Vec<NodeId> = Vec::new();
    for chunk in 0..4 {
        let batch: Vec<Vec<f32>> = (0..1_000)
            .map(|i| point(&mut rng, chunk * 1_000 + i))
            .collect();
        ids.extend(
            db.write(|tx| {
                batch
                    .iter()
                    .map(|v| tx.create_node(NS, NewNode::new("Point").embedding(v.clone())))
                    .collect::<grafeno::Result<Vec<_>>>()
            })
            .unwrap(),
        );
    }
    let quality = |db: &Grafeno, when: &str| {
        let read = db.begin_read().unwrap();
        let report = read.check(NS).unwrap();
        assert!(report.is_ok(), "{when}: {:#?}", report.problems);
        let recall = read.vector_recall(NS, 200, 64).unwrap().unwrap();
        // HNSW levels come from random ids: recall varies a little per run.
        assert!(recall >= 0.95, "{when}: recall@10 {recall}");
        (recall, report.unreachable_vectors, report.dangling_links)
    };
    quality(&db, "built");

    // A third deleted and inserted back under the same ids: the links left
    // pointing at them must not make a node its own neighbor.
    let gone: Vec<NodeId> = ids.iter().step_by(3).copied().collect();
    let saved: Vec<Vec<f32>> = {
        let read = db.begin_read().unwrap();
        gone.iter()
            .map(|id| read.embedding(NS, *id).unwrap().unwrap())
            .collect()
    };
    db.write(|tx| {
        for id in &gone {
            tx.remove_embedding(NS, *id)?;
        }
        Ok(())
    })
    .unwrap();
    quality(&db, "deleted");
    db.write(|tx| {
        for (id, v) in gone.iter().zip(&saved) {
            tx.set_embedding(NS, *id, v)?;
        }
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    // Search is approximate: nearly every reinserted vector comes first
    // for itself (before 10.3, dozens did not come back at all). Levels
    // come from random ids, so the count varies: in 40 runs it missed 0
    // vectors 37 times, 1 twice and 3 once, and a CI run missed 5.
    let missed = gone
        .iter()
        .zip(&saved)
        .take(200)
        .filter(|(id, v)| read.search_vector(NS, v, 1).unwrap()[0].node != **id)
        .count();
    assert!(
        missed <= 8,
        "{missed} of 200 reinserted vectors not found first"
    );
    drop(read);
    quality(&db, "inserted back");

    // Churn: a quarter of the vectors move, three times.
    for round in 0..3 {
        db.write(|tx| {
            for (at, id) in ids
                .iter()
                .enumerate()
                .filter(|(at, _)| (at + round) % 4 == 0)
            {
                tx.set_embedding(NS, *id, &point(&mut rng, at + 7))?;
            }
            Ok(())
        })
        .unwrap();
    }
    let (before, _, _) = quality(&db, "churned");

    let report = db.write(|tx| tx.compact_vectors(NS)).unwrap();
    assert_eq!(report.vectors, 4_000);
    assert_eq!(report.unreachable_after, 0);
    let (after, unreachable, dangling) = quality(&db, "compacted");
    assert_eq!((unreachable, dangling), (0, 0));
    assert!(
        after >= before - 0.01,
        "compacting keeps quality: {before} → {after}"
    );
}
