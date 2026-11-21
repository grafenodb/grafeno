//! Fuzzing the lexer, parser, planner and executor: any text, however
//! broken, gives a result or an error, never a panic. Three sources: random
//! token sequences, valid queries with random edits, and arbitrary text.
//!
//! `PROPTEST_CASES=100000 cargo test --release -p grafeno-query --test fuzz`
//! runs longer.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::time::Duration;

use grafeno::{Grafeno, NewEdge, NewNode};
use grafeno_query::{Limits, Params, execute_with};
use proptest::prelude::*;

const NS: &str = "fuzz";

fn db() -> &'static Grafeno {
    static DB: OnceLock<Grafeno> = OnceLock::new();
    DB.get_or_init(build)
}

fn build() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        let mut ids = Vec::new();
        for i in 0..40u64 {
            let label = ["Person", "Company", "Topic"][i as usize % 3];
            ids.push(
                tx.create_node(
                    NS,
                    NewNode::new(label)
                        .prop("i", i)
                        .prop("name", format!("n{i}"))
                        .prop("tags", vec!["a", "b"]),
                )?,
            );
        }
        for (i, id) in ids.iter().enumerate() {
            tx.create_edge(NS, NewEdge::new(*id, "knows", ids[(i + 1) % ids.len()]))?;
            tx.create_edge(NS, NewEdge::new(*id, "likes", ids[(i * 7) % ids.len()]))?;
        }
        Ok(())
    })
    .unwrap();
    db
}

fn limits() -> Limits {
    Limits {
        timeout: Some(Duration::from_millis(300)),
        max_reads: Some(100_000),
        max_rows: Some(10_000),
        max_memory: Some(16 * 1024 * 1024),
        ..Limits::standard()
    }
}

/// Runs the text as a query (writes are rolled back by an error or kept;
/// either is fine) and fails on a panic.
fn survives(query: &str) -> Result<(), TestCaseError> {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let _ = execute_with(db(), NS, query, &Params::new(), &limits());
        let _ = execute_with(
            db(),
            NS,
            &format!("EXPLAIN {query}"),
            &Params::new(),
            &limits(),
        );
    }));
    prop_assert!(outcome.is_ok(), "panicked on {query:?}");
    Ok(())
}

const WORDS: &[&str] = &[
    "MATCH",
    "OPTIONAL",
    "WHERE",
    "RETURN",
    "WITH",
    "UNWIND",
    "AS",
    "CREATE",
    "MERGE",
    "SET",
    "REMOVE",
    "DELETE",
    "DETACH",
    "RETRACT",
    "HYPEREDGE",
    "EXACT",
    "FOREACH",
    "CALL",
    "YIELD",
    "ORDER",
    "BY",
    "DESC",
    "SKIP",
    "LIMIT",
    "DISTINCT",
    "UNION",
    "ALL",
    "AND",
    "OR",
    "XOR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "EXISTS",
    "STARTS",
    "ENDS",
    "CONTAINS",
    "VALID",
    "AT",
    "KNOWN",
    "EXPLAIN",
    "PROFILE",
    "INDEX",
    "FOR",
    "ON",
    "true",
    "false",
    "n",
    "m",
    "r",
    "x",
    "Person",
    "knows",
    "count",
    "collect",
    "size",
    "range",
    "reduce",
    "keys",
    "labels",
    "type",
    "algo.pageRank",
    "db.labels",
    "algo.shortestPath",
    "toString",
    "substring",
    "replace",
    "split",
    "datetime",
    "duration",
    "coalesce",
    "head",
    "abs",
    "0",
    "1",
    "-1",
    "42",
    "3.5",
    "1e309",
    "9223372036854775807",
    "-9223372036854775808",
    "'s'",
    "''",
    "\"q\"",
    "$p",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    ":",
    ",",
    ".",
    "..",
    "|",
    "-",
    "->",
    "<-",
    "--",
    "*",
    "+",
    "/",
    "%",
    "^",
    "=",
    "<>",
    "<",
    "<=",
    ">",
    ">=",
    "=~",
    ";",
    "`n`",
    "@id",
    "@valid_from",
    "@text",
    "//",
    "/*",
];

fn tokens() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(WORDS), 1..40).prop_map(|words| words.join(" "))
}

const SEEDS: &[&str] = &[
    "MATCH p = (a:Person)-[:knows*1..3]->(b) RETURN nodes(p), relationships(p), length(p)",
    "MATCH p = shortestPath((a:Person)-[:knows*]-(b:Topic)) RETURN p LIMIT 2",
    "MATCH (a:Person) CALL { WITH a MATCH (a)-->(b) RETURN count(b) AS c } RETURN a.i, c",
    "MATCH (a) RETURN a {.name, .*, k: COUNT { (a)-->() }}, [(a)-->(b) WHERE b.i > 1 | b.name]",
    "MATCH (a) WHERE a.name =~ 'n[0-9]+' RETURN reverse(a.name), left(a.name, 1), tail(a.tags)",
    "MATCH (a:Person {name: 'n1'})-[:knows]->(b) WHERE b.i > 3 RETURN a.name, b.name ORDER BY b.i LIMIT 5",
    "MATCH (a)-[r:knows*1..3]-(b) RETURN count(DISTINCT b)",
    "UNWIND range(1, 10) AS x WITH x WHERE x % 2 = 0 RETURN collect(x)",
    "MATCH (n) RETURN n.i, [x IN n.tags WHERE x <> 'a' | x + '!'], reduce(s = 0, y IN range(1, n.i) | s + y)",
    "MERGE (c:Company {name: 'x'}) ON CREATE SET c.new = true ON MATCH SET c.seen = 1 RETURN c",
    "CREATE (a:T {v: 1})-[:rel {w: 2}]->(b:T) SET a += {k: [1, 2]} RETURN a, b",
    "MATCH (n:Person) WHERE EXISTS { (n)-[:likes]->(:Topic) } RETURN CASE WHEN n.i > 5 THEN 'big' ELSE 'small' END",
    "MATCH (a {i: 0}), (b {i: 9}) CALL algo.shortestPath(a, b) YIELD nodes, cost RETURN size(nodes), cost",
    "CALL db.labels() YIELD label, count RETURN label ORDER BY count DESC",
    "MATCH (n) WITH n.i % 3 AS k, count(*) AS c RETURN k, c UNION MATCH (n) RETURN n.i AS k, 1 AS c",
    "RETURN datetime('2024-01-01T00:00:00Z') + duration('P1M'), substring('hello', 1, 3), split('a,b', ',')",
    "MATCH (n) WHERE n.name STARTS WITH 'n' AND NOT n.i IN [1, 2, 3] RETURN n.name SKIP 2 LIMIT 3",
    "FOREACH (x IN range(1, 3) | CREATE (:F {x: x}))",
    "MATCH (n:F) DETACH DELETE n",
    "MATCH (a)-[r]->(b) VALID AT '2024-06-01' RETURN type(r), r.@valid_from, a.@id",
];

/// A valid query with a few random edits: characters deleted, repeated,
/// swapped or replaced.
fn mutated() -> impl Strategy<Value = String> {
    (
        prop::sample::select(SEEDS),
        prop::collection::vec((any::<prop::sample::Index>(), 0u8..5, any::<char>()), 1..6),
    )
        .prop_map(|(seed, edits)| {
            let mut chars: Vec<char> = seed.chars().collect();
            for (at, edit, c) in edits {
                if chars.is_empty() {
                    break;
                }
                let at = at.index(chars.len());
                match edit {
                    0 => {
                        chars.remove(at);
                    }
                    1 => {
                        let c = chars[at];
                        chars.insert(at, c);
                    }
                    2 => {
                        let next = (at + 1) % chars.len();
                        chars.swap(at, next);
                    }
                    3 => chars[at] = c,
                    _ => {
                        let piece: Vec<char> = chars[at..].iter().take(12).copied().collect();
                        let end = (at + piece.len()).min(chars.len());
                        chars.splice(end..end, piece);
                    }
                }
            }
            chars.into_iter().collect()
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn random_tokens_never_panic(query in tokens()) {
        survives(&query)?;
    }

    #[test]
    fn edited_queries_never_panic(query in mutated()) {
        survives(&query)?;
    }

    #[test]
    fn arbitrary_text_never_panics(query in "\\PC{0,80}") {
        survives(&query)?;
    }
}

/// The seeds themselves run, so that edits start from working queries
/// (on a graph of their own: the fuzz tests change theirs).
#[test]
fn seeds_run() {
    let db = build();
    for seed in SEEDS {
        if let Err(error) = execute_with(&db, NS, seed, &Params::new(), &limits()) {
            panic!("{seed}: {}", error.message);
        }
    }
}
