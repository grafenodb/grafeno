//! Phase 16.8: analysis and simulation algorithms in GQ, against results
//! known on small graphs, and their modes (rows, statistics, writing back).

use grafeno::Grafeno;
use grafeno_query::{Params, QueryResult, execute, requirements};
use serde_json::{Value as Json, json};

const NS: &str = "g";

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

/// A triangle a-b-c, a tail c-d-e, and a square f-g-h-i apart.
fn shapes() -> Grafeno {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (a:P {n: 'a'}), (b:P {n: 'b'}), (c:P {n: 'c'}), (d:P {n: 'd'}), (e:P {n: 'e'}),
                (f:P {n: 'f'}), (g:P {n: 'g'}), (h:P {n: 'h'}), (i:P {n: 'i'}),
                (a)-[:L {w: 1}]->(b), (b)-[:L {w: 2}]->(c), (c)-[:L {w: 5}]->(a),
                (c)-[:L {w: 1}]->(d), (d)-[:L {w: 1}]->(e),
                (f)-[:L {w: 1}]->(g), (g)-[:L {w: 1}]->(h), (h)-[:L {w: 1}]->(i), (i)-[:L {w: 3}]->(f)",
    );
    db
}

fn names(value: &Json) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[0].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn analysis() {
    let db = shapes();
    let cores = rows(
        &db,
        "CALL algo.kCore() YIELD node, core RETURN node.n, core ORDER BY node.n",
    );
    assert_eq!(
        cores,
        json!([
            ["a", 2],
            ["b", 2],
            ["c", 2],
            ["d", 1],
            ["e", 1],
            ["f", 2],
            ["g", 2],
            ["h", 2],
            ["i", 2]
        ])
    );
    let points = rows(
        &db,
        "CALL algo.articulationPoints() YIELD node RETURN node.n ORDER BY node.n",
    );
    assert_eq!(names(&points), ["c", "d"]);
    let bridges = rows(
        &db,
        "CALL algo.bridges() YIELD edge RETURN startNode(edge).n + endNode(edge).n AS e ORDER BY e",
    );
    assert_eq!(names(&bridges), ["cd", "de"]);
    // Spanning forest: two trees, 4 + 3 edges, the cheapest.
    let tree = rows(
        &db,
        "CALL algo.spanningTree({weight: 'w'}) YIELD weight RETURN count(*), sum(weight)",
    );
    assert_eq!(tree, json!([[7, 8.0]]));
    let communities = rows(
        &db,
        "CALL algo.leiden() YIELD node, community WITH community, collect(node.n) AS members
         RETURN size(members) ORDER BY size(members) DESC",
    );
    assert!(communities.as_array().unwrap().len() >= 2);
    let clustering = rows(
        &db,
        "CALL algo.clustering() YIELD node, coefficient WHERE node.n = 'a' RETURN coefficient",
    );
    assert_eq!(clustering, json!([[1.0]]));
    // a and the triangle are central; the tail is less so.
    let eigen = rows(
        &db,
        "CALL algo.eigenvector({direction: 'both', labels: 'P'}) YIELD node RETURN node.n LIMIT 1",
    );
    assert_eq!(eigen[0][0], "c");
    let harmonic = rows(
        &db,
        "CALL algo.harmonic({direction: 'both', top: 1}) YIELD node RETURN node.n",
    );
    assert_eq!(harmonic[0][0], "c");
    let hits = rows(
        &db,
        "CALL algo.hits() YIELD node, hub, authority RETURN node.n, hub >= 0, authority >= 0 LIMIT 1",
    );
    assert_eq!(hits[0][1], true);
    // Order and cycles.
    assert!(fail(&db, "CALL algo.topologicalOrder()").contains("cycle"));
    let cycle = rows(&db, "CALL algo.cycle() YIELD nodes RETURN size(nodes)");
    assert!(cycle[0][0].as_i64().unwrap() >= 3);
    let dag = Grafeno::in_memory().unwrap();
    run(
        &dag,
        "CREATE (a:T {n: 'a'})-[:Then]->(b:T {n: 'b'})-[:Then]->(c:T {n: 'c'}), (a)-[:Then]->(c)",
    );
    assert_eq!(
        names(&rows(
            &dag,
            "CALL algo.topologicalOrder() YIELD node RETURN node.n ORDER BY node.n"
        )),
        ["a", "b", "c"]
    );
    assert_eq!(
        rows(
            &dag,
            "CALL algo.topologicalOrder() YIELD node, position WHERE node.n = 'c' RETURN position"
        ),
        json!([[2]])
    );
    assert_eq!(
        rows(&dag, "CALL algo.cycle() YIELD nodes RETURN nodes"),
        json!([])
    );
    // Walks repeat for a seed.
    let walk = "MATCH (a:P {n: 'a'}) CALL algo.randomWalks(a, {length: 5, walks: 3, seed: 9})
                YIELD walk RETURN [n IN walk | n.n]";
    assert_eq!(rows(&db, walk), rows(&db, walk));
    assert_eq!(rows(&db, walk).as_array().unwrap().len(), 3);
    // Conductance of groups: the square is closed, the triangle's tail
    // leaves it.
    run(
        &db,
        "MATCH (n:P) WHERE n.n IN ['f', 'g', 'h', 'i'] SET n.community = 2",
    );
    run(
        &db,
        "MATCH (n:P) WHERE n.n IN ['a', 'b', 'c'] SET n.community = 1",
    );
    let conductance = rows(
        &db,
        "CALL algo.conductance() YIELD community, conductance RETURN community, conductance",
    );
    assert_eq!(conductance[1], json!([2, 0.0]));
    assert!(conductance[0][1].as_f64().unwrap() > 0.0);
}

#[test]
fn flows_and_matching() {
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "CREATE (s:N {n: 's'}), (a:N {n: 'a'}), (b:N {n: 'b'}), (t:N {n: 't'}),
                (s)-[:Pipe {cap: 3, cost: 1}]->(a), (s)-[:Pipe {cap: 2, cost: 4}]->(b),
                (a)-[:Pipe {cap: 1, cost: 1}]->(b), (a)-[:Pipe {cap: 2, cost: 1}]->(t),
                (b)-[:Pipe {cap: 3, cost: 1}]->(t)",
    );
    let flow = rows(
        &db,
        "MATCH (s:N {n: 's'}), (t:N {n: 't'}) CALL algo.maxFlow(s, t, {weight: 'cap'})
         YIELD value, cut RETURN max(value), count(CASE WHEN cut THEN 1 END)",
    );
    assert_eq!(flow, json!([[5.0, 2]]));
    run(&db, "MATCH (n:N {n: 's'}) SET n.supply = 3");
    run(&db, "MATCH (n:N {n: 't'}) SET n.supply = -3");
    let cheap = rows(
        &db,
        "CALL algo.minCostFlow({supply: 'supply', capacity: 'cap', cost: 'cost'})
         YIELD value, cost, unmet RETURN max(value), max(cost), max(unmet)",
    );
    // 2 by s-a-t (cost 2 each), 1 by s-a-b-t (3): 7.
    assert_eq!(cheap, json!([[3.0, 7.0, 0.0]]));

    let jobs = Grafeno::in_memory().unwrap();
    run(
        &jobs,
        "CREATE (x:Agent {n: 'x'}), (y:Agent {n: 'y'}), (z:Agent {n: 'z'}),
                (p:Task {n: 'p'}), (q:Task {n: 'q'}), (r:Task {n: 'r'}),
                (x)-[:Can {cost: 4}]->(p), (x)-[:Can {cost: 1}]->(q), (x)-[:Can {cost: 3}]->(r),
                (y)-[:Can {cost: 2}]->(p), (y)-[:Can {cost: 0}]->(q), (y)-[:Can {cost: 5}]->(r),
                (z)-[:Can {cost: 3}]->(p), (z)-[:Can {cost: 2}]->(q), (z)-[:Can {cost: 2}]->(r)",
    );
    let assigned = rows(
        &jobs,
        "CALL algo.assignment({agents: 'Agent', tasks: 'Task', weight: 'cost'})
         YIELD agent, task, cost RETURN agent.n + task.n, cost ORDER BY agent.n",
    );
    assert_eq!(assigned, json!([["xq", 1.0], ["yp", 2.0], ["zr", 2.0]]));
    let matched = rows(
        &jobs,
        "CALL algo.bipartiteMatching({left: 'Agent'}) YIELD left RETURN count(left)",
    );
    assert_eq!(matched, json!([[3]]));
}

#[test]
fn simulation() {
    // A corridor of five rooms, with water (the goal) at both ends and a
    // wall in the middle room.
    let db = Grafeno::in_memory().unwrap();
    run(
        &db,
        "UNWIND range(0, 4) AS i CREATE (:Room {i: i, open: i <> 2, heat: CASE WHEN i = 0 THEN 10.0 ELSE 0.0 END})",
    );
    run(
        &db,
        "MATCH (a:Room), (b:Room) WHERE b.i = a.i + 1 CREATE (a)-[:Door {len: 1}]->(b)",
    );
    let field = rows(
        &db,
        "MATCH (s:Room) WHERE s.i IN [0, 4] WITH collect(s) AS goals
         CALL algo.distanceField(goals, {weight: 'len'}) YIELD node, cost, nearest
         RETURN node.i, cost, nearest.i ORDER BY node.i",
    );
    assert_eq!(
        field,
        json!([
            [0, 0.0, 0],
            [1, 1.0, 0],
            [2, 2.0, 0],
            [3, 1.0, 4],
            [4, 0.0, 4]
        ])
    );
    let map = rows(
        &db,
        "MATCH (s:Room {i: 0}) CALL algo.influence(s, {decay: 0.5, threshold: 0.2})
         YIELD node, influence RETURN node.i, influence ORDER BY node.i",
    );
    assert_eq!(map, json!([[0, 1.0], [1, 0.5], [2, 0.25]]));
    let region = rows(
        &db,
        "MATCH (s:Room {i: 0}) CALL algo.floodFill(s, {where: {open: true}})
         YIELD node, steps RETURN node.i, steps ORDER BY node.i",
    );
    assert_eq!(region, json!([[0, 0], [1, 1]]));
    let heat = rows(
        &db,
        "CALL algo.diffusion({amount: 'heat', rate: 0.5, steps: 20}) YIELD amount
         RETURN round(sum(amount) * 1000) / 1000, min(amount) > 0",
    );
    assert_eq!(heat, json!([[10.0, true]]));
    // Adoption spreading along the corridor from room 0.
    run(&db, "MATCH (r:Room) SET r.on = r.i = 0");
    let spread = rows(
        &db,
        "CALL algo.automaton({state: 'on', rule: 'threshold', share: 0.4, steps: 10})
         YIELD node, state, changedAt RETURN node.i, state, changedAt ORDER BY node.i",
    );
    assert_eq!(
        spread,
        json!([
            [0, true, null],
            [1, true, 1],
            [2, true, 2],
            [3, true, 3],
            [4, true, 4]
        ])
    );
}

#[test]
fn modes_and_errors() {
    let db = shapes();
    // Statistics: one row for the rows, one per numeric column.
    let stats = rows(
        &db,
        "CALL algo.kCore({mode: 'stats'}) YIELD column, count, min, max RETURN column, count, min, max",
    );
    assert_eq!(
        stats,
        json!([["rows", 9, null, null], ["core", 9, 1.0, 2.0]])
    );
    // Writing back: a write statement, needing a writable database.
    let query = "CALL algo.kCore({write: 'core'}) YIELD property, nodes RETURN property, nodes";
    assert_eq!(
        requirements(query, None).unwrap().kind,
        grafeno_query::StatementKind::Write
    );
    let written = run(&db, query);
    assert_eq!(written.to_json()["rows"], json!([["core", 9]]));
    assert_eq!(written.stats.properties_set, 9);
    assert_eq!(
        rows(&db, "MATCH (n:P {n: 'e'}) RETURN n.core"),
        json!([[1]])
    );
    let both = run(
        &db,
        "CALL algo.hits({write: {hub: 'hub', authority: 'auth'}}) YIELD property, nodes RETURN property, nodes ORDER BY property",
    );
    assert_eq!(both.to_json()["rows"], json!([["auth", 9], ["hub", 9]]));
    assert!(
        grafeno_query::execute_read(
            &db.begin_read().unwrap(),
            NS,
            "CALL algo.kCore({write: 'core'})",
            &Params::new()
        )
        .is_err()
    );
    for (query, expected) in [
        (
            "CALL algo.kCore({mode: 'sideways'})",
            "mode 'stream' or 'stats'",
        ),
        (
            "CALL algo.kCore({write: {nothing: 'x'}})",
            "no column nothing",
        ),
        ("CALL algo.diffusion()", "needs amount"),
        (
            "CALL algo.automaton({state: 's', rule: 'chaos'})",
            "'life', 'threshold' or 'majority'",
        ),
        (
            "CALL algo.assignment({agents: 'A'})",
            "needs agents and tasks",
        ),
        (
            "CALL algo.kCore({frobnicate: 1})",
            "has no setting frobnicate",
        ),
    ] {
        let error = fail(&db, query);
        assert!(error.contains(expected), "{query}: {error}");
    }
}

#[test]
fn modes_in_parameters_are_refused() {
    let db = shapes();
    let mut params = Params::new();
    params.insert(
        "s".into(),
        grafeno_query::Value::from_json(&json!({"write": "core"})),
    );
    let error = execute(&db, NS, "CALL algo.kCore($s)", &params)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not in a parameter"), "{error}");
    params.insert(
        "s".into(),
        grafeno_query::Value::from_json(&json!({"mode": "stats"})),
    );
    let error = execute(&db, NS, "CALL algo.kCore($s)", &params)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not in a parameter"), "{error}");
    // Nothing was written.
    assert_eq!(
        rows(&db, "MATCH (n:P) WHERE n.core IS NOT NULL RETURN count(n)"),
        json!([[0]])
    );
    // Written in the query, with the other settings in parameters, it runs.
    params.insert(
        "labels".into(),
        grafeno_query::Value::from_json(&json!(["P"])),
    );
    let stats = execute(
        &db,
        NS,
        "CALL algo.kCore({mode: 'stats', labels: $labels}) YIELD column, max RETURN column, max",
        &params,
    )
    .unwrap();
    assert_eq!(
        stats.to_json()["rows"],
        json!([["rows", null], ["core", 2.0]])
    );
}
