//! GQ's clauses and expressions of phase 8.1: OPTIONAL MATCH, WITH,
//! UNWIND, UNION, CALL, MERGE, FOREACH, CASE, list comprehensions,
//! quantifiers, reduce, pattern predicates, EXPLAIN and PROFILE.

mod common;

use grafeno::Grafeno;
use grafeno_query::{Params, QueryResult, execute, execute_read};
use serde_json::{Value as Json, json};

const NS: &str = "c";

fn run(db: &Grafeno, query: &str) -> QueryResult {
    let result =
        execute(db, NS, query, &Params::new()).unwrap_or_else(|e| panic!("{query}\n→ {e}"));
    // Split across members, a read gives the same rows (13.4).
    common::check_split(db, NS, query, &Params::new());
    result
}

fn rows(db: &Grafeno, query: &str) -> Json {
    run(db, query).to_json()["rows"].clone()
}

/// Ana knows Bob and Carla; Bob knows Carla; Dan knows nobody. Ana and Bob
/// work at Acme, Carla at Initech.
fn people() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (ana:Person {name: 'Ana', age: 34, langs: ['rust', 'go']}),
                (bob:Person {name: 'Bob', age: 28, langs: ['go']}),
                (carla:Person {name: 'Carla', age: 41, langs: []}),
                (dan:Person {name: 'Dan', age: 19}),
                (acme:Company {name: 'Acme'}), (ini:Company {name: 'Initech'}),
                (ana)-[:knows]->(bob), (ana)-[:knows]->(carla), (bob)-[:knows]->(carla),
                (ana)-[:works_at]->(acme), (bob)-[:works_at]->(acme), (carla)-[:works_at]->(ini)",
    );
    db
}

#[test]
fn optional_match_keeps_rows_without_matches() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:knows]->(f) RETURN p.name, f.name ORDER BY p.name, f.name"
        ),
        json!([
            ["Ana", "Bob"],
            ["Ana", "Carla"],
            ["Bob", "Carla"],
            ["Carla", null],
            ["Dan", null]
        ])
    );
    // The WHERE of an OPTIONAL MATCH filters the optional part only.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:knows]->(f) WHERE f.age > 30
             RETURN p.name, count(f) ORDER BY p.name"
        ),
        json!([["Ana", 1], ["Bob", 1], ["Carla", 0], ["Dan", 0]])
    );
}

#[test]
fn with_chains_steps_and_filters_aggregates() {
    let db = people();
    // HAVING-style: companies with more than one person.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person)-[:works_at]->(c) WITH c, count(p) AS people WHERE people > 1
             RETURN c.name, people"
        ),
        json!([["Acme", 2]])
    );
    // An aggregate of an aggregate.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) OPTIONAL MATCH (p)-[:knows]->(f)
             WITH p, count(f) AS friends RETURN max(friends) AS most, avg(friends) AS mean"
        ),
        json!([[2, 0.75]])
    );
    // ORDER BY and LIMIT inside WITH, then more matching.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WITH p ORDER BY p.age DESC LIMIT 2
             MATCH (p)-[:works_at]->(c) RETURN p.name, c.name"
        ),
        json!([["Carla", "Initech"], ["Ana", "Acme"]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person {name: 'Dan'}) WITH *, p.age * 2 AS double RETURN p.name, double"
        ),
        json!([["Dan", 38]])
    );
}

#[test]
fn unwind_and_union() {
    let db = people();
    assert_eq!(
        rows(&db, "UNWIND [3, 1, 2] AS x RETURN x ORDER BY x"),
        json!([[1], [2], [3]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) UNWIND p.langs AS lang RETURN lang, count(*) AS people ORDER BY lang"
        ),
        json!([["go", 2], ["rust", 1]])
    );
    assert_eq!(rows(&db, "UNWIND null AS x RETURN x"), json!([]));
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person {name: 'Ana'}) RETURN p.name AS name
             UNION MATCH (c:Company) RETURN c.name AS name
             UNION RETURN 'Ana' AS name"
        ),
        json!([["Ana"], ["Acme"], ["Initech"]])
    );
    assert_eq!(
        rows(&db, "RETURN 1 AS x UNION ALL RETURN 1 AS x"),
        json!([[1], [1]])
    );
}

#[test]
fn case_comprehensions_quantifiers_and_reduce() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) RETURN p.name,
                CASE WHEN p.age < 21 THEN 'young' WHEN p.age < 40 THEN 'adult' ELSE 'senior' END AS band,
                CASE p.name WHEN 'Ana' THEN 1 WHEN 'Bob' THEN 2 END AS code
             ORDER BY p.name"
        ),
        json!([["Ana", "adult", 1], ["Bob", "adult", 2], ["Carla", "senior", null], ["Dan", "young", null]])
    );
    assert_eq!(
        rows(
            &db,
            "RETURN [x IN range(1, 6) WHERE x % 2 = 0 | x * 10] AS evens,
                    [x IN [1, 2, 3] | x + 1] AS next, [x IN [1, null, 3] WHERE x > 1] AS big"
        ),
        json!([[[20, 40, 60], [2, 3, 4], [3]]])
    );
    assert_eq!(
        rows(
            &db,
            "RETURN all(x IN [1, 2] WHERE x > 0), any(x IN [1, 2] WHERE x > 1),
                    none(x IN [1, 2] WHERE x > 5), single(x IN [1, 2] WHERE x > 1),
                    all(x IN [] WHERE x > 0), any(x IN [1, null] WHERE x > 1)"
        ),
        json!([[true, true, true, true, true, null]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WHERE any(l IN p.langs WHERE l = 'rust') RETURN p.name"
        ),
        json!([["Ana"]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WITH collect(p.age) AS ages
             RETURN reduce(total = 0, a IN ages | total + a) AS total, ages[0..2], ages[-1], 2 ^ 10"
        ),
        json!([[122, [34, 28], 19, 1024.0]])
    );
}

#[test]
fn pattern_predicates() {
    let db = people();
    // People who know someone, and people nobody knows.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WHERE (p)-[:knows]->() RETURN p.name ORDER BY p.name"
        ),
        json!([["Ana"], ["Bob"]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WHERE NOT (p)<-[:knows]-() RETURN p.name ORDER BY p.name"
        ),
        json!([["Ana"], ["Dan"]])
    );
    // A pattern with a condition, and exists() on a pattern or a property.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person)
             WHERE EXISTS { MATCH (p)-[:knows]->(f) WHERE f.age > 40 } AND exists(p.langs)
             RETURN p.name ORDER BY p.name"
        ),
        json!([["Ana"], ["Bob"]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (a:Person {name: 'Ana'}), (c:Person {name: 'Carla'})
             RETURN exists((a)-[:knows]->(c)), exists((c)-[:knows]->(a))"
        ),
        json!([[true, false]])
    );
}

#[test]
fn merge_matches_or_creates() {
    let db = people();
    let result = run(
        &db,
        "MERGE (p:Person {name: 'Ana'}) ON CREATE SET p.new = true ON MATCH SET p.seen = true
         RETURN p.name, p.new, p.seen",
    );
    assert_eq!(result.to_json()["rows"], json!([["Ana", null, true]]));
    assert_eq!(result.stats.nodes_created, 0);

    let result = run(
        &db,
        "MERGE (p:Person {name: 'Eve'}) ON CREATE SET p.new = true RETURN p.name, p.new",
    );
    assert_eq!(result.to_json()["rows"], json!([["Eve", true]]));
    assert_eq!(result.stats.nodes_created, 1);

    // Rows of the same query see what earlier rows merged: one node, not two.
    let result = run(
        &db,
        "UNWIND ['X', 'X', 'Y'] AS name MERGE (t:Tag {name: name}) RETURN count(*)",
    );
    assert_eq!(result.stats.nodes_created, 2);
    assert_eq!(rows(&db, "MATCH (t:Tag) RETURN count(t)"), json!([[2]]));

    // Edges: only one `knows` between Ana and Dan however often it is merged.
    for _ in 0..2 {
        run(
            &db,
            "MATCH (a:Person {name: 'Ana'}), (d:Person {name: 'Dan'}) MERGE (a)-[k:knows]->(d) RETURN k",
        );
    }
    assert_eq!(
        rows(
            &db,
            "MATCH (:Person {name: 'Ana'})-[k:knows]->(:Person {name: 'Dan'}) RETURN count(k)"
        ),
        json!([[1]])
    );
    // An ended fact does not count as existing: MERGE records it again.
    run(
        &db,
        "MATCH (:Person {name: 'Ana'})-[k:knows]->(:Person {name: 'Dan'}) SET k.@valid_to = '2020-01-01'",
    );
    let result = run(
        &db,
        "MATCH (a:Person {name: 'Ana'}), (d:Person {name: 'Dan'}) MERGE (a)-[:knows]->(d)",
    );
    assert_eq!(result.stats.edges_created, 1);
    // The whole path when nothing is bound.
    let result = run(
        &db,
        "MERGE (a:City {name: 'Lima'})-[:in]->(c:Country {name: 'Peru'})",
    );
    assert_eq!(
        (result.stats.nodes_created, result.stats.edges_created),
        (2, 1)
    );
}

#[test]
fn foreach_and_writes_after_reads() {
    let db = people();
    let result = run(
        &db,
        "MATCH (a:Person {name: 'Ana'})
         FOREACH (topic IN ['graphs', 'rust'] | MERGE (t:Topic {name: topic}) CREATE (a)-[:likes]->(t))",
    );
    assert_eq!(
        (result.stats.nodes_created, result.stats.edges_created),
        (2, 2)
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (:Person {name: 'Ana'})-[:likes]->(t) RETURN t.name ORDER BY t.name"
        ),
        json!([["graphs"], ["rust"]])
    );
    // A MATCH after a write sees what was written.
    assert_eq!(
        rows(
            &db,
            "CREATE (:Note {title: 'fresh'}) WITH 1 AS one MATCH (n:Note) RETURN n.title"
        ),
        json!([["fresh"]])
    );
    // A failing statement leaves nothing behind.
    assert!(
        execute(
            &db,
            NS,
            "CREATE (:Note {title: 'gone'}) WITH 1 AS x RETURN 1 / 0",
            &Params::new()
        )
        .is_err()
    );
    assert_eq!(rows(&db, "MATCH (n:Note) RETURN count(n)"), json!([[1]]));
}

#[test]
fn procedures() {
    let db = people();
    assert_eq!(
        rows(&db, "CALL db.labels()"),
        json!([["Company", 2], ["Person", 4]])
    );
    assert_eq!(
        rows(
            &db,
            "CALL db.relationshipTypes() YIELD label, count AS edges WHERE edges > 2 RETURN label, edges"
        ),
        json!([["knows", 3], ["works_at", 3]])
    );
    assert_eq!(
        rows(&db, "CALL db.propertyKeys() YIELD key RETURN collect(key)"),
        json!([[["age", "langs", "name"]]])
    );
    let stats = run(&db, "CALL db.stats()");
    assert_eq!(
        stats.columns,
        ["nodes", "edges", "texts", "vectors", "dimension"]
    );
    assert_eq!(stats.to_json()["rows"][0][0], 6);
    // Procedures inside a query, with arguments from each row.
    run(
        &db,
        "MATCH (p:Person {name: 'Ana'}) SET p.@text = 'Ana writes Rust compilers'",
    );
    assert_eq!(
        rows(
            &db,
            "UNWIND ['rust', 'nothing'] AS word CALL db.search(word, 5) YIELD node RETURN word, node.name"
        ),
        json!([["rust", "Ana"]])
    );
    let listed = rows(
        &db,
        "CALL dbms.procedures() YIELD name RETURN collect(name)",
    );
    assert!(
        listed[0][0]
            .as_array()
            .unwrap()
            .contains(&json!("db.searchVector"))
    );
}

#[test]
fn explain_and_profile() {
    let db = people();
    let plan = run(
        &db,
        "EXPLAIN MATCH (a:Person {name: 'Ana'})-[:knows]->(f) WITH f MATCH (f)-[:works_at]->(c) RETURN c.name",
    );
    assert_eq!(
        plan.columns,
        ["step", "operation", "details", "rows", "cost"]
    );
    let steps: Vec<(String, String)> = plan
        .rows
        .iter()
        .map(|row| {
            (
                row[1].to_json().as_str().unwrap().to_owned(),
                row[2].to_json().as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(steps.len(), 4);
    assert_eq!(steps[0].0, "MATCH");
    assert!(
        steps[0]
            .1
            .contains("start at (a:Person {name}) (scan of :Person, 4 nodes)"),
        "{}",
        steps[0].1
    );
    assert!(
        steps[2].1.contains("start at (f) (the bound node)"),
        "{}",
        steps[2].1
    );
    // EXPLAIN runs nothing, so it may describe writes on a read snapshot.
    let read = db.begin_read().unwrap();
    let explained = execute_read(&read, NS, "EXPLAIN CREATE (:X)", &Params::new()).unwrap();
    assert_eq!(explained.rows.len(), 1);
    assert_eq!(rows(&db, "MATCH (x:X) RETURN count(x)"), json!([[0]]));

    let profiled = run(
        &db,
        "PROFILE MATCH (p:Person) WHERE p.age > 30 RETURN p.name ORDER BY p.name",
    );
    assert_eq!(profiled.to_json()["rows"], json!([["Ana"], ["Carla"]]));
    let profile = profiled.to_json()["profile"].clone();
    assert_eq!(profile[0]["operation"], "MATCH");
    assert_eq!(profile[0]["rows"], 2);
    assert_eq!(profile[1]["operation"], "RETURN");
    assert!(profile[0]["time_ms"].as_f64().unwrap() >= 0.0);
}
