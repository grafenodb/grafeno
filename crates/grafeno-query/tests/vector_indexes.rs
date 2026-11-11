//! Phase 16.7 in GQ: `CREATE VECTOR INDEX`, `db.searchVector` with an
//! index and a filter, several vectors at once, `db.similar`, background
//! builds, listings and errors.

use grafeno::Grafeno;
use grafeno_query::{Params, QueryResult, execute};
use serde_json::{Value as Json, json};

const NS: &str = "q";

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

fn items() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "UNWIND range(0, 199) AS i
         CREATE (:Item {i: i, tier: i % 4,
                        v: [cos(radians(i)), sin(radians(i))],
                        w: [toFloat(i), 0.0, 1.0]})",
    );
    db
}

#[test]
fn vector_indexes_in_gq() {
    let db = items();
    let made = rows(
        &db,
        "CREATE VECTOR INDEX angle FOR (n:Item) ON (n.v) OPTIONS {dimension: 2}",
    );
    assert_eq!(made[0][0], "angle");
    assert_eq!(made[0][4], 200);
    assert_eq!(made[0][8], "vector");
    assert_eq!(made[0][9], "online");
    run(
        &db,
        "CREATE VECTOR INDEX place FOR (n:Item) ON (n.w)
         OPTIONS {dimension: 3, similarity: 'euclidean', m: 8, ef_search: 32}",
    );
    let nearest = rows(
        &db,
        "CALL db.searchVector([0.0, 1.0], {index: 'angle', limit: 3}) YIELD node RETURN node.i",
    );
    assert_eq!(nearest[0][0], 90);
    let euclid = rows(
        &db,
        "CALL db.searchVector([50.2, 0.0, 1.0], {index: 'place', limit: 2}) YIELD node RETURN node.i",
    );
    assert_eq!(euclid, json!([[50], [51]]));
    // A filter during the walk: a quarter of the nodes, still 5 results.
    let tier = rows(
        &db,
        "CALL db.searchVector([1.0, 0.0], {index: 'angle', limit: 5, labels: ['Item'], where: {tier: 3}})
         YIELD node RETURN node.i",
    );
    assert_eq!(tier.as_array().unwrap().len(), 5);
    assert!(
        tier.as_array()
            .unwrap()
            .iter()
            .all(|row| row[0].as_i64().unwrap() % 4 == 3)
    );
    // Several vectors at once.
    let many = rows(
        &db,
        "CALL db.searchVectors([[1.0, 0.0], [0.0, 1.0]], {index: 'angle', limit: 1})
         YIELD query, node RETURN query, node.i",
    );
    assert_eq!(many, json!([[0, 0], [1, 90]]));
    // A node's own vector.
    let similar = rows(
        &db,
        "MATCH (n:Item {i: 120}) CALL db.similar(n, {index: 'angle', limit: 2}) YIELD node
         RETURN node.i ORDER BY node.i",
    );
    assert_eq!(similar, json!([[119], [121]]));
    // Hybrid search may name a vector index.
    run(&db, "MATCH (n:Item) SET n.text = 'item ' + toString(n.i)");
    let shown = rows(&db, "SHOW INDEXES");
    assert_eq!(shown.as_array().unwrap().len(), 2);
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    run(&db, "DROP INDEX place");
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn background_builds() {
    let db = items();
    let made = rows(
        &db,
        "CREATE VECTOR INDEX later FOR (n:Item) ON (n.v) OPTIONS {dimension: 2, background: true}",
    );
    assert_eq!(made[0][4], 0);
    assert_eq!(made[0][9], "building");
    db.build_pending_indexes(NS, 64).unwrap();
    let shown = rows(&db, "SHOW INDEXES");
    assert_eq!(shown[0][4], 200);
    assert_eq!(shown[0][9], "online");
}

#[test]
fn vector_index_errors() {
    let db = items();
    run(
        &db,
        "CREATE VECTOR INDEX angle FOR (n:Item) ON (n.v) OPTIONS {dimension: 2}",
    );
    for (query, expected) in [
        (
            "CREATE VECTOR INDEX x FOR (n:Item) ON (n.v)",
            "needs OPTIONS",
        ),
        (
            "CREATE VECTOR INDEX x FOR (n:Item) ON (n.v) OPTIONS {similarity: 'cosine'}",
            "needs its dimension",
        ),
        (
            "CREATE VECTOR INDEX x FOR (n:Item) ON (n.v) OPTIONS {dimension: 2, similarity: 'manhattan'}",
            "unknown similarity",
        ),
        (
            "CREATE VECTOR INDEX x FOR (n:Item) ON (n.v) OPTIONS {dimension: 3}",
            "not 3",
        ),
        (
            "CREATE VECTOR INDEX x FOR (n:Item) ON (n.v, n.w) OPTIONS {dimension: 2}",
            "one node property",
        ),
        (
            "CALL db.searchVector([1.0, 0.0, 0.0], {index: 'angle'})",
            "dimension 2",
        ),
        (
            "CALL db.searchVector([1.0, 0.0], {index: 'nothing'})",
            "no index named",
        ),
        (
            "CALL db.searchVector([1.0, 0.0], {lim: 2})",
            "unknown option",
        ),
        ("CREATE (:Item {v: [1.0, 2.0, 3.0]})", "dimension 2"),
    ] {
        let error = fail(&db, query);
        assert!(error.contains(expected), "{query}: {error}");
    }
}
