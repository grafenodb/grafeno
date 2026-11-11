//! Phase 8.5: graph algorithms through `CALL algo.*` and the `algo.*`
//! functions.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use grafeno::Grafeno;
use grafeno_query::{Limits, Params, QueryResult, execute, execute_with};
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

/// A road network on a grid (x, y in km; latitude and longitude about
/// half a km apart): A - B - C in a row, D beside B, with a slow direct
/// road A -> C. Straight lines are never longer than the roads.
fn roads() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (a:City {name: 'A', x: 0, y: 0, lat: -12.0, lon: -77.0}),
                (b:City {name: 'B', x: 1, y: 0, lat: -12.0, lon: -76.995}),
                (c:City {name: 'C', x: 2, y: 0, lat: -12.0, lon: -76.99}),
                (d:City {name: 'D', x: 1, y: 1, lat: -11.995, lon: -76.995}),
                (a)-[:road {km: 1}]->(b), (b)-[:road {km: 1}]->(c),
                (a)-[:road {km: 5}]->(c), (a)-[:road {km: 2}]->(d),
                (d)-[:road {km: 1}]->(c)",
    );
    db
}

#[test]
fn paths() {
    let db = roads();
    let ends = "MATCH (a:City {name: 'A'}), (c:City {name: 'C'})";
    let path = rows(
        &db,
        &format!(
            "{ends} CALL algo.shortestPath(a, c, {{weight: 'km'}}) YIELD nodes, edges, cost
             RETURN [n IN nodes | n.name], size(edges), cost"
        ),
    );
    assert_eq!(path, json!([[["A", "B", "C"], 2, 2.0]]));
    // Without a weight: the fewest hops, the direct road.
    let hops = rows(
        &db,
        &format!(
            "{ends} CALL algo.shortestPath(a, c) YIELD nodes, cost RETURN [n IN nodes | n.name], cost"
        ),
    );
    assert_eq!(hops, json!([[["A", "C"], 1.0]]));
    // Against the direction there is no path, unless it is ignored.
    assert_eq!(
        rows(
            &db,
            &format!("{ends} CALL algo.shortestPath(c, a) YIELD cost RETURN cost")
        ),
        json!([])
    );
    assert_eq!(
        rows(
            &db,
            &format!(
                "{ends} CALL algo.shortestPath(c, a, {{weight: 'km', direction: 'both'}}) YIELD cost RETURN cost"
            )
        ),
        json!([[2.0]])
    );

    for coordinates in ["x: 'x', y: 'y'", "latitude: 'lat', longitude: 'lon'"] {
        let found = rows(
            &db,
            &format!(
                "{ends} CALL algo.astar(a, c, {{weight: 'km', {coordinates}}}) YIELD nodes, cost
                 RETURN [n IN nodes | n.name], cost"
            ),
        );
        assert_eq!(found, json!([[["A", "B", "C"], 2.0]]), "{coordinates}");
    }
    assert!(
        fail(
            &db,
            &format!("{ends} CALL algo.astar(a, c) YIELD cost RETURN cost")
        )
        .contains("coordinate")
    );

    let ranked = rows(
        &db,
        &format!(
            "{ends} CALL algo.kShortestPaths(a, c, 5, {{weight: 'km'}}) YIELD rank, nodes, cost
             RETURN rank, [n IN nodes | n.name], cost"
        ),
    );
    assert_eq!(
        ranked,
        json!([
            [1, ["A", "B", "C"], 2.0],
            [2, ["A", "D", "C"], 3.0],
            [3, ["A", "C"], 5.0]
        ])
    );

    // Two cheapest paths of 2 hops each.
    let all = rows(
        &db,
        &format!(
            "{ends} CALL algo.allShortestPaths(a, c, {{direction: 'out', edgeLabels: 'road'}}) YIELD nodes
             RETURN [n IN nodes | n.name]"
        ),
    );
    assert_eq!(all, json!([[["A", "C"]]]));
    let all = rows(
        &db,
        "MATCH (a:City {name: 'A'}), (c:City {name: 'C'}) WITH a, c
         CALL algo.allShortestPaths(a, c, {weight: 'km', direction: 'both'}) YIELD nodes, cost
         RETURN [n IN nodes | n.name] AS route, cost ORDER BY route",
    );
    assert_eq!(all, json!([[["A", "B", "C"], 2.0]]));

    let distances = rows(
        &db,
        "MATCH (a:City {name: 'A'}) CALL algo.distances(a, {weight: 'km'}) YIELD node, cost
         RETURN node.name, cost",
    );
    assert_eq!(
        distances,
        json!([["A", 0.0], ["B", 1.0], ["C", 2.0], ["D", 2.0]])
    );
    // By id, as a string.
    let id = rows(&db, "MATCH (a:City {name: 'A'}) RETURN a.@id")[0][0].clone();
    let mut params = Params::new();
    params.insert(
        "id".into(),
        grafeno_query::Value::Str(id.as_str().unwrap().into()),
    );
    let by_id = execute(
        &db,
        NS,
        "CALL algo.distances($id) YIELD node RETURN count(node)",
        &params,
    )
    .unwrap();
    assert_eq!(by_id.to_json()["rows"], json!([[4]]));
}

/// Two triangles joined by one edge, among people.
fn people() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (ana:Person {name: 'Ana'}), (bob:Person {name: 'Bob'}), (cy:Person {name: 'Cy'}),
                (dan:Person {name: 'Dan'}), (eve:Person {name: 'Eve'}), (fay:Person {name: 'Fay'}),
                (ana)-[:knows]->(bob), (bob)-[:knows]->(cy), (cy)-[:knows]->(ana),
                (dan)-[:knows]->(eve), (eve)-[:knows]->(fay), (fay)-[:knows]->(dan),
                (cy)-[:knows]->(dan),
                (ana)-[:likes]->(:Topic {name: 'Rust'})",
    );
    db
}

#[test]
fn centrality() {
    let db = people();
    let degree = rows(
        &db,
        "CALL algo.degree({labels: 'Person', edgeLabels: 'knows'}) YIELD node, score
         RETURN node.name, score LIMIT 2",
    );
    assert_eq!(degree, json!([["Cy", 3.0], ["Dan", 3.0]]));
    // `top` keeps the best rows only, in the same order.
    let top = rows(
        &db,
        "CALL algo.degree({labels: 'Person', edgeLabels: 'knows', top: 2}) YIELD node, score
         RETURN node.name, score",
    );
    assert_eq!(top, degree);
    assert_eq!(
        rows(
            &db,
            "CALL algo.pageRank({top: 0}) YIELD node RETURN count(node)"
        ),
        json!([[0]])
    );

    let rank = rows(
        &db,
        "CALL algo.pageRank({labels: 'Person', direction: 'both', iterations: 50}) YIELD node, score
         RETURN node.name, round(score * 1000) / 1000.0",
    );
    let top: Vec<&str> = rank.as_array().unwrap()[..2]
        .iter()
        .map(|row| row[0].as_str().unwrap())
        .collect();
    assert_eq!(top, ["Cy", "Dan"]);
    let total = rows(
        &db,
        "CALL algo.pageRank() YIELD score RETURN round(sum(score) * 1e6) / 1e6",
    );
    assert_eq!(total, json!([[1.0]]));

    // The bridge carries every path between the triangles.
    let between = rows(
        &db,
        "CALL algo.betweenness({labels: 'Person', direction: 'both'}) YIELD node, score
         RETURN node.name, score LIMIT 2",
    );
    assert_eq!(between, json!([["Cy", 6.0], ["Dan", 6.0]]));
    let close = rows(
        &db,
        "CALL algo.closeness({labels: 'Person', direction: 'both', harmonic: true}) YIELD node
         RETURN node.name LIMIT 2",
    );
    assert_eq!(close, json!([["Cy"], ["Dan"]]));

    // Write mode: SET what the procedure yields.
    let stats = run(
        &db,
        "CALL algo.pageRank({direction: 'both'}) YIELD node, score SET node.rank = score",
    );
    assert_eq!(stats.stats.properties_set, 7);
    assert_eq!(
        rows(&db, "MATCH (p:Person) WHERE p.rank IS NULL RETURN count(p)"),
        json!([[0]])
    );
}

#[test]
fn communities() {
    let db = people();
    let components = rows(
        &db,
        "CALL algo.wcc() YIELD component RETURN count(DISTINCT component)",
    );
    assert_eq!(components, json!([[1]]));
    let split = rows(
        &db,
        "CALL algo.wcc({edgeLabels: 'likes'}) YIELD component RETURN count(DISTINCT component)",
    );
    assert_eq!(split, json!([[6]]), "Ana with Rust, five people alone");
    let strong = rows(
        &db,
        "CALL algo.scc({labels: 'Person'}) YIELD node, component
         RETURN component, collect(node.name) AS members ORDER BY size(members) DESC, component",
    );
    assert_eq!(strong.as_array().unwrap().len(), 2, "two directed cycles");

    let louvain = rows(
        &db,
        "CALL algo.louvain({labels: 'Person'}) YIELD node, community, modularity
         RETURN community, collect(node.name), round(modularity * 1000) / 1000.0 ORDER BY community",
    );
    assert_eq!(
        louvain,
        json!([
            [0, ["Ana", "Bob", "Cy"], 0.357],
            [1, ["Dan", "Eve", "Fay"], 0.357]
        ])
    );
    let propagated = rows(
        &db,
        "CALL algo.labelPropagation({labels: 'Person'}) YIELD node, community
         WITH community, collect(node.name) AS members RETURN size(members) ORDER BY size(members)",
    );
    assert!(
        propagated == json!([[3], [3]]) || propagated == json!([[6]]),
        "{propagated}"
    );
    let triangles = rows(
        &db,
        "CALL algo.triangles({labels: 'Person'}) YIELD node, triangles, coefficient
         RETURN node.name, triangles, round(coefficient * 1000) / 1000.0 ORDER BY node.name",
    );
    assert_eq!(triangles[0], json!(["Ana", 1, 1.0]));
    assert_eq!(triangles[2], json!(["Cy", 1, 0.333]));
}

#[test]
fn similarity_and_link_prediction() {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (ana:Person {name: 'Ana'}), (bob:Person {name: 'Bob'}), (cy:Person {name: 'Cy'}),
                (rust:Topic {name: 'Rust'}), (go:Topic {name: 'Go'}), (sql:Topic {name: 'SQL'}),
                (ana)-[:likes]->(rust), (ana)-[:likes]->(go),
                (bob)-[:likes]->(rust), (bob)-[:likes]->(go), (bob)-[:likes]->(sql),
                (cy)-[:likes]->(sql)",
    );
    let similar = rows(
        &db,
        "CALL algo.nodeSimilarity({topK: 1}) YIELD node, other, score
         RETURN node.name, other.name, round(score * 1000) / 1000.0 ORDER BY node.name",
    );
    assert_eq!(
        similar,
        json!([
            ["Ana", "Bob", 0.667],
            ["Bob", "Ana", 0.667],
            ["Cy", "Bob", 0.333]
        ])
    );
    let one = rows(
        &db,
        "MATCH (c:Person {name: 'Cy'}) CALL algo.nodeSimilarity({node: c, metric: 'overlap'})
         YIELD other, score RETURN other.name, score",
    );
    assert_eq!(one, json!([["Bob", 1.0]]));

    let predicted = rows(
        &db,
        "MATCH (c:Person {name: 'Cy'}) CALL algo.linkPrediction(c, {metric: 'commonNeighbors'})
         YIELD other, score RETURN other.name, score",
    );
    assert_eq!(
        predicted,
        json!([["Bob", 1.0]]),
        "Bob is two hops away, through SQL"
    );
    let pair = rows(
        &db,
        "MATCH (a:Person {name: 'Ana'}), (b:Person {name: 'Bob'})
         RETURN algo.commonNeighbors(a, b), algo.jaccard(a, b), algo.overlap(a, b),
                algo.preferentialAttachment(a, b), algo.totalNeighbors(a, b),
                round(algo.adamicAdar(a, b) * 1000) / 1000.0, algo.resourceAllocation(a, b),
                algo.cosine([1, 0], [1, 1]) > 0.7",
    );
    // Rust and Go each have two neighbors: 1 / ln 2 each.
    assert_eq!(
        pair,
        json!([[2.0, 0.6666666666666666, 1.0, 6.0, 3.0, 2.885, 1.0, true]])
    );
}

#[test]
fn embeddings() {
    let db = people();
    run(
        &db,
        "CALL algo.fastRP({labels: 'Person', dimension: 16}) YIELD node, embedding
         SET node.@embedding = embedding",
    );
    assert_eq!(
        rows(&db, "MATCH (p:Person) RETURN size(p.@embedding) LIMIT 1"),
        json!([[16]])
    );
    // Nearest by embedding: Ana's neighbors in her own triangle.
    let near = rows(
        &db,
        "MATCH (a:Person {name: 'Ana'}) CALL algo.knn({node: a, topK: 2}) YIELD other
         RETURN other.name ORDER BY other.name",
    );
    assert_eq!(near, json!([["Bob"], ["Cy"]]));
    let all = rows(
        &db,
        "CALL algo.knn({topK: 1}) YIELD node RETURN count(node)",
    );
    assert_eq!(all, json!([[6]]));
}

#[test]
fn settings_are_checked() {
    let db = people();
    assert!(
        fail(
            &db,
            "CALL algo.pageRank({dampng: 0.5}) YIELD score RETURN score"
        )
        .contains("no setting dampng")
    );
    assert!(
        fail(
            &db,
            "CALL algo.pageRank({damping: 1.5}) YIELD score RETURN score"
        )
        .contains("damping")
    );
    assert!(
        fail(
            &db,
            "CALL algo.degree({direction: 'up'}) YIELD score RETURN score"
        )
        .contains("direction")
    );
    assert!(fail(&db, "CALL algo.pageRank(3) YIELD score RETURN score").contains("map"));
    assert!(
        fail(
            &db,
            "MATCH (t:Topic) CALL algo.distances(t, {labels: 'Person'}) YIELD cost RETURN cost"
        )
        .contains("not in the projection")
    );
    assert!(
        fail(&db, "CALL algo.shortestPath() YIELD cost RETURN cost")
            .contains("takes source, target")
    );
    assert!(
        fail(
            &db,
            "CALL algo.nodeSimilarity({metric: 'euclid'}) YIELD score RETURN score"
        )
        .contains("metric")
    );
    let listed = rows(
        &db,
        "CALL dbms.procedures() YIELD name WHERE name STARTS WITH 'algo.' RETURN count(name)",
    );
    assert_eq!(listed, json!([[40]]));
}

#[test]
fn a_cancelled_query_stops_the_algorithm() {
    let db = people();
    let cancel = Arc::new(AtomicBool::new(true));
    let limits = Limits {
        cancel: Some(cancel),
        ..Limits::none()
    };
    let error = execute_with(
        &db,
        NS,
        "CALL algo.pageRank() YIELD score RETURN score",
        &Params::new(),
        &limits,
    )
    .unwrap_err();
    assert!(error.limit, "{error}");
}
