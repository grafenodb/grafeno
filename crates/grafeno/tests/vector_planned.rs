//! New vectors join their graph at commit, planned on a snapshot when a
//! transaction writes several: the graph is sound, searches find what they
//! should, removals and replacements in the same transaction are honored,
//! the result is the same every time, and a replica builds the same graph.

use grafeno::{Grafeno, NewNode, NodeId};

const NS: &str = "planned";
const DIMENSION: usize = 24;

struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
    }
    /// Near one of 12 centers: neighbors crowd, so plans meet.
    fn clustered(&mut self, centers: &[Vec<f32>]) -> Vec<f32> {
        let at = ((self.unit() + 1.0) * 6.0) as usize % centers.len();
        centers[at].iter().map(|c| c + 0.2 * self.unit()).collect()
    }
}

/// The same ids in every database given the same seed: a vector's level
/// in the graph comes from its node's id.
fn fixed_id(seed: u64, n: u64) -> NodeId {
    let mix = |mut x: u64| {
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    };
    let high = mix(seed.wrapping_add(n.wrapping_mul(2)));
    let low = mix(seed.wrapping_add(n.wrapping_mul(2) + 1));
    NodeId::from_u128(((high as u128) << 64) | low as u128)
}

fn centers(rng: &mut Rng) -> Vec<Vec<f32>> {
    (0..12)
        .map(|_| (0..DIMENSION).map(|_| rng.unit()).collect())
        .collect()
}

/// Rounds of writes: each round a transaction of 300 new nodes with vectors,
/// 40 nodes of earlier rounds given new vectors and 30 losing theirs, and a
/// few nodes whose vector is set and removed within the round.
fn workload(db: &Grafeno, seed: u64) -> Vec<NodeId> {
    let mut rng = Rng(seed);
    let centers = centers(&mut rng);
    let mut ids: Vec<NodeId> = Vec::new();
    for round in 0..4 {
        let fresh: Vec<Vec<f32>> = (0..300).map(|_| rng.clustered(&centers)).collect();
        let moved: Vec<(usize, Vec<f32>)> = if ids.is_empty() {
            Vec::new()
        } else {
            (0..40)
                .map(|i| ((i * 37 + round * 11) % ids.len(), rng.clustered(&centers)))
                .collect()
        };
        let gone: Vec<usize> = if ids.is_empty() {
            Vec::new()
        } else {
            (0..30)
                .map(|i| (i * 53 + round * 7 + 5) % ids.len())
                .collect()
        };
        let made = db
            .write(|tx| {
                let mut made = Vec::new();
                for (n, vector) in fresh.iter().enumerate() {
                    let id = fixed_id(seed, (round * 1_000 + n) as u64);
                    tx.upsert_node(NS, id, NewNode::new("Item").embedding(vector.clone()))?;
                    made.push(id);
                }
                for (at, vector) in &moved {
                    tx.set_embedding(NS, ids[*at], vector)?;
                }
                for at in &gone {
                    tx.remove_embedding(NS, ids[*at])?;
                }
                // Set and removed before the commit: never in the graph.
                for vector in fresh.iter().take(5) {
                    let id = tx.create_node(NS, NewNode::new("Brief").embedding(vector.clone()))?;
                    tx.remove_embedding(NS, id)?;
                }
                Ok(made)
            })
            .unwrap();
        ids.extend(made);
    }
    ids
}

/// The 10 nearest of `queries`, as ids and scores.
fn answers(db: &Grafeno, queries: &[Vec<f32>]) -> Vec<Vec<(NodeId, u32)>> {
    let read = db.begin_read().unwrap();
    queries
        .iter()
        .map(|query| {
            read.search_vector_with(NS, query, 10, 16)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.node, hit.score.to_bits()))
                .collect()
        })
        .collect()
}

fn queries(seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centers = centers(&mut rng);
    (0..60).map(|_| rng.clustered(&centers)).collect()
}

#[test]
fn planned_graphs_are_sound_and_find_what_is_there() {
    let db = Grafeno::in_memory().unwrap();
    let ids = workload(&db, 0x5EED);
    let read = db.begin_read().unwrap();
    let report = read.check(NS).unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
    assert_eq!(report.dangling_links, 0);
    assert_eq!(report.unreachable_vectors, 0);

    // Every vector that is there is found as its own nearest; removed ones
    // never come back.
    let mut with_vector = 0;
    for id in &ids {
        match read.embedding(NS, *id).unwrap() {
            Some(vector) => {
                with_vector += 1;
                let hits = read.search_vector_with(NS, &vector, 1, 64).unwrap();
                assert_eq!(hits[0].node, *id);
            }
            None => {
                let hits = read
                    .search_vector_with(NS, &[1.0; DIMENSION], 1_500, 1_500)
                    .unwrap();
                assert!(hits.iter().all(|hit| hit.node != *id));
            }
        }
    }
    assert!(with_vector > 1_000, "{with_vector}");
    assert!(read.vector_recall(NS, 100, 64).unwrap().unwrap() >= 0.99);
}

#[test]
fn the_same_writes_build_the_same_graph() {
    let (a, b) = (Grafeno::in_memory().unwrap(), Grafeno::in_memory().unwrap());
    workload(&a, 0xABC);
    workload(&b, 0xABC);
    let queries = queries(0x123);
    assert_eq!(answers(&a, &queries), answers(&b, &queries));
}

#[test]
fn a_replica_builds_the_same_graph() {
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    workload(&leader, 0xFEED);
    let follower = Grafeno::in_memory().unwrap();
    let entries = leader
        .begin_read()
        .unwrap()
        .log_entries(0, usize::MAX)
        .unwrap();
    follower.apply_entries(&entries).unwrap();
    let queries = queries(0x456);
    assert_eq!(answers(&follower, &queries), answers(&leader, &queries));
    assert!(follower.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn a_first_transaction_of_many_vectors_makes_an_entry_point() {
    let db = Grafeno::in_memory().unwrap();
    let mut rng = Rng(0x0E0E);
    let centers = centers(&mut rng);
    let vectors: Vec<Vec<f32>> = (0..500).map(|_| rng.clustered(&centers)).collect();
    let ids = db
        .write(|tx| {
            vectors
                .iter()
                .map(|v| tx.create_node(NS, NewNode::new("Item").embedding(v.clone())))
                .collect::<grafeno::Result<Vec<_>>>()
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.check(NS).unwrap().is_ok());
    for (id, vector) in ids.iter().zip(&vectors) {
        assert_eq!(
            read.search_vector_with(NS, vector, 1, 64).unwrap()[0].node,
            *id
        );
    }
}
