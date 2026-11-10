//! The GQ of phase 12.1: path variables, shortestPath, CALL { }
//! subqueries, COUNT { }, pattern comprehensions, map projections, `=~`
//! and more list, string and math functions.

// Expected values rounded to six digits, not approximations of constants.
#![allow(clippy::approx_constant)]

mod common;

use grafeno::Grafeno;
use grafeno_query::{Params, QueryResult, execute};
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

fn fails(db: &Grafeno, query: &str) -> String {
    match execute(db, NS, query, &Params::new()) {
        Ok(_) => panic!("{query} should fail"),
        Err(error) => error.to_string(),
    }
}

/// Ana knows Bob and Carla; Bob knows Carla; Carla knows Dan.
fn people() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (ana:Person {name: 'Ana', age: 34, email: 'ana@example.org'}),
                (bob:Person {name: 'Bob', age: 28, email: 'bob@example'}),
                (carla:Person {name: 'Carla', age: 41}),
                (dan:Person {name: 'Dan', age: 19}),
                (acme:Company {name: 'Acme'}),
                (ana)-[:knows {since: 2020}]->(bob), (ana)-[:knows]->(carla),
                (bob)-[:knows]->(carla), (carla)-[:knows]->(dan),
                (ana)-[:works_at]->(acme), (bob)-[:works_at]->(acme)",
    );
    db
}

#[test]
fn path_variables_hold_nodes_and_edges_in_order() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            "MATCH p = (a:Person {name: 'Ana'})-[:knows]->(b)-[:knows]->(c)
             RETURN [n IN nodes(p) | n.name], length(p), [r IN relationships(p) | type(r)]
             ORDER BY [n IN nodes(p) | n.name]"
        ),
        json!([
            [["Ana", "Bob", "Carla"], 2, ["knows", "knows"]],
            [["Ana", "Carla", "Dan"], 2, ["knows", "knows"]]
        ])
    );
    // Written order, whichever end the match started from; against the
    // edges' direction too.
    assert_eq!(
        rows(
            &db,
            "MATCH p = (d {name: 'Dan'})<-[:knows]-(c)<-[:knows]-(b {name: 'Bob'})
             RETURN [n IN nodes(p) | n.name]"
        ),
        json!([[["Dan", "Carla", "Bob"]]])
    );
    // Variable length.
    assert_eq!(
        rows(
            &db,
            "MATCH p = (a {name: 'Ana'})-[:knows*..3]->(d {name: 'Dan'}) RETURN [n IN nodes(p) | n.name], length(p)"
        ),
        json!([[["Ana", "Carla", "Dan"], 2]]),
        "each end at its shortest distance: Dan is two hops away"
    );
    let path = run(&db, "MATCH p = (a {name: 'Ana'})-[:works_at]->(c) RETURN p").to_json();
    assert_eq!(path["rows"][0][0]["relationships"][0]["label"], "works_at");
    assert_eq!(path["rows"][0][0]["nodes"].as_array().unwrap().len(), 2);
}

#[test]
fn shortest_paths() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            "MATCH (a {name: 'Ana'}), (d {name: 'Dan'}), p = shortestPath((a)-[:knows*]-(d))
             RETURN [n IN nodes(p) | n.name]"
        ),
        json!([[["Ana", "Carla", "Dan"]]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH p = allShortestPaths((a {name: 'Bob'})-[*..4]->(d {name: 'Dan'})) RETURN length(p)"
        ),
        json!([[2]])
    );
    assert!(
        fails(&db, "MATCH p = shortestPath((a)-->(b)-->(c)) RETURN p").contains("one edge pattern")
    );
    assert!(fails(&db, "CREATE p = (a)-[:x]->(b)").contains("MATCH, not CREATE"));
}

#[test]
fn call_subqueries_run_once_per_row() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person)
             CALL { WITH p MATCH (p)-[:knows]->(f) RETURN count(f) AS friends }
             RETURN p.name, friends ORDER BY p.name"
        ),
        json!([["Ana", 2], ["Bob", 1], ["Carla", 1], ["Dan", 0]])
    );
    // Several rows per outer row; an outer row without any is dropped.
    assert_eq!(
        rows(
            &db,
            "MATCH (c:Company)
             CALL { MATCH (p)-[:works_at]->(c) RETURN p.name AS who ORDER BY who }
             RETURN c.name, who"
        ),
        json!([["Acme", "Ana"], ["Acme", "Bob"]])
    );
    // A subquery that only writes keeps the rows.
    let result = run(
        &db,
        "MATCH (p:Person) WHERE p.age > 30 CALL { WITH p SET p.senior = true } RETURN count(p)",
    );
    assert_eq!(result.to_json()["rows"], json!([[2]]));
    assert_eq!(
        rows(
            &db,
            "MATCH (p {senior: true}) RETURN p.name ORDER BY p.name"
        ),
        json!([["Ana"], ["Carla"]])
    );
    assert!(
        fails(&db, "MATCH (p) CALL { WITH p RETURN p.name AS p } RETURN p")
            .contains("already a variable")
    );
    assert!(fails(&db, "CALL { MATCH (n) } RETURN 1").contains("ends with RETURN"));
}

#[test]
fn count_subqueries_and_pattern_comprehensions() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) RETURN p.name, COUNT { (p)-[:knows]->() } AS out ORDER BY p.name"
        ),
        json!([["Ana", 2], ["Bob", 1], ["Carla", 1], ["Dan", 0]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (p:Person) WHERE COUNT { MATCH (p)-[:knows]->(f) WHERE f.age > 30 } = 1
             RETURN p.name ORDER BY p.name"
        ),
        json!([["Ana"], ["Bob"]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (a {name: 'Ana'})
             RETURN [(a)-[r:knows]->(f) WHERE f.age > 30 | f.name + coalesce(' since ' + r.since, '')],
                    [(a)-[:knows]->(f) | f.name]"
        ),
        json!([[["Carla"], ["Bob", "Carla"]]])
    );
    assert_eq!(
        rows(
            &db,
            "MATCH (a {name: 'Ana'}) RETURN size([p = (a)-[:knows*1..2]->() | length(p)])"
        ),
        json!([[3]])
    );
}

#[test]
fn map_projections() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            "MATCH (p {name: 'Bob'}) WITH p, 'x' AS tag
             RETURN p {.name, .age, tag, friends: COUNT { (p)-->(:Person) }}"
        ),
        json!([[{ "name": "Bob", "age": 28, "tag": "x", "friends": 1 }]])
    );
    let all = rows(&db, "MATCH (p {name: 'Dan'}) RETURN p {.*, extra: 1}");
    assert_eq!(all, json!([[{ "name": "Dan", "age": 19, "extra": 1 }]]));
    assert_eq!(
        rows(&db, "WITH {a: 1, b: 2} AS m RETURN m {.a, c: 3}"),
        json!([[{ "a": 1, "c": 3 }]])
    );
    assert_eq!(
        rows(&db, "OPTIONAL MATCH (n:Nothing) RETURN n {.name}"),
        json!([[null]])
    );
}

#[test]
fn regular_expressions_match_the_whole_string() {
    let db = people();
    assert_eq!(
        rows(
            &db,
            r"MATCH (p:Person) WHERE p.email =~ '.*@example\\.org' RETURN p.name"
        ),
        json!([["Ana"]])
    );
    assert_eq!(
        rows(
            &db,
            "RETURN 'abc' =~ 'b', 'abc' =~ 'a.c', 'ABC' =~ '(?i)abc', null =~ 'x'"
        ),
        json!([[false, true, true, null]])
    );
    assert!(fails(&db, "RETURN 'a' =~ '('").contains("invalid regular expression"));
    // A huge pattern is refused rather than compiled.
    let huge = format!("RETURN 'a' =~ '{}'", "(a|b){1000}".repeat(20));
    assert!(fails(&db, &huge).contains("invalid regular expression"));
}

#[test]
fn more_functions() {
    let db = Grafeno::in_memory().unwrap();
    assert_eq!(
        rows(
            &db,
            "RETURN reverse('abc'), reverse([1, 2, 3]), tail([1, 2, 3]), isEmpty([]), isEmpty('x'),
                    left('hello', 2), right('hello', 3), ltrim('  a '), rtrim(' a  '),
                    char_length('ñandú')"
        ),
        json!([[
            "cba",
            [3, 2, 1],
            [2, 3],
            true,
            false,
            "he",
            "llo",
            "a ",
            " a",
            5
        ]])
    );
    assert_eq!(
        rows(
            &db,
            "RETURN toBoolean('TRUE'), toBoolean('x'), toBoolean(0), toIntegerOrNull('x'),
                    toIntegerOrNull('42'), toFloatOrNull([1]), toStringOrNull(1.5),
                    toIntegerList(['1', 'x', null, 2.7]), toBooleanList(['true', 1]),
                    nullIf(1, 1), nullIf(1, 2), nullIf(1, null)"
        ),
        json!([[
            true,
            null,
            false,
            null,
            42,
            null,
            "1.5",
            [1, null, null, 2],
            [true, true],
            null,
            1,
            1
        ]])
    );
    let math = rows(
        &db,
        "RETURN round(degrees(pi()), 6), round(radians(180), 6), round(atan2(1, 1), 6),
                round(asin(1), 6), round(acos(1), 6), round(haversin(0), 6), isNaN(0.0 / 0.0),
                rand() < 1.0, size(randomUUID())",
    );
    assert_eq!(
        math,
        json!([[
            180.0, 3.141593, 0.785398, 1.570796, 0.0, 0.0, true, true, 36
        ]])
    );
}
