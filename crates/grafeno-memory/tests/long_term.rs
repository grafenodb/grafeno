//! Long-term memory as a floor (`examples/long_term.rs`): among 3 000
//! conversations over two years, decisions made in the first months come
//! back when asked for as decisions, and a replaced one ranks below what
//! replaced it.

#[path = "../examples/common/recall_fixture.rs"]
mod fixture;

use grafeno::Grafeno;
use grafeno_memory::{ContextOptions, RecallQuery, context_for, recall};

#[test]
fn old_decisions_come_back_among_thousands_of_conversations() {
    let (conversations, decisions, questions) = fixture::long_term();
    let db = Grafeno::in_memory().unwrap();
    let mut all = conversations;
    all.extend(decisions);
    let ids = fixture::load(&db, "long", &all);
    let read = db.begin_read().unwrap();

    let (mut in_three, mut in_context, mut newer_first, mut replaced) = (0, 0, 0, 0);
    for q in &questions {
        let query = RecallQuery {
            text: Some(q.text.clone()),
            embedding: Some(q.embedding.clone()),
            labels: vec!["Decision".into()],
            ..RecallQuery::default()
        };
        let found = recall(&read, "long", &query).unwrap();
        let answer = ids[&q.answer[0]];
        let at = found.iter().position(|r| r.node.id == answer);
        in_three += usize::from(at.is_some_and(|at| at < 3));
        if let Some(old) = &q.replaced {
            replaced += 1;
            let old = found.iter().position(|r| r.node.id == ids[old]);
            newer_first += usize::from(at.is_some() && (old.is_none() || at < old));
        }
        let context =
            context_for(&read, "long", &query, &ContextOptions::with_budget(600)).unwrap();
        in_context += usize::from(context.items.iter().any(|i| i.node == answer));
    }
    // Measured: 88 % in the first three, 95 % in the context, every new
    // decision ahead of the one it replaced.
    let n = questions.len();
    assert!(in_three * 100 >= n * 80, "in 3: {in_three}/{n}");
    assert!(
        in_context * 100 >= n * 90,
        "in the context: {in_context}/{n}"
    );
    assert_eq!(newer_first, replaced, "new ahead of replaced");
}
