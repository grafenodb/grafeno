//! Phase 16.6: full-text indexes in GQ: declared over chosen properties
//! with weights and an analyzer, kept on writes, searched with the query
//! language, with fragments; and `db.search` with a mode.

use grafeno::Grafeno;
use grafeno_query::{Params, QueryResult, execute};
use serde_json::{Value as Json, json};

const NS: &str = "f";

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

/// The titles a search finds, best first.
fn titles(db: &Grafeno, search: &str) -> Vec<String> {
    let query = format!(
        "CALL db.searchIndex('notes', '{}', 20) YIELD node RETURN node.title",
        search.replace('\'', "\\'")
    );
    rows(db, &query)
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[0].as_str().unwrap().to_owned())
        .collect()
}

fn notes() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    for (title, body) in [
        (
            "Graph databases",
            "Storing graphs: nodes, edges and their properties.",
        ),
        (
            "Graphing tools",
            "Drawing charts and plotting data quickly.",
        ),
        (
            "Memory for agents",
            "An agent remembers facts in a graph of memories.",
        ),
        (
            "Cooking",
            "A recipe with rice, beans and a little graph paper.",
        ),
        (
            "Vector search",
            "Embeddings found by nearest neighbours, not words.",
        ),
        (
            "Graph memory",
            "Agents keep their memory as a graph database.",
        ),
    ] {
        run(
            &db,
            &format!("CREATE (:Note {{title: '{title}', body: '{body}'}})"),
        );
    }
    run(
        &db,
        "CREATE (:Draft {title: 'Graph draft', body: 'not indexed'})",
    );
    db
}

#[test]
fn a_fulltext_index_over_weighted_fields() {
    let db = notes();
    let created = rows(
        &db,
        "CREATE FULLTEXT INDEX notes FOR (n:Note) ON EACH [n.title, n.body]
         OPTIONS {analyzer: 'english', weights: {title: 3}}",
    );
    assert_eq!(created[0][0], "notes");
    assert_eq!(created[0][8], "fulltext");
    // A word in the title outranks the same word in the body; stems match
    // (`graphs`, `graphing` are `graph`); a label outside it is not found.
    let found = titles(&db, "graph");
    assert_eq!(found.len(), 5, "{found:?}");
    assert!(
        found[..3].iter().all(|t| t.starts_with("Graph")),
        "{found:?}"
    );
    assert!(!found.contains(&"Graph draft".to_owned()));
    // Phrases, required and excluded terms, AND, OR, fields.
    assert_eq!(
        titles(&db, "\"graph database\""),
        ["Graph databases", "Graph memory"]
    );
    assert_eq!(
        titles(&db, "+graph +memory -database"),
        ["Memory for agents"]
    );
    assert_eq!(titles(&db, "agent AND recipe"), Vec::<String>::new());
    assert_eq!(titles(&db, "recipe OR embeddings").len(), 2);
    assert_eq!(
        titles(&db, "title:memory"),
        ["Memory for agents", "Graph memory"]
    );
    assert_eq!(titles(&db, "body:\"graph paper\""), ["Cooking"]);
    // Prefixes, fuzzy terms, words near each other.
    assert_eq!(titles(&db, "embed*"), ["Vector search"]);
    assert_eq!(titles(&db, "recipy~1"), ["Cooking"]);
    assert!(titles(&db, "\"agents graph\"~3").contains(&"Memory for agents".to_owned()));
    assert!(!titles(&db, "\"agents graph\"").contains(&"Memory for agents".to_owned()));

    // Fragments: where it matched, in characters.
    let found = rows(
        &db,
        "CALL db.searchIndex('notes', 'recipe', {limit: 1}) YIELD node, fragments
         RETURN fragments",
    );
    let fragment = &found[0][0][0];
    assert_eq!(fragment["field"], "body");
    let text = fragment["text"].as_str().unwrap();
    let (start, end) = (
        fragment["matches"][0][0].as_u64().unwrap() as usize,
        fragment["matches"][0][1].as_u64().unwrap() as usize,
    );
    let word: String = text.chars().skip(start).take(end - start).collect();
    assert_eq!(word, "recipe");

    // Kept on writes.
    run(
        &db,
        "MATCH (n:Note {title: 'Cooking'}) SET n.body = 'Soup.'",
    );
    assert!(titles(&db, "recipe").is_empty());
    run(&db, "MATCH (d:Draft) SET d:Note");
    assert!(titles(&db, "draft").contains(&"Graph draft".to_owned()));
    run(&db, "MATCH (n:Note {title: 'Graph draft'}) DETACH DELETE n");
    assert!(titles(&db, "draft").is_empty());
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
    db.write(|tx| tx.rebuild_indexes(NS)).unwrap();
    assert_eq!(titles(&db, "\"graph database\"").len(), 2);

    // Listed, explained, dropped.
    let shown = rows(&db, "SHOW INDEXES");
    assert_eq!(shown[0][2], "title, body");
    let explained = rows(
        &db,
        "EXPLAIN CREATE FULLTEXT INDEX other FOR (n:Note) ON EACH [n.body]",
    );
    assert!(
        explained[0][2]
            .as_str()
            .unwrap()
            .contains("full-text index")
    );
    run(&db, "DROP INDEX notes");
    assert!(fail(&db, "CALL db.searchIndex('notes', 'graph')").contains("no index named"));
}

#[test]
fn search_modes_and_errors() {
    let db = notes();
    run(
        &db,
        "CREATE FULLTEXT INDEX notes FOR (n:Note) ON EACH [n.title, n.body]",
    );
    run(
        &db,
        "MATCH (n:Note) WITH n ORDER BY n.title
         WITH collect(n) AS all UNWIND range(0, size(all) - 1) AS i
         WITH all[i] AS n, i SET n.v = i",
    );
    db.write(|tx| {
        let read = db.begin_read()?;
        for node in read.nodes_with_label(NS, "Note")? {
            let v = read.node(NS, node)?.unwrap().props["v"].as_i64().unwrap() as f32;
            tx.set_embedding(NS, node, &[v.cos(), v.sin(), 1.0])?;
        }
        Ok(())
    })
    .unwrap();
    // Text through the index (its query language), vector, and both.
    let text = rows(
        &db,
        "CALL db.search('\"graph database\"', {index: 'notes'}) YIELD node RETURN node.title",
    );
    // The standard analyzer does not stem: `databases` is another word.
    assert_eq!(text, json!([["Graph memory"]]));
    let vector = rows(
        &db,
        "CALL db.search('', {mode: 'vector', embedding: [1.0, 0.0, 1.0], limit: 1})
         YIELD node RETURN node.v",
    );
    assert_eq!(vector, json!([[0]]));
    let hybrid = rows(
        &db,
        "CALL db.search('graph', {mode: 'hybrid', index: 'notes', embedding: [1.0, 0.0, 1.0], limit: 3})
         YIELD node, score RETURN node.title, score",
    );
    assert_eq!(hybrid.as_array().unwrap().len(), 3);
    let weighted = rows(
        &db,
        "CALL db.search('graph', {mode: 'hybrid', index: 'notes', embedding: [1.0, 0.0, 1.0],
                                  fusion: 'weighted', text_weight: 1.0, limit: 1})
         YIELD node RETURN node.title",
    );
    let best_text = titles(&db, "graph");
    assert_eq!(weighted[0][0], best_text[0].as_str());

    for (query, expected) in [
        (
            "CALL db.search('x', {mode: 'hybrid'})",
            "needs an embedding",
        ),
        ("CALL db.search('x', {mode: 'sideways'})", "unknown mode"),
        ("CALL db.search('x', {fusion: 'max'})", "unknown fusion"),
        ("CALL db.searchIndex('notes', '\"open')", "not closed"),
        ("CALL db.searchIndex('notes', '-only')", "not only terms"),
        (
            "CREATE FULLTEXT INDEX bad FOR (n:Note) ON EACH [n.title] OPTIONS {analyzer: 'latin'}",
            "unknown analyzer",
        ),
        (
            "CREATE FULLTEXT INDEX bad FOR (n:Note) ON EACH [n.title] OPTIONS {weights: {body: 2}}",
            "not on",
        ),
        (
            "CREATE FULLTEXT INDEX bad FOR ()-[r:x]-() ON EACH [r.title]",
            "node properties",
        ),
    ] {
        let error = fail(&db, query);
        assert!(error.contains(expected), "{query}: {error}");
    }
}

#[test]
fn spanish_and_simple_analyzers() {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (:Doc {t: 'Las canciones rápidamente'}), (:Doc {t: 'Una canción'}), (:Doc {t: 'El a b'})",
    );
    run(
        &db,
        "CREATE FULLTEXT INDEX es FOR (d:Doc) ON EACH [d.t] OPTIONS {analyzer: 'spanish'}",
    );
    run(
        &db,
        "CREATE FULLTEXT INDEX plain FOR (d:Doc) ON EACH [d.t] OPTIONS {analyzer: 'simple'}",
    );
    let count = |index: &str, query: &str| {
        rows(
            &db,
            &format!("CALL db.searchIndex('{index}', '{query}') YIELD node RETURN count(node)"),
        )[0][0]
            .as_i64()
            .unwrap()
    };
    assert_eq!(count("es", "CANCION"), 2, "plural and accents");
    assert_eq!(count("es", "rapida"), 1);
    assert_eq!(count("plain", "canciones"), 1, "no stemming");
    assert_eq!(count("plain", "el"), 1, "no stop words");
}
