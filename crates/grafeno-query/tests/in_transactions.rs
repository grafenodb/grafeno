//! `CALL { … } IN TRANSACTIONS`: a write over many rows, committed a batch
//! at a time, with the rows of the match before it handed over as found;
//! and rewiring edges with it, keeping their history.

use grafeno::Grafeno;
use grafeno_query::{Limits, Params, QueryResult, execute, execute_with};
use serde_json::{Value as Json, json};

const NS: &str = "t";

fn run(db: &Grafeno, query: &str) -> QueryResult {
    execute(db, NS, query, &Params::new()).unwrap_or_else(|e| panic!("{query}\n→ {e}"))
}

fn rows(db: &Grafeno, query: &str) -> Json {
    run(db, query).to_json()["rows"].clone()
}

fn fail(db: &Grafeno, query: &str) -> String {
    execute(db, NS, query, &Params::new())
        .err()
        .unwrap_or_else(|| panic!("{query} should fail"))
        .to_string()
}

#[test]
fn batches_of_rows_each_commit() {
    let db = Grafeno::in_memory().unwrap();
    let done = run(
        &db,
        "UNWIND range(1, 2500) AS i CALL { WITH i CREATE (:N {i: i}) } IN TRANSACTIONS OF 1000 ROWS",
    );
    assert_eq!(done.stats.nodes_created, 2500);
    assert_eq!(done.stats.transactions, 3);
    assert_eq!(rows(&db, "MATCH (n:N) RETURN count(n)"), json!([[2500]]));
    // Without OF: 1,000 rows a batch.
    let done = run(
        &db,
        "MATCH (n:N) CALL { WITH n SET n.even = n.i % 2 = 0 } IN TRANSACTIONS",
    );
    assert_eq!(done.stats.transactions, 3);
    assert_eq!(done.stats.properties_set, 2500);
    assert_eq!(
        rows(&db, "MATCH (n:N {even: true}) RETURN count(n)"),
        json!([[1250]])
    );
}

#[test]
fn a_match_hands_its_rows_over_instead_of_holding_them() {
    let db = Grafeno::in_memory().unwrap();
    run(&db, "UNWIND range(1, 3000) AS i CREATE (:N {i: i})");
    let limits = Limits {
        max_rows: Some(1_500),
        ..Limits::none()
    };
    // In one transaction the 3,000 matches are held at once.
    let whole = execute_with(
        &db,
        NS,
        "MATCH (n:N) WITH n SET n.x = 1",
        &Params::new(),
        &limits,
    );
    assert!(whole.is_err());
    let done = execute_with(
        &db,
        NS,
        "MATCH (n:N) CALL { WITH n SET n.x = 1 } IN TRANSACTIONS OF 200 ROWS",
        &Params::new(),
        &limits,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(done.stats.transactions, 15);
    assert_eq!(
        rows(&db, "MATCH (n:N {x: 1}) RETURN count(n)"),
        json!([[3000]])
    );
}

#[test]
fn a_failing_batch_stops_and_keeps_the_ones_before() {
    let db = Grafeno::in_memory().unwrap();
    run(&db, "CREATE CONSTRAINT FOR (n:N) REQUIRE n.i IS UNIQUE");
    run(&db, "CREATE (:N {i: 250})");
    let error = fail(
        &db,
        "UNWIND range(1, 1000) AS i CALL { WITH i CREATE (:N {i: i}) } IN TRANSACTIONS OF 100 ROWS",
    );
    assert!(!error.is_empty());
    // Batches 1 and 2 (1..200) committed; the third held 250 and failed.
    assert_eq!(rows(&db, "MATCH (n:N) RETURN count(n)"), json!([[201]]));
}

#[test]
fn where_it_may_stand() {
    let db = Grafeno::in_memory().unwrap();
    for query in [
        "UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS RETURN i",
        "CREATE (:M) WITH 1 AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS",
        "UNWIND [1] AS i CALL { WITH i CALL { CREATE (:N) } IN TRANSACTIONS } ",
        "UNWIND [1] AS i CALL { WITH i CREATE (n:N) RETURN n } IN TRANSACTIONS",
        "PROFILE UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS",
        "UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS OF 0 ROWS",
    ] {
        fail(&db, query);
    }
    // EXPLAIN describes it.
    let plan = run(
        &db,
        "EXPLAIN UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS OF 10 ROWS",
    )
    .to_json()
    .to_string();
    assert!(plan.contains("in batches of rows"), "{plan}");
    assert_eq!(rows(&db, "MATCH (n) RETURN count(n)"), json!([[0]]));
}

#[test]
fn rewiring_edges_keeps_their_history() {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "UNWIND range(1, 5) AS k CREATE (:Paciente {code: k}), (:Prueba {code: k})",
    );
    run(
        &db,
        "UNWIND range(1, 1200) AS i MATCH (x:Paciente {code: i % 5 + 1})
         CREATE (:Person {i: i})-[:visits {since: i}]->(x)",
    );
    let before = rows(&db, "RETURN datetime() AS t")[0][0].clone();
    let done = run(
        &db,
        "MATCH (p:Person)-[r:visits]->(x:Paciente)
         CALL { WITH p, r, x
                MATCH (t:Prueba {code: x.code})
                CREATE (p)-[:visits {since: r.since}]->(t)
                SET r.@valid_to = datetime() }
         IN TRANSACTIONS OF 500 ROWS",
    );
    assert_eq!(done.stats.edges_created, 1200);
    assert_eq!(done.stats.transactions, 3);
    // Now every person visits a Prueba, the same code as before; the old
    // edges hold no more, and are still there.
    assert_eq!(
        rows(
            &db,
            "MATCH (:Person)-[:visits]->(:Paciente) VALID AT datetime() RETURN count(*)"
        ),
        json!([[0]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (:Person)-[r:visits]->(:Paciente) WHERE r.@valid_to IS NOT NULL RETURN count(*)"
        ),
        json!([[1200]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person)-[r:visits]->(t:Prueba) WHERE t.code = p.i % 5 + 1 AND r.since = p.i RETURN count(*)"
        ),
        json!([[1200]])
    );
    // As it was before: the old edges, kept with the time they ended.
    let past = format!(
        "MATCH (:Person)-[:visits]->(x:Paciente) VALID AT '{}' RETURN count(*)",
        before.as_str().unwrap()
    );
    assert_eq!(rows(&db, &past), json!([[1200]]));
}

#[test]
fn the_timeout_bounds_each_batch_not_the_whole_run() {
    let db = Grafeno::in_memory().unwrap();
    let limits = Limits {
        timeout: Some(std::time::Duration::from_millis(300)),
        ..Limits::none()
    };
    let read = db.begin_read().unwrap();
    // Twenty batches of 30 ms each: 600 ms in all, every batch within 300.
    let mut commit = |batch: grafeno_query::Batch| {
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut tx = db.begin_write()?;
        let snapshot = db.begin_read()?;
        let stats = batch.run(&mut tx, &snapshot)?;
        tx.commit()?;
        Ok(stats)
    };
    let done = grafeno_query::execute_in_transactions(
        read,
        NS,
        "UNWIND range(1, 2000) AS i CALL { WITH i CREATE (:N {i: i}) } IN TRANSACTIONS OF 100 ROWS",
        &Params::new(),
        &limits,
        &mut commit,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(done.stats.transactions, 20);
    assert_eq!(rows(&db, "MATCH (n:N) RETURN count(n)"), json!([[2000]]));
}
