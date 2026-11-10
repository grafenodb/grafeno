//! Planning a pattern match: where each pattern starts, and in which order
//! the patterns of a `MATCH` run.
//!
//! The start (anchor) of a pattern is a variable bound by an earlier
//! clause, then an `@id`; otherwise the way to its nodes estimated to find
//! the fewest of them: a property index (from the pattern's own properties
//! or the `WHERE` conditions), a label, or every node. Estimates come from
//! the planner's statistics (`grafeno::PlannerStats`: the most common
//! values and equal-depth buckets of each index) with the values the query
//! gives, so `city = 'big'` (half the nodes) loses to `status = 'blocked'`
//! (one in a hundred). Everything else is reached by expanding edges.
//!
//! The patterns of one `MATCH` run cheapest start first; each one's
//! variables are then bound, so a pattern that shares one starts from it.

use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::sync::Arc;

use grafeno::{IndexDef, IndexRange, PlannerStats};
use serde_json::Value as Json;

use crate::QueryError;
use crate::ast::{BinOp, Expr, Key, NodePattern, Pattern, UnOp};
use crate::eval::{Context, Params, Row, Scope, eval};
use crate::value::Value;

/// What the planner knows about the namespace, read once per statement.
#[derive(Clone, Debug, Default)]
pub(crate) struct PlanInfo {
    pub(crate) indexes: Vec<IndexDef>,
    pub(crate) label_counts: HashMap<String, u64>,
    pub(crate) edge_labels: HashMap<String, u64>,
    pub(crate) nodes: u64,
    pub(crate) edges: u64,
    /// Value distributions and degrees, when they have been built.
    pub(crate) stats: Option<Arc<PlannerStats>>,
}

/// The value of an expression when it can be known while planning.
pub(crate) type ValueOf<'a> = &'a dyn Fn(&Expr) -> Option<Value>;

/// The value of an expression that reads no variable: literals, parameters
/// and lists of them.
pub(crate) fn constant(expr: &Expr, params: Option<&Params>) -> Option<Value> {
    match expr {
        Expr::Literal(value) => Some(value.clone()),
        Expr::Param(name) => params?.get(name).cloned(),
        Expr::List(items) => items
            .iter()
            .map(|item| constant(item, params))
            .collect::<Option<Vec<_>>>()
            .map(Value::List),
        Expr::Unary(UnOp::Neg, inner) => match constant(inner, params)? {
            Value::Int(i) => i.checked_neg().map(Value::Int),
            Value::Float(f) => Some(Value::Float(-f)),
            _ => None,
        },
        _ => None,
    }
}

impl PlanInfo {
    fn label_count(&self, label: &str) -> u64 {
        self.label_counts.get(label).copied().unwrap_or(0)
    }

    /// The records an index covers: nodes (or edges) with its labels.
    fn covered(&self, def: &IndexDef) -> f64 {
        def.labels()
            .iter()
            .map(|label| {
                if def.edges {
                    self.edge_labels.get(*label).copied().unwrap_or(0)
                } else {
                    self.label_count(label)
                }
            })
            .sum::<u64>() as f64
    }

    /// Records a lookup in index `def` is estimated to find: values equal
    /// to `prefix` for its first properties (of a composite index), then
    /// `seek` on the next.
    fn seek_rows(
        &self,
        def: &IndexDef,
        prefix: &[(String, &Expr)],
        seek: &Seek,
        value_of: ValueOf,
    ) -> f64 {
        if def.more_keys.is_empty() {
            return self.single_rows(def, seek, value_of);
        }
        let label = self.covered(def);
        let complete = prefix.len() + 1 == def.keys().len() && matches!(seek, Seek::Equal(_));
        let stats = self
            .stats
            .as_ref()
            .and_then(|stats| stats.index(&def.name))
            .filter(|stats| stats.label_nodes > 0);
        let values: Option<Vec<Json>> = prefix
            .iter()
            .map(|(_, expr)| value_of(expr).as_ref().and_then(indexable))
            .collect();
        if let (Some(stats), Some(values)) = (stats, values) {
            let scale = label / stats.label_nodes as f64;
            let rows = match seek {
                Seek::Equal(expr) => match value_of(expr).as_ref().and_then(indexable) {
                    Some(value) => {
                        stats.estimate_seek(def, &values, Some(&IndexRange::Equal(value)))
                    }
                    None => stats.estimate_seek(def, &values, None) / 10.0,
                },
                // The prefix alone bounds the rest.
                _ => stats.estimate_seek(def, &values, None) / 4.0,
            };
            return if def.unique {
                rows.min(label)
            } else {
                rows * scale
            };
        }
        if complete && def.unique {
            return 1.0;
        }
        let equalities = prefix.len() + usize::from(matches!(seek, Seek::Equal(_)));
        let narrowed = label / 10f64.powi(equalities as i32);
        let rows = if matches!(seek, Seek::Equal(_)) {
            narrowed
        } else {
            narrowed / 4.0
        };
        rows.max(1.0)
    }

    /// [`PlanInfo::seek_rows`] for an index on one property.
    fn single_rows(&self, def: &IndexDef, seek: &Seek, value_of: ValueOf) -> f64 {
        let label = self.covered(def);
        let stats = self
            .stats
            .as_ref()
            .and_then(|stats| stats.index(&def.name))
            .filter(|stats| stats.label_nodes > 0);
        let Some(stats) = stats else {
            // Without statistics: an equality finds a tenth of the label,
            // a range or a prefix a quarter.
            return match seek {
                Seek::Equal(_) if def.unique => 1.0,
                Seek::Equal(_) => (label / 10.0).max(1.0),
                Seek::In(list) => {
                    let items = match value_of(list) {
                        Some(Value::List(items)) => items.len() as f64,
                        _ => 10.0,
                    };
                    items
                        * if def.unique {
                            1.0
                        } else {
                            (label / 10.0).max(1.0)
                        }
                }
                Seek::Prefix(_) | Seek::Range { .. } => (label / 4.0).max(1.0),
            };
        };
        // The label grew or shrank since the statistics were built.
        let scale = label / stats.label_nodes as f64;
        let equal = |value: &Value| match indexable(value) {
            Some(json) => stats.estimate(&IndexRange::Equal(json)),
            None if value.is_null() => 0.0,
            None => stats.per_value(),
        };
        let wide = stats.entries as f64 / 4.0;
        let rows = match seek {
            Seek::Equal(expr) => value_of(expr).map_or(stats.per_value(), |value| equal(&value)),
            Seek::In(list) => match value_of(list) {
                Some(Value::List(items)) => items.iter().map(equal).sum(),
                Some(Value::Null) => 0.0,
                _ => 10.0 * stats.per_value(),
            },
            Seek::Prefix(expr) => match value_of(expr) {
                Some(Value::Str(prefix)) => stats.estimate(&IndexRange::Prefix(prefix)),
                Some(Value::Null) => 0.0,
                _ => wide,
            },
            Seek::Range { lower, upper } => {
                let side = |side: Option<(&Expr, bool)>| -> Option<Bound<Json>> {
                    match side {
                        None => Some(Bound::Unbounded),
                        Some((expr, _)) => indexable(&value_of(expr)?).map(Bound::Included),
                    }
                };
                match (side(*lower), side(*upper)) {
                    (Some(lower), Some(upper)) => {
                        stats.estimate(&IndexRange::Between { lower, upper })
                    }
                    _ => wide,
                }
            }
        };
        if def.unique {
            rows.min(label)
        } else {
            rows * scale
        }
    }

    /// Edges followed from one node over edges with `labels` (any when
    /// empty) in `direction`: each label's average degree that way, from
    /// the statistics, or its edges over all nodes without them.
    pub(crate) fn degree(&self, labels: &[String], direction: grafeno::Direction) -> f64 {
        let nodes = (self.nodes as f64).max(1.0);
        let plain = |edges: f64| match direction {
            grafeno::Direction::Both => 2.0 * edges / nodes,
            _ => edges / nodes,
        };
        if labels.is_empty() {
            return plain(self.edges as f64);
        }
        labels
            .iter()
            .map(|label| {
                let stats = self
                    .stats
                    .as_ref()
                    .and_then(|stats| stats.edge_label(label));
                match stats {
                    Some(stats) => match direction {
                        grafeno::Direction::Out => stats.out_degree(),
                        grafeno::Direction::In => stats.in_degree(),
                        grafeno::Direction::Both => stats.out_degree() + stats.in_degree(),
                    },
                    None => plain(self.edge_labels.get(label).copied().unwrap_or(0) as f64),
                }
            })
            .sum()
    }
}

/// The parts of a condition joined by `AND`.
pub(crate) fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary(BinOp::And, left, right) => {
            let mut parts = conjuncts(left);
            parts.extend(conjuncts(right));
            parts
        }
        other => vec![other],
    }
}

/// The variables an expression reads, or `None` when it holds a pattern
/// predicate or binds variables of its own (comprehensions and the like),
/// which only the full row can evaluate.
pub(crate) fn free_vars(expr: &Expr) -> Option<HashSet<String>> {
    let mut vars = HashSet::new();
    let mut local = true;
    expr.visit(&mut |inner| match inner {
        Expr::Var(name) => {
            vars.insert(name.clone());
        }
        Expr::Exists { .. }
        | Expr::PatternQuery { .. }
        | Expr::Comprehension { .. }
        | Expr::Quantified { .. }
        | Expr::Reduce { .. } => local = false,
        _ => {}
    });
    local.then_some(vars)
}

fn evaluable(expr: &Expr, is_bound: &dyn Fn(&str) -> bool) -> bool {
    free_vars(expr).is_some_and(|vars| vars.iter().all(|var| is_bound(var)))
}

/// Which values of an index to read.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Seek<'q> {
    Equal(&'q Expr),
    In(&'q Expr),
    Prefix(&'q Expr),
    /// Bounds, each with whether it is inclusive.
    Range {
        lower: Option<(&'q Expr, bool)>,
        upper: Option<(&'q Expr, bool)>,
    },
}

/// How the start of a pattern is found.
#[derive(Clone, Debug)]
pub(crate) enum Source<'q> {
    Bound,
    Id(&'q Expr),
    Index {
        name: String,
        /// The property `seek` reads.
        key: String,
        /// Values for the properties before `key` (a composite index).
        prefix: Vec<(String, &'q Expr)>,
        seek: Seek<'q>,
        /// Nodes it is estimated to find.
        rows: f64,
        /// An index on edges: the start is the end of each edge found on
        /// this side (`Out`: where it leaves, `In`: where it arrives,
        /// `Both`: either).
        edges: Option<grafeno::Direction>,
    },
    Label {
        label: String,
        count: u64,
    },
    Scan {
        nodes: u64,
    },
}

impl Source<'_> {
    /// Nodes this start is estimated to find.
    pub(crate) fn rows(&self) -> f64 {
        match self {
            Source::Bound | Source::Id(_) => 1.0,
            Source::Index { rows, .. } => *rows,
            Source::Label { count, .. } => *count as f64,
            Source::Scan { nodes } => *nodes as f64,
        }
    }

    /// Lower is better: a bound node, an `@id`, then the fewest nodes
    /// estimated, then (on a tie) an index over a label over a scan.
    fn rank(&self) -> (u8, f64, u8) {
        let kind = match self {
            Source::Bound => return (0, 0.0, 0),
            Source::Id(_) => return (1, 0.0, 0),
            Source::Index {
                seek: Seek::Equal(_),
                ..
            } => 0,
            Source::Index {
                seek: Seek::In(_), ..
            } => 1,
            Source::Index { .. } => 2,
            Source::Label { .. } => 3,
            Source::Scan { .. } => 4,
        };
        (2, self.rows(), kind)
    }

    pub(crate) fn better_than(&self, other: &Source) -> bool {
        let (a, b) = (self.rank(), other.rank());
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(a.2.cmp(&b.2))
            .is_lt()
    }

    pub(crate) fn describe(&self, node: &str) -> String {
        match self {
            Source::Bound => format!("start at {node} (the bound node)"),
            Source::Id(_) => format!("start at {node} (lookup by @id)"),
            Source::Index {
                name,
                key,
                prefix,
                seek,
                rows,
                edges,
            } => {
                let before: String = prefix
                    .iter()
                    .map(|(key, _)| format!("{key} = …, "))
                    .collect();
                let how = match seek {
                    Seek::Equal(_) => format!("{key} = …"),
                    Seek::In(_) => format!("{key} IN […]"),
                    Seek::Prefix(_) => format!("{key} STARTS WITH …"),
                    Seek::Range { lower, upper } => {
                        let low = lower.map_or(
                            "",
                            |(_, inclusive)| if inclusive { "… <= " } else { "… < " },
                        );
                        let high = upper.map_or(
                            "",
                            |(_, inclusive)| if inclusive { " <= …" } else { " < …" },
                        );
                        format!("{low}{key}{high}")
                    }
                };
                let what = match edges {
                    None => "index".to_owned(),
                    Some(grafeno::Direction::Out) => "edge index, where the edges leave".into(),
                    Some(grafeno::Direction::In) => "edge index, where the edges arrive".into(),
                    Some(grafeno::Direction::Both) => "edge index, either end".into(),
                };
                format!(
                    "start at {node} ({what} {name}: {before}{how}, ~{} nodes)",
                    rows.ceil() as u64
                )
            }
            Source::Label { label, count } => {
                format!("start at {node} (scan of :{label}, {count} nodes)")
            }
            Source::Scan { nodes } => {
                format!("start at {node} (scan of every node, {nodes} nodes)")
            }
        }
    }
}

/// The best way to start from one node pattern.
fn source_for<'q>(
    node: &'q NodePattern,
    is_bound: &dyn Fn(&str) -> bool,
    conditions: &[&'q Expr],
    info: &PlanInfo,
    value_of: ValueOf,
) -> Source<'q> {
    if node.var.as_deref().is_some_and(is_bound) {
        return Source::Bound;
    }
    if let Some((_, expr)) = node
        .props
        .iter()
        .find(|(key, expr)| *key == Key::Meta("id".into()) && evaluable(expr, is_bound))
    {
        return Source::Id(expr);
    }
    let mut best: Option<Source<'q>> = None;
    let mut consider = |candidate: Source<'q>| {
        if best
            .as_ref()
            .is_none_or(|current| candidate.better_than(current))
        {
            best = Some(candidate);
        }
    };
    for def in info
        .indexes
        .iter()
        .filter(|def| !def.fuzzy && !def.edges && def.covers(&node.labels))
    {
        let var = node.var.as_deref();
        if let Some((prefix, key, seek)) =
            seek_for_keys(&node.props, var, &def.keys(), is_bound, conditions)
        {
            consider(Source::Index {
                name: def.name.clone(),
                rows: info.seek_rows(def, &prefix, &seek, value_of),
                key,
                prefix,
                seek,
                edges: None,
            });
        }
    }
    for label in &node.labels {
        consider(Source::Label {
            label: label.clone(),
            count: info.label_count(label),
        });
    }
    best.unwrap_or(Source::Scan { nodes: info.nodes })
}

/// Ways to start at node `anchor` of `pattern` from an index on the edges
/// of a step next to it: the pattern's `[r:label {key: value}]` or
/// conditions on `r.key`, with the edges' end on the anchor's side.
fn edge_sources<'q>(
    pattern: &'q Pattern,
    anchor: usize,
    is_bound: &dyn Fn(&str) -> bool,
    conditions: &[&'q Expr],
    info: &PlanInfo,
    value_of: ValueOf,
) -> Vec<Source<'q>> {
    let mut out = Vec::new();
    for (step, (rel, _)) in pattern.steps.iter().enumerate() {
        // The step joins node `step` (its left) and node `step + 1`.
        let left = if anchor == step {
            true
        } else if anchor == step + 1 {
            false
        } else {
            continue;
        };
        if rel.length.is_some() || rel.labels.is_empty() {
            continue;
        }
        use grafeno::Direction::{Both, In, Out};
        let end = match (rel.direction, left) {
            (Both, _) => Both,
            (Out, true) | (In, false) => Out,
            (Out, false) | (In, true) => In,
        };
        for def in info.indexes.iter().filter(|def| {
            def.edges
                && !def.fuzzy
                && rel
                    .labels
                    .iter()
                    .all(|label| def.labels().contains(&label.as_str()))
        }) {
            if let Some((prefix, key, seek)) = seek_for_keys(
                &rel.props,
                rel.var.as_deref(),
                &def.keys(),
                is_bound,
                conditions,
            ) {
                let found = info.seek_rows(def, &prefix, &seek, value_of);
                out.push(Source::Index {
                    name: def.name.clone(),
                    rows: if end == Both { found * 2.0 } else { found },
                    key,
                    prefix,
                    seek,
                    edges: Some(end),
                });
            }
        }
    }
    out
}

/// A way to read an index on `keys`: values equal for a prefix of them,
/// then a seek on the next one (the prefix's last when nothing else).
#[allow(clippy::type_complexity)]
fn seek_for_keys<'q>(
    props: &'q [(Key, Expr)],
    var: Option<&str>,
    keys: &[&str],
    is_bound: &dyn Fn(&str) -> bool,
    conditions: &[&'q Expr],
) -> Option<(Vec<(String, &'q Expr)>, String, Seek<'q>)> {
    let mut prefix: Vec<(String, &'q Expr)> = Vec::new();
    for (at, key) in keys.iter().enumerate() {
        match seek_for(props, var, key, is_bound, conditions) {
            Some(Seek::Equal(expr)) if at + 1 < keys.len() => {
                prefix.push(((*key).to_owned(), expr));
            }
            Some(seek) => return Some((prefix, (*key).to_owned(), seek)),
            None => {
                let (key, expr) = prefix.pop()?;
                return Some((prefix, key, Seek::Equal(expr)));
            }
        }
    }
    None
}

/// A way to read the index on `key`: the pattern's own `{key: value}`, or
/// conditions such as `n.key = …`, `n.key IN …`, `n.key STARTS WITH …` and
/// ranges, whose other side can be computed before matching.
fn seek_for<'q>(
    props: &'q [(Key, Expr)],
    var: Option<&str>,
    key: &str,
    is_bound: &dyn Fn(&str) -> bool,
    conditions: &[&'q Expr],
) -> Option<Seek<'q>> {
    if let Some((_, expr)) = props
        .iter()
        .find(|(k, expr)| *k == Key::Prop(key.to_owned()) && evaluable(expr, is_bound))
    {
        return Some(Seek::Equal(expr));
    }
    let var = var?;
    let is_property = |expr: &Expr| {
        matches!(expr, Expr::Property(target, Key::Prop(name))
            if name == key && matches!(target.as_ref(), Expr::Var(v) if v == var))
    };
    let (mut lower, mut upper, mut other) = (None, None, None);
    for condition in conditions {
        let Expr::Binary(op, left, right) = condition else {
            continue;
        };
        // `n.key op value`, or `value op n.key` turned around.
        let (op, value) = if is_property(left) && evaluable(right, is_bound) {
            (*op, right.as_ref())
        } else if is_property(right) && evaluable(left, is_bound) {
            let flipped = match op {
                BinOp::Lt => BinOp::Gt,
                BinOp::Le => BinOp::Ge,
                BinOp::Gt => BinOp::Lt,
                BinOp::Ge => BinOp::Le,
                BinOp::Eq => BinOp::Eq,
                _ => continue,
            };
            (flipped, left.as_ref())
        } else {
            continue;
        };
        match op {
            BinOp::Eq => return Some(Seek::Equal(value)),
            BinOp::In => other = Some(Seek::In(value)),
            BinOp::StartsWith if other.is_none() => other = Some(Seek::Prefix(value)),
            BinOp::Gt => lower = Some((value, false)),
            BinOp::Ge => lower = Some((value, true)),
            BinOp::Lt => upper = Some((value, false)),
            BinOp::Le => upper = Some((value, true)),
            _ => {}
        }
    }
    other.or_else(|| (lower.is_some() || upper.is_some()).then_some(Seek::Range { lower, upper }))
}

/// Where to start a pattern: the position of its anchor node, how to find
/// it, and the work the whole match is estimated to take from there.
///
/// A node bound by an earlier clause or given by `@id` starts the pattern;
/// otherwise every node's best start is tried, and the one whose start and
/// expansions are estimated to take the least work wins: a label of five
/// companies loses to an index that finds twenty people when each company
/// employs hundreds.
pub(crate) fn plan_anchor<'q>(
    pattern: &'q Pattern,
    is_bound: &dyn Fn(&str) -> bool,
    conditions: &[&'q Expr],
    info: &PlanInfo,
    value_of: ValueOf,
) -> (usize, Source<'q>, f64) {
    let nodes = pattern.nodes();
    // A node whose properties need a variable not bound yet (`(q {team:
    // p.team})`) cannot start the pattern, unless no node can.
    let ready = |node: &NodePattern| {
        node.var.as_deref().is_some_and(is_bound)
            || node.props.iter().all(|(_, expr)| evaluable(expr, is_bound))
    };
    let any_ready = nodes.iter().any(|node| ready(node));
    let mut best: Option<(usize, Source<'q>, f64)> = None;
    for (index, node) in nodes.iter().enumerate() {
        if any_ready && !ready(node) {
            continue;
        }
        let mut sources = vec![source_for(node, is_bound, conditions, info, value_of)];
        if !matches!(sources[0], Source::Bound | Source::Id(_)) {
            sources.extend(edge_sources(
                pattern, index, is_bound, conditions, info, value_of,
            ));
        }
        for source in sources {
            let cost = pattern_cost(pattern, &nodes, index, &source, is_bound, info);
            let wins = best.as_ref().is_none_or(|(_, current, current_cost)| {
                let (a, b) = (source.rank(), current.rank());
                a.0.cmp(&b.0)
                    .then(cost.total_cmp(current_cost))
                    .then(a.2.cmp(&b.2))
                    .is_lt()
            });
            if wins {
                best = Some((index, source, cost));
            }
        }
    }
    best.expect("a pattern has at least one node")
}

/// Far beyond any real count, and still finite.
const CAP: f64 = 1e18;

/// The share of nodes reached that fit `node`: its labels, its properties
/// (a tenth each), and one node when it is already bound.
fn keep(node: &NodePattern, is_bound: &dyn Fn(&str) -> bool, info: &PlanInfo) -> f64 {
    let nodes = (info.nodes as f64).max(1.0);
    let mut share = node
        .labels
        .iter()
        .map(|label| (info.label_count(label) as f64 / nodes).clamp(1.0 / nodes, 1.0))
        .fold(1.0, f64::min);
    if node.var.as_deref().is_some_and(is_bound) {
        share = share.min(1.0 / nodes);
    }
    share * 0.1f64.powi(node.props.len().min(8) as i32)
}

/// The work of matching `pattern` from node `anchor` found by `source`:
/// the candidates read, then the edges followed at each step, both ways
/// from the anchor.
fn pattern_cost(
    pattern: &Pattern,
    nodes: &[&NodePattern],
    anchor: usize,
    source: &Source,
    is_bound: &dyn Fn(&str) -> bool,
    info: &PlanInfo,
) -> f64 {
    let start = nodes[anchor];
    let mut cost = source.rows();
    // The candidates' own properties not used to find them.
    let used = |key: &Key| match source {
        Source::Id(_) => *key == Key::Meta("id".into()),
        Source::Index {
            key: index_key,
            prefix,
            edges: None,
            ..
        } => {
            *key == Key::Prop(index_key.clone())
                || prefix
                    .iter()
                    .any(|(name, _)| *key == Key::Prop(name.clone()))
        }
        _ => false,
    };
    let extra = start.props.iter().filter(|(key, _)| !used(key)).count();
    let mut rows = source.rows() * 0.1f64.powi(extra.min(8) as i32);
    let order = (anchor..pattern.steps.len())
        .map(|step| (step, step + 1, false))
        .chain((0..anchor).rev().map(|step| (step, step, true)));
    for (step, target, backwards) in order {
        let rel = &pattern.steps[step].0;
        let direction = match (rel.direction, backwards) {
            (grafeno::Direction::Out, true) => grafeno::Direction::In,
            (grafeno::Direction::In, true) => grafeno::Direction::Out,
            (direction, _) => direction,
        };
        let degree = info.degree(&rel.labels, direction);
        let reach = match rel.length {
            None => degree,
            Some((min, max)) => {
                let max = max.unwrap_or(crate::exec::MAX_PATH_LENGTH);
                (min..=max)
                    .map(|hops| degree.powi(hops as i32))
                    .sum::<f64>()
            }
        };
        cost = (cost + rows * reach.max(1.0)).min(CAP);
        rows = (rows * reach * keep(nodes[target], is_bound, info)).min(CAP);
    }
    cost
}

/// The order to run the patterns of one `MATCH` in: each time, a pattern
/// that shares a variable bound so far if there is one, the one estimated
/// to take the least work otherwise. Ties keep the written order.
pub(crate) fn order_patterns(
    patterns: &[Pattern],
    is_bound: &dyn Fn(&str) -> bool,
    conditions: &[&Expr],
    info: &PlanInfo,
    value_of: ValueOf,
) -> Vec<usize> {
    if patterns.len() < 2 {
        return (0..patterns.len()).collect();
    }
    let mut bound: HashSet<String> = HashSet::new();
    let mut left: Vec<usize> = (0..patterns.len()).collect();
    let mut order = Vec::with_capacity(patterns.len());
    while !left.is_empty() {
        let known = |var: &str| is_bound(var) || bound.contains(var);
        let mut pick: Option<(usize, (u8, f64))> = None;
        for (slot, &index) in left.iter().enumerate() {
            let (_, source, cost) =
                plan_anchor(&patterns[index], &known, conditions, info, value_of);
            let key = (source.rank().0.min(2), cost);
            if pick
                .is_none_or(|(_, best)| key.0.cmp(&best.0).then(key.1.total_cmp(&best.1)).is_lt())
            {
                pick = Some((slot, key));
            }
        }
        let index = left.remove(pick.map_or(0, |(slot, _)| slot));
        bound.extend(patterns[index].variables().into_iter().map(str::to_owned));
        order.push(index);
    }
    order
}

/// A value the index can look up: numbers, strings and booleans.
pub(crate) fn indexable(value: &Value) -> Option<Json> {
    match value {
        Value::Int(i) => Some(Json::from(*i)),
        Value::Float(f) if f.is_finite() => serde_json::Number::from_f64(*f).map(Json::Number),
        Value::Str(s) => Some(Json::String(s.clone())),
        Value::Bool(b) => Some(Json::Bool(*b)),
        _ => None,
    }
}

/// The index ranges to read for `seek` in this row. `None`: the values
/// cannot be looked up (a time, a list…), so scan the label instead. An
/// empty list: nothing can match (`= null`).
pub(crate) fn seek_ranges(
    ctx: &Context,
    seek: Seek,
    row: &Row,
) -> Result<Option<Vec<IndexRange>>, QueryError> {
    let value = |expr: &Expr| eval(ctx, expr, Scope::Row(row));
    Ok(match seek {
        Seek::Equal(expr) => match value(expr)? {
            Value::Null => Some(Vec::new()),
            other => indexable(&other).map(|json| vec![IndexRange::Equal(json)]),
        },
        Seek::In(expr) => match value(expr)? {
            Value::Null => Some(Vec::new()),
            Value::List(items) => {
                let mut ranges = Vec::new();
                for item in items.iter().filter(|item| !item.is_null()) {
                    match indexable(item) {
                        Some(json) => ranges.push(IndexRange::Equal(json)),
                        None => return Ok(None),
                    }
                }
                Some(ranges)
            }
            _ => None,
        },
        Seek::Prefix(expr) => match value(expr)? {
            Value::Null => Some(Vec::new()),
            Value::Str(prefix) => Some(vec![IndexRange::Prefix(prefix)]),
            _ => None,
        },
        Seek::Range { lower, upper } => {
            // Inclusive on both ends: the condition itself still runs on
            // every node found.
            let bound = |side: Option<(&Expr, bool)>| -> Result<Option<Bound<Json>>, QueryError> {
                Ok(match side {
                    None => Some(Bound::Unbounded),
                    Some((expr, _)) => match value(expr)? {
                        Value::Null => None,
                        other => indexable(&other).map(Bound::Included),
                    },
                })
            };
            match (bound(lower)?, bound(upper)?) {
                (Some(lower), Some(upper)) => Some(vec![IndexRange::Between { lower, upper }]),
                // A null bound: the comparison is never true.
                _ if [lower, upper]
                    .iter()
                    .flatten()
                    .any(|(expr, _)| value(expr).is_ok_and(|value| value.is_null())) =>
                {
                    Some(Vec::new())
                }
                _ => None,
            }
        }
    })
}
