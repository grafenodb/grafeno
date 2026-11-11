//! `TRACE` (16.9): a record of how a statement walked the graph.
//!
//! While a traced statement runs, the executor reports what it does to a
//! [`Trace`]: the nodes each pattern starts from (`anchor`), every edge it
//! follows (`expand`), the nodes and edges a pattern or a condition keeps
//! (`keep`) or drops (`drop`, with why), and the rows it gives back
//! (`row`), in the order they happen, each with the step (the clause) it
//! belongs to. Untraced statements carry no trace and pay one check per
//! event site.
//!
//! The record is bounded by events and bytes; past either it stops
//! recording, says so (`cut`), and the statement finishes as usual.
//!
//! Event format (part of the API, version 1): every event has `seq` (from
//! 0), `step` (the clause, from 1) and `kind`, and:
//!
//! - `anchor`: `node`, `via` (`bound`, `id`, `index <name>`, `label
//!   <label>` or `scan`);
//! - `expand`: `from`, `to`, and `edge` (one edge) or `edges` (the edges of
//!   a variable-length step, in order);
//! - `keep`: `node`, and `edge` when an edge led to it;
//! - `drop`: `node` or `edge` (or `nodes`, the row's, for a condition),
//!   `reason` (`pattern`, `edge pattern`, `edge used` or `condition`) and,
//!   for a condition, `condition` (as written, roughly);
//! - `row`: `index` (from 0), `nodes` and `edges` (the ids the row holds).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use grafeno::{EdgeId, NodeId};
use serde_json::{Value as Json, json};

use crate::ast::{BinOp, Expr, Key, UnOp};
use crate::value::Value;

/// Events a trace keeps, by default.
pub const TRACE_EVENTS: usize = 10_000;
/// Bytes of events a trace keeps, by default.
pub const TRACE_BYTES: usize = 4 << 20;
/// The version of the event format.
pub const TRACE_FORMAT: u32 = 1;

/// A statement's walk, being recorded.
pub(crate) struct Trace {
    events: Mutex<Vec<Json>>,
    max_events: usize,
    max_bytes: usize,
    bytes: AtomicUsize,
    cut: AtomicBool,
    step: AtomicUsize,
    /// The clause of each step, by step.
    steps: Mutex<Vec<(usize, String)>>,
}

impl Trace {
    pub(crate) fn new(max_events: usize, max_bytes: usize) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            max_events,
            max_bytes,
            bytes: AtomicUsize::new(0),
            cut: AtomicBool::new(false),
            step: AtomicUsize::new(0),
            steps: Mutex::new(Vec::new()),
        }
    }

    /// Events from now on belong to step `step` (clause `clause`).
    pub(crate) fn enter(&self, step: usize, clause: &str) {
        self.step.store(step, Ordering::Relaxed);
        let mut steps = self.steps.lock().unwrap_or_else(|e| e.into_inner());
        if !steps.iter().any(|(at, _)| *at == step) {
            steps.push((step, clause.to_owned()));
        }
    }

    fn push(&self, kind: &str, mut fields: Json) {
        if self.cut.load(Ordering::Relaxed) {
            return;
        }
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        if let Json::Object(map) = &mut fields {
            map.insert("seq".into(), json!(events.len()));
            map.insert("step".into(), json!(self.step.load(Ordering::Relaxed)));
            map.insert("kind".into(), json!(kind));
        }
        let size = fields.to_string().len();
        if events.len() >= self.max_events
            || self.bytes.load(Ordering::Relaxed) + size > self.max_bytes
        {
            self.cut.store(true, Ordering::Relaxed);
            return;
        }
        self.bytes.fetch_add(size, Ordering::Relaxed);
        events.push(fields);
    }

    pub(crate) fn anchor(&self, node: NodeId, via: &str) {
        self.push("anchor", json!({ "node": node, "via": via }));
    }

    pub(crate) fn expand(&self, from: NodeId, to: NodeId, edge: EdgeId) {
        self.push("expand", json!({ "from": from, "to": to, "edge": edge }));
    }

    pub(crate) fn expand_path(&self, from: NodeId, to: NodeId, edges: &[EdgeId]) {
        self.push("expand", json!({ "from": from, "to": to, "edges": edges }));
    }

    pub(crate) fn keep(&self, node: NodeId, edge: Option<EdgeId>) {
        match edge {
            Some(edge) => self.push("keep", json!({ "node": node, "edge": edge })),
            None => self.push("keep", json!({ "node": node })),
        }
    }

    pub(crate) fn drop_node(&self, node: NodeId, reason: &str) {
        self.push("drop", json!({ "node": node, "reason": reason }));
    }

    pub(crate) fn drop_edge(&self, edge: EdgeId, reason: &str) {
        self.push("drop", json!({ "edge": edge, "reason": reason }));
    }

    /// A row a condition dropped: the nodes it held.
    pub(crate) fn drop_row(&self, values: &[&Value], condition: &Expr) {
        let (nodes, _) = ids(values);
        self.push(
            "drop",
            json!({ "nodes": nodes, "reason": "condition", "condition": text(condition) }),
        );
    }

    pub(crate) fn row(&self, index: usize, values: &[&Value]) {
        let (nodes, edges) = ids(values);
        self.push(
            "row",
            json!({ "index": index, "nodes": nodes, "edges": edges }),
        );
    }

    /// The record, for the result.
    pub(crate) fn finish(self) -> Json {
        let events = self.events.into_inner().unwrap_or_else(|e| e.into_inner());
        let steps: Vec<Json> = self
            .steps
            .into_inner()
            .unwrap_or_else(|e| e.into_inner())
            .into_iter()
            .map(|(step, clause)| json!({ "step": step, "clause": clause }))
            .collect();
        json!({
            "format": TRACE_FORMAT,
            "steps": steps,
            "events": events,
            "cut": self.cut.into_inner(),
        })
    }
}

/// The node and edge ids in values (paths and lists included), each once,
/// in order.
fn ids(values: &[&Value]) -> (Vec<NodeId>, Vec<EdgeId>) {
    let (mut nodes, mut edges) = (Vec::new(), Vec::new());
    fn walk(value: &Value, nodes: &mut Vec<NodeId>, edges: &mut Vec<EdgeId>) {
        match value {
            Value::Node(node) if !nodes.contains(&node.id) => nodes.push(node.id),
            Value::Edge(edge) if !edges.contains(&edge.id) => edges.push(edge.id),
            Value::Path(path) => {
                for node in &path.nodes {
                    walk(&Value::Node(node.clone()), nodes, edges);
                }
                for edge in &path.edges {
                    walk(&Value::Edge(edge.clone()), nodes, edges);
                }
            }
            Value::List(items) => items.iter().for_each(|item| walk(item, nodes, edges)),
            Value::Map(map) => map.values().for_each(|item| walk(item, nodes, edges)),
            _ => {}
        }
    }
    for value in values {
        walk(value, &mut nodes, &mut edges);
    }
    (nodes, edges)
}

/// An expression as text, close to how it was written (what the trace
/// names a condition by).
pub(crate) fn text(expr: &Expr) -> String {
    let op = |op: &BinOp| match op {
        BinOp::Or => "OR",
        BinOp::Xor => "XOR",
        BinOp::And => "AND",
        BinOp::Eq => "=",
        BinOp::Neq => "<>",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Pow => "^",
        BinOp::Contains => "CONTAINS",
        BinOp::StartsWith => "STARTS WITH",
        BinOp::EndsWith => "ENDS WITH",
        BinOp::In => "IN",
        BinOp::Regex => "=~",
    };
    match expr {
        Expr::Literal(Value::Str(s)) => format!("'{}'", s.replace('\'', "\\'")),
        Expr::Literal(value) => value.to_json().to_string(),
        Expr::Param(name) => format!("${name}"),
        Expr::Var(name) => name.clone(),
        Expr::Property(target, Key::Prop(key)) => format!("{}.{key}", text(target)),
        Expr::Property(target, Key::Meta(key)) => format!("{}.@{key}", text(target)),
        Expr::List(items) => format!(
            "[{}]",
            items.iter().map(text).collect::<Vec<_>>().join(", ")
        ),
        Expr::Unary(UnOp::Not, inner) => format!("NOT {}", text(inner)),
        Expr::Unary(UnOp::Neg, inner) => format!("-{}", text(inner)),
        Expr::Binary(o, a, b) => format!("{} {} {}", text(a), op(o), text(b)),
        Expr::IsNull { expr, negated } => format!(
            "{} IS {}NULL",
            text(expr),
            if *negated { "NOT " } else { "" }
        ),
        _ => "…".into(),
    }
}
