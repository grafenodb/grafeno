//! Looking entities up as rows: `ORDER BY … LIMIT` over many nodes keeps
//! only the rows it returns, and reads an index in its order when one holds
//! the sorted property (after equal values for its first ones).

use grafeno::Grafeno;
use grafeno_query::{Limits, Params, QueryResult, execute, execute_with};
use serde_json::Value as Json;

const NS: &str = "e";

fn run(db: &Grafeno, query: &str) -> QueryResult {
    execute(db, NS, query, &Params::new()).unwrap_or_else(|e| panic!("{query}\n→ {e}"))
}

fn rows(db: &Grafeno, query: &str) -> Json {
    run(db, query).to_json()["rows"].clone()
}

/// The reads of a PROFILE, summed.
fn reads(db: &Grafeno, query: &str) -> u64 {
    run(db, &format!("PROFILE {query}"))
        .profile
        .unwrap()
        .iter()
        .filter_map(|step| step.reads)
        .sum()
}

fn details(db: &Grafeno, query: &str) -> String {
    run(db, &format!("PROFILE {query}"))
        .profile
        .unwrap()
        .iter()
        .map(|step| step.details.clone())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 3,000 people in five countries, names in no particular order, some
/// sharing a name.
fn people() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "UNWIND range(0, 2999) AS i
         CREATE (:Person {name: 'N' + toString((i * 7919) % 2500),
                          country: ['PE', 'AR', 'CL', 'MX', 'ES'][i % 5],
                          age: i % 90})",
    );
    db
}

/// The same query sorted the slow way: every row through a WITH first.
fn reference(db: &Grafeno, query: &str) -> Json {
    let slow = query.replacen(" RETURN ", " WITH p RETURN ", 1);
    assert_ne!(slow, query);
    rows(db, &slow)
}

#[test]
fn a_sort_with_a_limit_holds_only_the_rows_it_returns() {
    let db = people();
    // Holding every match to sort them went over max_rows.
    let limits = Limits {
        max_rows: Some(2000),
        ..Limits::none()
    };
    let query = "MATCH (p:Person) RETURN p.name, p.age ORDER BY p.name DESC, p.age LIMIT 7";
    let result =
        execute_with(&db, NS, query, &Params::new(), &limits).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(result.to_json()["rows"], reference(&db, query));
    // With SKIP, by an alias and over an expression.
    for query in [
        "MATCH (p:Person) RETURN p.name AS name ORDER BY name SKIP 40 LIMIT 5",
        "MATCH (p:Person) WHERE p.age > 10 RETURN p.name ORDER BY p.age * -1, p.name LIMIT 9",
        "MATCH (p:Person) RETURN p ORDER BY p.country, p.name SKIP 1500 LIMIT 50",
    ] {
        let found = execute_with(&db, NS, query, &Params::new(), &limits)
            .unwrap_or_else(|e| panic!("{query}: {e}"));
        assert_eq!(found.to_json()["rows"], reference(&db, query), "{query}");
    }
}

#[test]
fn a_sort_reads_an_index_in_order_and_stops() {
    let db = people();
    let query = "MATCH (p:Person) RETURN p.name ORDER BY p.name LIMIT 10";
    let before = reads(&db, query);
    run(&db, "CREATE INDEX FOR (p:Person) ON (p.name)");
    assert!(details(&db, query).contains("index Person_name in order"));
    let after = reads(&db, query);
    assert!(
        after < 100,
        "{after} reads with the index, {before} without"
    );
    for query in [
        query,
        "MATCH (p:Person) RETURN p.name ORDER BY p.name DESC LIMIT 10",
        "MATCH (p:Person) RETURN p.name, p.age ORDER BY p.name SKIP 1000 LIMIT 25",
        "MATCH (p:Person) WHERE p.age = 3 RETURN p.name ORDER BY p.name LIMIT 10",
        "MATCH (p:Person) RETURN p.name ORDER BY p.name DESC SKIP 2990 LIMIT 20",
    ] {
        assert_eq!(rows(&db, query), reference(&db, query), "{query}");
    }
}

#[test]
fn equal_values_first_then_the_sorted_property() {
    let db = people();
    run(&db, "CREATE INDEX FOR (p:Person) ON (p.country)");
    run(&db, "CREATE INDEX FOR (p:Person) ON (p.country, p.name)");
    let query = "MATCH (p:Person) WHERE p.country = 'PE' RETURN p.name ORDER BY p.name LIMIT 20";
    assert!(
        details(&db, query).contains("index Person_country_name in order"),
        "{}",
        details(&db, query)
    );
    assert!(reads(&db, query) < 100);
    for query in [
        query,
        "MATCH (p:Person {country: 'CL'}) RETURN p.name ORDER BY p.name DESC SKIP 5 LIMIT 20",
        "MATCH (p:Person {country: $c}) RETURN p.name ORDER BY p.name LIMIT 3",
        "MATCH (p:Person) WHERE p.country = 'XX' RETURN p.name ORDER BY p.name LIMIT 3",
    ] {
        let params: Params = [("c".to_owned(), grafeno_query::Value::Str("MX".into()))]
            .into_iter()
            .collect();
        let fast = execute(&db, NS, query, &params).unwrap().to_json()["rows"].clone();
        let slow = execute(
            &db,
            NS,
            &query.replacen(" RETURN ", " WITH p RETURN ", 1),
            &params,
        )
        .unwrap()
        .to_json()["rows"]
            .clone();
        assert_eq!(fast, slow, "{query}");
    }
}

#[test]
fn kinds_ties_and_nodes_without_a_value_sort_as_without_an_index() {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "UNWIND range(0, 99) AS i CREATE (:Thing {v: CASE i % 4
             WHEN 0 THEN toString(i % 7) WHEN 1 THEN i % 5 WHEN 2 THEN i % 2 = 0 ELSE (i % 5) * 1.0 END, i: i})",
    );
    run(&db, "CREATE INDEX FOR (t:Thing) ON (t.v)");
    let queries = [
        "MATCH (p:Thing) RETURN p.v, p.i ORDER BY p.v LIMIT 30",
        "MATCH (p:Thing) RETURN p.v, p.i ORDER BY p.v DESC LIMIT 30",
        "MATCH (p:Thing) RETURN p.v, p.i ORDER BY p.v SKIP 60 LIMIT 30",
    ];
    for query in queries {
        assert!(details(&db, query).contains("in order"), "{query}");
        assert_eq!(rows(&db, query), reference(&db, query), "{query}");
    }
    // A node without the value (or with a list) is not in the index: the
    // sort no longer reads it, and the nodes still sort as they should.
    run(&db, "CREATE (:Thing {i: 1000})");
    run(&db, "CREATE (:Thing {v: [1, 2], i: 1001})");
    for query in queries {
        assert!(!details(&db, query).contains("in order"), "{query}");
        assert_eq!(rows(&db, query), reference(&db, query), "{query}");
    }
    let first = rows(&db, "MATCH (p:Thing) RETURN p.i ORDER BY p.v LIMIT 1");
    assert_eq!(first[0][0], 1001);
    let first = rows(&db, "MATCH (p:Thing) RETURN p.i ORDER BY p.v DESC LIMIT 1");
    assert_eq!(first[0][0], 1000);
}

#[test]
fn a_composite_index_never_misses_nodes_without_its_later_properties() {
    let db = Grafeno::in_memory().unwrap();
    run(&db, "CREATE INDEX FOR (p:P) ON (p.last, p.first)");
    run(
        &db,
        "CREATE (:P {last: 'Ruiz'}), (:P {last: 'Ruiz', first: 'Ana'})",
    );
    let query = "MATCH (p:P) WHERE p.last = 'Ruiz' RETURN count(p)";
    assert_eq!(rows(&db, query), serde_json::json!([[2]]));
    assert!(!details(&db, query).contains("P_last_first"));
    // Both given: the index serves it.
    let both = "MATCH (p:P {last: 'Ruiz', first: 'Ana'}) RETURN count(p)";
    assert_eq!(rows(&db, both), serde_json::json!([[1]]));
    assert!(details(&db, both).contains("P_last_first"));
}
