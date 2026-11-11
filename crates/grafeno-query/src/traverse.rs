//! Batched and parallel traversal: a pattern grows from a batch of
//! anchors one step at a time, every branch of the batch together.
//!
//! The partial matches of a batch form a frontier. At each step the
//! distinct nodes the frontier stands on are read once, in id order (a
//! forward walk through the adjacency B-tree, whose keys start with the
//! node), and what was read is kept for the rest of the traversal: each
//! node's edges for that step, each target node decoded once, each edge
//! record once, the reach of a variable-length step from each node once.
//! So 10 000 people who work at 1 000 companies read each company's
//! colleagues once, not once per employee, and decode each colleague once.
//!
//! The caches belong to the whole traversal from one input row and are
//! shared by every thread working on it (maps split in shards, each behind a
//! read-write lock), so the chunks of a parallel scan and the slices of a
//! large frontier reuse each other's reads. Each map is bounded; past its
//! bound it is emptied and fills again.
//!
//! The frontier keeps the order of the anchors and of their branches, so the
//! rows come out exactly as one anchor at a time would give them. A frontier
//! larger than [`FRONTIER`] goes on in slices of that size (bounding what is
//! held); in a read-only query with steps left, the slices are spread over
//! the shared threads (`parallel.rs`) and their rows joined in slice order.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use grafeno::{Direction, Edge, EdgeFilter, EdgeId, Node, NodeId};

use crate::QueryError;
use crate::ast::{Expr, NodePattern, Pattern, RelPattern};
use crate::eval::{Context, Row, Scope, eval, truth};
use crate::exec::{MAX_PATH_LENGTH, all_true, node_fits, props_fit, reverse};
use crate::value::Value;

/// Partial matches a frontier holds before it goes on in slices.
pub(crate) const FRONTIER: usize = 4096;
/// Anchors grown together.
pub(crate) const ANCHORS: usize = 64;
/// Edges listed (or nodes reached) kept per step before its cache empties.
const CACHED_EDGES: usize = 1 << 20;
/// Nodes and edge records kept before their cache empties.
const CACHED_RECORDS: usize = 1 << 18;
/// Shards per cache.
const SHARDS: usize = 32;

#[derive(Clone)]
pub(crate) struct Partial {
    row: Row,
    /// Edges used by this match: an edge appears at most once per pattern.
    used: Vec<EdgeId>,
    /// The node bound at each position of the pattern.
    nodes: Vec<Option<NodeId>>,
    /// The edges of each step, from its left node to its right one (kept
    /// only when the pattern has a path variable).
    steps: Vec<Vec<EdgeId>>,
}

/// One step of the traversal, in the order it is walked: right of the
/// anchor in the written direction, then left of it against it.
pub(crate) struct Op<'q> {
    rel: &'q RelPattern,
    direction: Direction,
    /// Positions of the node walked from and of the node reached.
    from: usize,
    target: &'q NodePattern,
    target_index: usize,
    step: usize,
    /// Walked from the step's right node (paths keep the written order).
    leftwards: bool,
    filter: EdgeFilter,
}

/// A node's edges for one step: (edge, other end).
type Edges = Arc<[(EdgeId, NodeId)]>;

/// A variable-length step's ends from one node, each with the edges that
/// lead there.
type Reach = Arc<[(NodeId, Vec<EdgeId>)]>;

/// A key spread over the shards.
trait Shard: Copy + Eq + Hash {
    fn shard(self) -> usize;
}

impl Shard for NodeId {
    fn shard(self) -> usize {
        let id = self.as_u128();
        ((id ^ (id >> 64)) as usize) % SHARDS
    }
}

impl Shard for EdgeId {
    fn shard(self) -> usize {
        let id = self.as_u128();
        ((id ^ (id >> 64)) as usize) % SHARDS
    }
}

/// A bounded map shared by threads: shards behind read-write locks.
struct Shards<K, V> {
    shards: Vec<RwLock<HashMap<K, V>>>,
    held: AtomicUsize,
    max: usize,
}

impl<K: Shard, V: Clone> Shards<K, V> {
    fn new(max: usize) -> Self {
        Self {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
            held: AtomicUsize::new(0),
            max,
        }
    }

    fn get(&self, key: K) -> Option<V> {
        self.shards[key.shard()]
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    fn contains(&self, key: K) -> bool {
        self.shards[key.shard()]
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&key)
    }

    /// Keeps `value`, counted as `weight` entries; past the bound, the map
    /// empties first.
    fn insert(&self, key: K, value: V, weight: usize) {
        if self.held.fetch_add(weight, Ordering::Relaxed) + weight > self.max {
            for shard in &self.shards {
                shard.write().unwrap_or_else(|e| e.into_inner()).clear();
            }
            self.held.store(weight, Ordering::Relaxed);
        }
        self.shards[key.shard()]
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value);
    }
}

/// What a traversal has read, kept for its other branches and shared by
/// the threads working on it.
pub(crate) struct Caches {
    /// Per step: each node's edges (id, other end).
    adjacency: Vec<Shards<NodeId, Edges>>,
    /// Per variable-length step: each node's reach.
    reach: Vec<Shards<NodeId, Reach>>,
    nodes: Shards<NodeId, Option<Arc<Node>>>,
    edges: Shards<EdgeId, Option<Arc<Edge>>>,
}

impl Caches {
    pub(crate) fn new(steps: usize) -> Self {
        Self {
            adjacency: (0..steps).map(|_| Shards::new(CACHED_EDGES)).collect(),
            reach: (0..steps).map(|_| Shards::new(CACHED_EDGES)).collect(),
            nodes: Shards::new(CACHED_RECORDS),
            edges: Shards::new(CACHED_RECORDS),
        }
    }

    fn node(&self, ctx: &Context, id: NodeId) -> Result<Option<Arc<Node>>, QueryError> {
        if let Some(node) = self.nodes.get(id) {
            return Ok(node);
        }
        let node = ctx.node(id)?.map(Arc::new);
        self.nodes.insert(id, node.clone(), 1);
        Ok(node)
    }

    fn edge(&self, ctx: &Context, id: EdgeId) -> Result<Option<Arc<Edge>>, QueryError> {
        if let Some(edge) = self.edges.get(id) {
            return Ok(edge);
        }
        let edge = ctx.edge(id)?.map(Arc::new);
        self.edges.insert(id, edge.clone(), 1);
        Ok(edge)
    }
}

/// A pattern bound at its anchor, ready to grow from batches of candidates.
pub(crate) struct Start<'s, 'q> {
    pub(crate) pattern: &'q Pattern,
    pub(crate) nodes: &'s [&'q NodePattern],
    pub(crate) anchor: usize,
    /// Conditions that need only the anchor (and earlier variables).
    pub(crate) early: &'s [&'q Expr],
    /// The filter for complete rows.
    pub(crate) filter: Option<&'q Expr>,
}

impl<'q> Start<'_, 'q> {
    /// The steps in the order they are walked.
    pub(crate) fn ops(&self, ctx: &Context) -> Vec<Op<'q>> {
        let (pattern, nodes, anchor) = (self.pattern, self.nodes, self.anchor);
        let filter_of = |rel: &RelPattern| {
            let mut filter = ctx.edges.clone();
            for label in &rel.labels {
                filter = filter.label(label);
            }
            filter
        };
        let mut ops = Vec::with_capacity(pattern.steps.len());
        for step in anchor..pattern.steps.len() {
            let (rel, target) = &pattern.steps[step];
            ops.push(Op {
                rel,
                direction: rel.direction,
                from: step,
                target,
                target_index: step + 1,
                step,
                leftwards: false,
                filter: filter_of(rel),
            });
        }
        for step in (0..anchor).rev() {
            let (rel, _) = &pattern.steps[step];
            ops.push(Op {
                rel,
                direction: reverse(rel.direction),
                from: step + 1,
                target: nodes[step],
                target_index: step,
                step,
                leftwards: true,
                filter: filter_of(rel),
            });
        }
        ops
    }

    /// Every match of the pattern with its anchor at one of `anchors`, in
    /// anchor order, appended to `out`; whether `out` reached `cap`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn extend_batch(
        &self,
        ctx: &Context,
        row: &Row,
        ops: &[Op],
        anchors: Vec<Arc<Node>>,
        out: &mut Vec<Row>,
        cap: Option<usize>,
        caches: &Caches,
    ) -> Result<bool, QueryError> {
        let (pattern, nodes, anchor) = (self.pattern, self.nodes, self.anchor);
        let mut states = Vec::with_capacity(anchors.len());
        for node in anchors {
            let mut start = Partial {
                row: row.clone(),
                used: Vec::new(),
                nodes: vec![None; nodes.len()],
                steps: vec![Vec::new(); pattern.steps.len()],
            };
            start.nodes[anchor] = Some(node.id);
            if let Some(var) = nodes[anchor].var.as_deref() {
                start.row.insert(var.to_owned(), Value::Node(node));
            }
            if !self.early.is_empty() && !all_true(ctx, self.early, &start.row)? {
                continue;
            }
            states.push(start);
        }
        self.grow(ctx, ops, states, 0, out, cap, caches)
    }

    /// Grows a frontier from step `level` to the end.
    #[allow(clippy::too_many_arguments)]
    fn grow(
        &self,
        ctx: &Context,
        ops: &[Op],
        states: Vec<Partial>,
        level: usize,
        out: &mut Vec<Row>,
        cap: Option<usize>,
        caches: &Caches,
    ) -> Result<bool, QueryError> {
        if level == ops.len() {
            return self.finish(ctx, states, out, cap);
        }
        let next = step(ctx, ops, &states, level, caches)?;
        drop(states);
        self.go_on(ctx, ops, next, level + 1, out, cap, caches)
    }

    /// Goes on from a frontier at step `level`: whole when it is small or
    /// complete, in slices otherwise, across threads when it may.
    #[allow(clippy::too_many_arguments)]
    fn go_on(
        &self,
        ctx: &Context,
        ops: &[Op],
        states: Vec<Partial>,
        level: usize,
        out: &mut Vec<Row>,
        cap: Option<usize>,
        caches: &Caches,
    ) -> Result<bool, QueryError> {
        if states.len() <= FRONTIER || level == ops.len() {
            return self.grow(ctx, ops, states, level, out, cap, caches);
        }
        if ctx.write.is_none()
            && ctx.trace.is_none()
            && cap.is_none()
            && crate::parallel::available()
        {
            crate::parallel::map_chunks(
                ctx,
                &states,
                |ctx, slice: &[Partial]| -> Result<Vec<Row>, QueryError> {
                    let mut rows = Vec::new();
                    for part in slice.chunks(FRONTIER) {
                        let next = step(ctx, ops, part, level, caches)?;
                        self.go_on(ctx, ops, next, level + 1, &mut rows, None, caches)?;
                    }
                    Ok(rows)
                },
                &mut |rows: Vec<Row>| {
                    out.extend(rows);
                    ctx.budget.rows(out.len())
                },
            )?;
            return Ok(false);
        }
        for part in states.chunks(FRONTIER) {
            let next = step(ctx, ops, part, level, caches)?;
            if self.go_on(ctx, ops, next, level + 1, out, cap, caches)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Complete matches: their paths, the row filter, and the cap.
    fn finish(
        &self,
        ctx: &Context,
        states: Vec<Partial>,
        out: &mut Vec<Row>,
        cap: Option<usize>,
    ) -> Result<bool, QueryError> {
        for mut state in states {
            if let Some(var) = &self.pattern.path {
                let path = build_path(ctx, &state.nodes, &state.steps)?;
                state.row.insert(var.clone(), path);
            }
            if let Some(filter) = self.filter
                && truth(&eval(ctx, filter, Scope::Row(&state.row))?)? != Some(true)
            {
                if let Some(trace) = ctx.trace {
                    trace.drop_row(&crate::exec::row_values(&state.row), filter);
                }
                continue;
            }
            out.push(state.row);
            if cap.is_some_and(|cap| out.len() >= cap) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// One step of a frontier: the nodes it stands on read once, in id order
/// (unless cached), then every partial match extended, in order.
fn step(
    ctx: &Context,
    ops: &[Op],
    states: &[Partial],
    level: usize,
    caches: &Caches,
) -> Result<Vec<Partial>, QueryError> {
    let op = &ops[level];
    let mut froms: Vec<NodeId> = states
        .iter()
        .filter_map(|state| state.nodes[op.from])
        .filter(|from| match op.rel.length {
            None => !caches.adjacency[level].contains(*from),
            Some(_) => !caches.reach[level].contains(*from),
        })
        .collect();
    froms.sort_unstable();
    froms.dedup();
    for from in froms {
        match op.rel.length {
            None => {
                let edges: Edges = ctx.neighbor_ids(from, op.direction, &op.filter)?.into();
                let weight = edges.len() + 1;
                caches.adjacency[level].insert(from, edges, weight);
            }
            Some((min, max)) => {
                let reach = reach_from(ctx, from, op, min, max)?;
                let weight = reach.len() + 1;
                caches.reach[level].insert(from, reach, weight);
            }
        }
    }
    let mut next = Vec::new();
    for state in states {
        let from = state.nodes[op.from].expect("bound while extending");
        expand(ctx, caches, level, op, state, from, &mut next)?;
    }
    ctx.budget.rows(next.len())?;
    Ok(next)
}

/// The ends of a variable-length step from `from`, breadth first: each node
/// once, at its shortest distance, with the edges from `from` to it.
fn reach_from(
    ctx: &Context,
    from: NodeId,
    op: &Op,
    min: u32,
    max: Option<u32>,
) -> Result<Reach, QueryError> {
    let max = max.unwrap_or(MAX_PATH_LENGTH).min(MAX_PATH_LENGTH);
    let mut parent: HashMap<NodeId, (NodeId, EdgeId)> = HashMap::new();
    let mut distance = HashMap::from([(from, 0u32)]);
    let mut queue = VecDeque::from([from]);
    let mut reached = Vec::new();
    if min == 0 {
        reached.push(from);
    }
    while let Some(current) = queue.pop_front() {
        let depth = distance[&current];
        if depth == max {
            continue;
        }
        for (edge_id, other) in ctx.neighbor_ids(current, op.direction, &op.filter)? {
            if distance.contains_key(&other) {
                continue;
            }
            distance.insert(other, depth + 1);
            parent.insert(other, (current, edge_id));
            if depth + 1 >= min {
                reached.push(other);
            }
            queue.push_back(other);
        }
    }
    Ok(reached
        .into_iter()
        .map(|end| {
            let mut ids = Vec::new();
            let mut current = end;
            while let Some(&(previous, edge_id)) = parent.get(&current) {
                ids.push(edge_id);
                current = previous;
            }
            ids.reverse();
            (end, ids)
        })
        .collect())
}

/// The node at `id` if it fits `pattern` (and the row's binding of its
/// variable, if any).
fn target_node(
    ctx: &Context,
    caches: &Caches,
    id: NodeId,
    pattern: &NodePattern,
    row: &Row,
) -> Result<Option<Arc<Node>>, QueryError> {
    if let Some(var) = &pattern.var
        && let Some(bound) = row.get(var)
    {
        return match bound {
            Value::Node(node) if node.id == id && node_fits(ctx, node, pattern, row)? => {
                Ok(Some(node.clone()))
            }
            _ => Ok(None),
        };
    }
    let Some(node) = caches.node(ctx, id)? else {
        return Ok(None);
    };
    Ok(node_fits(ctx, &node, pattern, row)?.then_some(node))
}

/// Extends a partial match by one step, from what the caches hold, into
/// `out`.
fn expand(
    ctx: &Context,
    caches: &Caches,
    level: usize,
    op: &Op,
    state: &Partial,
    from: NodeId,
    out: &mut Vec<Partial>,
) -> Result<(), QueryError> {
    let (rel, target) = (op.rel, op.target);
    if rel.length.is_none() {
        // Emptied by its bound since the read: read again.
        let edges = match caches.adjacency[level].get(from) {
            Some(edges) => edges,
            None => ctx.neighbor_ids(from, op.direction, &op.filter)?.into(),
        };
        for &(edge_id, other) in edges.iter() {
            if let Some(trace) = ctx.trace {
                trace.expand(from, other, edge_id);
            }
            if state.used.contains(&edge_id) {
                if let Some(trace) = ctx.trace {
                    trace.drop_edge(edge_id, "edge used");
                }
                continue;
            }
            let edge = if rel.var.is_some() || !rel.props.is_empty() {
                let Some(edge) = caches.edge(ctx, edge_id)? else {
                    continue;
                };
                let fits = props_fit(ctx, &Value::Edge(edge.clone()), &rel.props, &state.row)?
                    && !rel.var.as_ref().is_some_and(|var| {
                        state.row.get(var).is_some_and(|bound| {
                            !matches!(bound, Value::Edge(existing) if existing.id == edge_id)
                        })
                    });
                if !fits {
                    if let Some(trace) = ctx.trace {
                        trace.drop_edge(edge_id, "edge pattern");
                    }
                    continue;
                }
                Some(edge)
            } else {
                None
            };
            let Some(node) = target_node(ctx, caches, other, target, &state.row)? else {
                if let Some(trace) = ctx.trace {
                    trace.drop_node(other, "pattern");
                }
                continue;
            };
            if let Some(trace) = ctx.trace {
                trace.keep(other, Some(edge_id));
            }
            let mut next = state.clone();
            next.used.push(edge_id);
            next.steps[op.step] = vec![edge_id];
            next.nodes[op.target_index] = Some(other);
            if let Some(var) = &target.var {
                next.row.insert(var.clone(), Value::Node(node));
            }
            if let (Some(var), Some(edge)) = (&rel.var, edge) {
                next.row.insert(var.clone(), Value::Edge(edge));
            }
            out.push(next);
        }
        return Ok(());
    }
    if !rel.props.is_empty() {
        return Err(QueryError::runtime(
            "properties on a variable-length edge pattern are not supported",
        ));
    }
    let reach = match caches.reach[level].get(from) {
        Some(reach) => reach,
        None => match rel.length {
            Some((min, max)) => reach_from(ctx, from, op, min, max)?,
            None => unreachable!("a variable-length step"),
        },
    };
    for (end, ids) in reach.iter() {
        if let Some(trace) = ctx.trace {
            trace.expand_path(from, *end, ids);
        }
        let Some(node) = target_node(ctx, caches, *end, target, &state.row)? else {
            if let Some(trace) = ctx.trace {
                trace.drop_node(*end, "pattern");
            }
            continue;
        };
        if let Some(trace) = ctx.trace {
            trace.keep(*end, None);
        }
        let mut next = state.clone();
        next.nodes[op.target_index] = Some(*end);
        if let Some(var) = &rel.var {
            let mut edges = Vec::new();
            for &edge_id in ids {
                if let Some(edge) = caches.edge(ctx, edge_id)? {
                    edges.push(Value::Edge(edge));
                }
            }
            next.row.insert(var.clone(), Value::List(edges));
        }
        let mut ids = ids.clone();
        if op.leftwards {
            ids.reverse();
        }
        next.steps[op.step] = ids;
        if let Some(var) = &target.var {
            next.row.insert(var.clone(), Value::Node(node));
        }
        out.push(next);
    }
    Ok(())
}

/// The path a match went through: nodes and edges in written order.
fn build_path(
    ctx: &Context,
    nodes: &[Option<NodeId>],
    steps: &[Vec<EdgeId>],
) -> Result<Value, QueryError> {
    let start = nodes[0].expect("every position is bound after a match");
    let node = |id: NodeId| -> Result<Arc<Node>, QueryError> {
        ctx.node(id)?
            .map(Arc::new)
            .ok_or_else(|| QueryError::runtime(format!("node {id} disappeared during the query")))
    };
    let mut path = crate::value::Path {
        nodes: vec![node(start)?],
        edges: Vec::new(),
    };
    let mut current = start;
    for step in steps {
        for &edge_id in step {
            let edge = ctx
                .edge(edge_id)?
                .ok_or_else(|| QueryError::runtime(format!("edge {edge_id} disappeared")))?;
            current = if edge.from == current {
                edge.to
            } else {
                edge.from
            };
            path.edges.push(Arc::new(edge));
            path.nodes.push(node(current)?);
        }
    }
    Ok(Value::Path(Arc::new(path)))
}
