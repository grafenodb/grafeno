//! Phase 16.5: indexes on several properties, on edges and over several
//! labels, and constraints on properties (not null, types, keys), declared
//! in GQ, kept on every write, listed, and used by plans.

use grafeno::Grafeno;
use grafeno_query::{Params, QueryResult, execute};
use serde_json::{Value as Json, json};

const NS: &str = "a";

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

/// 2,000 people with a last and a first name, rating 50 films.
fn people() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "UNWIND range(0, 49) AS i CREATE (:Film {title: 'T' + toString(i)})",
    );
    run(
        &db,
        "UNWIND range(0, 1999) AS i
         CREATE (:Person {last: 'L' + toString(i % 40), first: 'F' + toString(i % 50), n: i})",
    );
    run(
        &db,
        "MATCH (p:Person) MATCH (f:Film {title: 'T' + toString(p.n % 50)})
         CREATE (p)-[:rated {stars: p.n % 5 + 1, at: p.n}]->(f)",
    );
    db
}

#[test]
fn composite_indexes_are_used_by_a_prefix_of_their_properties() {
    let db = people();
    let query = "MATCH (p:Person {last: 'L3', first: 'F3'}) RETURN p.n ORDER BY p.n";
    let expected = rows(&db, query);
    let scanned = reads(&db, query);
    let created = rows(&db, "CREATE INDEX FOR (p:Person) ON (p.last, p.first)");
    assert_eq!(
        created,
        json!([[
            "Person_last_first",
            "Person",
            "last, first",
            false,
            2000,
            false,
            "nodes",
            false,
            "value",
            "online"
        ]])
    );
    assert!(
        plan(&db, query).contains("index Person_last_first: last = …, first = …"),
        "{}",
        plan(&db, query)
    );
    assert_eq!(rows(&db, query), expected);
    assert!(reads(&db, query) * 10 < scanned);

    // The first property alone, and a range on the second after it.
    let prefix = "MATCH (p:Person) WHERE p.last = 'L7' RETURN count(p)";
    assert!(plan(&db, prefix).contains("Person_last_first: last = …"));
    assert_eq!(rows(&db, prefix), json!([[50]]));
    let ranged =
        "MATCH (p:Person) WHERE p.last = 'L7' AND p.first STARTS WITH 'F1' RETURN count(p)";
    assert!(plan(&db, ranged).contains("last = …, first STARTS WITH …"));
    let starting: i64 = (0..2000)
        .filter(|i| i % 40 == 7 && format!("F{}", i % 50).starts_with("F1"))
        .count() as i64;
    assert_eq!(rows(&db, ranged), json!([[starting]]));
    // The second property alone cannot use it.
    assert!(!plan(&db, "MATCH (p:Person {first: 'F1'}) RETURN p").contains("Person_last_first"));
    // Parameters and null values.
    let mut params = Params::new();
    params.insert("last".into(), grafeno_query::Value::Str("L3".into()));
    let found = execute(
        &db,
        NS,
        "MATCH (p:Person {last: $last, first: 'F3'}) RETURN p.n ORDER BY p.n",
        &params,
    )
    .unwrap();
    assert_eq!(found.to_json()["rows"], expected);
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person {last: null, first: 'F3'}) RETURN count(p)"
        ),
        json!([[0]])
    );
    // Kept on writes: a changed first name moves the entry.
    run(&db, "MATCH (p:Person {n: 3}) SET p.first = 'Zed'");
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person {last: 'L3', first: 'Zed'}) RETURN p.n"
        ),
        json!([[3]])
    );
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn unique_and_key_constraints_on_several_properties() {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (:Book {isbn: '1', ed: 1}), (:Book {isbn: '1', ed: 2}), (:Book {isbn: '2', ed: 1})",
    );
    run(
        &db,
        "CREATE CONSTRAINT FOR (b:Book) REQUIRE (b.isbn, b.ed) IS UNIQUE",
    );
    let clash = fail(&db, "CREATE (:Book {isbn: '1', ed: 2})");
    assert!(clash.contains("isbn, ed must be unique"), "{clash}");
    run(&db, "CREATE (:Book {isbn: '1', ed: 3})");
    // A missing part: not in the index, so no clash.
    run(&db, "CREATE (:Book {isbn: '1'}), (:Book {isbn: '1'})");
    // Existing duplicates are named and refuse a new constraint.
    let refused = fail(
        &db,
        "CREATE CONSTRAINT FOR (b:Book) REQUIRE b.isbn IS UNIQUE",
    );
    assert!(
        refused.contains("cannot make :Book(isbn) unique"),
        "{refused}"
    );

    // A key: present (not null, a value an index can hold) and unique.
    let missing = fail(
        &db,
        "CREATE CONSTRAINT FOR (b:Book) REQUIRE (b.isbn, b.ed) IS KEY",
    );
    assert!(missing.contains("has no ed"), "{missing}");
    run(&db, "MATCH (b:Book) WHERE b.ed IS NULL DELETE b");
    let key = rows(
        &db,
        "CREATE CONSTRAINT book_key FOR (b:Book) REQUIRE (b.isbn, b.ed) IS KEY",
    );
    assert_eq!(key[0][1], "key");
    for broken in [
        "CREATE (:Book {isbn: '9'})",
        "CREATE (:Book {isbn: '9', ed: null})",
        "CREATE (:Book {isbn: '9', ed: [1]})",
        "MATCH (b:Book {isbn: '2'}) REMOVE b.ed",
        "MATCH (b:Book {isbn: '2'}) SET b.ed = null",
        "CREATE (:Book {isbn: '1', ed: 1})",
    ] {
        fail(&db, broken);
    }
    // Checked at the statement's end: MERGE may set the key after creating.
    run(
        &db,
        "MERGE (b:Book {isbn: '7'}) ON CREATE SET b.ed = 1 RETURN b",
    );
    // A label added later brings the rule.
    run(&db, "CREATE (:Draft {title: 'x'})");
    fail(&db, "MATCH (d:Draft) SET d:Book");
    let shown = rows(&db, "SHOW CONSTRAINTS");
    let kinds: Vec<&str> = shown
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[1].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"key") && kinds.contains(&"unique"),
        "{shown}"
    );
    run(&db, "DROP CONSTRAINT book_key");
    run(&db, "CREATE (:Book {isbn: '9'})");
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn not_null_and_type_constraints() {
    let db = Grafeno::in_memory().unwrap();
    run(&db, "CREATE (:User {email: 'a@x'}), (:User {name: 'b'})");
    let refused = fail(
        &db,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS NOT NULL",
    );
    assert!(refused.contains("has no email"), "{refused}");
    run(&db, "MATCH (u:User {name: 'b'}) SET u.email = 'b@x'");
    let created = rows(
        &db,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS NOT NULL",
    );
    assert_eq!(created[0][0], "User_email_not_null");
    for broken in [
        "CREATE (:User {name: 'c'})",
        "CREATE (:User {email: null})",
        "MATCH (u:User {name: 'b'}) SET u.email = null",
        "MATCH (u:User {name: 'b'}) REMOVE u.email",
        "MATCH (u:User {name: 'b'}) SET u = {name: 'b'}",
    ] {
        let error = fail(&db, broken);
        assert!(error.contains("User_email_not_null"), "{broken}: {error}");
    }
    run(
        &db,
        "MERGE (u:User {name: 'd'}) ON CREATE SET u.email = 'd@x'",
    );
    // Other labels are free.
    run(&db, "CREATE (:Guest {name: 'e'})");

    // Types: null passes (whether it may be null is the other rule).
    run(
        &db,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.age IS :: INTEGER",
    );
    run(
        &db,
        "CREATE CONSTRAINT FOR (u:User) REQUIRE u.seen IS TYPED DATETIME",
    );
    run(
        &db,
        "CREATE (:User {email: 'f@x', age: 30, seen: datetime('2026-01-02T03:04:05Z')})",
    );
    run(&db, "CREATE (:User {email: 'g@x', age: null})");
    for broken in [
        "CREATE (:User {email: 'h@x', age: 'thirty'})",
        "CREATE (:User {email: 'h@x', age: 30.5})",
        "CREATE (:User {email: 'h@x', seen: 'yesterday'})",
    ] {
        let error = fail(&db, broken);
        assert!(error.contains("not of type"), "{broken}: {error}");
    }
    // Existing values of the wrong type refuse a new rule.
    run(&db, "CREATE (:Item {price: '12'})");
    let refused = fail(
        &db,
        "CREATE CONSTRAINT FOR (i:Item) REQUIRE i.price IS :: FLOAT",
    );
    assert!(refused.contains("not of type FLOAT"), "{refused}");
    let unknown = fail(
        &db,
        "CREATE CONSTRAINT FOR (i:Item) REQUIRE i.price IS :: MONEY",
    );
    assert!(unknown.contains("unknown type MONEY"), "{unknown}");

    let shown = rows(&db, "SHOW CONSTRAINTS");
    assert!(
        shown.as_array().unwrap().iter().any(|row| row[1] == "type"
            && row[3] == "age"
            && row[4] == ":: INTEGER"
            && row[5] == "nodes"),
        "{shown}"
    );
    run(&db, "DROP CONSTRAINT User_email_not_null");
    run(&db, "CREATE (:User {name: 'i'})");
}

#[test]
fn edge_indexes_and_rules() {
    let db = people();
    let query = "MATCH (p:Person)-[r:rated {stars: 5}]->(f:Film {title: 'T4'}) RETURN count(p)";
    let expected = rows(&db, query);
    let between = "MATCH (p:Person)-[r:rated]->(f:Film) WHERE r.at >= 10 AND r.at < 20 RETURN p.n ORDER BY p.n";
    let expected_between = rows(&db, between);
    assert_eq!(expected_between.as_array().unwrap().len(), 10);
    let scanned = reads(&db, between);
    let created = rows(&db, "CREATE INDEX FOR ()-[r:rated]-() ON (r.at)");
    assert_eq!(created[0][0], "rated_at");
    assert_eq!(created[0][4], 2000);
    assert_eq!(created[0][6], "edges");
    assert!(
        plan(&db, between).contains("edge index"),
        "{}",
        plan(&db, between)
    );
    assert_eq!(rows(&db, between), expected_between);
    assert!(reads(&db, between) * 10 < scanned);
    // From the other end, and pointing in.
    let inward = "MATCH (f:Film)<-[r:rated]-(p:Person) WHERE r.at = 77 RETURN p.n, f.title";
    assert_eq!(rows(&db, inward), json!([[77, "T27"]]));
    assert_eq!(rows(&db, query), expected);

    // Kept on writes: changed, deleted, written by a plain create.
    run(&db, "MATCH ()-[r:rated {at: 77}]->() SET r.at = 7000");
    assert_eq!(rows(&db, inward), json!([]));
    assert_eq!(
        rows(&db, "MATCH (p)-[r:rated {at: 7000}]->() RETURN p.n"),
        json!([[77]])
    );
    run(&db, "MATCH ()-[r:rated {at: 7000}]->() DELETE r");
    assert_eq!(
        rows(&db, "MATCH ()-[r:rated {at: 7000}]->() RETURN count(r)"),
        json!([[0]])
    );

    // Unique, not null and typed on edges.
    run(
        &db,
        "CREATE CONSTRAINT FOR ()-[r:rated]-() REQUIRE r.at IS UNIQUE",
    );
    let clash = fail(
        &db,
        "MATCH (p:Person {n: 1}), (f:Film {title: 'T9'}) CREATE (p)-[:rated {at: 5, stars: 1}]->(f)",
    );
    assert!(clash.contains("must be unique"), "{clash}");
    run(
        &db,
        "CREATE CONSTRAINT FOR ()-[r:rated]-() REQUIRE r.stars IS NOT NULL",
    );
    run(
        &db,
        "CREATE CONSTRAINT FOR ()-[r:rated]-() REQUIRE r.stars IS :: INTEGER",
    );
    fail(
        &db,
        "MATCH (p:Person {n: 1}), (f:Film {title: 'T9'}) CREATE (p)-[:rated {at: 9001}]->(f)",
    );
    fail(
        &db,
        "MATCH (p:Person {n: 1}), (f:Film {title: 'T9'}) CREATE (p)-[:rated {at: 9001, stars: 'many'}]->(f)",
    );
    run(
        &db,
        "MATCH (p:Person {n: 1}), (f:Film {title: 'T9'}) CREATE (p)-[:rated {at: 9001, stars: 2}]->(f)",
    );
    let shown = rows(&db, "SHOW CONSTRAINTS");
    assert_eq!(
        shown
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row[5] == "edges")
            .count(),
        3,
        "{shown}"
    );
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    // A rebuild puts the edge entries back.
    db.write(|tx| tx.rebuild_indexes(NS)).unwrap();
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    assert_eq!(
        rows(&db, "MATCH (p)-[r:rated {at: 9001}]->() RETURN p.n"),
        json!([[1]])
    );
}

#[test]
fn indexes_over_several_labels() {
    let db = people();
    run(
        &db,
        "UNWIND range(0, 9) AS i CREATE (:Company {name: 'C' + toString(i)})",
    );
    run(&db, "MATCH (p:Person) SET p.name = 'P' + toString(p.n)");
    let created = rows(&db, "CREATE INDEX named FOR (n:Person|Company) ON (n.name)");
    assert_eq!(created[0][1], "Person|Company");
    assert_eq!(created[0][4], 2010);
    let company = "MATCH (c:Company {name: 'C3'}) RETURN c.name";
    assert!(
        plan(&db, company).contains("index named"),
        "{}",
        plan(&db, company)
    );
    assert_eq!(rows(&db, company), json!([["C3"]]));
    // The label still decides: a person's name does not make it a company.
    assert_eq!(
        rows(&db, "MATCH (c:Company {name: 'P3'}) RETURN count(c)"),
        json!([[0]])
    );
    assert_eq!(
        rows(&db, "MATCH (p:Person {name: 'P3'}) RETURN p.n"),
        json!([[3]])
    );
    // Unique across both labels.
    run(
        &db,
        "CREATE CONSTRAINT FOR (n:Person|Company) REQUIRE n.name IS UNIQUE",
    );
    fail(&db, "CREATE (:Company {name: 'P5'})");
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn syntax_errors_say_what_is_expected() {
    let db = Grafeno::in_memory().unwrap();
    for (query, expected) in [
        (
            "CREATE CONSTRAINT FOR (n:A|B) REQUIRE n.x IS NOT NULL",
            "one property of one label",
        ),
        (
            "CREATE CONSTRAINT FOR (n:A) REQUIRE (n.x, n.y) IS :: STRING",
            "one property of one label",
        ),
        (
            "CREATE FUZZY INDEX FOR (n:A) ON (n.x, n.y)",
            "one property of one node label",
        ),
        ("CREATE INDEX FOR ()-[:rated]-() ON (r.x)", "name the edge"),
        (
            "CREATE CONSTRAINT FOR (n:A) REQUIRE n.x IS SOMETHING",
            "UNIQUE, KEY, NOT NULL",
        ),
        (
            "CREATE INDEX FOR (n:A) ON (n.x, n.x)",
            "names a property twice",
        ),
    ] {
        let error = fail(&db, query);
        assert!(error.contains(expected), "{query}: {error}");
    }
}
