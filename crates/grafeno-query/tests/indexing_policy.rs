//! Indexing policies in GQ (phase 6 of the storage roadmap): `CREATE
//! NAMESPACE … OPTIONS {indexing: …}`, `ALTER NAMESPACE … SET INDEXING`,
//! and automatic indexes in `SHOW INDEXES` and in plans.

use grafeno::Grafeno;
use grafeno_query::{Params, QueryResult, execute};
use serde_json::{Value as Json, json};

fn run(db: &Grafeno, ns: &str, query: &str) -> QueryResult {
    execute(db, ns, query, &Params::new()).unwrap_or_else(|e| panic!("{query}\n→ {e}"))
}

fn rows(db: &Grafeno, ns: &str, query: &str) -> Json {
    run(db, ns, query).to_json()["rows"].clone()
}

/// `SHOW INDEXES`, these columns, by name.
fn show(db: &Grafeno, ns: &str, wanted: &[&str]) -> Json {
    let result = run(db, ns, "SHOW INDEXES").to_json();
    let columns: Vec<String> = serde_json::from_value(result["columns"].clone()).unwrap();
    let at = |c: &str| columns.iter().position(|x| x == c).unwrap();
    let mut out: Vec<Json> = result["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| Json::Array(wanted.iter().map(|c| row[at(c)].clone()).collect()))
        .collect();
    out.sort_by_key(|row| row[0].to_string());
    Json::Array(out)
}

#[test]
fn a_namespace_created_automatic_indexes_what_it_is_given() {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "shop",
        "CREATE NAMESPACE shop OPTIONS {indexing: {mode: 'automatic'}}",
    );
    run(
        &db,
        "shop",
        "UNWIND range(1, 200) AS i CREATE (:Item {code: 'C' + toString(i), price: i})",
    );
    let shown = show(&db, "shop", &["name", "origin", "entries"]);
    assert_eq!(
        shown,
        json!([
            ["auto_Item_code", "auto", 200],
            ["auto_Item_price", "auto", 200]
        ])
    );
    // The planner reads the automatic index.
    let plan = run(
        &db,
        "shop",
        "EXPLAIN MATCH (n:Item {code: 'C7'}) RETURN n.price",
    )
    .to_json()
    .to_string();
    assert!(plan.contains("auto_Item_code"), "{plan}");
    assert_eq!(
        rows(&db, "shop", "MATCH (n:Item {code: 'C7'}) RETURN n.price"),
        json!([[7]])
    );
    let uses = show(&db, "shop", &["name", "uses"]);
    assert!(uses[0][1].as_i64().unwrap() >= 1, "{uses}");
}

#[test]
fn alter_namespace_sets_the_policy() {
    let db = Grafeno::in_memory().unwrap();
    run(&db, "docs", "CREATE NAMESPACE docs");
    run(
        &db,
        "docs",
        "ALTER NAMESPACE docs SET INDEXING {mode: 'hybrid', exclude: ['raw'], labels: {Doc: {properties: {title: ['fulltext']}}}}",
    );
    run(
        &db,
        "docs",
        "CREATE (:Doc {title: 'Spring catalogue', raw: 'x', year: 2026})",
    );
    let shown = show(&db, "docs", &["name"]);
    assert_eq!(shown, json!([["auto_Doc_title_text"], ["auto_Doc_year"]]));
    let error = execute(
        &db,
        "docs",
        "ALTER NAMESPACE docs SET INDEXING {mode: 'sometimes'}",
        &Params::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("indexing policy"), "{error}");
}
