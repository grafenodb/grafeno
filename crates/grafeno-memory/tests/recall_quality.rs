//! Retrieval quality on the fixture questions (`data/recall`), as a floor:
//! what `examples/recall_quality.rs` measures, held so it does not get
//! worse. Graph expansion must not cost the first place.

#[path = "../examples/common/recall_fixture.rs"]
mod fixture;

use std::collections::HashMap;

use grafeno::{Grafeno, NodeId, SearchMode};
use grafeno_memory::{Expansion, RecallQuery, recall};

fn first_ranks(
    db: &Grafeno,
    ids: &HashMap<String, NodeId>,
    questions: &[fixture::Question],
    mode: SearchMode,
    expand: bool,
) -> Vec<Option<usize>> {
    let read = db.begin_read().unwrap();
    questions
        .iter()
        .map(|q| {
            let query = RecallQuery {
                text: Some(q.text.clone()),
                embedding: Some(q.embedding.clone()),
                limit: 10,
                mode: Some(mode),
                expand: expand.then(Expansion::default),
                ranking: None,
                ..RecallQuery::default()
            };
            let answers: Vec<NodeId> = q.answer.iter().map(|k| ids[k]).collect();
            recall(&read, "plain", &query)
                .unwrap()
                .iter()
                .position(|r| answers.contains(&r.node.id))
        })
        .collect()
}

fn share(ranks: &[Option<usize>], within: usize) -> f64 {
    ranks
        .iter()
        .filter(|r| r.is_some_and(|r| r < within))
        .count() as f64
        / ranks.len() as f64
}

#[test]
fn recall_holds_its_quality_on_the_fixture_questions() {
    let (memories, questions) = fixture::corpus();
    let db = Grafeno::in_memory().unwrap();
    let ids = fixture::load(&db, "plain", &memories);

    let text = first_ranks(&db, &ids, &questions, SearchMode::Text, false);
    let vector = first_ranks(&db, &ids, &questions, SearchMode::Vector, false);
    let hybrid = first_ranks(&db, &ids, &questions, SearchMode::Hybrid, false);
    let graph = first_ranks(&db, &ids, &questions, SearchMode::Hybrid, true);

    // Measured: BM25 44/70/79 %, HNSW 63/86/96 %, hybrid 61/89/97 %,
    // hybrid with the graph 61/89/99 % (first, in 3, in 10).
    assert!(share(&text, 10) >= 0.75, "BM25 in 10: {}", share(&text, 10));
    assert!(
        share(&vector, 3) >= 0.80,
        "HNSW in 3: {}",
        share(&vector, 3)
    );
    assert!(
        share(&hybrid, 3) >= 0.85,
        "hybrid in 3: {}",
        share(&hybrid, 3)
    );
    assert!(
        share(&graph, 10) >= 0.95,
        "graph in 10: {}",
        share(&graph, 10)
    );
    assert!(
        share(&graph, 1) >= share(&hybrid, 1),
        "the graph costs the first place: {} < {}",
        share(&graph, 1),
        share(&hybrid, 1)
    );

    // Questions that share the answer's words: first, whatever the way.
    for (name, ranks) in [("BM25", &text), ("hybrid", &hybrid), ("graph", &graph)] {
        let words: Vec<Option<usize>> = questions
            .iter()
            .zip(ranks.iter())
            .filter(|(q, _)| q.kind == "words")
            .map(|(_, r)| *r)
            .collect();
        assert_eq!(share(&words, 1), 1.0, "{name}: {words:?}");
    }
}
