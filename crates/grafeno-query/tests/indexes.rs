//! Phase 8.3: index commands, the planner (indexes, selective labels, early
//! LIMIT) and the limits that keep a query from taking the service down.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use grafeno::Grafeno;
use grafeno_query::{Limits, Params, QueryResult, execute, execute_with};
use serde_json::{Value as Json, json};

const NS: &str = "i";

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

/// 2,000 people aged 0 to 99, a few companies, everyone working somewhere.
fn crowd() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "UNWIND range(1, 5) AS i CREATE (:Company {name: 'C' + toString(i)})",
    );
    run(
        &db,
        "UNWIND range(0, 1999) AS i
         MATCH (c:Company {name: 'C' + toString(i % 5 + 1)})
         CREATE (:Person {name: 'P' + toString(i), age: i % 100, email: 'p' + toString(i) + '@x.org'})-[:works_at]->(c)",
    );
    db
}

/// The details of the first step of a plan.
fn plan(db: &Grafeno, query: &str) -> String {
    run(db, &format!("EXPLAIN {query}")).to_json()["rows"][0][2]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Records read by the whole query, under PROFILE.
fn reads(db: &Grafeno, query: &str) -> u64 {
    run(db, &format!("PROFILE {query}"))
        .profile
        .unwrap()
        .iter()
        .map(|step| step.reads.unwrap())
        .sum()
}

#[test]
fn index_commands() {
    let db = crowd();
    let created = rows(&db, "CREATE INDEX FOR (p:Person) ON (p.age)");
    assert_eq!(
        created,
        json!([[
            "Person_age",
            "Person",
            "age",
            false,
            2000,
            false,
            "nodes",
            false,
            "value",
            "online"
        ]])
    );
    run(
        &db,
        "CREATE INDEX names IF NOT EXISTS FOR (p:Person) ON (p.name)",
    );
    run(
        &db,
        "CREATE INDEX names IF NOT EXISTS FOR (p:Person) ON (p.name)",
    );
    run(&db, "CREATE INDEX ON :Company(name)");
    run(
        &db,
        "CREATE CONSTRAINT FOR (p:Person) REQUIRE p.email IS UNIQUE",
    );
    assert_eq!(
        rows(&db, "SHOW INDEXES"),
        json!([
            [
                "Company_name",
                "Company",
                "name",
                false,
                5,
                false,
                "nodes",
                false,
                "value",
                "online"
            ],
            [
                "Person_age",
                "Person",
                "age",
                false,
                2000,
                false,
                "nodes",
                false,
                "value",
                "online"
            ],
            [
                "Person_email_unique",
                "Person",
                "email",
                true,
                2000,
                false,
                "nodes",
                false,
                "value",
                "online"
            ],
            [
                "names", "Person", "name", false, 2000, false, "nodes", false, "value", "online"
            ]
        ])
    );
    assert_eq!(rows(&db, "SHOW CONSTRAINTS").as_array().unwrap().len(), 1);
    assert_eq!(
        rows(&db, "CALL db.indexes() YIELD name RETURN count(name)"),
        json!([[4]])
    );

    // The constraint holds for later writes, and a failed write leaves nothing.
    assert!(
        fail(&db, "CREATE (:Person {name: 'Dup', email: 'p1@x.org'})").contains("must be unique")
    );
    assert_eq!(
        rows(&db, "MATCH (p:Person {name: 'Dup'}) RETURN count(p)"),
        json!([[0]])
    );
    assert!(
        fail(
            &db,
            "CREATE CONSTRAINT FOR (p:Person) REQUIRE p.age IS UNIQUE"
        )
        .contains("both have")
    );
    assert!(fail(&db, "CREATE INDEX names FOR (c:Company) ON (c.name)").contains("already exists"));

    assert!(fail(&db, "DROP CONSTRAINT names").contains("not a constraint"));
    run(&db, "DROP INDEX names");
    run(&db, "DROP INDEX names IF EXISTS");
    assert!(fail(&db, "DROP INDEX names").contains("no index named"));
    run(&db, "DROP CONSTRAINT Person_email_unique");
    assert_eq!(rows(&db, "SHOW INDEXES").as_array().unwrap().len(), 2);
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    assert!(fail(&db, "PROFILE SHOW INDEXES").contains("PROFILE"));
}

#[test]
fn the_planner_uses_indexes_and_small_labels() {
    let db = crowd();
    let query = "MATCH (p:Person) WHERE p.age > 97 RETURN p.name ORDER BY p.name";
    let before = rows(&db, query);
    let scanned = reads(&db, query);
    assert!(plan(&db, query).contains("scan of :Person, 2000 nodes"));

    run(&db, "CREATE INDEX FOR (p:Person) ON (p.age)");
    assert!(
        plan(&db, query).contains("index Person_age: … < age"),
        "{}",
        plan(&db, query)
    );
    assert_eq!(rows(&db, query), before, "same rows with the index");
    assert_eq!(before.as_array().unwrap().len(), 40);
    let indexed = reads(&db, query);
    assert!(
        indexed * 10 < scanned,
        "{indexed} reads with the index, {scanned} without"
    );

    // Exclusive and inclusive bounds, both ends, and the value on the left.
    for (query, expected) in [
        (
            "MATCH (p:Person) WHERE p.age >= 98 AND p.age < 99 RETURN count(p)",
            20,
        ),
        ("MATCH (p:Person) WHERE 2 > p.age RETURN count(p)", 40),
        (
            "MATCH (p:Person) WHERE p.age IN [1, 3, 3.0, null] RETURN count(p)",
            40,
        ),
        ("MATCH (p:Person {age: 7}) RETURN count(p)", 20),
        ("MATCH (p:Person) WHERE p.age = 7.0 RETURN count(p)", 20),
        ("MATCH (p:Person) WHERE p.age = null RETURN count(p)", 0),
        ("MATCH (p:Person) WHERE p.age = '7' RETURN count(p)", 0),
    ] {
        assert_eq!(rows(&db, query), json!([[expected]]), "{query}");
        assert!(
            plan(&db, query).contains("index Person_age"),
            "{query}: {}",
            plan(&db, query)
        );
    }
    run(&db, "CREATE INDEX FOR (p:Person) ON (p.name)");
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WHERE p.name STARTS WITH 'P199' RETURN count(p)"
        ),
        json!([[11]])
    );
    assert!(
        plan(
            &db,
            "MATCH (p:Person) WHERE p.name STARTS WITH 'P199' RETURN p"
        )
        .contains("STARTS WITH")
    );

    // A pattern starts from its smallest label, or from an index on the far end.
    assert!(
        plan(&db, "MATCH (p:Person)-[:works_at]->(c:Company) RETURN p")
            .contains("start at (c:Company) (scan of :Company, 5 nodes)")
    );
    assert!(
        plan(
            &db,
            "MATCH (c:Company)<-[:works_at]-(p:Person {age: 3}) RETURN c"
        )
        .contains("index Person_age")
    );
    // Values computed from earlier rows use the index too.
    assert_eq!(
        rows(
            &db,
            "UNWIND [10, 20] AS a MATCH (p:Person) WHERE p.age = a RETURN a, count(p) ORDER BY a"
        ),
        json!([[10, 20], [20, 20]])
    );
}

#[test]
fn limit_stops_the_scan_early() {
    let db = crowd();
    let all = reads(&db, "MATCH (p:Person) RETURN p.name");
    let limited = reads(&db, "MATCH (p:Person) RETURN p.name LIMIT 5");
    assert!(
        limited * 3 < all,
        "{limited} reads for LIMIT 5, {all} for every row"
    );
    assert_eq!(
        rows(&db, "MATCH (p:Person) RETURN p.name SKIP 2 LIMIT 3")
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // With a filter, it still returns the first rows that pass it.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WHERE p.age = 99 RETURN p.age LIMIT 2"
        ),
        json!([[99], [99]])
    );
    // Sorting needs every row: no early stop, still correct.
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) RETURN p.age ORDER BY p.age DESC LIMIT 1"
        ),
        json!([[99]])
    );
}

#[test]
fn limits_stop_runaway_queries() {
    let db = crowd();
    let run_with =
        |query: &str, limits: Limits| execute_with(&db, NS, query, &Params::new(), &limits);

    let error = run_with(
        "MATCH (p:Person) RETURN count(p)",
        Limits {
            max_reads: Some(100),
            ..Limits::none()
        },
    )
    .unwrap_err();
    assert!(
        error.limit && error.message.contains("more than 100 records"),
        "{error}"
    );

    // Rows held at once: a cartesian product returned whole. (Counted, it
    // folds as it goes and holds no rows: 12.3.)
    let error = run_with(
        "MATCH (a:Person), (b:Person) RETURN a.name, b.name",
        Limits {
            max_rows: Some(10_000),
            ..Limits::none()
        },
    )
    .unwrap_err();
    assert!(
        error.limit && error.message.contains("rows at once"),
        "{error}"
    );

    let error = run_with(
        "MATCH (p:Person) RETURN p",
        Limits {
            timeout: Some(Duration::ZERO),
            ..Limits::none()
        },
    )
    .unwrap_err();
    assert!(
        error.limit && error.message.contains("longer than"),
        "{error}"
    );

    let cancel = Arc::new(AtomicBool::new(true));
    let error = run_with(
        "MATCH (p:Person) RETURN p",
        Limits {
            cancel: Some(cancel),
            ..Limits::none()
        },
    )
    .unwrap_err();
    assert!(
        error.limit && error.message.contains("cancelled"),
        "{error}"
    );

    // A write over its limit leaves nothing behind.
    let error = run_with(
        "MATCH (p:Person) SET p.flag = true",
        Limits {
            max_reads: Some(500),
            ..Limits::none()
        },
    )
    .unwrap_err();
    assert!(error.limit, "{error}");
    assert_eq!(
        rows(&db, "MATCH (p:Person) WHERE p.flag RETURN count(p)"),
        json!([[0]])
    );

    // The standard limits leave ordinary queries alone.
    assert!(
        run_with(
            "MATCH (p:Person)-[:works_at]->(c) RETURN c.name, count(p)",
            Limits::standard()
        )
        .is_ok()
    );
}

#[test]
fn fuzzy_indexes_in_gq() {
    let db = grafeno::Grafeno::in_memory().unwrap();
    let run = |q: &str| {
        grafeno_query::execute(&db, "f", q, &grafeno_query::Params::new())
            .unwrap_or_else(|e| panic!("{q}: {}", e.message))
            .to_json()
    };
    run(
        "UNWIND ['John Smith', 'Jane Smith', 'María García', 'Pedro López'] AS n CREATE (:Person {name: n})",
    );
    let created = run("CREATE FUZZY INDEX FOR (p:Person) ON (p.name)");
    assert_eq!(created["rows"][0][0], "Person_name_fuzzy");
    assert_eq!(created["rows"][0][5], true, "{created}");
    let found = run(
        "CALL db.searchFuzzy('Person', 'name', 'jonh smith', 2) YIELD node, score RETURN node.name, score > 0.5",
    );
    assert_eq!(found["rows"][0], serde_json::json!(["John Smith", true]));
    let found = run(
        "CALL db.searchFuzzy('Person', 'name', 'maria garsia', 1, 'jaro_winkler') YIELD node RETURN node.name",
    );
    assert_eq!(found["rows"][0][0], "María García");
    // An equality still finds the exact value: the planner does not use
    // the fuzzy index for it.
    let exact = run("MATCH (p:Person {name: 'Jane Smith'}) RETURN count(p)");
    assert_eq!(exact["rows"][0][0], 1);
    let plan = run("EXPLAIN MATCH (p:Person {name: 'Jane Smith'}) RETURN p");
    assert!(
        !plan["rows"][0][2].as_str().unwrap().contains("fuzzy"),
        "{plan}"
    );
    let shown = run("SHOW INDEXES");
    assert_eq!(shown["columns"][5], "fuzzy");
    run("DROP INDEX Person_name_fuzzy");
    let error = grafeno_query::execute(
        &db,
        "f",
        "CALL db.searchFuzzy('Person', 'name', 'x') YIELD node RETURN node",
        &grafeno_query::Params::new(),
    )
    .unwrap_err();
    assert!(
        error.message.contains("CREATE FUZZY INDEX"),
        "{}",
        error.message
    );
}
