//! `MATCH HYPEREDGE`: finding hyperedges and binding their participants.
//!
//! Each row starts from the cheapest anchor its bindings allow: the
//! hyperedge variable already bound, an `@id`, the incidence lists of the
//! members whose nodes are bound (intersected), an index on hyperedges of
//! its label whose first properties the pattern or `WHERE` fix, a label's
//! hyperedges, or every hyperedge of the namespace. A candidate must have one of the
//! labels, the properties, the `VALID AT` and `KNOWN AT` time and be visible;
//! then each member is given a participant of its own: its node fits the
//! member's node pattern (or is the node its variable holds) and plays its
//! role when one is named. With `EXACT`, the members' nodes are all the
//! hyperedge's nodes. A hyperedge gives one row per distinct way of binding
//! the members' variables.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use grafeno::{HyperEdge, HyperEdgeFilter, HyperEdgeId, IndexDef, IndexRange, Node, NodeId};

use crate::QueryError;
use crate::ast::{Expr, HyperPattern, Key};
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
    /// This index on hyperedges, by the values the pattern or `WHERE` give.
    Index(String),
    /// The label index, for each label.
    Labels,
    /// Every hyperedge of the namespace.
    All,
}

/// An index chosen for a hyperedge pattern: the index, the values for its
/// first properties, and the seek on the next one.
pub(crate) type IndexSeek<'q> = (&'q IndexDef, Vec<(String, &'q Expr)>, crate::plan::Seek<'q>);

/// The best index on hyperedges for `pattern`: by value, on every one of
/// its labels, with the longest run of fixed properties (a unique index
/// wins a tie); its name and the seek on it.
pub(crate) fn index_seek<'q>(
    pattern: &'q HyperPattern,
    bound: &dyn Fn(&str) -> bool,
    conditions: &[&'q Expr],
    indexes: &'q [IndexDef],
) -> Option<IndexSeek<'q>> {
    if pattern.labels.is_empty() {
        return None;
    }
    let mut best: Option<IndexSeek> = None;
    for def in indexes.iter().filter(|def| {
        def.hyperedges
            && def.by_value()
            && pattern
                .labels
                .iter()
                .all(|label| def.labels().contains(&label.as_str()))
    }) {
        let Some((prefix, _key, seek)) = crate::plan::seek_for_keys(
            &pattern.props,
            pattern.var.as_deref(),
            &def.keys(),
            bound,
            conditions,
        ) else {
            continue;
        };
        let fixed = |prefix: &[(String, &Expr)], seek: &crate::plan::Seek| {
            prefix.len() * 2 + usize::from(matches!(seek, crate::plan::Seek::Equal(_))) * 2 + 1
        };
        let better = match &best {
            None => true,
            Some((other, other_prefix, other_seek)) => {
                let (mine, theirs) = (fixed(&prefix, &seek), fixed(other_prefix, other_seek));
                mine > theirs || (mine == theirs && def.unique && !other.unique)
            }
        };
        if better {
            best = Some((def, prefix, seek));
        }
    }
    best
}

impl Anchor {
    pub(crate) fn of(
        pattern: &HyperPattern,
        bound: &dyn Fn(&str) -> bool,
        conditions: &[&Expr],
        indexes: &[IndexDef],
    ) -> Anchor {
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
        if let Some((def, _, _)) = index_seek(pattern, bound, conditions, indexes) {
            return Anchor::Index(def.name.clone());
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
            Anchor::Index(name) => {
                format!("hyperedges {var}{labels} by index {name} (property index)")
            }
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

    /// The hyperedges index `def` holds for `prefix` and `range`, those the
    /// caller may see (retracted ones too: the pattern checks them).
    fn hyperedge_index_seek(
        &self,
        def: &IndexDef,
        prefix: &[serde_json::Value],
        range: &IndexRange,
        limit: usize,
    ) -> Result<Vec<HyperEdgeId>, QueryError> {
        if self.blind() {
            return Ok(Vec::new());
        }
        // An index on a property the caller may not read finds nothing.
        if let Some(view) = self.per_record()
            && def.keys().iter().any(|key| {
                def.labels()
                    .iter()
                    .any(|label| view.property_hidden(self.ns, &[(*label).to_owned()], key))
            })
        {
            return Ok(Vec::new());
        }
        let ids = match self.write {
            Some(tx) => tx.index_seek_hyperedges(self.ns, &def.name, prefix, Some(range), limit)?,
            None => {
                self.read
                    .index_seek_hyperedges(self.ns, &def.name, prefix, Some(range), limit)?
            }
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
    conditions: &[&Expr],
    row: &Row,
) -> Result<Vec<HyperEdgeId>, QueryError> {
    let bound = |var: &str| row.contains_key(var);
    let labels: Vec<Option<&str>> = if pattern.labels.is_empty() {
        vec![None]
    } else {
        pattern.labels.iter().map(|l| Some(l.as_str())).collect()
    };
    Ok(
        match Anchor::of(pattern, &bound, conditions, &ctx.info.indexes) {
            Anchor::Index(_) => {
                let (def, prefix, seek) =
                    index_seek(pattern, &bound, conditions, &ctx.info.indexes)
                        .expect("the anchor found it");
                match index_candidates(ctx, def, &prefix, seek, row, usize::MAX)? {
                    Some(ids) => ids,
                    // Values an index cannot hold (a list, a time): by label.
                    None => label_candidates(ctx, &labels)?,
                }
            }
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
                let common: Vec<HyperEdgeId> = common.unwrap_or_default().into_iter().collect();
                // A property index that finds fewer (a unique key among a
                // busy node's hyperedges) is read instead: asked for no more
                // than the incidence lists hold, it never costs more.
                if common.len() > 1
                    && let Some((def, prefix, seek)) =
                        index_seek(pattern, &bound, conditions, &ctx.info.indexes)
                    && let Some(fewer) =
                        index_candidates(ctx, def, &prefix, seek, row, common.len())?
                    && fewer.len() < common.len()
                {
                    return Ok(fewer);
                }
                common
            }
            Anchor::Labels | Anchor::All => label_candidates(ctx, &labels)?,
        },
    )
}

/// Every hyperedge id of these labels (`None`: of the namespace).
fn label_candidates(
    ctx: &Context,
    labels: &[Option<&str>],
) -> Result<Vec<HyperEdgeId>, QueryError> {
    let mut ids = BTreeSet::new();
    for label in labels {
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
    Ok(ids.into_iter().collect())
}

/// The hyperedges index `def` finds for this row, in id order, up to
/// `limit` of them (more than `limit` gives `limit`); `None` when its
/// values cannot be looked up. A superset: what it finds is checked against
/// the pattern and `WHERE` afterwards.
fn index_candidates(
    ctx: &Context,
    def: &IndexDef,
    prefix: &[(String, &Expr)],
    seek: crate::plan::Seek,
    row: &Row,
    limit: usize,
) -> Result<Option<Vec<HyperEdgeId>>, QueryError> {
    let Some(ranges) = crate::plan::seek_ranges(ctx, seek, row)? else {
        return Ok(None);
    };
    let mut values = Vec::with_capacity(prefix.len());
    for (_, expr) in prefix {
        match eval(ctx, expr, Scope::Row(row))? {
            // `= null` is never true.
            Value::Null => return Ok(Some(Vec::new())),
            value => match crate::plan::indexable(&value) {
                Some(json) => values.push(json),
                None => return Ok(None),
            },
        }
    }
    let mut ids = BTreeSet::new();
    for range in &ranges {
        ids.extend(ctx.hyperedge_index_seek(def, &values, range, limit)?);
        if ids.len() >= limit {
            break;
        }
    }
    Ok(Some(ids.into_iter().take(limit).collect()))
}

/// Rows extending `row` with each match of `pattern`. `filter` (the
/// clause's `WHERE`) only picks where to start; the caller applies it.
/// With `slice` (part, parts), only that share of the candidates, in id
/// order: a part of a query split across cluster members.
pub(crate) fn match_rows(
    ctx: &Context,
    pattern: &HyperPattern,
    time: &HyperEdgeFilter,
    filter: Option<&Expr>,
    row: &Row,
    slice: Option<(usize, usize)>,
) -> Result<Vec<Row>, QueryError> {
    let conditions = filter.map(crate::plan::conjuncts).unwrap_or_default();
    let mut ids = candidates(ctx, pattern, &conditions, row)?;
    if let Some((part, parts)) = slice {
        let (from, to) = (ids.len() * part / parts, ids.len() * (part + 1) / parts);
        ids = ids[from..to].to_vec();
    }
    let mut out = Vec::new();
    for id in ids {
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
    // No members: one way, binding nothing, and no node to read.
    if pattern.members.is_empty() {
        return Ok(vec![Vec::new()]);
    }
    // The nodes a member may take, read once each when first needed
    // (hidden ones are absent: the hyperedge would not be visible either).
    let mut nodes: Vec<(NodeId, Option<Arc<Node>>)> = Vec::new();
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
            let at = match nodes.iter().position(|(n, _)| *n == participant.node) {
                Some(at) => at,
                None => {
                    let read = ctx.node(participant.node)?.map(Arc::new);
                    nodes.push((participant.node, read));
                    nodes.len() - 1
                }
            };
            let Some(node) = &nodes[at].1 else {
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
    let node_of = |id: NodeId| {
        nodes
            .iter()
            .find(|(n, _)| *n == id)
            .and_then(|(_, node)| node.as_ref())
    };
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
    // A match the caller may not see would be duplicated by creating one:
    // refuse instead, without saying which (as deleting a node with a
    // hidden hyperedge does).
    if out.is_empty()
        && ctx.per_record().is_some()
        && let Some(tx) = ctx.write
        && hidden_match(tx, ctx.ns, new, &wanted, now)?
    {
        return Err(QueryError::from(grafeno::Error::Forbidden(
            "MERGE HYPEREDGE matches a hyperedge the caller may not see; it creates none".into(),
        )));
    }
    Ok(out)
}

/// Whether a hyperedge holding at `now` is the one `new` names, as stored
/// (no view): its label, exactly its participants, and its properties.
fn hidden_match(
    tx: &grafeno::WriteTx,
    ns: &str,
    new: &grafeno::NewHyperEdge,
    wanted: &[grafeno::Participant],
    now: grafeno::Timestamp,
) -> Result<bool, QueryError> {
    let Some(first) = wanted.first() else {
        return Ok(false);
    };
    let holds = HyperEdgeFilter::any().valid_at(now).known_at(now);
    for id in tx.hyperedge_ids_of(ns, first.node, Some(&new.label))? {
        let Some(edge) = tx.hyperedge(ns, id)? else {
            continue;
        };
        if edge.participants == wanted
            && holds.accepts(&edge)
            && new.props.iter().all(|(key, value)| {
                edge.props
                    .get(key)
                    .is_some_and(|stored| same(stored, value))
            })
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Equal stored values, numbers compared as numbers (`7` and `7.0`).
fn same(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}
