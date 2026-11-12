//! Traversals over a read snapshot: neighbors, k hops, shortest path and subgraph.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

use grafeno_core::{Edge, EdgeId, Error, Node, NodeId, Result, Timestamp};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use crate::store::{ADJ_IN, ADJ_OUT, Adj, AdjKey, Adjacent, adjacency, adjacency_each, storage};
use crate::{ReadTx, WriteTx};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Out,
    In,
    #[default]
    Both,
}

/// Which edges a traversal follows.
///
/// By default, current knowledge: edges that were not retracted, with any
/// label, regardless of their valid time.
#[derive(Clone, Debug, Default)]
pub struct EdgeFilter {
    labels: Vec<String>,
    valid_at: Option<Timestamp>,
    known_at: Option<Timestamp>,
    /// Only what this caller may see (14.4): traversals do not go through
    /// hidden nodes or edges, and records come without hidden properties.
    view: Option<std::sync::Arc<crate::View>>,
}

impl EdgeFilter {
    pub fn any() -> Self {
        Self::default()
    }

    /// Only edges with this label (can be repeated).
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }

    /// Only what `view` may see (14.4).
    pub fn with_view(mut self, view: Option<std::sync::Arc<crate::View>>) -> Self {
        self.view = view;
        self
    }

    /// The view the filter applies, if any.
    pub fn view(&self) -> Option<&crate::View> {
        self.view.as_deref()
    }

    /// Only facts that are true at `at`.
    pub fn valid_at(mut self, at: Timestamp) -> Self {
        self.valid_at = Some(at);
        self
    }

    /// What was known at `at`, including edges retracted later.
    pub fn known_at(mut self, at: Timestamp) -> Self {
        self.known_at = Some(at);
        self
    }

    pub fn accepts(&self, edge: &Edge) -> bool {
        let known = match self.known_at {
            Some(at) => edge.known_at(at),
            None => !edge.is_retracted(),
        };
        known
            && self.valid_at.is_none_or(|at| edge.holds_at(at))
            && (self.labels.is_empty() || self.labels.contains(&edge.label))
    }

    /// Like [`EdgeFilter::accepts`], for an adjacency entry. Labels are not
    /// checked here: entries are read by label already.
    pub(crate) fn accepts_entry(&self, entry: &Adjacent) -> bool {
        let known = match self.known_at {
            Some(at) => entry.recorded_at <= at.as_micros() && at.as_micros() < entry.expired_at,
            None => entry.expired_at == i64::MAX,
        };
        known
            && self.valid_at.is_none_or(|at| {
                entry.valid_from <= at.as_micros() && at.as_micros() < entry.valid_to
            })
    }
}

/// A neighbor: the node at the other end and the edge that leads to it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Neighbor {
    pub node: NodeId,
    pub edge: Edge,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Path {
    /// From the origin to the target, both included.
    pub nodes: Vec<NodeId>,
    /// `edges[i]` joins `nodes[i]` and `nodes[i + 1]`.
    pub edges: Vec<EdgeId>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Subgraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl ReadTx {
    pub fn neighbors(
        &self,
        ns: &str,
        node: NodeId,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Vec<Neighbor>> {
        let mut out = Vec::new();
        for entry in self.adjacent(ns, node, direction, filter)? {
            let mut edge = self
                .edge(ns, entry.edge)?
                .ok_or(Error::EdgeNotFound(entry.edge))?;
            if let Some(view) = filter.view() {
                edge = view
                    .edge(ns, edge, |_| Ok(true))?
                    .ok_or(Error::EdgeNotFound(entry.edge))?;
            }
            out.push(Neighbor {
                node: entry.other,
                edge,
            });
        }
        Ok(out)
    }

    /// The edges of `node` that pass `filter`, as `(edge, other end)`, each
    /// edge once. The cheap form of [`ReadTx::neighbors`]: it reads no edge
    /// records.
    pub fn neighbor_ids(
        &self,
        ns: &str,
        node: NodeId,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        Ok(self
            .adjacent(ns, node, direction, filter)?
            .into_iter()
            .map(|entry| (entry.edge, entry.other))
            .collect())
    }

    /// The adjacency entries of `node` that pass `filter`, each edge once.
    /// Reads no edge records: the filter's fields are in the entries.
    pub(crate) fn adjacent(
        &self,
        ns: &str,
        node: NodeId,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Vec<Adjacent>> {
        let entries = adjacent(&self.out, &self.inn, ns, node, direction, filter)?;
        let Some(view) = filter.view().or(self.view.as_deref()) else {
            return Ok(entries);
        };
        if !view.reads(ns) {
            return Ok(Vec::new());
        }
        if !view.per_record(ns) {
            return Ok(entries);
        }
        let sees = |id: NodeId| -> Result<bool> {
            let node: Option<Node> = crate::store::read_record(&self.nodes, ns, id.as_u128())?;
            Ok(node.is_some_and(|n| view.node_visible(ns, &n)))
        };
        // From a node the caller may not see, nothing leads anywhere.
        if !sees(node)? {
            return Ok(Vec::new());
        }
        let mut visible = Vec::with_capacity(entries.len());
        for entry in entries {
            let edge: Option<Edge> =
                crate::store::read_record(&self.edges, ns, entry.edge.as_u128())?;
            if let Some(edge) = edge
                && view.edge_visible(ns, &edge, sees)?
            {
                visible.push(entry);
            }
        }
        Ok(visible)
    }

    /// How many edges of `node` pass `filter`, counting up to `at_most`
    /// (at most `at_most` entries are read, however many it has). With
    /// [`Direction::Both`], a self-loop counts twice. Under a view that
    /// decides record by record, the edges it lets the caller see.
    pub fn degree(
        &self,
        ns: &str,
        node: NodeId,
        direction: Direction,
        filter: &EdgeFilter,
        at_most: usize,
    ) -> Result<usize> {
        if let Some(view) = filter.view().or(self.view.as_deref()) {
            if !view.reads(ns) {
                return Ok(0);
            }
            if view.per_record(ns) {
                return Ok(self
                    .adjacent(ns, node, direction, filter)?
                    .len()
                    .min(at_most));
            }
        }
        let mut count = 0;
        let mut tables: Vec<&redb::ReadOnlyTable<AdjKey, Adj>> = Vec::new();
        if matches!(direction, Direction::Out | Direction::Both) {
            tables.push(&self.out);
        }
        if matches!(direction, Direction::In | Direction::Both) {
            tables.push(&self.inn);
        }
        let labels: Vec<Option<&str>> = if filter.labels.is_empty() {
            vec![None]
        } else {
            filter
                .labels
                .iter()
                .map(|label| Some(label.as_str()))
                .collect()
        };
        for table in tables {
            for label in &labels {
                if count >= at_most {
                    return Ok(at_most);
                }
                adjacency_each(table, ns, node, *label, |entry| {
                    if filter.accepts_entry(entry) {
                        count += 1;
                    }
                    count < at_most
                })?;
            }
        }
        Ok(count.min(at_most))
    }

    /// Nodes `1..=depth` hops away from `start`, with their distance, in BFS order.
    pub fn k_hop(
        &self,
        ns: &str,
        start: NodeId,
        depth: usize,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Vec<(NodeId, usize)>> {
        let mut distance = HashMap::from([(start, 0usize)]);
        let mut queue = VecDeque::from([start]);
        let mut out = Vec::new();
        while let Some(current) = queue.pop_front() {
            let d = distance[&current];
            if d == depth {
                continue;
            }
            for neighbor in self.adjacent(ns, current, direction, filter)? {
                if let Entry::Vacant(entry) = distance.entry(neighbor.other) {
                    entry.insert(d + 1);
                    out.push((neighbor.other, d + 1));
                    queue.push_back(neighbor.other);
                }
            }
        }
        Ok(out)
    }

    /// The path with the fewest hops from `from` to `to`, of at most `max_depth` hops.
    pub fn shortest_path(
        &self,
        ns: &str,
        from: NodeId,
        to: NodeId,
        max_depth: usize,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Option<Path>> {
        if from == to {
            return Ok(Some(Path {
                nodes: vec![from],
                edges: Vec::new(),
            }));
        }
        let mut parent: HashMap<NodeId, (NodeId, EdgeId)> = HashMap::new();
        let mut distance = HashMap::from([(from, 0usize)]);
        let mut queue = VecDeque::from([from]);
        while let Some(current) = queue.pop_front() {
            let d = distance[&current];
            if d == max_depth {
                continue;
            }
            for neighbor in self.adjacent(ns, current, direction, filter)? {
                if distance.contains_key(&neighbor.other) {
                    continue;
                }
                distance.insert(neighbor.other, d + 1);
                parent.insert(neighbor.other, (current, neighbor.edge));
                if neighbor.other == to {
                    return Ok(Some(rebuild(&parent, from, to)));
                }
                queue.push_back(neighbor.other);
            }
        }
        Ok(None)
    }

    /// The nodes at most `depth` hops away from `seeds` and the edges among them.
    pub fn subgraph(
        &self,
        ns: &str,
        seeds: &[NodeId],
        depth: usize,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Subgraph> {
        let mut order: Vec<NodeId> = Vec::new();
        let mut members = HashSet::new();
        for &seed in seeds {
            if members.insert(seed) {
                order.push(seed);
            }
            for (node, _) in self.k_hop(ns, seed, depth, direction, filter)? {
                if members.insert(node) {
                    order.push(node);
                }
            }
        }
        let mut result = Subgraph::default();
        let mut seen_edges = HashSet::new();
        for &id in &order {
            let node = self.node(ns, id)?.ok_or(Error::NodeNotFound(id))?;
            let node = match filter.view() {
                Some(view) => view.node(ns, node).ok_or(Error::NodeNotFound(id))?,
                None => node,
            };
            result.nodes.push(node);
            for neighbor in self.neighbors(ns, id, Direction::Out, filter)? {
                if members.contains(&neighbor.node) && seen_edges.insert(neighbor.edge.id) {
                    result.edges.push(neighbor.edge);
                }
            }
        }
        Ok(result)
    }
}

fn rebuild(parent: &HashMap<NodeId, (NodeId, EdgeId)>, from: NodeId, to: NodeId) -> Path {
    let mut nodes = vec![to];
    let mut edges = Vec::new();
    let mut current = to;
    while current != from {
        let (previous, edge) = parent[&current];
        nodes.push(previous);
        edges.push(edge);
        current = previous;
    }
    nodes.reverse();
    edges.reverse();
    Path { nodes, edges }
}

impl WriteTx {
    /// [`ReadTx::neighbor_ids`] as written so far in this transaction.
    pub fn neighbor_ids(
        &self,
        ns: &str,
        node: NodeId,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Vec<(EdgeId, NodeId)>> {
        let out = self.tx.open_table(ADJ_OUT).map_err(storage)?;
        let inn = self.tx.open_table(ADJ_IN).map_err(storage)?;
        Ok(adjacent(&out, &inn, ns, node, direction, filter)?
            .into_iter()
            .map(|entry| (entry.edge, entry.other))
            .collect())
    }
}

/// The adjacency entries of `node` that pass `filter`, each edge once.
fn adjacent(
    out: &impl ReadableTable<AdjKey, Adj>,
    inn: &impl ReadableTable<AdjKey, Adj>,
    ns: &str,
    node: NodeId,
    direction: Direction,
    filter: &EdgeFilter,
) -> Result<Vec<Adjacent>> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut collect = |entries: Vec<Adjacent>| {
        for entry in entries {
            // With `Both`, a self-loop shows up in both tables.
            if filter.accepts_entry(&entry) && seen.insert(entry.edge) {
                result.push(entry);
            }
        }
    };
    let read = |table: &dyn Fn(Option<&str>) -> Result<Vec<Adjacent>>| -> Result<Vec<Adjacent>> {
        if filter.labels.is_empty() {
            table(None)
        } else {
            let mut entries = Vec::new();
            for label in &filter.labels {
                entries.extend(table(Some(label))?);
            }
            Ok(entries)
        }
    };
    if matches!(direction, Direction::Out | Direction::Both) {
        collect(read(&|label| adjacency(out, ns, node, label))?);
    }
    if matches!(direction, Direction::In | Direction::Both) {
        collect(read(&|label| adjacency(inn, ns, node, label))?);
    }
    Ok(result)
}
