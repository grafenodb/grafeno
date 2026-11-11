//! Graph algorithms over an in-memory projection of a namespace.
//!
//! A [`Projection`] holds the nodes of a namespace (or of some labels) and
//! the edges among them as compressed adjacency arrays (CSR): the
//! neighbors of a node are one slice of a shared array, in both
//! directions. Algorithms that visit the whole graph many times (PageRank,
//! Louvain, betweenness) then read no storage at all.
//!
//! [`ReadTx::projection`] builds one on demand and keeps it in a small
//! cache shared by every read transaction of the database, until the next
//! commit changes the data.
//!
//! Nodes are numbered `0..len()` in id order (creation order); algorithms
//! take and return these positions, and [`Projection::id`] turns them back
//! into node ids.

mod analysis;
mod centrality;
mod community;
mod embedding;
mod paths;
mod similarity;
mod simulation;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use grafeno_core::{Edge, EdgeId, Error, NodeId, Result, Timestamp};
use redb::ReadableTable;

pub use analysis::{
    Flow, bipartite_matching, conductance, cut_points, eigenvector, hits, k_core, leiden, max_flow,
    random_walks, spanning_forest, topological_order,
};
pub use centrality::{PageRank, betweenness, closeness, degree, page_rank};
pub use community::{
    Louvain, Triangles, label_propagation, louvain, louvain_levels, modularity, strongly_connected,
    triangles, weakly_connected,
};
pub use embedding::{FastRp, fast_rp};
pub use paths::{
    WeightedPath, all_shortest_paths, astar, distances, k_shortest_paths, shortest_path,
};
pub use similarity::{
    LinkMetric, SimilarityMetric, link_score, neighbor_similarity, predict_links, similar_neighbors,
};
pub use simulation::{
    CostFlow, Diffusion, Influence, Rule, assignment, automaton, diffuse, distance_field,
    flood_fill, influence, min_cost_flow,
};

use crate::store::{Adj, AdjKey, Adjacent, LabelKey, RecordKey, decode, nodes_with_label, storage};
use crate::traverse::{Direction, EdgeFilter};
use crate::{ReadTx, WriteTx};

/// Which part of a namespace a projection holds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ProjectionSpec {
    /// Nodes with any of these labels; every node when empty.
    pub labels: Vec<String>,
    /// Edges with any of these labels; every edge when empty.
    pub edge_labels: Vec<String>,
    /// Only facts true at this time.
    pub valid_at: Option<Timestamp>,
    /// What was known at this time; current knowledge (no retracted
    /// edges) when `None`.
    pub known_at: Option<Timestamp>,
    /// The edge property that weighs each edge. Edges without it, or with
    /// a value that is not a number, weigh 1.
    pub weight: Option<String>,
    /// Keep the id of every edge (16 bytes each), for algorithms that
    /// return edges (paths). Without it, [`Projection::edge_id`] is `None`.
    pub edge_ids: bool,
    /// Only what this caller may see (14.4): algorithms run on the visible
    /// subgraph.
    pub view: Option<ViewRef>,
}

/// A caller's view in a [`ProjectionSpec`]: the same view (the same
/// allocation) is the same projection.
#[derive(Clone, Debug)]
pub struct ViewRef(pub Arc<crate::View>);

impl PartialEq for ViewRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ViewRef {}

impl std::hash::Hash for ViewRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

impl ProjectionSpec {
    fn filter(&self) -> EdgeFilter {
        let mut filter = EdgeFilter::any();
        if let Some(at) = self.valid_at {
            filter = filter.valid_at(at);
        }
        if let Some(at) = self.known_at {
            filter = filter.known_at(at);
        }
        filter
    }
}

/// The out-edges of every node: the edges of node `i` are
/// `start[i]..start[i + 1]` in the other arrays. An edge's position there
/// is its number in the projection.
#[derive(Debug, Default)]
struct Out {
    start: Vec<u32>,
    other: Vec<u32>,
    /// Empty unless the projection keeps edge ids.
    edge: Vec<EdgeId>,
    /// Empty when the projection is not weighted.
    weight: Vec<f64>,
}

/// The in-edges of every node, by the positions of the out-edges.
#[derive(Debug, Default)]
struct In {
    start: Vec<u32>,
    /// The node each in-edge comes from.
    other: Vec<u32>,
    /// The out-edge each in-edge is.
    edge: Vec<u32>,
}

fn range(start: &[u32], node: u32) -> std::ops::Range<usize> {
    start[node as usize] as usize..start[node as usize + 1] as usize
}

/// Builds a projection from out-edges given in order of their source
/// node: `push` them, then `finish`.
pub(crate) struct Builder {
    ids: Vec<NodeId>,
    out: Out,
    weighted: bool,
    edge_ids: bool,
    /// The source of the last edge pushed.
    last: u32,
}

impl Builder {
    /// `ids` must be sorted and without duplicates.
    pub(crate) fn new(ids: Vec<NodeId>, weighted: bool, edge_ids: bool) -> Self {
        let mut start = Vec::with_capacity(ids.len() + 1);
        start.push(0);
        Self {
            ids,
            out: Out {
                start,
                ..Out::default()
            },
            weighted,
            edge_ids,
            last: 0,
        }
    }

    pub(crate) fn position(&self, id: NodeId) -> Option<u32> {
        self.ids.binary_search(&id).ok().map(|at| at as u32)
    }

    fn reserve(&mut self, edges: usize) {
        self.out.other.reserve_exact(edges);
        if self.edge_ids {
            self.out.edge.reserve_exact(edges);
        }
        if self.weighted {
            self.out.weight.reserve_exact(edges);
        }
    }

    /// Adds an edge; sources must not decrease.
    pub(crate) fn push(&mut self, from: u32, to: u32, edge: EdgeId, weight: f64) -> Result<()> {
        debug_assert!(from >= self.last, "edges pushed in source order");
        if self.out.other.len() >= u32::MAX as usize {
            return Err(Error::Invalid(
                "a projection holds at most 4 294 967 295 edges".into(),
            ));
        }
        self.close_up_to(from);
        self.out.other.push(to);
        if self.edge_ids {
            self.out.edge.push(edge);
        }
        if self.weighted {
            self.out.weight.push(weight);
        }
        Ok(())
    }

    /// Ends the edge lists of every node before `node`.
    fn close_up_to(&mut self, node: u32) {
        let at = self.out.other.len() as u32;
        while self.out.start.len() <= node as usize {
            self.out.start.push(at);
        }
        self.last = node;
    }

    pub(crate) fn finish(mut self) -> Projection {
        let len = self.ids.len() as u32;
        self.close_up_to(len);
        // The ends: `start` has `len + 1` entries.
        let total = self.out.other.len() as u32;
        self.out.start.truncate(len as usize);
        self.out.start.push(total);
        // A copy to shrink is worth it only for much room left over.
        let used = self.out.other.len();
        if self.out.other.capacity() > used + used / 8 + 1024 {
            self.out.other.shrink_to_fit();
            self.out.edge.shrink_to_fit();
            self.out.weight.shrink_to_fit();
        }
        // The in-edges, by counting.
        let mut start = vec![0u32; len as usize + 1];
        for &to in &self.out.other {
            start[to as usize + 1] += 1;
        }
        for i in 0..len as usize {
            start[i + 1] += start[i];
        }
        let mut next = start.clone();
        let mut other = vec![0u32; total as usize];
        let mut edge = vec![0u32; total as usize];
        for from in 0..len {
            for at in range(&self.out.start, from) {
                let to = self.out.other[at] as usize;
                let slot = next[to] as usize;
                next[to] += 1;
                other[slot] = from;
                edge[slot] = at as u32;
            }
        }
        Projection {
            ids: self.ids,
            out: self.out,
            inn: In { start, other, edge },
            weighted: self.weighted,
        }
    }
}

/// One edge seen from a node: the node at the other end, the edge's
/// number in the projection (see [`Projection::edge_id`]), and its weight
/// (1 when the projection is not weighted).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub node: u32,
    pub edge: u32,
    pub weight: f64,
}

/// The nodes of a namespace and the edges among them, in memory: about 12
/// bytes per edge, 16 more with edge ids and 8 more with weights, and 24
/// per node.
#[derive(Debug, Default)]
pub struct Projection {
    ids: Vec<NodeId>,
    out: Out,
    inn: In,
    weighted: bool,
}

impl Projection {
    /// A projection of these nodes and edges `(from, to, edge, weight)`,
    /// keeping edge ids. Edges with an end outside `ids` are left out.
    pub fn from_edges(
        mut ids: Vec<NodeId>,
        edges: impl IntoIterator<Item = (NodeId, NodeId, EdgeId, f64)>,
        weighted: bool,
    ) -> Self {
        ids.sort_unstable();
        ids.dedup();
        let mut builder = Builder::new(ids, weighted, true);
        let mut links: Vec<(u32, u32, EdgeId, f64)> = edges
            .into_iter()
            .filter_map(|(from, to, edge, weight)| {
                Some((builder.position(from)?, builder.position(to)?, edge, weight))
            })
            .collect();
        links.sort_by_key(|link| link.0);
        for (from, to, edge, weight) in links {
            builder
                .push(from, to, edge, weight)
                .expect("fewer than 2^32 edges");
        }
        builder.finish()
    }

    /// How many nodes it holds.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// How many edges it holds.
    pub fn edge_count(&self) -> usize {
        self.out.other.len()
    }

    /// Whether edges carry weights from a property.
    pub fn is_weighted(&self) -> bool {
        self.weighted
    }

    /// Whether it keeps the ids of its edges ([`ProjectionSpec::edge_ids`]).
    pub fn has_edge_ids(&self) -> bool {
        !self.out.edge.is_empty() || self.out.other.is_empty()
    }

    /// The id of edge number `edge`, when the projection keeps edge ids.
    pub fn edge_id(&self, edge: u32) -> Option<EdgeId> {
        self.out.edge.get(edge as usize).copied()
    }

    /// The memory it takes, in bytes.
    pub fn bytes(&self) -> usize {
        use std::mem::size_of;
        self.ids.capacity() * size_of::<NodeId>()
            + (self.out.start.capacity() + self.out.other.capacity()) * 4
            + self.out.edge.capacity() * size_of::<EdgeId>()
            + self.out.weight.capacity() * 8
            + (self.inn.start.capacity() + self.inn.other.capacity() + self.inn.edge.capacity()) * 4
    }

    /// The id of the node at `position`.
    pub fn id(&self, position: u32) -> NodeId {
        self.ids[position as usize]
    }

    /// The position of a node, if the projection holds it.
    pub fn position(&self, id: NodeId) -> Option<u32> {
        self.ids.binary_search(&id).ok().map(|at| at as u32)
    }

    /// The edges of `node` in `direction`. With `Both`, a self-loop shows
    /// up once.
    pub fn neighbors(&self, node: u32, direction: Direction) -> impl Iterator<Item = Step> + '_ {
        let out = match direction {
            Direction::Out | Direction::Both => range(&self.out.start, node),
            Direction::In => 0..0,
        };
        let inn = match direction {
            Direction::In | Direction::Both => range(&self.inn.start, node),
            Direction::Out => 0..0,
        };
        let both = direction == Direction::Both;
        let weight = move |edge: usize| {
            if self.weighted {
                self.out.weight[edge]
            } else {
                1.0
            }
        };
        out.map(move |at| Step {
            node: self.out.other[at],
            edge: at as u32,
            weight: weight(at),
        })
        .chain(
            inn.filter(move |&at| !(both && self.inn.other[at] == node))
                .map(move |at| {
                    let edge = self.inn.edge[at];
                    Step {
                        node: self.inn.other[at],
                        edge,
                        weight: weight(edge as usize),
                    }
                }),
        )
    }

    /// The nodes the out-edges of `node` lead to.
    pub(crate) fn targets(&self, node: u32) -> &[u32] {
        &self.out.other[range(&self.out.start, node)]
    }

    /// How many edges `node` has in `direction` (a self-loop counts once
    /// with `Both`).
    pub fn degree(&self, node: u32, direction: Direction) -> usize {
        match direction {
            Direction::Out => range(&self.out.start, node).len(),
            Direction::In => range(&self.inn.start, node).len(),
            Direction::Both => self.neighbors(node, direction).count(),
        }
    }

    /// The distinct neighbors of every node in `direction`, sorted, without
    /// the node itself: the simple graph that similarity, link prediction
    /// and triangle counting work on.
    pub(crate) fn neighbor_sets(&self, direction: Direction) -> Vec<Vec<u32>> {
        (0..self.len() as u32)
            .map(|node| {
                let mut set: Vec<u32> = self
                    .neighbors(node, direction)
                    .map(|step| step.node)
                    .filter(|&other| other != node)
                    .collect();
                set.sort_unstable();
                set.dedup();
                set
            })
            .collect()
    }
}

/// Builds a projection from the tables of a read or write transaction.
fn project(
    nodes: &impl ReadableTable<RecordKey, &'static [u8]>,
    labels: &impl ReadableTable<LabelKey, ()>,
    out: &impl ReadableTable<AdjKey, Adj>,
    edges: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    spec: &ProjectionSpec,
    edge_count: u64,
) -> Result<Projection> {
    let ids = if spec.labels.is_empty() {
        let mut ids = Vec::new();
        for item in nodes.range((ns, 0u128)..).map_err(storage)? {
            let (key, _) = item.map_err(storage)?;
            let (key_ns, id) = key.value();
            if key_ns != ns {
                break;
            }
            ids.push(NodeId::from_u128(id));
        }
        ids
    } else {
        let mut ids = Vec::new();
        for label in &spec.labels {
            ids.extend(nodes_with_label(labels, ns, label)?);
        }
        ids
    };
    let mut ids = ids;
    ids.sort_unstable();
    ids.dedup();
    let view = spec.view.as_ref().map(|v| v.0.as_ref());
    if let Some(view) = view {
        if !view.reads(ns) {
            ids.clear();
        } else if view.per_record(ns) {
            let mut visible = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(guard) = nodes.get((ns, id.as_u128())).map_err(storage)?
                    && view.node_visible(ns, &decode::<grafeno_core::Node>(guard.value())?)
                {
                    visible.push(id);
                }
            }
            ids = visible;
        }
    }
    let per_record = view.filter(|view| view.per_record(ns));
    let mut builder = Builder::new(ids, spec.weight.is_some(), spec.edge_ids);
    // Without a label filter nearly every edge goes in: room for all of them
    // up front, instead of arrays that double as they grow (and briefly
    // take up to three times their size).
    if spec.edge_labels.is_empty() && spec.labels.is_empty() {
        builder.reserve(usize::try_from(edge_count).unwrap_or(0));
    }
    let filter = spec.filter();
    // Adjacency is in order of the source id, as positions are.
    let mut source: Option<(u128, Option<u32>)> = None;
    for item in out.range((ns, 0u128, "", 0u128)..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let (key_ns, from, label, edge) = key.value();
        if key_ns != ns {
            break;
        }
        if !spec.edge_labels.is_empty() && !spec.edge_labels.iter().any(|l| l == label) {
            continue;
        }
        let from_position = match source {
            Some((id, position)) if id == from => position,
            _ => {
                let position = builder.position(NodeId::from_u128(from));
                source = Some((from, position));
                position
            }
        };
        let Some(from_position) = from_position else {
            continue;
        };
        let (to, recorded_at, expired_at, valid_from, valid_to) = value.value();
        let Some(to_position) = builder.position(NodeId::from_u128(to)) else {
            continue;
        };
        let entry = Adjacent {
            edge: EdgeId::from_u128(edge),
            other: NodeId::from_u128(to),
            recorded_at,
            expired_at,
            valid_from,
            valid_to,
        };
        if !filter.accepts_entry(&entry) {
            continue;
        }
        // Both ends are visible (they have positions); the edge itself too?
        let record = match (per_record, &spec.weight) {
            (None, None) => None,
            _ => match edges.get((ns, edge)).map_err(storage)? {
                Some(guard) => Some(decode::<Edge>(guard.value())?),
                None => None,
            },
        };
        if let (Some(view), Some(record)) = (per_record, &record)
            && !view.edge_visible(ns, record, |_| Ok(true))?
        {
            continue;
        }
        let weight = match (&spec.weight, &record) {
            (Some(key), Some(record))
                if !per_record.is_some_and(|view| {
                    view.property_hidden(ns, std::slice::from_ref(&record.label), key)
                }) =>
            {
                record
                    .props
                    .get(key)
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(1.0)
            }
            _ => 1.0,
        };
        builder.push(from_position, to_position, entry.edge, weight)?;
    }
    Ok(builder.finish())
}

// ---------------------------------------------------------------- cache

/// The memory projections kept between read transactions may take, by
/// default (see [`crate::OpenOptions`]).
pub const DEFAULT_PROJECTION_CACHE: usize = 256 * 1024 * 1024;

/// What every transaction of a database shares: a commit counter and the
/// projection cache.
pub(crate) struct Shared {
    /// Odd while a commit is under way; it grows by two with each commit.
    generation: AtomicU64,
    projections: Mutex<Vec<Cached>>,
    /// The bytes the cached projections may take.
    projection_budget: std::sync::atomic::AtomicUsize,
    /// The change log's window and commit listeners.
    pub(crate) changes: crate::changes::Hub,
    /// The planner's statistics per namespace.
    pub(crate) planner: crate::planstats::Cache,
    /// Whether commits append to the replication log.
    pub(crate) replication: std::sync::atomic::AtomicBool,
    /// How much of the audit log is kept (14.5).
    pub(crate) audit_retention: crate::audit::Retention,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            generation: AtomicU64::new(0),
            projections: Mutex::default(),
            projection_budget: std::sync::atomic::AtomicUsize::new(DEFAULT_PROJECTION_CACHE),
            changes: crate::changes::Hub::default(),
            audit_retention: crate::audit::Retention::default(),
            planner: crate::planstats::Cache::default(),
            replication: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

struct Cached {
    generation: u64,
    ns: String,
    spec: ProjectionSpec,
    projection: Arc<Projection>,
}

impl Shared {
    pub(crate) fn set_projection_budget(&self, bytes: usize) {
        self.projection_budget.store(bytes, Ordering::Relaxed);
        let mut cached = self.projections.lock().unwrap_or_else(|e| e.into_inner());
        while cached.iter().map(|c| c.projection.bytes()).sum::<usize>() > bytes {
            cached.remove(0);
        }
    }

    /// The bytes the cached projections take now.
    pub(crate) fn projection_bytes(&self) -> usize {
        let cached = self.projections.lock().unwrap_or_else(|e| e.into_inner());
        cached.iter().map(|c| c.projection.bytes()).sum()
    }

    /// Runs a commit, so that readers know the data changed.
    pub(crate) fn committing<T>(&self, commit: impl FnOnce() -> T) -> T {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let result = commit();
        self.generation.fetch_add(1, Ordering::SeqCst);
        result
    }

    /// Opens a snapshot with `begin`, and tells which generation it sees,
    /// if no commit ran meanwhile.
    pub(crate) fn snapshot<T>(&self, begin: impl FnOnce() -> T) -> (T, Option<u64>) {
        let before = self.generation.load(Ordering::SeqCst);
        let snapshot = begin();
        let after = self.generation.load(Ordering::SeqCst);
        let generation = (before == after && before.is_multiple_of(2)).then_some(before);
        (snapshot, generation)
    }

    fn get(&self, generation: u64, ns: &str, spec: &ProjectionSpec) -> Option<Arc<Projection>> {
        let cached = self.projections.lock().unwrap_or_else(|e| e.into_inner());
        cached
            .iter()
            .find(|c| c.generation == generation && c.ns == ns && &c.spec == spec)
            .map(|c| c.projection.clone())
    }

    fn put(&self, generation: u64, ns: &str, spec: &ProjectionSpec, projection: Arc<Projection>) {
        let mut cached = self.projections.lock().unwrap_or_else(|e| e.into_inner());
        // Older generations will not be asked for again by new readers.
        cached.retain(|c| c.generation >= generation);
        let budget = self.projection_budget.load(Ordering::Relaxed);
        let bytes = projection.bytes();
        if bytes > budget {
            return;
        }
        // The oldest go first, until the new one fits.
        while cached.iter().map(|c| c.projection.bytes()).sum::<usize>() + bytes > budget {
            cached.remove(0);
        }
        cached.push(Cached {
            generation,
            ns: ns.to_owned(),
            spec: spec.clone(),
            projection,
        });
    }
}

impl ReadTx {
    /// The projection of `ns` that `spec` describes, from the cache when
    /// this snapshot's data has been projected before.
    pub fn projection(&self, ns: &str, spec: &ProjectionSpec) -> Result<Arc<Projection>> {
        if let Some(generation) = self.generation
            && let Some(hit) = self.shared.get(generation, ns, spec)
        {
            return Ok(hit);
        }
        let edge_count = self.stats(ns)?.edges;
        let projection = Arc::new(project(
            &self.nodes,
            &self.labels,
            &self.out,
            &self.edges,
            ns,
            spec,
            edge_count,
        )?);
        if let Some(generation) = self.generation {
            self.shared.put(generation, ns, spec, projection.clone());
        }
        Ok(projection)
    }
}

impl WriteTx {
    /// [`ReadTx::projection`] of the data as written so far in this
    /// transaction (never cached).
    pub fn projection(&self, ns: &str, spec: &ProjectionSpec) -> Result<Arc<Projection>> {
        use crate::store::{ADJ_OUT, EDGES, NODE_LABELS, NODES};
        let nodes = self.tx.open_table(NODES).map_err(storage)?;
        let labels = self.tx.open_table(NODE_LABELS).map_err(storage)?;
        let out = self.tx.open_table(ADJ_OUT).map_err(storage)?;
        let edges = self.tx.open_table(EDGES).map_err(storage)?;
        let edge_count = self
            .tx
            .open_table(crate::store::NS_COUNTS)
            .map_err(storage)?
            .get(ns)
            .map_err(storage)?
            .map_or(0, |guard| guard.value().1);
        Ok(Arc::new(project(
            &nodes, &labels, &out, &edges, ns, spec, edge_count,
        )?))
    }
}

/// Stops an algorithm when it returns true (a time limit or a cancelled
/// request). Checked between rounds and every few hundred nodes.
pub type Stop<'a> = &'a (dyn Fn() -> bool + Sync);

/// A [`Stop`] that never stops.
pub fn never() -> bool {
    false
}

pub(crate) fn stopped() -> Error {
    Error::Invalid("the algorithm was stopped".into())
}

pub(crate) fn check(stop: Stop, i: usize) -> Result<()> {
    if i.is_multiple_of(256) && stop() {
        return Err(stopped());
    }
    Ok(())
}

/// A small fast random generator (SplitMix64) so results repeat for a seed.
pub(crate) struct Random(u64);

impl Random {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A projection from edges between positions (ids `1..=n`).
    pub(crate) fn graph(n: usize, edges: &[(u32, u32)]) -> Projection {
        weighted(
            n,
            &edges.iter().map(|&(a, b)| (a, b, 1.0)).collect::<Vec<_>>(),
            false,
        )
    }

    pub(crate) fn weighted(n: usize, edges: &[(u32, u32, f64)], weighted: bool) -> Projection {
        let ids: Vec<NodeId> = (1..=n as u128).map(NodeId::from_u128).collect();
        Projection::from_edges(
            ids,
            edges.iter().enumerate().map(|(at, &(a, b, w))| {
                (
                    NodeId::from_u128(a as u128 + 1),
                    NodeId::from_u128(b as u128 + 1),
                    EdgeId::from_u128(at as u128 + 1),
                    w,
                )
            }),
            weighted,
        )
    }

    #[test]
    fn neighbors_in_each_direction() {
        let p = graph(3, &[(0, 1), (1, 2), (2, 2)]);
        let out: Vec<u32> = p.neighbors(1, Direction::Out).map(|s| s.node).collect();
        let inn: Vec<u32> = p.neighbors(1, Direction::In).map(|s| s.node).collect();
        let both: Vec<u32> = p.neighbors(2, Direction::Both).map(|s| s.node).collect();
        assert_eq!(out, vec![2]);
        assert_eq!(inn, vec![0]);
        assert_eq!(both, vec![2, 1], "the self-loop once");
        assert_eq!(p.edge_count(), 3);
        assert_eq!(p.degree(2, Direction::Both), 2);
    }
}
