//! Expression evaluation: operators, pseudo-properties, functions and
//! aggregates.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use grafeno::{
    Direction, Edge, EdgeFilter, EdgeId, IndexRange, Node, NodeId, ReadTx, Timestamp, WriteTx,
};

use crate::QueryError;
use crate::ast::{AGGREGATES, BinOp, Expr, Key, MapItem, PatternKind, Quantifier, UnOp};
use crate::limits::Budget;
use crate::plan::PlanInfo;
use crate::temporal::{self, DURATION_COMPONENTS, Duration, TIME_PARTS};
use crate::value::{Value, compare, equals, sort_order};

/// Variable bindings of one result row.
pub(crate) type Row = HashMap<String, Value>;

/// A row as exact JSON (see [`Value::to_wire`]).
pub(crate) fn row_to_wire(row: &Row) -> serde_json::Value {
    serde_json::Value::Object(
        row.iter()
            .map(|(name, value)| (name.clone(), value.to_wire()))
            .collect(),
    )
}

/// A row from [`row_to_wire`].
pub(crate) fn row_from_wire(json: &serde_json::Value) -> Result<Row, QueryError> {
    json.as_object()
        .ok_or_else(|| QueryError::runtime("a partial row is malformed"))?
        .iter()
        .map(|(name, value)| Ok((name.clone(), Value::from_wire(value)?)))
        .collect()
}

/// Query parameters (`$name`).
pub type Params = HashMap<String, Value>;

/// What expressions can read: the snapshot the query started on and, in a
/// writing query, the write transaction, which sees the query's own writes
/// and is read instead. Every read counts against the query's budget.
pub(crate) struct Context<'a> {
    pub(crate) read: &'a ReadTx,
    pub(crate) write: Option<&'a WriteTx>,
    pub(crate) ns: &'a str,
    pub(crate) params: &'a Params,
    /// The edges patterns may follow: the enclosing `MATCH`'s `VALID AT`
    /// and `KNOWN AT`, for pattern predicates too.
    pub(crate) edges: &'a EdgeFilter,
    pub(crate) info: &'a PlanInfo,
    pub(crate) budget: &'a Budget,
    /// What the caller may see (14.4); everything when `None`.
    pub(crate) view: Option<&'a std::sync::Arc<grafeno::View>>,
    /// Whether the caller may read the audit log (14.5).
    pub(crate) may_audit: bool,
}

/// Part `part` of `parts` of the candidates of a query's first match (a
/// query split across cluster members). Taken by the first match only.
pub(crate) struct Slice {
    taken: std::sync::atomic::AtomicBool,
    part: usize,
    parts: usize,
}

impl Slice {
    pub(crate) fn new(part: usize, parts: usize) -> Self {
        Self {
            taken: std::sync::atomic::AtomicBool::new(false),
            part,
            parts,
        }
    }

    /// `(part, parts)` the first time; `None` after.
    pub(crate) fn take(&self) -> Option<(usize, usize)> {
        (!self.taken.swap(true, std::sync::atomic::Ordering::AcqRel))
            .then_some((self.part, self.parts))
    }
}

impl Context<'_> {
    /// The view, when records of this namespace are checked one by one.
    pub(crate) fn per_record(&self) -> Option<&grafeno::View> {
        self.view
            .map(|view| view.as_ref())
            .filter(|view| view.per_record(self.ns))
    }

    /// Whether nothing of this namespace may be read.
    fn blind(&self) -> bool {
        self.view.is_some_and(|view| !view.reads(self.ns))
    }

    fn raw_node(&self, id: NodeId) -> Result<Option<Node>, QueryError> {
        Ok(match self.write {
            Some(tx) => tx.node(self.ns, id)?,
            None => self.read.node(self.ns, id)?,
        })
    }

    /// Whether the node `id` exists and the caller may see it.
    pub(crate) fn visible(&self, id: NodeId) -> Result<bool, QueryError> {
        if self.blind() {
            return Ok(false);
        }
        match self.per_record() {
            None => Ok(true),
            Some(view) => Ok(self
                .raw_node(id)?
                .is_some_and(|node| view.node_visible(self.ns, &node))),
        }
    }

    pub(crate) fn node(&self, id: NodeId) -> Result<Option<Node>, QueryError> {
        self.budget.read(1)?;
        if self.blind() {
            return Ok(None);
        }
        let node = self.raw_node(id)?;
        Ok(match self.per_record() {
            Some(view) => node.and_then(|node| view.node(self.ns, node)),
            None => node,
        })
    }

    pub(crate) fn edge(&self, id: EdgeId) -> Result<Option<Edge>, QueryError> {
        self.budget.read(1)?;
        if self.blind() {
            return Ok(None);
        }
        let edge = match self.write {
            Some(tx) => tx.edge(self.ns, id)?,
            None => self.read.edge(self.ns, id)?,
        };
        match (edge, self.per_record()) {
            (Some(edge), Some(view)) => Ok(view.edge(self.ns, edge, |end| {
                self.visible(end)
                    .map_err(|e| grafeno::Error::Invalid(e.message))
            })?),
            (edge, _) => Ok(edge),
        }
    }

    /// Up to `limit` visible nodes after `after`, of `label` or of every
    /// label (fewer only at the end).
    pub(crate) fn nodes_page(
        &self,
        label: Option<&str>,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<Node>, QueryError> {
        if self.blind() {
            return Ok(Vec::new());
        }
        let fetch = |after: Option<NodeId>| -> Result<Vec<Node>, QueryError> {
            let page = match self.write {
                Some(tx) => tx.nodes_page(self.ns, label, after, limit)?,
                None => self.read.nodes_page(self.ns, label, after, limit)?,
            };
            self.budget.read(page.len())?;
            Ok(page)
        };
        let Some(view) = self.per_record() else {
            return fetch(after);
        };
        let mut out = Vec::new();
        let mut after = after;
        loop {
            let page = fetch(after)?;
            let full = page.len() == limit;
            after = page.last().map(|node| node.id).or(after);
            out.extend(page.into_iter().filter_map(|node| view.node(self.ns, node)));
            if out.len() >= limit || !full {
                out.truncate(limit);
                return Ok(out);
            }
        }
    }

    pub(crate) fn neighbor_ids(
        &self,
        from: NodeId,
        direction: Direction,
        filter: &EdgeFilter,
    ) -> Result<Vec<(EdgeId, NodeId)>, QueryError> {
        if self.blind() {
            return Ok(Vec::new());
        }
        let found = match self.write {
            Some(tx) => tx.neighbor_ids(self.ns, from, direction, filter)?,
            None => self.read.neighbor_ids(self.ns, from, direction, filter)?,
        };
        self.budget.read(found.len().max(1))?;
        if self.per_record().is_none() {
            return Ok(found);
        }
        let mut visible = Vec::with_capacity(found.len());
        for (edge, node) in found {
            if self.edge(edge)?.is_some() {
                visible.push((edge, node));
            }
        }
        Ok(visible)
    }

    /// The nodes index `name` finds: values equal to `prefix` for its
    /// first properties (a composite index) and in `range` for the next.
    /// For an index on edges (`edges`), the ends of the edges found on
    /// that side (`Out`: where they leave, `In`: where they arrive).
    pub(crate) fn index_seek(
        &self,
        name: &str,
        prefix: &[serde_json::Value],
        range: &IndexRange,
        edges: Option<grafeno::Direction>,
    ) -> Result<Vec<NodeId>, QueryError> {
        let Some(def) = self.info.indexes.iter().find(|def| def.name == name) else {
            return Ok(Vec::new());
        };
        if edges.is_none() && def.is_simple() && prefix.is_empty() {
            return self.index_lookup(name, range);
        }
        if self.blind() {
            return Ok(Vec::new());
        }
        let Some(end) = edges else {
            if let Some(view) = self.per_record()
                && def.keys().iter().any(|key| {
                    def.labels()
                        .iter()
                        .any(|label| view.property_hidden(self.ns, &[(*label).to_owned()], key))
                })
            {
                return Ok(Vec::new());
            }
            let found = match self.write {
                Some(tx) => tx.index_seek_nodes(self.ns, name, prefix, Some(range), usize::MAX)?,
                None => {
                    self.read
                        .index_seek_nodes(self.ns, name, prefix, Some(range), usize::MAX)?
                }
            };
            self.budget.read(found.len().max(1))?;
            if self.per_record().is_none() {
                return Ok(found);
            }
            let mut visible = Vec::with_capacity(found.len());
            for id in found {
                if self.visible(id)? {
                    visible.push(id);
                }
            }
            return Ok(visible);
        };
        let found = match self.write {
            Some(tx) => tx.index_seek_edges(self.ns, name, prefix, Some(range), usize::MAX)?,
            None => self
                .read
                .index_seek_edges(self.ns, name, prefix, Some(range), usize::MAX)?,
        };
        self.budget.read(found.len().max(1))?;
        let mut out = Vec::with_capacity(found.len());
        for id in found {
            // Retracted facts stay candidates: a query known at an earlier
            // time may still follow them.
            if let Some(edge) = self.edge(id)? {
                match end {
                    grafeno::Direction::Out => out.push(edge.from),
                    grafeno::Direction::In => out.push(edge.to),
                    grafeno::Direction::Both => out.extend([edge.from, edge.to]),
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn index_lookup(
        &self,
        name: &str,
        range: &IndexRange,
    ) -> Result<Vec<NodeId>, QueryError> {
        if self.blind() {
            return Ok(Vec::new());
        }
        // An index on a property the caller may not read finds nothing.
        if let Some(view) = self.per_record()
            && let Some(def) = self.info.indexes.iter().find(|def| def.name == name)
            && view.property_hidden(self.ns, std::slice::from_ref(&def.label), &def.key)
        {
            return Ok(Vec::new());
        }
        let found = match self.write {
            Some(tx) => tx.index_lookup(self.ns, name, range, usize::MAX)?,
            None => self.read.index_lookup(self.ns, name, range, usize::MAX)?,
        };
        self.budget.read(found.len().max(1))?;
        if self.per_record().is_none() {
            return Ok(found);
        }
        let mut visible = Vec::with_capacity(found.len());
        for id in found {
            if self.visible(id)? {
                visible.push(id);
            }
        }
        Ok(visible)
    }

    fn text(&self, id: NodeId) -> Result<Option<String>, QueryError> {
        if !self.visible(id)? {
            return Ok(None);
        }
        Ok(match self.write {
            Some(tx) => tx.text(self.ns, id)?,
            None => self.read.text(self.ns, id)?,
        })
    }

    fn memory(&self, node: &Node) -> Result<grafeno::MemoryState, QueryError> {
        Ok(match self.write {
            Some(tx) => tx.memory(self.ns, node)?,
            None => self.read.memory(self.ns, node)?,
        })
    }

    pub(crate) fn embedding(&self, id: NodeId) -> Result<Option<Vec<f32>>, QueryError> {
        if !self.visible(id)? {
            return Ok(None);
        }
        Ok(match self.write {
            Some(tx) => tx.embedding(self.ns, id)?,
            None => self.read.embedding(self.ns, id)?,
        })
    }

    /// Search hits the caller may see, up to `limit`: `fetch(n)` asks for
    /// the best `n`, and asks again for more while hidden ones crowd the
    /// visible out.
    pub(crate) fn visible_hits(
        &self,
        limit: usize,
        fetch: impl Fn(usize) -> Result<Vec<grafeno::Hit>, QueryError>,
    ) -> Result<Vec<(Node, f32)>, QueryError> {
        let mut ask = limit.max(1);
        loop {
            let hits = fetch(ask)?;
            let exhausted = hits.len() < ask;
            let mut out = Vec::new();
            for hit in hits {
                if let Some(node) = self.node(hit.node)? {
                    out.push((node, hit.score));
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
            if exhausted || self.per_record().is_none() || ask >= 1 << 20 {
                return Ok(out);
            }
            ask *= 4;
        }
    }
}

/// Where variables come from: one row, or a group of rows already folded
/// (`aggregate.rs`): its first row, the value of each aggregate in it, and
/// whether the group is empty.
#[derive(Clone, Copy)]
pub(crate) enum Scope<'a> {
    Row(&'a Row),
    Folded(&'a Row, &'a [(Expr, Value)], bool),
}

/// The folded value of an aggregate expression.
fn folded(values: &[(Expr, Value)], expr: &Expr) -> Result<Value, QueryError> {
    values
        .iter()
        .find(|(aggregate, _)| aggregate == expr)
        .map(|(_, value)| value.clone())
        .ok_or_else(|| error("an aggregate was not folded"))
}

impl Scope<'_> {
    fn var(&self, name: &str) -> Option<&Value> {
        match self {
            Scope::Row(row) => row.get(name),
            Scope::Folded(row, _, _) => row.get(name),
        }
    }

    /// The row variables come from (a group's first row).
    fn row(&self) -> Row {
        match self {
            Scope::Row(row) => (*row).clone(),
            Scope::Folded(row, _, _) => (*row).clone(),
        }
    }
}

/// The items of a list (`null` gives none; anything else is an error).
fn items_of(value: Value, what: &str) -> Result<Vec<Value>, QueryError> {
    match value {
        Value::List(items) => Ok(items),
        Value::Null => Ok(Vec::new()),
        other => Err(error(format!(
            "{what} needs a list, got a {}",
            other.type_name()
        ))),
    }
}

fn error(message: impl Into<String>) -> QueryError {
    QueryError::runtime(message)
}

/// `true`, `false` or unknown (`null`); anything else is an error.
pub(crate) fn truth(value: &Value) -> Result<Option<bool>, QueryError> {
    match value {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(error(format!(
            "expected a boolean, got a {}",
            other.type_name()
        ))),
    }
}

fn from_truth(value: Option<bool>) -> Value {
    value.map_or(Value::Null, Value::Bool)
}

/// A point in time: a time, an ISO 8601 string or microseconds.
pub(crate) fn to_time(value: &Value) -> Result<Timestamp, QueryError> {
    match value {
        Value::Time(t) => Ok(*t),
        Value::Str(s) => Ok(Timestamp::parse(s)?),
        Value::Int(micros) => Ok(Timestamp::from_micros(*micros)),
        other => Err(error(format!(
            "expected a time, got a {}",
            other.type_name()
        ))),
    }
}

fn display(value: &Value) -> String {
    match value {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Time(t) => t.iso(),
        Value::Duration(d) => d.iso(),
        other => other.to_json().to_string(),
    }
}

fn ids(ids: &[NodeId]) -> Value {
    Value::List(ids.iter().map(|id| Value::Str(id.to_string())).collect())
}

fn time_or_null(time: Option<Timestamp>) -> Value {
    time.map_or(Value::Null, Value::Time)
}

const NODE_META: &str = "@id, @labels, @text, @embedding, @sources, @created_at, @updated_at, @importance, @accesses, @last_access";
const EDGE_META: &str =
    "@id, @label, @from, @to, @valid_from, @valid_to, @recorded_at, @expired_at, @sources";

pub(crate) fn node_meta(ctx: &Context, node: &Node, name: &str) -> Result<Value, QueryError> {
    Ok(match name {
        "id" => Value::Str(node.id.to_string()),
        "labels" => Value::List(node.labels.iter().cloned().map(Value::Str).collect()),
        "text" => ctx.text(node.id)?.map_or(Value::Null, Value::Str),
        "embedding" => ctx.embedding(node.id)?.map_or(Value::Null, |vector| {
            Value::List(
                vector
                    .into_iter()
                    .map(|x| Value::Float(f64::from(x)))
                    .collect(),
            )
        }),
        "sources" => ids(&node.sources),
        "created_at" => Value::Time(node.created_at),
        "recorded_by" => node.recorded_by.clone().map_or(Value::Null, Value::Str),
        // Through the shortest decimal form, so 0.9 reads as 0.9 and not as the
        // f64 closest to the stored f32.
        "importance" => Value::Float(
            ctx.memory(node)?
                .importance
                .to_string()
                .parse()
                .unwrap_or_default(),
        ),
        "accesses" => Value::Int(i64::from(ctx.memory(node)?.accesses)),
        "last_access" => Value::Time(ctx.memory(node)?.last_access),
        "updated_at" => Value::Time(node.updated_at),
        other => {
            return Err(error(format!(
                "nodes have no pseudo-property @{other} (they have {NODE_META})"
            )));
        }
    })
}

fn edge_meta(edge: &Edge, name: &str) -> Result<Value, QueryError> {
    Ok(match name {
        "id" => Value::Str(edge.id.to_string()),
        "label" => Value::Str(edge.label.clone()),
        "from" => Value::Str(edge.from.to_string()),
        "to" => Value::Str(edge.to.to_string()),
        "valid_from" => time_or_null(edge.valid_from),
        "valid_to" => time_or_null(edge.valid_to),
        "recorded_at" => Value::Time(edge.recorded_at),
        "recorded_by" => edge.recorded_by.clone().map_or(Value::Null, Value::Str),
        "expired_at" => time_or_null(edge.expired_at),
        "sources" => ids(&edge.sources),
        other => {
            return Err(error(format!(
                "edges have no pseudo-property @{other} (they have {EDGE_META})"
            )));
        }
    })
}

pub(crate) fn property(ctx: &Context, target: &Value, key: &Key) -> Result<Value, QueryError> {
    let prop =
        |props: &grafeno::Props, name: &str| props.get(name).map_or(Value::Null, Value::from_json);
    match (target, key) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Node(node), Key::Prop(name)) => Ok(prop(&node.props, name)),
        (Value::Node(node), Key::Meta(name)) => node_meta(ctx, node, name),
        (Value::Edge(edge), Key::Prop(name)) => Ok(prop(&edge.props, name)),
        (Value::Edge(edge), Key::Meta(name)) => edge_meta(edge, name),
        (Value::Map(map), Key::Prop(name)) => Ok(map.get(name).cloned().unwrap_or(Value::Null)),
        (Value::Time(time), Key::Prop(name)) => {
            temporal::part(*time, name).map(Value::Int).ok_or_else(|| {
                error(format!(
                    "times have no part {name:?} (they have {TIME_PARTS})"
                ))
            })
        }
        (Value::Duration(duration), Key::Prop(name)) => {
            duration.component(name).map(Value::Int).ok_or_else(|| {
                error(format!(
                    "durations have no component {name:?} (they have {DURATION_COMPONENTS})"
                ))
            })
        }
        (other, _) => Err(error(format!(
            "cannot read a property of a {}",
            other.type_name()
        ))),
    }
}

pub(crate) fn eval(ctx: &Context, expr: &Expr, scope: Scope) -> Result<Value, QueryError> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Param(name) => ctx
            .params
            .get(name)
            .cloned()
            .ok_or_else(|| error(format!("missing parameter ${name}"))),
        Expr::Var(name) => match scope.var(name) {
            Some(value) => Ok(value.clone()),
            None if matches!(scope, Scope::Folded(_, _, true)) => Ok(Value::Null),
            None => Err(error(format!("unknown variable `{name}`"))),
        },
        Expr::Property(target, key) => property(ctx, &eval(ctx, target, scope)?, key),
        Expr::Index(target, index) => {
            let (target, index) = (eval(ctx, target, scope)?, eval(ctx, index, scope)?);
            match (&target, &index) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::List(items), Value::Int(i)) => {
                    let len = items.len() as i64;
                    let at = if *i < 0 { len + i } else { *i };
                    Ok(usize::try_from(at)
                        .ok()
                        .and_then(|at| items.get(at).cloned())
                        .unwrap_or(Value::Null))
                }
                (Value::Map(map), Value::Str(key)) => {
                    Ok(map.get(key).cloned().unwrap_or(Value::Null))
                }
                (Value::Node(_) | Value::Edge(_), Value::Str(key)) => {
                    property(ctx, &target, &Key::Prop(key.clone()))
                }
                _ => Err(error(format!(
                    "cannot index a {} with a {}",
                    target.type_name(),
                    index.type_name()
                ))),
            }
        }
        Expr::List(items) => {
            let value = Value::List(
                items
                    .iter()
                    .map(|item| eval(ctx, item, scope))
                    .collect::<Result<_, _>>()?,
            );
            ctx.budget.value(&value)?;
            Ok(value)
        }
        Expr::Map(entries) => {
            let value = Value::Map(
                entries
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), eval(ctx, value, scope)?)))
                    .collect::<Result<BTreeMap<_, _>, QueryError>>()?,
            );
            ctx.budget.value(&value)?;
            Ok(value)
        }
        Expr::Unary(UnOp::Not, inner) => {
            Ok(from_truth(truth(&eval(ctx, inner, scope)?)?.map(|b| !b)))
        }
        Expr::Unary(UnOp::Neg, inner) => match eval(ctx, inner, scope)? {
            Value::Null => Ok(Value::Null),
            Value::Int(i) => i
                .checked_neg()
                .map(Value::Int)
                .ok_or_else(|| error("integer overflow")),
            Value::Float(f) => Ok(Value::Float(-f)),
            Value::Duration(d) => Ok(Value::Duration(d.negated())),
            other => Err(error(format!("cannot negate a {}", other.type_name()))),
        },
        Expr::Binary(BinOp::And, left, right) => {
            let left = truth(&eval(ctx, left, scope)?)?;
            if left == Some(false) {
                return Ok(Value::Bool(false));
            }
            let right = truth(&eval(ctx, right, scope)?)?;
            Ok(match (left, right) {
                (_, Some(false)) => Value::Bool(false),
                (Some(true), Some(true)) => Value::Bool(true),
                _ => Value::Null,
            })
        }
        Expr::Binary(BinOp::Or, left, right) => {
            let left = truth(&eval(ctx, left, scope)?)?;
            if left == Some(true) {
                return Ok(Value::Bool(true));
            }
            let right = truth(&eval(ctx, right, scope)?)?;
            Ok(match (left, right) {
                (_, Some(true)) => Value::Bool(true),
                (Some(false), Some(false)) => Value::Bool(false),
                _ => Value::Null,
            })
        }
        Expr::Binary(BinOp::Xor, left, right) => {
            let left = truth(&eval(ctx, left, scope)?)?;
            let right = truth(&eval(ctx, right, scope)?)?;
            Ok(from_truth(left.zip(right).map(|(a, b)| a != b)))
        }
        Expr::Binary(op, left, right) => {
            let (left, right) = (eval(ctx, left, scope)?, eval(ctx, right, scope)?);
            if *op == BinOp::Add {
                // Before the concatenation allocates: a string doubled
                // forty times must stop at the limit, not past it.
                let shallow = |value: &Value| match value {
                    Value::Str(s) => s.len(),
                    Value::List(items) => items.len() * std::mem::size_of::<Value>(),
                    _ => 0,
                };
                ctx.budget
                    .memory(shallow(&left) + shallow(&right), "built a value")?;
            }
            let value = binary(*op, &left, &right)?;
            if *op == BinOp::Add && matches!(value, Value::Str(_) | Value::List(_)) {
                ctx.budget.value(&value)?;
            }
            Ok(value)
        }
        Expr::IsNull { expr, negated } => {
            Ok(Value::Bool(eval(ctx, expr, scope)?.is_null() != *negated))
        }
        Expr::HasLabels(target, labels) => match eval(ctx, target, scope)? {
            Value::Null => Ok(Value::Null),
            Value::Node(node) => Ok(Value::Bool(
                labels.iter().all(|label| node.has_label(label)),
            )),
            other => Err(error(format!(
                "only nodes have labels, not a {}",
                other.type_name()
            ))),
        },
        Expr::CountStar => match scope {
            Scope::Folded(_, values, _) => folded(values, expr),
            Scope::Row(_) => Err(error("count(*) is an aggregate: use it in RETURN or WITH")),
        },
        Expr::Slice(target, from, to) => {
            let target = eval(ctx, target, scope)?;
            let from = from
                .as_ref()
                .map(|expr| eval(ctx, expr, scope))
                .transpose()?;
            let to = to.as_ref().map(|expr| eval(ctx, expr, scope)).transpose()?;
            let items = match target {
                Value::List(items) => items,
                Value::Null => return Ok(Value::Null),
                other => return Err(error(format!("cannot slice a {}", other.type_name()))),
            };
            let len = items.len() as i64;
            let at = |value: Option<Value>, default: i64| -> Result<Option<i64>, QueryError> {
                match value {
                    None => Ok(Some(default)),
                    Some(Value::Null) => Ok(None),
                    Some(Value::Int(i)) if i < 0 => Ok(Some((len + i).max(0))),
                    Some(Value::Int(i)) => Ok(Some(i.min(len))),
                    Some(other) => Err(error(format!(
                        "a slice needs integers, got a {}",
                        other.type_name()
                    ))),
                }
            };
            Ok(match (at(from, 0)?, at(to, len)?) {
                (Some(from), Some(to)) if from < to => {
                    Value::List(items[from as usize..to as usize].to_vec())
                }
                (Some(_), Some(_)) => Value::List(Vec::new()),
                _ => Value::Null,
            })
        }
        Expr::Case {
            subject,
            branches,
            otherwise,
        } => {
            let subject = subject
                .as_ref()
                .map(|subject| eval(ctx, subject, scope))
                .transpose()?;
            for (when, then) in branches {
                let when = eval(ctx, when, scope)?;
                let hit = match &subject {
                    Some(subject) => equals(subject, &when) == Some(true),
                    None => truth(&when)? == Some(true),
                };
                if hit {
                    return eval(ctx, then, scope);
                }
            }
            match otherwise {
                Some(otherwise) => eval(ctx, otherwise, scope),
                None => Ok(Value::Null),
            }
        }
        Expr::Comprehension {
            var,
            list,
            filter,
            map,
        } => {
            let items = items_of(eval(ctx, list, scope)?, "a list comprehension")?;
            let mut row = scope.row();
            let mut out = Vec::new();
            let mut bytes = 0usize;
            for item in items {
                ctx.budget.work(1)?;
                row.insert(var.clone(), item.clone());
                if let Some(filter) = filter
                    && truth(&eval(ctx, filter, Scope::Row(&row))?)? != Some(true)
                {
                    continue;
                }
                let item = match map {
                    Some(map) => eval(ctx, map, Scope::Row(&row))?,
                    None => item,
                };
                bytes += std::mem::size_of::<Value>()
                    + crate::limits::footprint(&item, crate::limits::MAX_VALUE_DEPTH).unwrap_or(0);
                ctx.budget.memory(bytes, "built a list")?;
                out.push(item);
            }
            let value = Value::List(out);
            ctx.budget.value(&value)?;
            Ok(value)
        }
        Expr::Quantified {
            quantifier,
            var,
            list,
            predicate,
        } => {
            let value = eval(ctx, list, scope)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let items = items_of(value, "a quantifier")?;
            let mut row = scope.row();
            let (mut yes, mut no, mut unknown) = (0usize, 0usize, 0usize);
            for item in items {
                ctx.budget.work(1)?;
                row.insert(var.clone(), item);
                match truth(&eval(ctx, predicate, Scope::Row(&row))?)? {
                    Some(true) => yes += 1,
                    Some(false) => no += 1,
                    None => unknown += 1,
                }
            }
            // Three-valued: unknown items make the answer
            // unknown unless the known ones already decide it.
            Ok(match quantifier {
                Quantifier::All if no > 0 => Value::Bool(false),
                Quantifier::Any if yes > 0 => Value::Bool(true),
                Quantifier::None if yes > 0 => Value::Bool(false),
                Quantifier::Single if yes > 1 => Value::Bool(false),
                _ if unknown > 0 => Value::Null,
                Quantifier::All => Value::Bool(true),
                Quantifier::Any => Value::Bool(false),
                Quantifier::None => Value::Bool(true),
                Quantifier::Single => Value::Bool(yes == 1),
            })
        }
        Expr::Reduce {
            acc,
            init,
            var,
            list,
            expr,
        } => {
            let mut value = eval(ctx, init, scope)?;
            let items = items_of(eval(ctx, list, scope)?, "reduce()")?;
            let mut row = scope.row();
            for item in items {
                ctx.budget.work(1)?;
                row.insert(acc.clone(), value);
                row.insert(var.clone(), item);
                value = eval(ctx, expr, Scope::Row(&row))?;
            }
            Ok(value)
        }
        Expr::Exists { patterns, filter } => Ok(Value::Bool(crate::exec::exists(
            ctx,
            &scope.row(),
            patterns,
            filter.as_deref(),
        )?)),
        Expr::PatternQuery {
            kind,
            patterns,
            filter,
            map,
        } => {
            let rows = crate::exec::pattern_rows(ctx, &scope.row(), patterns, filter.as_deref())?;
            match kind {
                PatternKind::Count => Ok(Value::Int(rows.len() as i64)),
                PatternKind::Collect => {
                    let mut out = Vec::with_capacity(rows.len());
                    for row in &rows {
                        ctx.budget.work(1)?;
                        out.push(match map {
                            Some(map) => eval(ctx, map, Scope::Row(row))?,
                            None => Value::Null,
                        });
                    }
                    let value = Value::List(out);
                    ctx.budget.value(&value)?;
                    Ok(value)
                }
            }
        }
        Expr::MapProjection { base, items } => {
            let target = eval(ctx, base, scope)?;
            if target.is_null() {
                return Ok(Value::Null);
            }
            let mut map = std::collections::BTreeMap::new();
            for item in items {
                match item {
                    MapItem::Property(key) => {
                        map.insert(
                            key.clone(),
                            property(ctx, &target, &Key::Prop(key.clone()))?,
                        );
                    }
                    MapItem::All => match &target {
                        Value::Node(node) => {
                            for (key, value) in &node.props {
                                map.insert(key.clone(), Value::from_json(value));
                            }
                        }
                        Value::Edge(edge) => {
                            for (key, value) in &edge.props {
                                map.insert(key.clone(), Value::from_json(value));
                            }
                        }
                        Value::Map(entries) => {
                            map.extend(entries.iter().map(|(k, v)| (k.clone(), v.clone())));
                        }
                        other => {
                            return Err(error(format!(
                                "a map projection needs a node, an edge or a map, not a {}",
                                other.type_name()
                            )));
                        }
                    },
                    MapItem::Var(name) => {
                        map.insert(name.clone(), eval(ctx, &Expr::Var(name.clone()), scope)?);
                    }
                    MapItem::Entry(name, value) => {
                        map.insert(name.clone(), eval(ctx, value, scope)?);
                    }
                }
            }
            Ok(Value::Map(map))
        }
        Expr::Call { name, .. } if AGGREGATES.contains(&name.as_str()) => match scope {
            Scope::Folded(_, values, _) => folded(values, expr),
            Scope::Row(_) => Err(error(format!(
                "{name}() is an aggregate: use it in RETURN or WITH"
            ))),
        },
        Expr::Call { name, args, .. } => {
            let values = args
                .iter()
                .map(|arg| eval(ctx, arg, scope))
                .collect::<Result<Vec<_>, _>>()?;
            let value = function(ctx, name, values)?;
            if matches!(value, Value::Str(_) | Value::List(_) | Value::Map(_)) {
                ctx.budget.value(&value)?;
            }
            Ok(value)
        }
    }
}

pub(crate) fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn arithmetic(
    op: BinOp,
    left: &Value,
    right: &Value,
    ints: fn(i64, i64) -> Option<i64>,
    floats: fn(f64, f64) -> f64,
) -> Result<Value, QueryError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => ints(*a, *b).map(Value::Int).ok_or_else(|| {
            error(if matches!(op, BinOp::Div | BinOp::Mod) && *b == 0 {
                "division by zero"
            } else {
                "integer overflow"
            })
        }),
        _ => match (number(left), number(right)) {
            (Some(a), Some(b)) => Ok(Value::Float(floats(a, b))),
            _ => Err(error(format!(
                "cannot apply {op:?} to a {} and a {}",
                left.type_name(),
                right.type_name()
            ))),
        },
    }
}

fn binary(op: BinOp, left: &Value, right: &Value) -> Result<Value, QueryError> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }
    if let Some(value) = temporal_arithmetic(op, left, right)? {
        return Ok(value);
    }
    let ordering = || compare(left, right);
    Ok(match op {
        BinOp::Eq => from_truth(equals(left, right)),
        BinOp::Neq => from_truth(equals(left, right).map(|b| !b)),
        BinOp::Lt => from_truth(ordering().map(|o| o.is_lt())),
        BinOp::Le => from_truth(ordering().map(|o| o.is_le())),
        BinOp::Gt => from_truth(ordering().map(|o| o.is_gt())),
        BinOp::Ge => from_truth(ordering().map(|o| o.is_ge())),
        BinOp::Add => match (left, right) {
            (Value::List(a), Value::List(b)) => Value::List(a.iter().chain(b).cloned().collect()),
            (Value::List(a), other) => {
                Value::List(a.iter().cloned().chain([other.clone()]).collect())
            }
            (Value::Str(_), _) | (_, Value::Str(_)) => {
                Value::Str(format!("{}{}", display(left), display(right)))
            }
            _ => arithmetic(op, left, right, i64::checked_add, |a, b| a + b)?,
        },
        BinOp::Sub => arithmetic(op, left, right, i64::checked_sub, |a, b| a - b)?,
        BinOp::Mul => arithmetic(op, left, right, i64::checked_mul, |a, b| a * b)?,
        BinOp::Div => arithmetic(op, left, right, i64::checked_div, |a, b| a / b)?,
        BinOp::Mod => arithmetic(op, left, right, i64::checked_rem, |a, b| a % b)?,
        // `^` always gives a float.
        BinOp::Pow => match (number(left), number(right)) {
            (Some(a), Some(b)) => Value::Float(a.powf(b)),
            _ => {
                return Err(error(format!(
                    "cannot raise a {} to a {}",
                    left.type_name(),
                    right.type_name()
                )));
            }
        },
        BinOp::Contains | BinOp::StartsWith | BinOp::EndsWith => match (left, right) {
            (Value::Str(a), Value::Str(b)) => Value::Bool(match op {
                BinOp::Contains => a.contains(b.as_str()),
                BinOp::StartsWith => a.starts_with(b.as_str()),
                _ => a.ends_with(b.as_str()),
            }),
            _ => {
                return Err(error(format!(
                    "{op:?} needs two strings, got a {} and a {}",
                    left.type_name(),
                    right.type_name()
                )));
            }
        },
        BinOp::In => match right {
            Value::List(items) => {
                let mut unknown = false;
                for item in items {
                    match equals(left, item) {
                        Some(true) => return Ok(Value::Bool(true)),
                        None => unknown = true,
                        Some(false) => {}
                    }
                }
                if unknown {
                    Value::Null
                } else {
                    Value::Bool(false)
                }
            }
            other => {
                return Err(error(format!(
                    "IN needs a list, got a {}",
                    other.type_name()
                )));
            }
        },
        BinOp::Regex => match (left, right) {
            (Value::Str(text), Value::Str(pattern)) => Value::Bool(regex_matches(pattern, text)?),
            _ => {
                return Err(error(format!(
                    "=~ needs a string and a regular expression, got a {} and a {}",
                    left.type_name(),
                    right.type_name()
                )));
            }
        },
        BinOp::And | BinOp::Or | BinOp::Xor => unreachable!("handled with short-circuit"),
    })
}

/// Whether the whole of `text` matches `pattern` (GQ's `=~`).
/// Compiled expressions are kept per thread; their size is bounded, and
/// matching takes linear time, so a pattern cannot stall a query.
fn regex_matches(pattern: &str, text: &str) -> Result<bool, QueryError> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static CACHE: RefCell<HashMap<String, regex::Regex>> = RefCell::new(HashMap::new());
    }
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !cache.contains_key(pattern) {
            let compiled = regex::RegexBuilder::new(&format!("^(?:{pattern})$"))
                .size_limit(1 << 20)
                .dfa_size_limit(1 << 20)
                .build()
                .map_err(|e| error(format!("invalid regular expression {pattern:?}: {e}")))?;
            if cache.len() >= 128 {
                cache.clear();
            }
            cache.insert(pattern.to_owned(), compiled);
        }
        Ok(cache[pattern].is_match(text))
    })
}

/// A time, or a string that reads as one.
fn time_of(value: &Value) -> Option<Timestamp> {
    match value {
        Value::Time(t) => Some(*t),
        Value::Str(s) => Timestamp::parse(s).ok(),
        _ => None,
    }
}

/// `time ± duration`, `duration ± duration`, `time - time`, and durations
/// multiplied or divided by numbers. `None` when no side is a duration or
/// both are not times.
fn temporal_arithmetic(
    op: BinOp,
    left: &Value,
    right: &Value,
) -> Result<Option<Value>, QueryError> {
    Ok(Some(match (op, left, right) {
        (BinOp::Add, Value::Duration(a), Value::Duration(b)) => Value::Duration(a.plus(*b)?),
        (BinOp::Sub, Value::Duration(a), Value::Duration(b)) => {
            Value::Duration(a.plus(b.negated())?)
        }
        (BinOp::Add, other, Value::Duration(d)) | (BinOp::Add, Value::Duration(d), other) => {
            match time_of(other) {
                Some(time) => Value::Time(temporal::add(time, *d)?),
                None => return Ok(None),
            }
        }
        (BinOp::Sub, other, Value::Duration(d)) => match time_of(other) {
            Some(time) => Value::Time(temporal::add(time, d.negated())?),
            None => return Ok(None),
        },
        (BinOp::Sub, Value::Time(a), Value::Time(b)) => {
            Value::Duration(Duration::of_micros(a.as_micros() - b.as_micros()))
        }
        (BinOp::Mul, Value::Duration(d), factor) | (BinOp::Mul, factor, Value::Duration(d)) => {
            match number(factor) {
                Some(factor) => Value::Duration(d.scale(factor)?),
                None => return Ok(None),
            }
        }
        (BinOp::Div, Value::Duration(d), divisor) => match number(divisor) {
            Some(0.0) => return Err(error("division by zero")),
            Some(divisor) => Value::Duration(d.scale(1.0 / divisor)?),
            None => return Ok(None),
        },
        _ => return Ok(None),
    }))
}

fn numbers(name: &str, values: &[Value]) -> Result<Vec<f64>, QueryError> {
    values
        .iter()
        .map(|value| {
            number(value).ok_or_else(|| {
                error(format!(
                    "{name}() needs numbers, got a {}",
                    value.type_name()
                ))
            })
        })
        .collect()
}

/// Variance with `ddof` degrees of freedom removed (1: sample, 0: population).
fn variance(values: &[f64], ddof: usize) -> Option<f64> {
    if values.len() <= ddof {
        return None;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let squares: f64 = values.iter().map(|x| (x - mean).powi(2)).sum();
    Some(squares / (values.len() - ddof) as f64)
}

/// The percentile argument of `percentileCont` and `percentileDisc`.
pub(crate) fn percentile_of(name: &str, value: &Value) -> Result<f64, QueryError> {
    match number(value) {
        Some(p) if (0.0..=1.0).contains(&p) => Ok(p),
        _ => Err(error(format!(
            "{name}() needs a percentile between 0 and 1, got {}",
            value.to_json()
        ))),
    }
}

/// An aggregate over its values (nulls and, with DISTINCT, repeats already
/// left out), in the order they came.
/// The numbers among `values` added without rounding errors, so that the
/// sum does not depend on their order.
fn exact_sum(values: &[Value]) -> f64 {
    let mut sum = crate::aggregate::ExactSum::default();
    values.iter().filter_map(number).for_each(|f| sum.add(f));
    sum.value()
}

pub(crate) fn reduce(name: &str, values: Vec<Value>, percentile: f64) -> Result<Value, QueryError> {
    Ok(match name {
        "count" => Value::Int(values.len() as i64),
        "collect" => Value::List(values),
        "min" => values.into_iter().min_by(sort_order).unwrap_or(Value::Null),
        "max" => values.into_iter().max_by(sort_order).unwrap_or(Value::Null),
        "sum" | "avg"
            if values.iter().all(|v| matches!(v, Value::Duration(_))) && !values.is_empty() =>
        {
            let mut total = Duration::ZERO;
            for value in &values {
                if let Value::Duration(d) = value {
                    total = total.plus(*d)?;
                }
            }
            Value::Duration(if name == "avg" {
                total.scale(1.0 / values.len() as f64)?
            } else {
                total
            })
        }
        "stdev" | "stdevp" | "variance" | "variancep" => {
            let numbers = numbers(name, &values)?;
            let ddof = usize::from(matches!(name, "stdev" | "variance"));
            match variance(&numbers, ddof) {
                None if values.is_empty() => Value::Null,
                // One value: no spread.
                None => Value::Float(0.0),
                Some(v) if name.starts_with("stdev") => Value::Float(v.sqrt()),
                Some(v) => Value::Float(v),
            }
        }
        "median" | "percentilecont" | "percentiledisc" => {
            let mut numbers = numbers(name, &values)?;
            if numbers.is_empty() {
                return Ok(Value::Null);
            }
            numbers.sort_by(f64::total_cmp);
            let p = if name == "median" { 0.5 } else { percentile };
            if name == "percentiledisc" {
                // The smallest value with at least p of the values at or below it.
                let rank = ((p * numbers.len() as f64).ceil() as usize).max(1) - 1;
                let value = &values[0];
                let chosen = numbers[rank.min(numbers.len() - 1)];
                return Ok(if matches!(value, Value::Int(_)) && chosen.fract() == 0.0 {
                    Value::Int(chosen as i64)
                } else {
                    Value::Float(chosen)
                });
            }
            // Linear interpolation between the closest ranks.
            let position = p * (numbers.len() - 1) as f64;
            let (low, high) = (position.floor() as usize, position.ceil() as usize);
            let fraction = position - low as f64;
            Value::Float(numbers[low] + (numbers[high] - numbers[low]) * fraction)
        }
        "mode" => {
            // The most frequent value; ties go to the one that sorts first.
            let mut counts: Vec<(Value, usize)> = Vec::new();
            for value in values {
                match counts
                    .iter_mut()
                    .find(|(seen, _)| seen.key() == value.key())
                {
                    Some((_, count)) => *count += 1,
                    None => counts.push((value, 1)),
                }
            }
            counts
                .into_iter()
                .max_by(|(a, x), (b, y)| x.cmp(y).then_with(|| sort_order(b, a)))
                .map_or(Value::Null, |(value, _)| value)
        }
        "sum" | "avg" => {
            if values.iter().any(|v| number(v).is_none()) {
                return Err(error(format!("{name}() needs numbers (or durations)")));
            }
            if name == "avg" {
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                Value::Float(exact_sum(&values) / values.len() as f64)
            } else if values.iter().all(|v| matches!(v, Value::Int(_))) {
                // In 128 bits: only a total that does not fit is an error,
                // whatever the order of the values.
                let total: i128 = values
                    .iter()
                    .map(|v| match v {
                        Value::Int(i) => *i as i128,
                        _ => 0,
                    })
                    .sum();
                Value::Int(i64::try_from(total).map_err(|_| error("integer overflow"))?)
            } else {
                Value::Float(exact_sum(&values))
            }
        }
        _ => unreachable!("not an aggregate"),
    })
}

fn arity(
    name: &str,
    args: &[Value],
    range: std::ops::RangeInclusive<usize>,
) -> Result<(), QueryError> {
    if range.contains(&args.len()) {
        Ok(())
    } else if range.start() == range.end() {
        Err(error(format!(
            "{name}() takes {} argument(s), got {}",
            range.start(),
            args.len()
        )))
    } else {
        Err(error(format!(
            "{name}() takes {} to {} arguments, got {}",
            range.start(),
            range.end(),
            args.len()
        )))
    }
}

fn string_arg<'a>(name: &str, value: &'a Value) -> Result<&'a str, QueryError> {
    match value {
        Value::Str(s) => Ok(s),
        other => Err(error(format!(
            "{name}() needs a string, got a {}",
            other.type_name()
        ))),
    }
}

fn int_arg(name: &str, value: &Value) -> Result<i64, QueryError> {
    match value {
        Value::Int(i) => Ok(*i),
        other => Err(error(format!(
            "{name}() needs an integer, got a {}",
            other.type_name()
        ))),
    }
}

/// `duration('P1DT2H')` or `duration({days: 1, hours: 2})`.
fn duration_from(value: &Value) -> Result<Duration, QueryError> {
    match value {
        Value::Duration(d) => Ok(*d),
        Value::Str(text) => Duration::parse(text),
        Value::Map(map) => {
            let mut total = Duration::ZERO;
            for (key, amount) in map {
                let amount = number(amount)
                    .ok_or_else(|| error(format!("duration(): {key} needs a number")))?;
                let part = match key.as_str() {
                    "years" => Duration {
                        months: 12,
                        ..Duration::ZERO
                    },
                    "quarters" => Duration {
                        months: 3,
                        ..Duration::ZERO
                    },
                    "months" => Duration {
                        months: 1,
                        ..Duration::ZERO
                    },
                    "weeks" => Duration {
                        days: 7,
                        ..Duration::ZERO
                    },
                    "days" => Duration {
                        days: 1,
                        ..Duration::ZERO
                    },
                    "hours" => Duration::of_micros(3_600_000_000),
                    "minutes" => Duration::of_micros(60_000_000),
                    "seconds" => Duration::of_micros(1_000_000),
                    "milliseconds" => Duration::of_micros(1_000),
                    "microseconds" => Duration::of_micros(1),
                    other => {
                        return Err(error(format!(
                            "duration(): unknown unit {other:?} (years, quarters, months, weeks, days, hours, minutes, seconds, milliseconds, microseconds)"
                        )));
                    }
                };
                total = total.plus(part.scale(amount)?)?;
            }
            Ok(total)
        }
        other => Err(error(format!(
            "duration() needs an ISO 8601 string or a map, not a {}",
            other.type_name()
        ))),
    }
}

fn node_of(ctx: &Context, id: NodeId) -> Result<Value, QueryError> {
    Ok(ctx
        .node(id)?
        .map_or(Value::Null, |node| Value::Node(Arc::new(node))))
}

fn function(ctx: &Context, name: &str, args: Vec<Value>) -> Result<Value, QueryError> {
    // Most functions return null for a null argument.
    let strict = !matches!(
        name,
        "coalesce"
            | "now"
            | "datetime"
            | "time"
            | "range"
            | "date"
            | "timestamp"
            | "pi"
            | "e"
            | "nullif"
            | "rand"
            | "randomuuid"
    ) && !name.ends_with("ornull");
    if strict && args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let first = args.first();
    Ok(match name {
        "id" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Node(node) => Value::Str(node.id.to_string()),
                Value::Edge(edge) => Value::Str(edge.id.to_string()),
                other => {
                    return Err(error(format!(
                        "id() needs a node or edge, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "labels" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Node(node) => node_meta(ctx, node, "labels")?,
                other => {
                    return Err(error(format!(
                        "labels() needs a node, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "type" | "label" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Edge(edge) => Value::Str(edge.label.clone()),
                other => {
                    return Err(error(format!(
                        "{name}() needs an edge, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "properties" | "props" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Node(node) => {
                    Value::from_json(&serde_json::Value::Object(node.props.clone()))
                }
                Value::Edge(edge) => {
                    Value::from_json(&serde_json::Value::Object(edge.props.clone()))
                }
                Value::Map(map) => Value::Map(map.clone()),
                other => {
                    return Err(error(format!(
                        "{name}() needs a node, edge or map, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "keys" => {
            arity(name, &args, 1..=1)?;
            let keys: Vec<String> = match &args[0] {
                Value::Node(node) => node.props.keys().cloned().collect(),
                Value::Edge(edge) => edge.props.keys().cloned().collect(),
                Value::Map(map) => map.keys().cloned().collect(),
                other => {
                    return Err(error(format!(
                        "keys() needs a node, edge or map, not a {}",
                        other.type_name()
                    )));
                }
            };
            Value::List(keys.into_iter().map(Value::Str).collect())
        }
        "text" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Node(node) => node_meta(ctx, node, "text")?,
                other => {
                    return Err(error(format!(
                        "text() needs a node, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "startnode" | "endnode" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Edge(edge) => node_of(
                    ctx,
                    if name == "startnode" {
                        edge.from
                    } else {
                        edge.to
                    },
                )?,
                other => {
                    return Err(error(format!(
                        "{name}() needs an edge, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "size" | "length" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Path(path) if name == "length" => Value::Int(path.edges.len() as i64),
                Value::Str(s) => Value::Int(s.chars().count() as i64),
                Value::List(items) => Value::Int(items.len() as i64),
                Value::Map(map) => Value::Int(map.len() as i64),
                other => {
                    return Err(error(format!(
                        "{name}() needs a string or list, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "lower" | "tolower" => {
            Value::Str(string_arg(name, first.unwrap_or(&Value::Null))?.to_lowercase())
        }
        "upper" | "toupper" => {
            Value::Str(string_arg(name, first.unwrap_or(&Value::Null))?.to_uppercase())
        }
        "trim" => Value::Str(
            string_arg(name, first.unwrap_or(&Value::Null))?
                .trim()
                .to_owned(),
        ),
        "substring" => {
            arity(name, &args, 2..=3)?;
            let text = string_arg(name, &args[0])?;
            let start = usize::try_from(int_arg(name, &args[1])?).unwrap_or(0);
            let chars = text.chars().skip(start);
            match args.get(2) {
                Some(length) => {
                    let length = usize::try_from(int_arg(name, length)?).unwrap_or(0);
                    Value::Str(chars.take(length).collect())
                }
                None => Value::Str(chars.collect()),
            }
        }
        "replace" => {
            arity(name, &args, 3..=3)?;
            let text = string_arg(name, &args[0])?;
            let (from, to) = (string_arg(name, &args[1])?, string_arg(name, &args[2])?);
            if to.len() > from.len() {
                let found = if from.is_empty() {
                    text.chars().count() + 1
                } else {
                    text.matches(from).count()
                };
                ctx.budget.memory(
                    text.len() + found.saturating_mul(to.len() - from.len()),
                    "built a value",
                )?;
            }
            Value::Str(text.replace(from, to))
        }
        "split" => {
            arity(name, &args, 2..=2)?;
            let text = string_arg(name, &args[0])?;
            let separator = string_arg(name, &args[1])?;
            Value::List(
                text.split(separator)
                    .map(|part| Value::Str(part.to_owned()))
                    .collect(),
            )
        }
        "coalesce" => args
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null),
        "tostring" => {
            arity(name, &args, 1..=1)?;
            Value::Str(display(&args[0]))
        }
        "tointeger" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Int(i) => Value::Int(*i),
                Value::Float(f) if f.is_finite() => Value::Int(f.trunc() as i64),
                Value::Str(s) => s
                    .trim()
                    .parse::<i64>()
                    .map(Value::Int)
                    .or_else(|_| {
                        s.trim()
                            .parse::<f64>()
                            .map(|f| Value::Int(f.trunc() as i64))
                    })
                    .unwrap_or(Value::Null),
                Value::Bool(b) => Value::Int(i64::from(*b)),
                _ => Value::Null,
            }
        }
        "tofloat" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::Int(i) => Value::Float(*i as f64),
                Value::Float(f) => Value::Float(*f),
                Value::Str(s) => s.trim().parse().map(Value::Float).unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        "round" if args.len() == 2 => {
            let x = number(&args[0]).ok_or_else(|| {
                error(format!(
                    "round() needs a number, not a {}",
                    args[0].type_name()
                ))
            })?;
            let digits = int_arg(name, &args[1])?.clamp(-15, 15);
            let scale = 10f64.powi(digits as i32);
            Value::Float((x * scale).round() / scale)
        }
        "abs" | "round" | "floor" | "ceil" | "sqrt" => {
            arity(name, &args, 1..=1)?;
            match (&args[0], name) {
                (Value::Int(i), "abs") => {
                    Value::Int(i.checked_abs().ok_or_else(|| error("integer overflow"))?)
                }
                (Value::Int(i), "round" | "floor" | "ceil") => Value::Int(*i),
                (value, _) => {
                    let x = number(value).ok_or_else(|| {
                        error(format!(
                            "{name}() needs a number, not a {}",
                            value.type_name()
                        ))
                    })?;
                    Value::Float(match name {
                        "abs" => x.abs(),
                        "round" => x.round(),
                        "floor" => x.floor(),
                        "ceil" => x.ceil(),
                        _ => x.sqrt(),
                    })
                }
            }
        }
        "datetime" | "time" => {
            arity(name, &args, 0..=1)?;
            match first {
                None => Value::Time(Timestamp::now()),
                Some(Value::Null) => Value::Null,
                Some(value) => Value::Time(to_time(value)?),
            }
        }
        "date" => {
            arity(name, &args, 0..=1)?;
            match first {
                None => Value::Str(Timestamp::now().date()),
                Some(Value::Null) => Value::Null,
                Some(value) => Value::Str(to_time(value)?.date()),
            }
        }
        "timestamp" => {
            arity(name, &args, 0..=0)?;
            Value::Int(Timestamp::now().as_micros().div_euclid(1_000))
        }
        "duration" => {
            arity(name, &args, 1..=1)?;
            Value::Duration(duration_from(&args[0])?)
        }
        "duration.between" | "duration.inmonths" | "duration.indays" | "duration.inseconds" => {
            arity(name, &args, 2..=2)?;
            let (a, b) = (to_time(&args[0])?, to_time(&args[1])?);
            Value::Duration(match name {
                "duration.between" => temporal::between(a, b)?,
                "duration.inmonths" => temporal::between_in("months", a, b)?,
                "duration.indays" => temporal::between_in("days", a, b)?,
                _ => temporal::between_in("seconds", a, b)?,
            })
        }
        "date.truncate" | "datetime.truncate" => {
            arity(name, &args, 2..=2)?;
            let unit = string_arg(name, &args[0])?;
            Value::Time(temporal::truncate(unit, to_time(&args[1])?)?)
        }
        "pi" => {
            arity(name, &args, 0..=0)?;
            Value::Float(std::f64::consts::PI)
        }
        "e" => {
            arity(name, &args, 0..=0)?;
            Value::Float(std::f64::consts::E)
        }
        "log" | "log10" | "exp" | "sign" | "sin" | "cos" | "tan" => {
            arity(name, &args, 1..=1)?;
            let x = number(&args[0]).ok_or_else(|| {
                error(format!(
                    "{name}() needs a number, not a {}",
                    args[0].type_name()
                ))
            })?;
            match name {
                "sign" => Value::Int(if x > 0.0 {
                    1
                } else if x < 0.0 {
                    -1
                } else {
                    0
                }),
                "log" if x <= 0.0 => Value::Null,
                "log10" if x <= 0.0 => Value::Null,
                "log" => Value::Float(x.ln()),
                "log10" => Value::Float(x.log10()),
                "exp" => Value::Float(x.exp()),
                "sin" => Value::Float(x.sin()),
                "cos" => Value::Float(x.cos()),
                _ => Value::Float(x.tan()),
            }
        }
        "now" => {
            arity(name, &args, 0..=0)?;
            Value::Time(Timestamp::now())
        }
        "head" | "last" => {
            arity(name, &args, 1..=1)?;
            match &args[0] {
                Value::List(items) => {
                    let item = if name == "head" {
                        items.first()
                    } else {
                        items.last()
                    };
                    item.cloned().unwrap_or(Value::Null)
                }
                other => {
                    return Err(error(format!(
                        "{name}() needs a list, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        "history" => {
            arity(name, &args, 2..=2)?;
            let Value::Node(node) = &args[0] else {
                return Err(error("history(node, property) needs a node"));
            };
            let key = string_arg(name, &args[1])?;
            // A property the caller may not read has no history for it.
            if ctx
                .per_record()
                .is_some_and(|view| view.property_hidden(ctx.ns, &node.labels, key))
                || !ctx.visible(node.id)?
            {
                return Ok(Value::List(Vec::new()));
            }
            let entries = match ctx.write {
                Some(tx) => tx.property_history(ctx.ns, node.id, key)?,
                None => ctx.read.property_history(ctx.ns, node.id, key)?,
            };
            Value::List(
                entries
                    .into_iter()
                    .map(|entry| {
                        let mut map = std::collections::BTreeMap::new();
                        map.insert("value".to_owned(), Value::from_json(&entry.value));
                        map.insert("from".to_owned(), Value::Time(entry.from));
                        map.insert("to".to_owned(), Value::Time(entry.to));
                        map.insert("by".to_owned(), entry.by.map_or(Value::Null, Value::Str));
                        Value::Map(map)
                    })
                    .collect(),
            )
        }
        "range" => {
            arity(name, &args, 2..=3)?;
            let (start, end) = (int_arg(name, &args[0])?, int_arg(name, &args[1])?);
            let step = args
                .get(2)
                .map(|step| int_arg(name, step))
                .transpose()?
                .unwrap_or(1);
            if step == 0 {
                return Err(error("range() step must not be zero"));
            }
            let mut out = Vec::new();
            let mut current = start;
            while (step > 0 && current <= end) || (step < 0 && current >= end) {
                if out.len() >= 1_000_000 {
                    return Err(error("range() would return more than a million items"));
                }
                out.push(Value::Int(current));
                current += step;
            }
            Value::List(out)
        }
        other if other.starts_with("text.") => match crate::textfns::function(other, &args) {
            Some(result) => result?,
            None => return Err(error(format!("unknown function {other}()"))),
        },
        // `toIntegerOrNull(x)` and the like: null instead of an error.
        other if other.ends_with("ornull") && other.starts_with("to") => {
            let base = &other[..other.len() - "ornull".len()];
            if args.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            match function(ctx, base, args) {
                Ok(value) => value,
                Err(_) => Value::Null,
            }
        }
        // `toIntegerList(list)` and the like: each item, null when it fails.
        "tointegerlist" | "tofloatlist" | "tostringlist" => {
            arity(name, &args, 1..=1)?;
            let base = &name[..name.len() - "list".len()];
            match &args[0] {
                Value::List(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(if item.is_null() {
                            Value::Null
                        } else {
                            function(ctx, base, vec![item.clone()]).unwrap_or(Value::Null)
                        });
                    }
                    Value::List(out)
                }
                other => {
                    return Err(error(format!(
                        "{name}() needs a list, not a {}",
                        other.type_name()
                    )));
                }
            }
        }
        other => match crate::morefns::function(other, &args) {
            Some(result) => result?,
            None => match crate::algorithms::function(ctx, other, &args) {
                Some(result) => result?,
                None => return Err(error(format!("unknown function {other}()"))),
            },
        },
    })
}
