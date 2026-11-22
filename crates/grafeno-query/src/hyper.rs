//! `MATCH HYPEREDGE`: finding hyperedges and binding their participants.
//!
//! Each row starts from the cheapest anchor its bindings allow: the
//! hyperedge variable already bound, an `@id`, the incidence lists of the
//! members whose nodes are bound (intersected), a label's hyperedges, or
//! every hyperedge of the namespace. A candidate must have one of the
//! labels, the properties, the `VALID AT` and `KNOWN AT` time and be visible;
//! then each member is given a participant of its own: its node fits the
//! member's node pattern (or is the node its variable holds) and plays its
//! role when one is named. With `EXACT`, the members' nodes are all the
//! hyperedge's nodes. A hyperedge gives one row per distinct way of binding
//! the members' variables.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use grafeno::{HyperEdge, HyperEdgeFilter, HyperEdgeId, Node, NodeId};

use crate::QueryError;
use crate::ast::{HyperPattern, Key};
use crate::eval::{Context, Row, Scope, eval};
use crate::exec::{node_fits, props_fit, uuid_from_text};
use crate::value::Value;

/// Hyperedge ids read per page when a label or the namespace is scanned.
const PAGE: usize = 512;

/// Member variables and the nodes they hold.
type Bindings = Vec<(String, Arc<Node>)>;

/// Where a hyperedge pattern starts.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Anchor {
    /// The hyperedge variable is bound.
    Bound,
    /// `{@id: …}`.
    Id,
    /// The incidence lists of these bound members' nodes.
    Nodes(Vec<String>),
    /// The label index, for each label.
    Labels,
    /// Every hyperedge of the namespace.
    All,
}

impl Anchor {
    pub(crate) fn of(pattern: &HyperPattern, bound: &dyn Fn(&str) -> bool) -> Anchor {
        if pattern.var.as_deref().is_some_and(bound) {
            return Anchor::Bound;
        }
        if pattern
            .props
            .iter()
            .any(|(key, _)| *key == Key::Meta("id".into()))
        {
            return Anchor::Id;
        }
        let mut nodes: Vec<String> = Vec::new();
        for member in &pattern.members {
            if let Some(var) = member.node.var.as_deref()
                && bound(var)
                && !nodes.iter().any(|n| n == var)
            {
                nodes.push(var.to_owned());
            }
        }
        if !nodes.is_empty() {
            return Anchor::Nodes(nodes);
        }
        if !pattern.labels.is_empty() {
            return Anchor::Labels;
        }
        Anchor::All
    }

    /// How `EXPLAIN` says it.
    pub(crate) fn describe(&self, pattern: &HyperPattern) -> String {
        let labels = if pattern.labels.is_empty() {
            String::new()
        } else {
            format!(":{}", pattern.labels.join("|"))
        };
        let var = pattern.var.as_deref().unwrap_or("");
        let start = match self {
            Anchor::Bound => format!("the bound hyperedge {var}"),
            Anchor::Id => format!("hyperedge {var}{labels} by id"),
            Anchor::Nodes(nodes) => format!(
                "hyperedges{labels} of {} (incidence index)",
                nodes.join(", ")
            ),
            Anchor::Labels => format!("hyperedges {var}{labels} (label index)"),
            Anchor::All => format!("every hyperedge {var} (scan)"),
        };
        let mut text = format!("{start}; {} participant(s)", pattern.members.len());
        if pattern.exact {
            text.push_str(", exactly");
        }
        text
    }
}

impl Context<'_> {
    /// The hyperedge, if it exists and the caller may see it (without the
    /// properties hidden from it).
    pub(crate) fn hyperedge(&self, id: HyperEdgeId) -> Result<Option<HyperEdge>, QueryError> {
        self.budget.read(1)?;
        if self.blind() {
            return Ok(None);
        }
        let edge = match self.write {
            Some(tx) => tx.hyperedge(self.ns, id)?,
            None => self.read.hyperedge(self.ns, id)?,
        };
        match (edge, self.per_record()) {
            (Some(edge), Some(view)) => Ok(view.hyperedge(self.ns, edge, |node| {
                self.visible(node)
                    .map_err(|e| grafeno::Error::Invalid(e.message))
            })?),
            (edge, _) => Ok(edge),
        }
    }

    fn hyperedge_ids_of(
        &self,
        node: NodeId,
        label: Option<&str>,
    ) -> Result<Vec<HyperEdgeId>, QueryError> {
        let ids = match self.write {
            Some(tx) => tx.hyperedge_ids_of(self.ns, node, label)?,
            None => self.read.hyperedge_ids_of(self.ns, node, label)?,
        };
        self.budget.read(ids.len().max(1))?;
        Ok(ids)
    }

    fn hyperedge_page(
        &self,
        label: Option<&str>,
        after: Option<HyperEdgeId>,
    ) -> Result<Vec<HyperEdgeId>, QueryError> {
        let ids = match (self.write, label) {
            (Some(tx), Some(label)) => tx.hyperedge_ids_with_label(self.ns, label, after, PAGE)?,
            (Some(tx), None) => tx.hyperedge_ids(self.ns, after, PAGE)?,
            (None, Some(label)) => self
                .read
                .hyperedge_ids_with_label(self.ns, label, after, PAGE)?,
            (None, None) => self.read.hyperedge_ids(self.ns, after, PAGE)?,
        };
        self.budget.read(ids.len().max(1))?;
        Ok(ids)
    }
}

/// The node a member variable holds in `row`: `Ok(None)` when it is not
/// bound, an error when it holds something else than a node.
fn bound_node(row: &Row, var: &str) -> Result<Option<Option<NodeId>>, QueryError> {
    match row.get(var) {
        None => Ok(None),
        Some(Value::Node(node)) => Ok(Some(Some(node.id))),
        Some(Value::Null) => Ok(Some(None)),
        Some(other) => Err(QueryError::runtime(format!(
            "`{var}` is a {}, not a node: hyperedge participants are nodes",
            other.type_name()
        ))),
    }
}

/// The candidate hyperedge ids for `row`, in id order, each once.
fn candidates(
    ctx: &Context,
    pattern: &HyperPattern,
    row: &Row,
) -> Result<Vec<HyperEdgeId>, QueryError> {
    let bound = |var: &str| row.contains_key(var);
    let labels: Vec<Option<&str>> = if pattern.labels.is_empty() {
        vec![None]
    } else {
        pattern.labels.iter().map(|l| Some(l.as_str())).collect()
    };
    Ok(match Anchor::of(pattern, &bound) {
        Anchor::Bound => {
            let var = pattern.var.as_deref().unwrap_or_default();
            match row.get(var) {
                Some(Value::HyperEdge(edge)) => vec![edge.id],
                Some(Value::Null) => Vec::new(),
                Some(other) => {
                    return Err(QueryError::runtime(format!(
                        "`{var}` is a {}, not a hyperedge",
                        other.type_name()
                    )));
                }
                None => Vec::new(),
            }
        }
        Anchor::Id => {
            let expr = pattern
                .props
                .iter()
                .find(|(key, _)| *key == Key::Meta("id".into()))
                .map(|(_, expr)| expr)
                .expect("an anchor by id has an @id");
            match eval(ctx, expr, Scope::Row(row))? {
                Value::Str(text) => uuid_from_text(&text)
                    .map(|id| vec![HyperEdgeId::from(id.uuid())])
                    .unwrap_or_default(),
                Value::Null => Vec::new(),
                other => {
                    return Err(QueryError::runtime(format!(
                        "@id is a string, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        Anchor::Nodes(vars) => {
            let mut common: Option<BTreeSet<HyperEdgeId>> = None;
            for var in &vars {
                let Some(Some(node)) = bound_node(row, var)? else {
                    // A member bound to null matches nothing.
                    return Ok(Vec::new());
                };
                let mut ids = BTreeSet::new();
                for label in &labels {
                    ids.extend(ctx.hyperedge_ids_of(node, *label)?);
                }
                common = Some(match common {
                    None => ids,
                    Some(before) => before.intersection(&ids).copied().collect(),
                });
                if common.as_ref().is_some_and(BTreeSet::is_empty) {
                    break;
                }
            }
            common.unwrap_or_default().into_iter().collect()
        }
        Anchor::Labels | Anchor::All => {
            let mut ids = BTreeSet::new();
            for label in &labels {
                let mut after = None;
                loop {
                    let page = ctx.hyperedge_page(*label, after)?;
                    let full = page.len() == PAGE;
                    after = page.last().copied();
                    ids.extend(page);
                    if !full {
                        break;
                    }
                }
            }
            ids.into_iter().collect()
        }
    })
}

/// Rows extending `row` with each match of `pattern`.
pub(crate) fn match_rows(
    ctx: &Context,
    pattern: &HyperPattern,
    time: &HyperEdgeFilter,
    row: &Row,
) -> Result<Vec<Row>, QueryError> {
    let mut out = Vec::new();
    for id in candidates(ctx, pattern, row)? {
        let Some(edge) = ctx.hyperedge(id)? else {
            continue;
        };
        if !pattern.labels.is_empty() && !pattern.labels.contains(&edge.label) {
            continue;
        }
        if !time.accepts(&edge) {
            continue;
        }
        let edge = Arc::new(edge);
        let value = Value::HyperEdge(edge.clone());
        let props: Vec<(Key, crate::ast::Expr)> = pattern
            .props
            .iter()
            .filter(|(key, _)| *key != Key::Meta("id".into()))
            .cloned()
            .collect();
        if !props_fit(ctx, &value, &props, row)? {
            continue;
        }
        for bindings in assignments(ctx, pattern, &edge, row)? {
            let mut next = row.clone();
            if let Some(var) = &pattern.var {
                next.insert(var.clone(), value.clone());
            }
            for (var, node) in bindings {
                next.insert(var, Value::Node(node));
            }
            out.push(next);
            ctx.budget.rows(out.len())?;
        }
    }
    Ok(out)
}

/// Every distinct way of binding the members' variables to participants of
/// `edge`: each member a participant of its own.
fn assignments(
    ctx: &Context,
    pattern: &HyperPattern,
    edge: &HyperEdge,
    row: &Row,
) -> Result<Vec<Bindings>, QueryError> {
    // The nodes, read once each (hidden ones are absent: the hyperedge
    // would not be visible either).
    let mut nodes: Vec<(NodeId, Arc<Node>)> = Vec::new();
    for id in edge.nodes() {
        if let Some(node) = ctx.node(id)? {
            nodes.push((id, Arc::new(node)));
        }
    }
    let node_of = |id: NodeId| nodes.iter().find(|(n, _)| *n == id).map(|(_, node)| node);
    // For each member, the participants it may take.
    let mut options: Vec<Vec<usize>> = Vec::with_capacity(pattern.members.len());
    for member in &pattern.members {
        let fixed = match member.node.var.as_deref() {
            Some(var) => bound_node(row, var)?,
            None => None,
        };
        if fixed == Some(None) {
            return Ok(Vec::new());
        }
        let mut fits = Vec::new();
        for (slot, participant) in edge.participants.iter().enumerate() {
            if let Some(role) = &member.role
                && participant.role.as_deref() != Some(role.as_str())
            {
                continue;
            }
            if let Some(Some(id)) = fixed
                && participant.node != id
            {
                continue;
            }
            let Some(node) = node_of(participant.node) else {
                continue;
            };
            if node_fits(ctx, node, &member.node, row)? {
                fits.push(slot);
            }
        }
        if fits.is_empty() {
            return Ok(Vec::new());
        }
        options.push(fits);
    }
    let mut found: Vec<Bindings> = Vec::new();
    let mut seen: HashSet<Vec<NodeId>> = HashSet::new();
    let mut chosen: Vec<usize> = Vec::with_capacity(options.len());
    search(
        pattern,
        edge,
        &options,
        &mut chosen,
        &node_of,
        &mut found,
        &mut seen,
    );
    Ok(found)
}

#[allow(clippy::too_many_arguments)]
fn search<'n>(
    pattern: &HyperPattern,
    edge: &HyperEdge,
    options: &[Vec<usize>],
    chosen: &mut Vec<usize>,
    node_of: &dyn Fn(NodeId) -> Option<&'n Arc<Node>>,
    found: &mut Vec<Bindings>,
    seen: &mut HashSet<Vec<NodeId>>,
) {
    let depth = chosen.len();
    if depth == options.len() {
        let nodes: Vec<NodeId> = chosen.iter().map(|&s| edge.participants[s].node).collect();
        // A member variable named twice holds one node.
        let mut bindings: Vec<(String, Arc<Node>)> = Vec::new();
        for (member, &node) in pattern.members.iter().zip(&nodes) {
            let Some(var) = &member.node.var else {
                continue;
            };
            match bindings.iter().find(|(name, _)| name == var) {
                Some((_, held)) if held.id != node => return,
                Some(_) => {}
                None => {
                    if let Some(value) = node_of(node) {
                        bindings.push((var.clone(), value.clone()));
                    }
                }
            }
        }
        if pattern.exact {
            let mine: BTreeSet<NodeId> = nodes.iter().copied().collect();
            let theirs: BTreeSet<NodeId> = edge.participants.iter().map(|p| p.node).collect();
            if mine != theirs {
                return;
            }
        }
        // One row per distinct binding of the variables (anonymous members
        // and roles do not multiply rows).
        let key: Vec<NodeId> = bindings.iter().map(|(_, node)| node.id).collect();
        if seen.insert(key) {
            found.push(bindings);
        }
        return;
    }
    for &slot in &options[depth] {
        if chosen.contains(&slot) {
            continue;
        }
        chosen.push(slot);
        search(pattern, edge, options, chosen, node_of, found, seen);
        chosen.pop();
    }
}

/// The hyperedges `MERGE HYPEREDGE` takes as the one it names, in id
/// order: visible to the caller, holding now (neither invalidated nor
/// retracted), with its label, exactly its (node, role) pairs in any order,
/// and the properties of its pattern (others may differ).
pub(crate) fn merge_matches(
    ctx: &Context,
    new: &grafeno::NewHyperEdge,
    pattern: &HyperPattern,
    row: &Row,
) -> Result<Vec<HyperEdge>, QueryError> {
    let mut wanted = new.participants.clone();
    wanted.sort();
    let mut nodes: Vec<NodeId> = wanted.iter().map(|p| p.node).collect();
    nodes.dedup();
    // Every participant's incidence list holds the match: intersect them.
    let mut common: Option<BTreeSet<HyperEdgeId>> = None;
    for node in nodes {
        let ids: BTreeSet<HyperEdgeId> = ctx
            .hyperedge_ids_of(node, Some(&new.label))?
            .into_iter()
            .collect();
        common = Some(match common {
            None => ids,
            Some(before) => before.intersection(&ids).copied().collect(),
        });
        if common.as_ref().is_some_and(BTreeSet::is_empty) {
            break;
        }
    }
    let now = grafeno::Timestamp::now();
    let holds = HyperEdgeFilter::any().valid_at(now).known_at(now);
    let props: Vec<(Key, crate::ast::Expr)> = pattern.props.clone();
    let mut out = Vec::new();
    for id in common.unwrap_or_default() {
        let Some(edge) = ctx.hyperedge(id)? else {
            continue;
        };
        if edge.label != new.label || !holds.accepts(&edge) || edge.participants != wanted {
            continue;
        }
        let edge = Arc::new(edge);
        if !props_fit(ctx, &Value::HyperEdge(edge.clone()), &props, row)? {
            continue;
        }
        out.push(Arc::unwrap_or_clone(edge));
    }
    Ok(out)
}
