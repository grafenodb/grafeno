//! Graph algorithms in GQ: `CALL algo.<name>(…, {settings}) YIELD …`
//! procedures and the `algo.*` pair functions.
//!
//! Procedures run on a projection of the namespace (`grafeno::algo`),
//! which read queries share through a cache until the next commit. Every
//! procedure takes a map of settings as its last argument; these choose
//! the projection:
//!
//! - `labels`: nodes with these labels (a string or a list); every node
//!   by default.
//! - `edgeLabels`: edges with these labels; every edge by default.
//! - `weight`: the edge property with each edge's weight (1 without it).
//! - `direction`: `'out'`, `'in'` or `'both'`.
//! - `validAt`, `knownAt`: the time travel of `MATCH`.
//!
//! Scoring procedures (`degree`, `pageRank`, `betweenness`, `closeness`)
//! also take `top: n`: only the n best rows, whose nodes are the only ones
//! read (a million rows of nodes would take gigabytes).
//!
//! Results stream as rows, so `RETURN` aggregates them (stats) and `SET`
//! stores them (`CALL algo.pageRank() YIELD node, score SET node.rank =
//! score`).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use grafeno::algo::{
    self, FastRp, LinkMetric, Louvain, PageRank, Projection, ProjectionSpec, SimilarityMetric,
    WeightedPath,
};
use grafeno::{Direction, EdgeFilter, NodeId, Timestamp};

use crate::QueryError;
use crate::eval::{Context, to_time};
use crate::exec::uuid_from_text;
use crate::procedures::Procedure;
use crate::value::Value;

pub(crate) const ALGORITHMS: &[Procedure] = &[
    Procedure {
        name: "algo.shortestPath",
        args: "source, target, {weight, direction: 'out', …}",
        columns: &["nodes", "edges", "cost"],
        description: "The cheapest path (Dijkstra): the fewest hops without a weight.",
    },
    Procedure {
        name: "algo.astar",
        args: "source, target, {weight, latitude, longitude | x, y, …}",
        columns: &["nodes", "edges", "cost"],
        description: "The cheapest path with A*, guided by the straight-line distance between node coordinates (km for latitude/longitude).",
    },
    Procedure {
        name: "algo.kShortestPaths",
        args: "source, target, k, {weight, …}",
        columns: &["rank", "nodes", "edges", "cost"],
        description: "The k cheapest loopless paths (Yen), cheapest first.",
    },
    Procedure {
        name: "algo.allShortestPaths",
        args: "source, target, {weight, limit: 100, …}",
        columns: &["nodes", "edges", "cost"],
        description: "Every cheapest path between two nodes.",
    },
    Procedure {
        name: "algo.distances",
        args: "source, {weight, …}",
        columns: &["node", "cost"],
        description: "The cost of the cheapest path from source to every node it reaches, nearest first.",
    },
    Procedure {
        name: "algo.degree",
        args: "{direction: 'both', weight, top, …}",
        columns: &["node", "score"],
        description: "Degree centrality: edges per node (or the sum of their weights), highest first.",
    },
    Procedure {
        name: "algo.pageRank",
        args: "{damping: 0.85, iterations: 20, tolerance: 1e-7, sources, weight, top, …}",
        columns: &["node", "score"],
        description: "PageRank (personalized with sources), highest first; scores add up to 1.",
    },
    Procedure {
        name: "algo.betweenness",
        args: "{sample, seed, weight, direction: 'out', top, …}",
        columns: &["node", "score"],
        description: "Betweenness centrality (Brandes): shortest paths through each node, highest first. sample estimates it from that many sources.",
    },
    Procedure {
        name: "algo.closeness",
        args: "{harmonic: false, weight, direction: 'out', top, …}",
        columns: &["node", "score"],
        description: "Closeness centrality (Wasserman-Faust, or harmonic), highest first.",
    },
    Procedure {
        name: "algo.wcc",
        args: "{…}",
        columns: &["node", "component"],
        description: "Weakly connected components.",
    },
    Procedure {
        name: "algo.scc",
        args: "{…}",
        columns: &["node", "component"],
        description: "Strongly connected components (following edge directions).",
    },
    Procedure {
        name: "algo.labelPropagation",
        args: "{iterations: 10, seed, weight, …}",
        columns: &["node", "community"],
        description: "Communities by label propagation.",
    },
    Procedure {
        name: "algo.louvain",
        args: "{resolution: 1, levels: 10, iterations: 10, weight, …}",
        columns: &["node", "community", "modularity"],
        description: "Communities by modularity (Louvain).",
    },
    Procedure {
        name: "algo.triangles",
        args: "{…}",
        columns: &["node", "triangles", "coefficient"],
        description: "Triangles per node and the local clustering coefficient.",
    },
    Procedure {
        name: "algo.nodeSimilarity",
        args: "{metric: 'jaccard', topK: 10, cutoff: 0, node, direction: 'out', …}",
        columns: &["node", "other", "score"],
        description: "Most similar nodes by shared neighbors (jaccard, overlap or cosine), best first per node.",
    },
    Procedure {
        name: "algo.knn",
        args: "{topK: 10, cutoff: 0, node, labels}",
        columns: &["node", "other", "score"],
        description: "Nearest nodes by embedding (cosine), from the vector index.",
    },
    Procedure {
        name: "algo.linkPrediction",
        args: "node, {metric: 'adamicAdar', limit: 10, …}",
        columns: &["other", "score"],
        description: "Likely new edges of a node: nodes two hops away, scored by commonNeighbors, adamicAdar, resourceAllocation, preferentialAttachment, totalNeighbors or jaccard.",
    },
    Procedure {
        name: "algo.fastRP",
        args: "{dimension: 128, iterationWeights: [0, 1, 1], selfInfluence: 0, normalizationStrength: 0, seed: 42, …}",
        columns: &["node", "embedding"],
        description: "Structural embeddings (FastRP); SET n.@embedding = embedding stores them.",
    },
    // Analysis (16.8).
    Procedure {
        name: "algo.kCore",
        args: "{…}",
        columns: &["node", "core"],
        description: "k-core decomposition: each node's core number (the largest k for which it is in a subgraph where every node has k neighbors).",
    },
    Procedure {
        name: "algo.leiden",
        args: "{resolution: 1, levels: 10, iterations: 10, weight, …}",
        columns: &["node", "community", "modularity"],
        description: "Communities by modularity, each one connected (Louvain's, split into their connected parts).",
    },
    Procedure {
        name: "algo.hits",
        args: "{iterations: 20, tolerance: 1e-8, weight, top, …}",
        columns: &["node", "hub", "authority"],
        description: "Hubs and authorities (HITS): good hubs point to good authorities; ordered by authority.",
    },
    Procedure {
        name: "algo.eigenvector",
        args: "{iterations: 100, tolerance: 1e-6, direction: 'in', weight, top, …}",
        columns: &["node", "score"],
        description: "Eigenvector centrality: nodes scored by the scores of those whose edges reach them, highest first.",
    },
    Procedure {
        name: "algo.harmonic",
        args: "{weight, direction: 'out', top, …}",
        columns: &["node", "score"],
        description: "Harmonic centrality: the sum of 1 / distance to every other node, highest first.",
    },
    Procedure {
        name: "algo.articulationPoints",
        args: "{…}",
        columns: &["node"],
        description: "Nodes whose removal would split their component (edges taken both ways).",
    },
    Procedure {
        name: "algo.bridges",
        args: "{…}",
        columns: &["edge"],
        description: "Edges whose removal would split their component (edges taken both ways).",
    },
    Procedure {
        name: "algo.spanningTree",
        args: "{maximum: false, weight, …}",
        columns: &["edge", "weight"],
        description: "A minimum (or maximum) spanning forest by edge weight: one tree per component.",
    },
    Procedure {
        name: "algo.maxFlow",
        args: "source, sink, {weight: capacity, …}",
        columns: &["edge", "flow", "value", "cut"],
        description: "The maximum flow from source to sink along edge directions (capacities from `weight`): one row per edge that carries flow, `cut` for the edges of a minimum cut.",
    },
    Procedure {
        name: "algo.bipartiteMatching",
        args: "{left: label, …}",
        columns: &["left", "right", "edge"],
        description: "A maximum matching between the nodes with the `left` label and the others, over edges with one end on each side.",
    },
    Procedure {
        name: "algo.topologicalOrder",
        args: "{…}",
        columns: &["node", "position"],
        description: "The nodes in an order where every edge goes forward; fails, naming a cycle, when there is one.",
    },
    Procedure {
        name: "algo.cycle",
        args: "{…}",
        columns: &["nodes"],
        description: "One cycle of the edges, its nodes in order (no row when there is none).",
    },
    Procedure {
        name: "algo.randomWalks",
        args: "starts | null, {length: 10, walks: 1, seed: 42, direction: 'out', weight, …}",
        columns: &["start", "walk"],
        description: "Random walks (edges chosen by weight), the same for a seed: from the starts, or every node.",
    },
    Procedure {
        name: "algo.clustering",
        args: "{…}",
        columns: &["node", "coefficient"],
        description: "The local clustering coefficient: the share of a node's neighbor pairs that are linked.",
    },
    Procedure {
        name: "algo.conductance",
        args: "{property: 'community', …}",
        columns: &["community", "conductance"],
        description: "For each group of nodes (by the value of `property`), the weight of its edges going out over its volume: 0 closed, 1 open.",
    },
    // Simulation (16.8).
    Procedure {
        name: "algo.distanceField",
        args: "sources, {maxCost, weight, direction: 'both', …}",
        columns: &["node", "cost", "nearest"],
        description: "The cost from every node to the nearest of the sources, and which one, in one search: where agents go next to reach the nearest goal.",
    },
    Procedure {
        name: "algo.influence",
        args: "sources, {value: property, decay: 0.5, threshold: 0.01, maxCost, combine: 'sum' | 'max', weight, direction: 'both', …}",
        columns: &["node", "influence"],
        description: "An influence map: each source's value (its property, or 1) weakened by decay^cost along the cheapest way, combined at each node.",
    },
    Procedure {
        name: "algo.floodFill",
        args: "seeds, {where: {property: value}, inside: [labels], direction: 'both', …}",
        columns: &["node", "steps"],
        description: "The region reachable from the seeds through nodes that satisfy the condition, with how many steps away each is.",
    },
    Procedure {
        name: "algo.diffusion",
        args: "{amount: property, rate: 0.5, steps: 10, capacity: property, weight, direction: 'both', …}",
        columns: &["node", "amount"],
        description: "A quantity spreading along edges over steps (a share leaves each node, split by edge weight), at most `capacity` per node; the total stays.",
    },
    Procedure {
        name: "algo.automaton",
        args: "{state: property, rule: 'life' | 'threshold' | 'majority', birth: [3], survive: [2, 3], share: 0.5, recover: false, steps: 10, direction: 'both', …}",
        columns: &["node", "state", "changedAt"],
        description: "A rule applied to every node at once from its neighbors' states, step by step until it settles.",
    },
    Procedure {
        name: "algo.assignment",
        args: "{agents: label, tasks: label, weight: cost, …}",
        columns: &["agent", "task", "cost"],
        description: "Agents assigned to tasks at least total cost (an edge between them is an allowed pair, its weight the cost).",
    },
    Procedure {
        name: "algo.minCostFlow",
        args: "{supply: property, capacity: property, cost: property, …}",
        columns: &["edge", "flow", "value", "cost", "unmet"],
        description: "Resources sent from nodes that supply them (supply > 0) to nodes that need them (< 0) at least cost, each edge carrying at most its capacity.",
    },
];

/// The node positions an argument names: a node, an id, a list of them,
/// or null for every node.
fn positions(procedure: &str, p: &Projection, value: &Value) -> Result<Vec<u32>, QueryError> {
    match value {
        Value::Null => Ok((0..p.len() as u32).collect()),
        Value::List(items) => items
            .iter()
            .map(|item| position(procedure, p, node_id(procedure, item)?))
            .collect(),
        other => Ok(vec![position(procedure, p, node_id(procedure, other)?)?]),
    }
}

/// A property of every node of the projection (absent: `None`).
fn node_props(
    ctx: &Context,
    p: &Projection,
    key: &str,
) -> Result<Vec<Option<serde_json::Value>>, QueryError> {
    (0..p.len() as u32)
        .map(|at| {
            Ok(ctx
                .node(p.id(at))?
                .and_then(|node| node.props.get(key).cloned()))
        })
        .collect()
}

/// A numeric property of every node (`default` where absent).
fn node_numbers(
    ctx: &Context,
    p: &Projection,
    key: &str,
    default: f64,
) -> Result<Vec<f64>, QueryError> {
    Ok(node_props(ctx, p, key)?
        .into_iter()
        .map(|value| value.and_then(|v| v.as_f64()).unwrap_or(default))
        .collect())
}

/// The edge at index `edge` as a value.
fn edge_value(ctx: &Context, p: &Projection, edge: u32) -> Result<Value, QueryError> {
    Ok(match p.edge_id(edge) {
        Some(id) => ctx
            .edge(id)?
            .map_or(Value::Null, |edge| Value::Edge(Arc::new(edge))),
        None => Value::Null,
    })
}

/// Each edge's weight, by edge index.
fn edge_weights(p: &Projection) -> Vec<f64> {
    let mut out = vec![1.0; p.edge_count()];
    for node in 0..p.len() as u32 {
        for step in p.neighbors(node, Direction::Out) {
            out[step.edge as usize] = step.weight;
        }
    }
    out
}

/// The positions of the nodes with `label` in the projection.
fn labelled(ctx: &Context, p: &Projection, label: &str) -> Result<Vec<u32>, QueryError> {
    let ids = match ctx.write {
        // Writing: the transaction's own nodes, as the projection has them.
        Some(_) => {
            let mut ids = Vec::new();
            for at in 0..p.len() as u32 {
                if ctx
                    .node(p.id(at))?
                    .is_some_and(|node| node.has_label(label))
                {
                    ids.push(p.id(at));
                }
            }
            ids
        }
        None => ctx.read.nodes_with_label(ctx.ns, label)?,
    };
    Ok(ids.into_iter().filter_map(|id| p.position(id)).collect())
}

// ---------------------------------------------------------------- settings

/// The settings map of a procedure, checked for unknown keys.
struct Settings {
    procedure: &'static str,
    map: BTreeMap<String, Value>,
    used: RefCell<BTreeSet<&'static str>>,
}

impl Settings {
    fn new(procedure: &'static str, value: Option<&Value>) -> Result<Self, QueryError> {
        let map = match value {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Map(map)) => map.clone(),
            Some(other) => {
                return Err(QueryError::runtime(format!(
                    "{procedure}() takes its settings as a map ({{key: value}}), not a {}",
                    other.type_name()
                )));
            }
        };
        Ok(Self {
            procedure,
            map,
            used: RefCell::default(),
        })
    }

    fn get(&self, key: &'static str) -> Option<&Value> {
        self.used.borrow_mut().insert(key);
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn fail(&self, key: &str, wanted: &str, got: &Value) -> QueryError {
        QueryError::runtime(format!(
            "{}() needs {key} to be {wanted}, not {}",
            self.procedure,
            got.to_json()
        ))
    }

    fn float(&self, key: &'static str, default: f64) -> Result<f64, QueryError> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Int(n)) => Ok(*n as f64),
            Some(Value::Float(f)) if f.is_finite() => Ok(*f),
            Some(other) => Err(self.fail(key, "a number", other)),
        }
    }

    fn count(&self, key: &'static str, default: usize) -> Result<usize, QueryError> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Int(n)) if *n >= 0 => Ok(*n as usize),
            Some(other) => Err(self.fail(key, "a whole number, 0 or more", other)),
        }
    }

    fn optional_count(&self, key: &'static str) -> Result<Option<usize>, QueryError> {
        Ok(match self.get(key) {
            None => None,
            Some(_) => Some(self.count(key, 0)?),
        })
    }

    fn flag(&self, key: &'static str, default: bool) -> Result<bool, QueryError> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(self.fail(key, "true or false", other)),
        }
    }

    fn text(&self, key: &'static str) -> Result<Option<String>, QueryError> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s.clone())),
            Some(other) => Err(self.fail(key, "a string", other)),
        }
    }

    fn texts(&self, key: &'static str) -> Result<Vec<String>, QueryError> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Str(s)) => Ok(vec![s.clone()]),
            Some(Value::List(items)) => items
                .iter()
                .map(|item| match item {
                    Value::Str(s) => Ok(s.clone()),
                    other => Err(self.fail(key, "a string or a list of strings", other)),
                })
                .collect(),
            Some(other) => Err(self.fail(key, "a string or a list of strings", other)),
        }
    }

    fn time(&self, key: &'static str) -> Result<Option<Timestamp>, QueryError> {
        self.get(key).map(to_time).transpose()
    }

    fn seed(&self) -> Result<u64, QueryError> {
        match self.get("seed") {
            None => Ok(42),
            Some(Value::Int(n)) => Ok(*n as u64),
            Some(other) => Err(self.fail("seed", "an integer", other)),
        }
    }

    fn direction(&self, default: Direction) -> Result<Direction, QueryError> {
        match self
            .text("direction")?
            .as_deref()
            .map(str::to_ascii_lowercase)
        {
            None => Ok(default),
            Some(d) if d == "out" || d == "outgoing" => Ok(Direction::Out),
            Some(d) if d == "in" || d == "incoming" => Ok(Direction::In),
            Some(d) if d == "both" || d == "undirected" => Ok(Direction::Both),
            Some(d) => Err(self.fail("direction", "'out', 'in' or 'both'", &Value::Str(d))),
        }
    }

    fn spec(&self) -> Result<ProjectionSpec, QueryError> {
        Ok(ProjectionSpec {
            labels: self.texts("labels")?,
            edge_labels: self.texts("edgeLabels")?,
            valid_at: self.time("validAt")?,
            known_at: self.time("knownAt")?,
            weight: self.text("weight")?,
            // Only algorithms that return edges keep their ids.
            edge_ids: {
                let name = self.procedure.to_lowercase();
                name.contains("path")
                    || name.contains("astar")
                    || [
                        "algo.bridges",
                        "algo.spanningtree",
                        "algo.maxflow",
                        "algo.bipartitematching",
                        "algo.mincostflow",
                    ]
                    .contains(&name.as_str())
            },
            view: None,
        })
    }

    /// Fails on keys no setting read: a typo would otherwise be ignored.
    fn finish(&self) -> Result<(), QueryError> {
        let used = self.used.borrow();
        if let Some(unknown) = self.map.keys().find(|key| !used.contains(key.as_str())) {
            return Err(QueryError::runtime(format!(
                "{}() has no setting {unknown} (it takes {})",
                self.procedure,
                used.iter().copied().collect::<Vec<_>>().join(", ")
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

fn node_id(procedure: &str, value: &Value) -> Result<NodeId, QueryError> {
    match value {
        Value::Node(node) => Ok(node.id),
        Value::Str(text) => uuid_from_text(text).ok_or_else(|| {
            QueryError::runtime(format!("{procedure}(): {text:?} is not a node id"))
        }),
        other => Err(QueryError::runtime(format!(
            "{procedure}() needs a node (or its id), not a {}",
            other.type_name()
        ))),
    }
}

fn position(procedure: &str, p: &Projection, id: NodeId) -> Result<u32, QueryError> {
    p.position(id).ok_or_else(|| {
        QueryError::runtime(format!(
            "{procedure}(): node {id} is not in the projection (check labels)"
        ))
    })
}

/// The node at `at` as a value of a result row, counted against the
/// query's memory limit (a procedure can yield a row per node).
fn node_value(ctx: &Context, p: &Projection, at: u32) -> Result<Value, QueryError> {
    let value = ctx
        .node(p.id(at))?
        .map_or(Value::Null, |node| Value::Node(Arc::new(node)));
    ctx.budget.build(
        200 + crate::limits::footprint(&value, crate::limits::MAX_VALUE_DEPTH).unwrap_or(0),
    )?;
    Ok(value)
}

fn path_row(ctx: &Context, p: &Projection, path: &WeightedPath) -> Result<Vec<Value>, QueryError> {
    let nodes = path
        .nodes
        .iter()
        .map(|&at| node_value(ctx, p, at))
        .collect::<Result<Vec<_>, _>>()?;
    let mut edges = Vec::with_capacity(path.edges.len());
    for &edge in &path.edges {
        let Some(id) = p.edge_id(edge) else {
            edges.push(Value::Null);
            continue;
        };
        edges.push(
            ctx.edge(id)?
                .map_or(Value::Null, |edge| Value::Edge(Arc::new(edge))),
        );
    }
    Ok(vec![
        Value::List(nodes),
        Value::List(edges),
        Value::Float(path.cost),
    ])
}

/// One row per node with its score, highest first; with `top`, only that
/// many (their records are the only ones read).
fn ranked(
    ctx: &Context,
    p: &Projection,
    scores: &[f64],
    top: Option<usize>,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let mut order: Vec<u32> = (0..p.len() as u32).collect();
    let by_score = |a: &u32, b: &u32| {
        scores[*b as usize]
            .total_cmp(&scores[*a as usize])
            .then(a.cmp(b))
    };
    if let Some(top) = top.filter(|top| *top < order.len()) {
        if top > 0 {
            order.select_nth_unstable_by(top - 1, by_score);
        }
        order.truncate(top);
    }
    order.sort_by(by_score);
    order
        .into_iter()
        .map(|at| {
            Ok(vec![
                node_value(ctx, p, at)?,
                Value::Float(scores[at as usize]),
            ])
        })
        .collect()
}

/// One row per node with its group, in node order.
fn grouped(ctx: &Context, p: &Projection, groups: &[u32]) -> Result<Vec<Vec<Value>>, QueryError> {
    (0..p.len() as u32)
        .map(|at| {
            Ok(vec![
                node_value(ctx, p, at)?,
                Value::Int(i64::from(groups[at as usize])),
            ])
        })
        .collect()
}

fn haversine_km((lat1, lon1): (f64, f64), (lat2, lon2): (f64, f64)) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0088 * a.sqrt().asin()
}

// ---------------------------------------------------------------- running

/// Runs `algo.<name>`: one row per result, in the order of its columns.
pub(crate) fn call(
    ctx: &Context,
    procedure: &'static Procedure,
    args: &[Value],
) -> Result<Vec<Vec<Value>>, QueryError> {
    let name = procedure.name;
    // Positional arguments before the settings map.
    let positional = match name {
        "algo.shortestPath" | "algo.astar" | "algo.allShortestPaths" => 2,
        "algo.kShortestPaths" => 3,
        "algo.distances"
        | "algo.linkPrediction"
        | "algo.randomWalks"
        | "algo.distanceField"
        | "algo.influence"
        | "algo.floodFill" => 1,
        "algo.maxFlow" => 2,
        _ => 0,
    };
    if args.len() < positional || args.len() > positional + 1 {
        return Err(QueryError::runtime(format!(
            "{name}() takes {}",
            procedure.args
        )));
    }
    let settings = Settings::new(name, args.get(positional))?;
    // The modes of every algorithm, carried out where the rows go
    // (exec.rs): only checked here.
    let _ = settings.get("write");
    if let Some(mode) = settings.text("mode")?
        && !["stream", "stats"].contains(&mode.as_str())
    {
        return Err(QueryError::runtime(format!(
            "{name}() runs in mode 'stream' or 'stats' (with write: … it writes), not {mode:?}"
        )));
    }
    let mut spec = settings.spec()?;
    if name == "algo.assignment" && spec.labels.is_empty() {
        // The agents and the tasks, whatever else there is.
        for key in ["agents", "tasks"] {
            if let Some(Value::Str(label)) = settings.map.get(key) {
                spec.labels.push(label.clone());
            }
        }
    }
    let stop = ctx.budget.stopper();
    let stopped = |error: grafeno::Error| -> QueryError {
        if stop() {
            ctx.budget
                .tick()
                .err()
                .unwrap_or_else(|| QueryError::limit("the query was stopped"))
        } else {
            error.into()
        }
    };
    let project = |spec: &ProjectionSpec| -> Result<Arc<Projection>, QueryError> {
        // Algorithms run on the subgraph the caller sees (14.4).
        let mut spec = spec.clone();
        spec.view = ctx.view.map(|view| grafeno::algo::ViewRef(view.clone()));
        let spec = &spec;
        let p = match ctx.write {
            Some(tx) => tx.projection(ctx.ns, spec)?,
            None => ctx.read.projection(ctx.ns, spec)?,
        };
        ctx.budget.tick()?;
        Ok(p)
    };
    let endpoints = |p: &Projection| -> Result<(u32, u32), QueryError> {
        let from = position(name, p, node_id(name, &args[0])?)?;
        let to = position(name, p, node_id(name, &args[1])?)?;
        Ok((from, to))
    };
    Ok(match name {
        "algo.shortestPath" | "algo.astar" => {
            let direction = settings.direction(Direction::Out)?;
            let coordinates = if name == "algo.astar" {
                match (
                    settings.text("latitude")?,
                    settings.text("longitude")?,
                    settings.text("x")?,
                    settings.text("y")?,
                ) {
                    (Some(lat), Some(lon), None, None) => Some((lat, lon, true)),
                    (None, None, Some(x), Some(y)) => Some((x, y, false)),
                    _ => {
                        return Err(QueryError::runtime(
                            "algo.astar() needs the coordinate properties: latitude and longitude, or x and y",
                        ));
                    }
                }
            } else {
                None
            };
            settings.finish()?;
            let p = project(&spec)?;
            let (from, to) = endpoints(&p)?;
            let path = match coordinates {
                None => algo::shortest_path(&p, from, to, direction, &stop).map_err(stopped)?,
                Some((a, b, spherical)) => {
                    let cache: RefCell<HashMap<u32, Option<(f64, f64)>>> = RefCell::default();
                    let point = |at: u32| -> Option<(f64, f64)> {
                        if let Some(known) = cache.borrow().get(&at) {
                            return *known;
                        }
                        let found = ctx.node(p.id(at)).ok().flatten().and_then(|node| {
                            Some((node.props.get(&a)?.as_f64()?, node.props.get(&b)?.as_f64()?))
                        });
                        cache.borrow_mut().insert(at, found);
                        found
                    };
                    let goal = point(to);
                    let estimate = |at: u32| match (point(at), goal) {
                        (Some(here), Some(there)) if spherical => haversine_km(here, there),
                        (Some((x1, y1)), Some((x2, y2))) => (x1 - x2).hypot(y1 - y2),
                        _ => 0.0,
                    };
                    algo::astar(&p, from, to, direction, &estimate, &stop).map_err(stopped)?
                }
            };
            match path {
                Some(path) => vec![path_row(ctx, &p, &path)?],
                None => Vec::new(),
            }
        }
        "algo.kShortestPaths" => {
            let k = match &args[2] {
                Value::Int(k) if *k > 0 => *k as usize,
                other => {
                    return Err(QueryError::runtime(format!(
                        "algo.kShortestPaths() needs k, a positive integer, not {}",
                        other.to_json()
                    )));
                }
            };
            let direction = settings.direction(Direction::Out)?;
            settings.finish()?;
            let p = project(&spec)?;
            let (from, to) = endpoints(&p)?;
            let paths =
                algo::k_shortest_paths(&p, from, to, k, direction, &stop).map_err(stopped)?;
            let mut rows = Vec::new();
            for (rank, path) in paths.iter().enumerate() {
                let mut row = vec![Value::Int(rank as i64 + 1)];
                row.extend(path_row(ctx, &p, path)?);
                rows.push(row);
            }
            rows
        }
        "algo.allShortestPaths" => {
            let direction = settings.direction(Direction::Out)?;
            let limit = settings.count("limit", 100)?;
            settings.finish()?;
            let p = project(&spec)?;
            let (from, to) = endpoints(&p)?;
            algo::all_shortest_paths(&p, from, to, direction, limit, &stop)
                .map_err(stopped)?
                .iter()
                .map(|path| path_row(ctx, &p, path))
                .collect::<Result<_, _>>()?
        }
        "algo.distances" => {
            let direction = settings.direction(Direction::Out)?;
            settings.finish()?;
            let p = project(&spec)?;
            let from = position(name, &p, node_id(name, &args[0])?)?;
            let costs = algo::distances(&p, from, direction, &stop).map_err(stopped)?;
            let mut order: Vec<u32> = (0..p.len() as u32)
                .filter(|at| costs[*at as usize].is_finite())
                .collect();
            order.sort_by(|a, b| {
                costs[*a as usize]
                    .total_cmp(&costs[*b as usize])
                    .then(a.cmp(b))
            });
            order
                .into_iter()
                .map(|at| {
                    Ok(vec![
                        node_value(ctx, &p, at)?,
                        Value::Float(costs[at as usize]),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.degree" => {
            let direction = settings.direction(Direction::Both)?;
            let top = settings.optional_count("top")?;
            settings.finish()?;
            let p = project(&spec)?;
            ranked(ctx, &p, &algo::degree(&p, direction, p.is_weighted()), top)?
        }
        "algo.pageRank" => {
            let defaults = PageRank::default();
            let direction = settings.direction(Direction::Out)?;
            let damping = settings.float("damping", defaults.damping)?;
            if !(0.0..1.0).contains(&damping) {
                return Err(QueryError::runtime(
                    "algo.pageRank() needs damping from 0 to less than 1",
                ));
            }
            let max_iterations = settings.count("iterations", defaults.max_iterations)?;
            let tolerance = settings.float("tolerance", defaults.tolerance)?;
            let sources = match settings.get("sources") {
                None => Vec::new(),
                Some(Value::List(items)) => items.clone(),
                Some(single) => vec![single.clone()],
            };
            let top = settings.optional_count("top")?;
            settings.finish()?;
            let p = project(&spec)?;
            let sources = sources
                .iter()
                .map(|value| position(name, &p, node_id(name, value)?))
                .collect::<Result<Vec<_>, _>>()?;
            let page_rank = PageRank {
                damping,
                max_iterations,
                tolerance,
                direction,
                sources,
            };
            let (scores, _) = algo::page_rank(&p, &page_rank, &stop).map_err(stopped)?;
            ranked(ctx, &p, &scores, top)?
        }
        "algo.betweenness" => {
            let direction = settings.direction(Direction::Out)?;
            let sample = settings.optional_count("sample")?;
            let seed = settings.seed()?;
            let top = settings.optional_count("top")?;
            settings.finish()?;
            let p = project(&spec)?;
            let scores = algo::betweenness(&p, direction, sample, seed, &stop).map_err(stopped)?;
            ranked(ctx, &p, &scores, top)?
        }
        "algo.closeness" => {
            let direction = settings.direction(Direction::Out)?;
            let harmonic = settings.flag("harmonic", false)?;
            let top = settings.optional_count("top")?;
            settings.finish()?;
            let p = project(&spec)?;
            let scores = algo::closeness(&p, direction, harmonic, &stop).map_err(stopped)?;
            ranked(ctx, &p, &scores, top)?
        }
        "algo.wcc" | "algo.scc" => {
            settings.finish()?;
            let p = project(&spec)?;
            let groups = if name == "algo.wcc" {
                algo::weakly_connected(&p)
            } else {
                algo::strongly_connected(&p)
            };
            grouped(ctx, &p, &groups)?
        }
        "algo.labelPropagation" => {
            let iterations = settings.count("iterations", 10)?;
            let seed = settings.seed()?;
            settings.finish()?;
            let p = project(&spec)?;
            let (groups, _) =
                algo::label_propagation(&p, iterations, seed, &stop).map_err(stopped)?;
            grouped(ctx, &p, &groups)?
        }
        "algo.louvain" => {
            let defaults = Louvain::default();
            let louvain = Louvain {
                resolution: settings.float("resolution", defaults.resolution)?,
                max_levels: settings.count("levels", defaults.max_levels)?,
                max_iterations: settings.count("iterations", defaults.max_iterations)?,
                tolerance: settings.float("tolerance", defaults.tolerance)?,
            };
            settings.finish()?;
            let p = project(&spec)?;
            let (groups, modularity, _) = algo::louvain(&p, &louvain, &stop).map_err(stopped)?;
            let mut rows = grouped(ctx, &p, &groups)?;
            for row in &mut rows {
                row.push(Value::Float(modularity));
            }
            rows
        }
        "algo.triangles" => {
            settings.finish()?;
            let p = project(&spec)?;
            let found = algo::triangles(&p, &stop).map_err(stopped)?;
            (0..p.len() as u32)
                .map(|at| {
                    Ok(vec![
                        node_value(ctx, &p, at)?,
                        Value::Int(found.per_node[at as usize] as i64),
                        Value::Float(found.coefficient[at as usize]),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.nodeSimilarity" => {
            let metric =
                SimilarityMetric::parse(settings.text("metric")?.as_deref().unwrap_or("jaccard"))?;
            let direction = settings.direction(Direction::Out)?;
            let top_k = settings.count("topK", 10)?;
            let cutoff = settings.float("cutoff", 0.0)?;
            let node = settings.get("node").cloned();
            settings.finish()?;
            let p = project(&spec)?;
            let source = node
                .map(|value| position(name, &p, node_id(name, &value)?))
                .transpose()?;
            let pairs =
                algo::similar_neighbors(&p, metric, direction, top_k, cutoff, source, &stop)
                    .map_err(stopped)?;
            pairs
                .into_iter()
                .map(|(a, b, score)| {
                    Ok(vec![
                        node_value(ctx, &p, a)?,
                        node_value(ctx, &p, b)?,
                        Value::Float(score),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.knn" => {
            let top_k = settings.count("topK", 10)?;
            let cutoff = settings.float("cutoff", 0.0)?;
            let node = settings.get("node").cloned();
            settings.finish()?;
            // Only the nodes: no edge has an empty label.
            let p = project(&ProjectionSpec {
                labels: spec.labels.clone(),
                edge_labels: vec![String::new()],
                ..ProjectionSpec::default()
            })?;
            let sources: Vec<u32> = match node {
                Some(value) => vec![position(name, &p, node_id(name, &value)?)?],
                None => (0..p.len() as u32).collect(),
            };
            // Extra candidates make up for hits outside the labels.
            let ask = if spec.labels.is_empty() {
                top_k + 1
            } else {
                (top_k + 1) * 4
            };
            let mut rows = Vec::new();
            for (i, at) in sources.into_iter().enumerate() {
                if i % 64 == 0 {
                    ctx.budget.tick()?;
                }
                let id = p.id(at);
                let Some(embedding) = ctx.embedding(id)? else {
                    continue;
                };
                let hits = ctx.read.search_vector(ctx.ns, &embedding, ask)?;
                let mut kept = 0;
                for hit in hits {
                    let score = f64::from(hit.score);
                    if hit.node == id || score < cutoff || kept == top_k {
                        continue;
                    }
                    let Some(other) = p.position(hit.node) else {
                        continue;
                    };
                    kept += 1;
                    rows.push(vec![
                        node_value(ctx, &p, at)?,
                        node_value(ctx, &p, other)?,
                        Value::Float(score),
                    ]);
                }
            }
            rows
        }
        "algo.linkPrediction" => {
            let metric =
                LinkMetric::parse(settings.text("metric")?.as_deref().unwrap_or("adamicAdar"))?;
            let limit = settings.count("limit", 10)?;
            settings.finish()?;
            let p = project(&spec)?;
            let node = position(name, &p, node_id(name, &args[0])?)?;
            algo::predict_links(&p, node, metric, limit, &stop)
                .map_err(stopped)?
                .into_iter()
                .map(|(other, score)| Ok(vec![node_value(ctx, &p, other)?, Value::Float(score)]))
                .collect::<Result<_, QueryError>>()?
        }
        "algo.fastRP" => {
            let defaults = FastRp::default();
            let iteration_weights = match settings.get("iterationWeights") {
                None => defaults.iteration_weights.clone(),
                Some(Value::List(items)) => items
                    .iter()
                    .map(|item| match item {
                        Value::Int(n) => Ok(*n as f64),
                        Value::Float(f) => Ok(*f),
                        other => Err(settings.fail("iterationWeights", "a list of numbers", other)),
                    })
                    .collect::<Result<_, _>>()?,
                Some(other) => {
                    return Err(settings.fail("iterationWeights", "a list of numbers", other));
                }
            };
            let fast_rp = FastRp {
                dimension: settings.count("dimension", defaults.dimension)?,
                iteration_weights,
                self_influence: settings.float("selfInfluence", defaults.self_influence)?,
                normalization_strength: settings
                    .float("normalizationStrength", defaults.normalization_strength)?,
                seed: settings.seed()?,
                direction: settings.direction(Direction::Both)?,
            };
            settings.finish()?;
            let p = project(&spec)?;
            let vectors = algo::fast_rp(&p, &fast_rp, &stop).map_err(stopped)?;
            vectors
                .into_iter()
                .enumerate()
                .map(|(at, vector)| {
                    Ok(vec![
                        node_value(ctx, &p, at as u32)?,
                        Value::List(
                            vector
                                .into_iter()
                                .map(|x| Value::Float(f64::from(x)))
                                .collect(),
                        ),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.kCore" => {
            settings.finish()?;
            let p = project(&spec)?;
            let cores = algo::k_core(&p);
            (0..p.len() as u32)
                .map(|at| {
                    Ok(vec![
                        node_value(ctx, &p, at)?,
                        Value::Int(i64::from(cores[at as usize])),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.leiden" => {
            let defaults = Louvain::default();
            let louvain = Louvain {
                resolution: settings.float("resolution", defaults.resolution)?,
                max_levels: settings.count("levels", defaults.max_levels)?,
                max_iterations: settings.count("iterations", defaults.max_iterations)?,
                tolerance: settings.float("tolerance", defaults.tolerance)?,
            };
            settings.finish()?;
            let p = project(&spec)?;
            let (groups, modularity, _) = algo::leiden(&p, &louvain, &stop).map_err(stopped)?;
            let mut rows = grouped(ctx, &p, &groups)?;
            for row in &mut rows {
                row.push(Value::Float(modularity));
            }
            rows
        }
        "algo.hits" => {
            let iterations = settings.count("iterations", 20)?;
            let tolerance = settings.float("tolerance", 1e-8)?;
            let top = settings.optional_count("top")?;
            settings.finish()?;
            let p = project(&spec)?;
            let (hubs, authorities, _) =
                algo::hits(&p, iterations, tolerance, &stop).map_err(stopped)?;
            let mut rows = ranked(ctx, &p, &authorities, top)?;
            for row in &mut rows {
                let at = match &row[0] {
                    Value::Node(node) => p.position(node.id),
                    _ => None,
                };
                let authority = row.pop().expect("a score");
                row.push(Value::Float(at.map_or(0.0, |at| hubs[at as usize])));
                row.push(authority);
            }
            rows
        }
        "algo.eigenvector" => {
            let direction = settings.direction(Direction::In)?;
            let iterations = settings.count("iterations", 100)?;
            let tolerance = settings.float("tolerance", 1e-6)?;
            let top = settings.optional_count("top")?;
            settings.finish()?;
            let p = project(&spec)?;
            let (scores, _) =
                algo::eigenvector(&p, direction, iterations, tolerance, &stop).map_err(stopped)?;
            ranked(ctx, &p, &scores, top)?
        }
        "algo.harmonic" => {
            let direction = settings.direction(Direction::Out)?;
            let top = settings.optional_count("top")?;
            settings.finish()?;
            let p = project(&spec)?;
            let scores = algo::closeness(&p, direction, true, &stop).map_err(stopped)?;
            ranked(ctx, &p, &scores, top)?
        }
        "algo.articulationPoints" | "algo.bridges" => {
            settings.finish()?;
            let p = project(&spec)?;
            let (points, bridges) = algo::cut_points(&p);
            if name == "algo.bridges" {
                bridges
                    .into_iter()
                    .map(|edge| Ok(vec![edge_value(ctx, &p, edge)?]))
                    .collect::<Result<_, QueryError>>()?
            } else {
                points
                    .into_iter()
                    .map(|at| Ok(vec![node_value(ctx, &p, at)?]))
                    .collect::<Result<_, QueryError>>()?
            }
        }
        "algo.spanningTree" => {
            let maximum = settings.flag("maximum", false)?;
            settings.finish()?;
            let p = project(&spec)?;
            let weights = edge_weights(&p);
            let (edges, _) = algo::spanning_forest(&p, maximum);
            edges
                .into_iter()
                .map(|edge| {
                    Ok(vec![
                        edge_value(ctx, &p, edge)?,
                        Value::Float(weights[edge as usize]),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.maxFlow" => {
            settings.finish()?;
            let p = project(&spec)?;
            let (from, to) = endpoints(&p)?;
            let flow = algo::max_flow(&p, from, to, &stop).map_err(stopped)?;
            let cut: std::collections::HashSet<u32> = flow.cut.iter().copied().collect();
            flow.edges
                .iter()
                .map(|(edge, amount)| {
                    Ok(vec![
                        edge_value(ctx, &p, *edge)?,
                        Value::Float(*amount),
                        Value::Float(flow.value),
                        Value::Bool(cut.contains(edge)),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.bipartiteMatching" => {
            let Some(left) = settings.text("left")? else {
                return Err(QueryError::runtime(
                    "algo.bipartiteMatching() needs left: the label of one side",
                ));
            };
            settings.finish()?;
            let p = project(&spec)?;
            let mut is_left = vec![false; p.len()];
            for at in labelled(ctx, &p, &left)? {
                is_left[at as usize] = true;
            }
            algo::bipartite_matching(&p, &is_left)
                .into_iter()
                .map(|(a, b, edge)| {
                    Ok(vec![
                        node_value(ctx, &p, a)?,
                        node_value(ctx, &p, b)?,
                        edge_value(ctx, &p, edge)?,
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.topologicalOrder" | "algo.cycle" => {
            settings.finish()?;
            let p = project(&spec)?;
            match (algo::topological_order(&p), name) {
                (Ok(order), "algo.topologicalOrder") => order
                    .into_iter()
                    .enumerate()
                    .map(|(at, node)| Ok(vec![node_value(ctx, &p, node)?, Value::Int(at as i64)]))
                    .collect::<Result<_, QueryError>>()?,
                (Ok(_), _) => Vec::new(),
                (Err(cycle), "algo.cycle") => vec![vec![Value::List(
                    cycle
                        .into_iter()
                        .map(|at| node_value(ctx, &p, at))
                        .collect::<Result<_, _>>()?,
                )]],
                (Err(cycle), _) => {
                    let ids: Vec<String> = cycle.iter().map(|at| p.id(*at).to_string()).collect();
                    return Err(QueryError::runtime(format!(
                        "the edges make a cycle, so there is no topological order: {}",
                        ids.join(" → ")
                    )));
                }
            }
        }
        "algo.randomWalks" => {
            let length = settings.count("length", 10)?;
            let walks = settings.count("walks", 1)?;
            let direction = settings.direction(Direction::Out)?;
            let seed = settings.seed()?;
            settings.finish()?;
            let p = project(&spec)?;
            let starts = positions(name, &p, &args[0])?;
            algo::random_walks(&p, &starts, length, walks, direction, seed, &stop)
                .map_err(stopped)?
                .into_iter()
                .map(|walk| {
                    Ok(vec![
                        node_value(ctx, &p, walk[0])?,
                        Value::List(
                            walk.iter()
                                .map(|at| node_value(ctx, &p, *at))
                                .collect::<Result<_, _>>()?,
                        ),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.clustering" => {
            settings.finish()?;
            let p = project(&spec)?;
            let found = algo::triangles(&p, &stop).map_err(stopped)?;
            (0..p.len() as u32)
                .map(|at| {
                    Ok(vec![
                        node_value(ctx, &p, at)?,
                        Value::Float(found.coefficient[at as usize]),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.conductance" => {
            let property = settings
                .text("property")?
                .unwrap_or_else(|| "community".into());
            settings.finish()?;
            let p = project(&spec)?;
            let groups: Vec<Option<i64>> = node_props(ctx, &p, &property)?
                .into_iter()
                .map(|value| value.and_then(|v| v.as_i64()))
                .collect();
            algo::conductance(&p, &groups)
                .into_iter()
                .map(|(group, score)| vec![Value::Int(group), Value::Float(score)])
                .collect()
        }
        "algo.distanceField" => {
            let direction = settings.direction(Direction::Both)?;
            let max_cost = settings
                .get("maxCost")
                .map(|_| settings.float("maxCost", 0.0))
                .transpose()?;
            settings.finish()?;
            let p = project(&spec)?;
            let sources = positions(name, &p, &args[0])?;
            let field =
                algo::distance_field(&p, &sources, direction, max_cost, &stop).map_err(stopped)?;
            let mut rows = Vec::new();
            for (at, found) in field.into_iter().enumerate() {
                if let Some((cost, nearest)) = found {
                    rows.push(vec![
                        node_value(ctx, &p, at as u32)?,
                        Value::Float(cost),
                        node_value(ctx, &p, nearest)?,
                    ]);
                }
            }
            rows
        }
        "algo.influence" => {
            let direction = settings.direction(Direction::Both)?;
            let value = settings.text("value")?;
            let settings_value = algo::Influence {
                decay: settings.float("decay", 0.5)?,
                threshold: settings.float("threshold", 0.01)?,
                max_cost: settings
                    .get("maxCost")
                    .map(|_| settings.float("maxCost", 0.0))
                    .transpose()?,
                sum: match settings.text("combine")?.as_deref() {
                    None | Some("sum") => true,
                    Some("max") => false,
                    Some(other) => {
                        return Err(settings.fail(
                            "combine",
                            "'sum' or 'max'",
                            &Value::Str(other.into()),
                        ));
                    }
                },
            };
            settings.finish()?;
            let p = project(&spec)?;
            let sources = positions(name, &p, &args[0])?;
            let values = match &value {
                Some(key) => node_numbers(ctx, &p, key, 1.0)?,
                None => vec![1.0; p.len()],
            };
            let weighted: Vec<(u32, f64)> = sources
                .iter()
                .map(|at| (*at, values[*at as usize]))
                .collect();
            let map = algo::influence(&p, &weighted, direction, &settings_value, &stop)
                .map_err(stopped)?;
            let mut rows = Vec::new();
            for (at, here) in map.into_iter().enumerate() {
                if here != 0.0 {
                    rows.push(vec![node_value(ctx, &p, at as u32)?, Value::Float(here)]);
                }
            }
            rows
        }
        "algo.floodFill" => {
            let direction = settings.direction(Direction::Both)?;
            let condition = match settings.get("where") {
                None => None,
                Some(Value::Map(map)) => Some(map.clone()),
                Some(other) => {
                    return Err(settings.fail("where", "a map of property values", other));
                }
            };
            let inside_labels = settings.texts("inside")?;
            settings.finish()?;
            let p = project(&spec)?;
            let seeds = positions(name, &p, &args[0])?;
            let mut inside = vec![true; p.len()];
            if condition.is_some() || !inside_labels.is_empty() {
                for (at, slot) in inside.iter_mut().enumerate() {
                    let Some(node) = ctx.node(p.id(at as u32))? else {
                        *slot = false;
                        continue;
                    };
                    let labels_ok = inside_labels.is_empty()
                        || inside_labels.iter().any(|label| node.has_label(label));
                    let props_ok = condition.as_ref().is_none_or(|map| {
                        map.iter().all(|(key, value)| {
                            value
                                .to_property()
                                .is_ok_and(|wanted| node.props.get(key) == Some(&wanted))
                        })
                    });
                    *slot = labels_ok && props_ok;
                }
            }
            let steps = algo::flood_fill(&p, &seeds, &inside, direction);
            let mut rows = Vec::new();
            for (at, found) in steps.into_iter().enumerate() {
                if let Some(steps) = found {
                    rows.push(vec![
                        node_value(ctx, &p, at as u32)?,
                        Value::Int(i64::from(steps)),
                    ]);
                }
            }
            rows
        }
        "algo.diffusion" => {
            let Some(amount) = settings.text("amount")? else {
                return Err(QueryError::runtime(
                    "algo.diffusion() needs amount: the property that holds each node's amount",
                ));
            };
            let capacity = settings.text("capacity")?;
            let direction = settings.direction(Direction::Both)?;
            let rate = settings.float("rate", 0.5)?;
            let steps = settings.count("steps", 10)?;
            settings.finish()?;
            let p = project(&spec)?;
            let initial = node_numbers(ctx, &p, &amount, 0.0)?;
            let capacity = capacity
                .map(|key| node_numbers(ctx, &p, &key, f64::INFINITY))
                .transpose()?;
            let after = algo::diffuse(
                &p,
                &initial,
                direction,
                &algo::Diffusion {
                    rate,
                    steps,
                    capacity,
                },
                &stop,
            )
            .map_err(stopped)?;
            after
                .into_iter()
                .enumerate()
                .map(|(at, value)| Ok(vec![node_value(ctx, &p, at as u32)?, Value::Float(value)]))
                .collect::<Result<_, QueryError>>()?
        }
        "algo.automaton" => {
            let Some(state) = settings.text("state")? else {
                return Err(QueryError::runtime(
                    "algo.automaton() needs state: the property that holds each node's state (true or false)",
                ));
            };
            let counts =
                |key: &'static str, default: Vec<usize>| -> Result<Vec<usize>, QueryError> {
                    match settings.get(key) {
                        None => Ok(default),
                        Some(Value::List(items)) => items
                            .iter()
                            .map(|item| match item {
                                Value::Int(n) if *n >= 0 => Ok(*n as usize),
                                other => Err(settings.fail(key, "a list of whole numbers", other)),
                            })
                            .collect(),
                        Some(other) => Err(settings.fail(key, "a list of whole numbers", other)),
                    }
                };
            let rule = match settings.text("rule")?.as_deref().unwrap_or("life") {
                "life" => algo::Rule::Life {
                    birth: counts("birth", vec![3])?,
                    survive: counts("survive", vec![2, 3])?,
                },
                "threshold" => algo::Rule::Threshold {
                    share: settings.float("share", 0.5)?,
                    recover: settings.flag("recover", false)?,
                },
                "majority" => algo::Rule::Majority,
                other => {
                    return Err(settings.fail(
                        "rule",
                        "'life', 'threshold' or 'majority'",
                        &Value::Str(other.into()),
                    ));
                }
            };
            let steps = settings.count("steps", 10)?;
            let direction = settings.direction(Direction::Both)?;
            settings.finish()?;
            let p = project(&spec)?;
            let initial: Vec<bool> = node_props(ctx, &p, &state)?
                .into_iter()
                .map(|value| value.and_then(|v| v.as_bool()).unwrap_or(false))
                .collect();
            let (after, changed, _) =
                algo::automaton(&p, &initial, &rule, steps, direction, &stop).map_err(stopped)?;
            (0..p.len())
                .map(|at| {
                    Ok(vec![
                        node_value(ctx, &p, at as u32)?,
                        Value::Bool(after[at]),
                        changed[at].map_or(Value::Null, |step| Value::Int(step as i64)),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        "algo.assignment" => {
            let (Some(agents), Some(tasks)) = (settings.text("agents")?, settings.text("tasks")?)
            else {
                return Err(QueryError::runtime(
                    "algo.assignment() needs agents and tasks: the label of each side",
                ));
            };
            settings.finish()?;
            let p = project(&spec)?;
            let rows_at = labelled(ctx, &p, &agents)?;
            let columns_at = labelled(ctx, &p, &tasks)?;
            let column_of: HashMap<u32, usize> = columns_at
                .iter()
                .enumerate()
                .map(|(i, at)| (*at, i))
                .collect();
            let mut costs = vec![vec![None; columns_at.len()]; rows_at.len()];
            for (row, &agent) in rows_at.iter().enumerate() {
                for step in p.neighbors(agent, Direction::Both) {
                    if let Some(&column) = column_of.get(&step.node) {
                        let slot: &mut Option<f64> = &mut costs[row][column];
                        *slot = Some(slot.map_or(step.weight, |c: f64| c.min(step.weight)));
                    }
                }
            }
            let (pairs, _) = algo::assignment(&costs);
            let mut out = Vec::new();
            for (row, column) in pairs.into_iter().enumerate() {
                if let Some(column) = column {
                    out.push(vec![
                        node_value(ctx, &p, rows_at[row])?,
                        node_value(ctx, &p, columns_at[column])?,
                        Value::Float(costs[row][column].unwrap_or(0.0)),
                    ]);
                }
            }
            out
        }
        "algo.minCostFlow" => {
            let (Some(supply), Some(capacity), Some(cost)) = (
                settings.text("supply")?,
                settings.text("capacity")?,
                settings.text("cost")?,
            ) else {
                return Err(QueryError::runtime(
                    "algo.minCostFlow() needs supply (a node property), capacity and cost (edge properties)",
                ));
            };
            settings.finish()?;
            let by_capacity = project(&ProjectionSpec {
                weight: Some(capacity),
                ..spec.clone()
            })?;
            let by_cost = project(&ProjectionSpec {
                weight: Some(cost),
                ..spec.clone()
            })?;
            let (capacities, costs) = (edge_weights(&by_capacity), edge_weights(&by_cost));
            // The two projections hold the same edges in the same order.
            if capacities.len() != costs.len()
                || (0..capacities.len() as u32)
                    .any(|edge| by_capacity.edge_id(edge) != by_cost.edge_id(edge))
            {
                return Err(QueryError::runtime(
                    "algo.minCostFlow(): the graph changed while it was read; run it again",
                ));
            }
            let supplies = node_numbers(ctx, &by_capacity, &supply, 0.0)?;
            let flow = algo::min_cost_flow(&by_capacity, &capacities, &costs, &supplies, &stop)
                .map_err(stopped)?;
            flow.edges
                .iter()
                .map(|(edge, amount)| {
                    Ok(vec![
                        edge_value(ctx, &by_capacity, *edge)?,
                        Value::Float(*amount),
                        Value::Float(flow.value),
                        Value::Float(flow.cost),
                        Value::Float(flow.unmet),
                    ])
                })
                .collect::<Result<_, QueryError>>()?
        }
        other => unreachable!("algorithm {other} without a body"),
    })
}

// ---------------------------------------------------------------- functions

/// The distinct neighbors of a node in both directions, sorted.
fn neighbor_set(ctx: &Context, id: NodeId) -> Result<Vec<NodeId>, QueryError> {
    let mut set: Vec<NodeId> = ctx
        .neighbor_ids(id, Direction::Both, &EdgeFilter::any())?
        .into_iter()
        .map(|(_, other)| other)
        .filter(|other| *other != id)
        .collect();
    set.sort_unstable();
    set.dedup();
    Ok(set)
}

/// The `algo.*` functions, by lowercase name, or `None` when `name` is
/// not one of them.
pub(crate) fn function(
    ctx: &Context,
    name: &str,
    args: &[Value],
) -> Option<Result<Value, QueryError>> {
    let link = match name {
        "algo.commonneighbors" => Some(LinkMetric::CommonNeighbors),
        "algo.adamicadar" => Some(LinkMetric::AdamicAdar),
        "algo.resourceallocation" => Some(LinkMetric::ResourceAllocation),
        "algo.preferentialattachment" => Some(LinkMetric::PreferentialAttachment),
        "algo.totalneighbors" => Some(LinkMetric::TotalNeighbors),
        _ => None,
    };
    let similarity = match name {
        "algo.jaccard" => Some(SimilarityMetric::Jaccard),
        "algo.overlap" => Some(SimilarityMetric::Overlap),
        _ => None,
    };
    if link.is_none() && similarity.is_none() && name != "algo.cosine" {
        return None;
    }
    Some((|| {
        let [a, b] = args else {
            return Err(QueryError::runtime(format!("{name}() takes two arguments")));
        };
        if name == "algo.cosine" {
            let vector = |value: &Value| -> Result<Vec<f64>, QueryError> {
                let Value::List(items) = value else {
                    return Err(QueryError::runtime(
                        "algo.cosine() compares two lists of numbers",
                    ));
                };
                items
                    .iter()
                    .map(|item| match item {
                        Value::Int(n) => Ok(*n as f64),
                        Value::Float(f) => Ok(*f),
                        _ => Err(QueryError::runtime(
                            "algo.cosine() compares two lists of numbers",
                        )),
                    })
                    .collect()
            };
            let (x, y) = (vector(a)?, vector(b)?);
            if x.len() != y.len() {
                return Err(QueryError::runtime(format!(
                    "algo.cosine() needs lists of the same length, got {} and {}",
                    x.len(),
                    y.len()
                )));
            }
            let dot: f64 = x.iter().zip(&y).map(|(p, q)| p * q).sum();
            let norm = x.iter().map(|p| p * p).sum::<f64>().sqrt()
                * y.iter().map(|q| q * q).sum::<f64>().sqrt();
            return Ok(if norm > 0.0 {
                Value::Float(dot / norm)
            } else {
                Value::Null
            });
        }
        let (a, b) = (
            neighbor_set(ctx, node_id(name, a)?)?,
            neighbor_set(ctx, node_id(name, b)?)?,
        );
        Ok(Value::Float(match (link, similarity) {
            (Some(metric), _) => {
                let mut degrees = HashMap::new();
                let needs_degrees = matches!(
                    metric,
                    LinkMetric::AdamicAdar | LinkMetric::ResourceAllocation
                );
                if needs_degrees {
                    for w in a.iter().filter(|w| b.binary_search(w).is_ok()) {
                        degrees.insert(*w, neighbor_set(ctx, *w)?.len());
                    }
                }
                algo::link_score(&a, &b, metric, |w| degrees.get(&w).copied().unwrap_or(0))
            }
            (None, Some(metric)) => algo::neighbor_similarity(&a, &b, metric),
            (None, None) => unreachable!("checked above"),
        }))
    })())
}
