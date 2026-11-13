//! Query execution: a pipeline of clauses over rows of variables.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use grafeno::{
    Direction, EdgeFilter, EdgeId, Grafeno, IndexDef, NewEdge, NewNode, Node, NodeId, Props,
    ReadTx, Timestamp, WriteTx,
};
use serde::Serialize;

use crate::QueryError;
use crate::aggregate::{Groups, Spec};
use crate::ast::{
    Clause, EventsOp, Expr, Key, Mode, NamespaceOp, NodePattern, Pattern, ProcedureCall,
    Projection, Query, RelPattern, RemoveItem, Schema, SetItem, Statement,
};
use crate::eval::{Context, Params, Row, Scope, eval, property, to_time, truth};
use crate::limits::{Budget, Limits};
use crate::plan::{
    PlanInfo, Source, ValueOf, conjuncts, constant, free_vars, order_patterns, plan_anchor,
    seek_ranges,
};
use crate::procedures;
use crate::traverse::{ANCHORS, Caches, Start};
use crate::value::{Value, equals, sort_order};

#[path = "split.rs"]
pub(crate) mod split;

/// Variable-length paths (`*`) stop at this many hops.
pub const MAX_PATH_LENGTH: u32 = 32;

/// What a writing query changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WriteStats {
    pub nodes_created: u64,
    pub nodes_deleted: u64,
    pub edges_created: u64,
    pub edges_deleted: u64,
    pub edges_retracted: u64,
    /// Facts ended (or retracted) because a new one contradicted them under
    /// an edge constraint, and contradictions only flagged.
    pub facts_invalidated: u64,
    pub conflicts_flagged: u64,
    pub properties_set: u64,
    pub labels_added: u64,
    pub labels_removed: u64,
    /// `CREATE NAMESPACE` and `DROP NAMESPACE` (16.3).
    pub namespaces_created: u64,
    pub namespaces_dropped: u64,
}

impl WriteStats {
    pub fn is_empty(&self) -> bool {
        *self == WriteStats::default()
    }
}

/// One step of a query's plan, with what it did when the query ran under
/// `PROFILE`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PlanStep {
    pub step: usize,
    pub operation: String,
    pub details: String,
    /// Rows after the step (`PROFILE` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// Milliseconds the step took (`PROFILE` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_ms: Option<f64>,
    /// Records the step read: nodes, edges, adjacency and index entries
    /// (`PROFILE` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reads: Option<u64>,
}

/// The result of a query: named columns, rows of values, and what changed.
#[derive(Clone, Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub stats: WriteStats,
    /// The steps and what each did, under `PROFILE`.
    pub profile: Option<Vec<PlanStep>>,
    /// The walk, under `TRACE` (16.9): `{format, steps, events, cut}`.
    pub trace: Option<serde_json::Value>,
}

impl QueryResult {
    /// `{"columns": […], "rows": [[…]], "stats": {…}}`, plus `"profile"`
    /// under `PROFILE`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut json = serde_json::json!({
            "columns": self.columns,
            "rows": self
                .rows
                .iter()
                .map(|row| row.iter().map(Value::to_json).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            "stats": self.stats,
        });
        if let Some(trace) = &self.trace {
            json["trace"] = trace.clone();
        }
        if let Some(profile) = &self.profile {
            json["profile"] = serde_json::to_value(profile).unwrap_or_default();
        }
        json
    }
}

/// Runs one statement on namespace `ns`, without limits. A statement with
/// writes runs in one write transaction: all of its changes or none.
pub fn execute(
    db: &Grafeno,
    ns: &str,
    query: &str,
    params: &Params,
) -> Result<QueryResult, QueryError> {
    execute_with(db, ns, query, params, &Limits::none())
}

/// [`execute`] within `limits`: a query over them stops with an error, and
/// a writing one leaves nothing behind.
pub fn execute_with(
    db: &Grafeno,
    ns: &str,
    query: &str,
    params: &Params,
    limits: &Limits,
) -> Result<QueryResult, QueryError> {
    let statement = crate::cache::parsed(query)?;
    validate(&statement)?;
    if statement.mode == Mode::Explain {
        return explain(
            &statement,
            &plan_info(&db.begin_read()?, ns)?,
            params,
            hides(limits, ns),
        );
    }
    if statement.writes() {
        // Holding the write lock first makes the snapshot the latest state.
        let mut tx = db.begin_write()?;
        let read = db.begin_read()?;
        let outcome = match &statement.schema {
            Some(schema) => run_schema(&read, Some(&mut tx), ns, schema, params, limits),
            None => Runner::new(&read, Some(&mut tx), ns, params, statement.mode, limits)?
                .run(&statement),
        };
        match outcome {
            Ok(result) => {
                tx.commit()?;
                Ok(result)
            }
            Err(error) => {
                tx.abort()?;
                Err(error)
            }
        }
    } else {
        let read = db.begin_read()?;
        execute_read_with(&read, ns, query, params, limits)
    }
}

/// Runs a statement inside a write transaction someone else commits (a
/// group commit): its writes join that transaction. `read` is a recent
/// snapshot, used for planning and procedures; the statement's own matching
/// reads `tx`. On an error the caller must discard the transaction.
pub fn execute_in(
    tx: &mut WriteTx,
    read: &ReadTx,
    ns: &str,
    query: &str,
    params: &Params,
    limits: &Limits,
) -> Result<QueryResult, QueryError> {
    let statement = crate::cache::parsed(query)?;
    validate(&statement)?;
    if statement.mode == Mode::Explain {
        return explain(&statement, &plan_info(read, ns)?, params, hides(limits, ns));
    }
    match &statement.schema {
        Some(schema) => run_schema(read, Some(tx), ns, schema, params, limits),
        None => Runner::new(read, Some(tx), ns, params, statement.mode, limits)?.run(&statement),
    }
}

/// Runs a read-only statement on an existing snapshot, without limits.
pub fn execute_read(
    read: &ReadTx,
    ns: &str,
    query: &str,
    params: &Params,
) -> Result<QueryResult, QueryError> {
    execute_read_with(read, ns, query, params, &Limits::none())
}

/// [`execute_read`] within `limits`.
pub fn execute_read_with(
    read: &ReadTx,
    ns: &str,
    query: &str,
    params: &Params,
    limits: &Limits,
) -> Result<QueryResult, QueryError> {
    let statement = crate::cache::parsed(query)?;
    validate(&statement)?;
    if statement.mode == Mode::Explain {
        return explain(&statement, &plan_info(read, ns)?, params, hides(limits, ns));
    }
    if statement.writes() {
        return Err(QueryError::runtime(
            "this query writes; it needs a writable database",
        ));
    }
    if let Some(schema) = &statement.schema {
        return run_schema(read, None, ns, schema, params, limits);
    }
    Runner::new(read, None, ns, params, statement.mode, limits)?.run(&statement)
}

/// What the planner knows about `ns`. Statements that write (`building`
/// false) use the statistics in memory, however old, so that they do not
/// build them while holding the write lock.
fn plan_info(read: &ReadTx, ns: &str) -> Result<PlanInfo, QueryError> {
    plan_info_with(read, ns, true)
}

fn plan_info_with(read: &ReadTx, ns: &str, building: bool) -> Result<PlanInfo, QueryError> {
    let stats = read.stats(ns)?;
    Ok(PlanInfo {
        indexes: read.index_defs(ns)?,
        label_counts: stats.node_labels.into_iter().collect(),
        edge_labels: stats.edge_labels.into_iter().collect(),
        nodes: stats.nodes,
        edges: stats.edges,
        stats: if building {
            read.planner_stats(ns)?
        } else {
            read.cached_planner_stats(ns)
        },
    })
}

const CONSTRAINT_COLUMNS: [&str; 6] = ["name", "kind", "label", "property", "rule", "on"];

fn on(edges: bool) -> Value {
    Value::Str(if edges { "edges" } else { "nodes" }.into())
}

fn constraint_row(constraint: &grafeno::Constraint) -> Vec<Value> {
    let mut row = constraint_cells(constraint);
    row.push(on(constraint.on_edges()));
    row
}

/// A unique or key index, as a constraint row.
fn key_row(def: &IndexDef) -> Vec<Value> {
    vec![
        Value::Str(def.name.clone()),
        Value::Str(if def.required { "key" } else { "unique" }.into()),
        Value::Str(def.labels().join("|")),
        Value::Str(def.keys().join(", ")),
        if def.required {
            Value::Str("present and unique together".into())
        } else {
            Value::Null
        },
        on(def.edges),
    ]
}

fn constraint_cells(constraint: &grafeno::Constraint) -> Vec<Value> {
    match constraint {
        grafeno::Constraint::Edge {
            name,
            label,
            scope,
            on_conflict,
        } => vec![
            Value::Str(name.clone()),
            Value::Str("edge".into()),
            Value::Str(label.clone()),
            Value::Null,
            Value::Str(format!(
                "one per {}, on conflict {}",
                format!("{scope:?}").to_lowercase(),
                format!("{on_conflict:?}").to_lowercase()
            )),
        ],
        grafeno::Constraint::Functional { name, label, key } => vec![
            Value::Str(name.clone()),
            Value::Str("functional".into()),
            Value::Str(label.clone()),
            Value::Str(key.clone()),
            Value::Str("one value at a time, earlier ones kept".into()),
        ],
        grafeno::Constraint::Exists {
            name, label, key, ..
        } => vec![
            Value::Str(name.clone()),
            Value::Str("not_null".into()),
            Value::Str(label.clone()),
            Value::Str(key.clone()),
            Value::Str("present, not null".into()),
        ],
        grafeno::Constraint::Typed {
            name,
            label,
            key,
            of,
            ..
        } => vec![
            Value::Str(name.clone()),
            Value::Str("type".into()),
            Value::Str(label.clone()),
            Value::Str(key.clone()),
            Value::Str(format!(":: {}", of.keyword())),
        ],
    }
}

/// The first six as before 16.5 (several labels joined by `|`, several
/// properties by `, `), then what an index is on, whether it is a key, its
/// kind (`value`, `fuzzy`, `fulltext` or `vector`) and its state
/// (`online`, or `building` in the background).
const INDEX_COLUMNS: [&str; 10] = [
    "name", "label", "property", "unique", "entries", "fuzzy", "on", "key", "kind", "state",
];

fn index_row(def: &IndexDef, entries: u64) -> Vec<Value> {
    vec![
        Value::Str(def.name.clone()),
        Value::Str(def.labels().join("|")),
        Value::Str(def.keys().join(", ")),
        Value::Bool(def.unique),
        Value::Int(entries as i64),
        Value::Bool(def.fuzzy),
        on(def.edges),
        Value::Bool(def.required),
        Value::Str(
            if def.fuzzy {
                "fuzzy"
            } else if def.fulltext.is_some() {
                "fulltext"
            } else if def.vector.is_some() {
                "vector"
            } else {
                "value"
            }
            .into(),
        ),
        Value::Str(
            if def
                .vector
                .as_ref()
                .is_some_and(|spec| spec.building.is_some())
            {
                "building"
            } else {
                "online"
            }
            .into(),
        ),
    ]
}

fn constraint_table(rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult {
        columns: CONSTRAINT_COLUMNS.map(String::from).to_vec(),
        rows,
        stats: WriteStats::default(),
        profile: None,
        trace: None,
    }
}

/// The columns of `SHOW EVENT SUBSCRIPTIONS` (16.10).
const EVENT_COLUMNS: [&str; 9] = [
    "name",
    "on",
    "labels",
    "keys",
    "where",
    "vectors",
    "delivery",
    "created_at",
    "created_by",
];

fn subscription_row(subscription: grafeno::Subscription) -> Vec<Value> {
    let names = |items: Vec<String>| Value::List(items.into_iter().map(Value::Str).collect());
    vec![
        Value::Str(subscription.name),
        names(
            subscription
                .on
                .iter()
                .map(|target| {
                    serde_json::to_value(target)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                })
                .collect(),
        ),
        names(subscription.labels),
        names(subscription.keys),
        Value::from_json(&serde_json::Value::Object(subscription.equal)),
        Value::Bool(subscription.vector_values),
        // The secret stays unsaid.
        Value::Str(match subscription.delivery {
            grafeno::Delivery::Stream => "stream".into(),
            grafeno::Delivery::Webhook { url, .. } => url,
        }),
        Value::Time(subscription.created_at),
        subscription.created_by.map_or(Value::Null, Value::Str),
    ]
}

/// Event subscription statements (16.10).
fn run_events(
    read: &ReadTx,
    tx: Option<&mut WriteTx>,
    ns: &str,
    op: &EventsOp,
    params: &Params,
) -> Result<QueryResult, QueryError> {
    let table = |rows: Vec<Vec<Value>>| QueryResult {
        columns: EVENT_COLUMNS.map(String::from).to_vec(),
        rows,
        stats: WriteStats::default(),
        profile: None,
        trace: None,
    };
    let text = |expr: &Expr, what: &str| match crate::plan::constant(expr, Some(params)) {
        Some(Value::Str(text)) => Ok(text),
        _ => Err(QueryError::runtime(format!(
            "the {what} must be a string or a parameter holding one"
        ))),
    };
    match op {
        EventsOp::Show => Ok(table(
            read.subscriptions(ns)?
                .into_iter()
                .map(subscription_row)
                .collect(),
        )),
        EventsOp::Drop { name, if_exists } => {
            let tx = tx.ok_or_else(|| {
                QueryError::runtime("event subscriptions need a write transaction")
            })?;
            if !tx.drop_subscription(ns, name)? && !*if_exists {
                return Err(QueryError::runtime(format!(
                    "no event subscription {name:?} (IF EXISTS ignores that)"
                )));
            }
            Ok(table(Vec::new()))
        }
        EventsOp::Create {
            name,
            if_not_exists,
            on,
            labels,
            keys,
            equal,
            vector_values,
            webhook,
        } => {
            let tx = tx.ok_or_else(|| {
                QueryError::runtime("event subscriptions need a write transaction")
            })?;
            if let Some(existing) = tx
                .subscriptions(ns)?
                .into_iter()
                .find(|subscription| &subscription.name == name)
            {
                if *if_not_exists {
                    return Ok(table(vec![subscription_row(existing)]));
                }
                return Err(QueryError::runtime(format!(
                    "event subscription {name:?} already exists (drop it first, or IF NOT EXISTS leaves it as it is)"
                )));
            }
            let mut values = grafeno::Props::new();
            for (key, expr) in equal {
                let value = crate::plan::constant(expr, Some(params)).ok_or_else(|| {
                    QueryError::runtime(format!(
                        "WHERE {key} = …: the value must be a literal or a parameter"
                    ))
                })?;
                values.insert(key.clone(), value.to_json());
            }
            let delivery = match webhook {
                None => grafeno::Delivery::Stream,
                Some(webhook) => {
                    let (url, secret) = webhook.as_ref();
                    let url = text(url, "webhook's URL")?;
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return Err(QueryError::runtime(
                            "the webhook's URL must start with http:// or https://",
                        ));
                    }
                    grafeno::Delivery::Webhook {
                        url,
                        secret: secret
                            .as_ref()
                            .map(|secret| text(secret, "secret"))
                            .transpose()?,
                    }
                }
            };
            let subscription = grafeno::Subscription {
                labels: labels.clone(),
                keys: keys.clone(),
                equal: values,
                vector_values: *vector_values,
                delivery,
                ..grafeno::Subscription::new(name, on.clone())
            };
            tx.create_subscription(ns, subscription)?;
            let created = tx
                .subscriptions(ns)?
                .into_iter()
                .filter(|subscription| &subscription.name == name)
                .map(subscription_row)
                .collect();
            Ok(table(created))
        }
    }
}

/// `CREATE INDEX`, `DROP INDEX`, `SHOW INDEXES` and the constraint forms.
fn run_schema(
    read: &ReadTx,
    tx: Option<&mut WriteTx>,
    ns: &str,
    schema: &Schema,
    params: &Params,
    limits: &Limits,
) -> Result<QueryResult, QueryError> {
    let table = |rows: Vec<Vec<Value>>| QueryResult {
        columns: INDEX_COLUMNS.iter().map(|c| (*c).to_owned()).collect(),
        rows,
        stats: WriteStats::default(),
        profile: None,
        trace: None,
    };
    match schema {
        Schema::Access(access) => {
            crate::access::run(read, tx, access, params, limits.access.as_ref())
        }
        Schema::Namespace(op) => {
            let tx = tx.ok_or_else(|| {
                QueryError::runtime("namespace statements need a write transaction")
            })?;
            let mut stats = WriteStats::default();
            let row = |info: grafeno::NamespaceInfo| {
                vec![
                    Value::Str(info.name),
                    Value::Int(info.nodes as i64),
                    Value::Int(info.edges as i64),
                    Value::Time(info.created_at),
                    info.description.map_or(Value::Null, Value::Str),
                ]
            };
            let columns: Vec<String> = ["name", "nodes", "edges", "created_at", "description"]
                .iter()
                .map(|c| (*c).to_owned())
                .collect();
            let rows = match op {
                NamespaceOp::Create {
                    name,
                    if_not_exists,
                    comment,
                } => {
                    if tx.namespace(name)?.is_some() {
                        if !*if_not_exists {
                            return Err(QueryError::runtime(format!(
                                "namespace {name:?} already exists (IF NOT EXISTS leaves it as it is)"
                            )));
                        }
                    } else {
                        tx.create_namespace(name, comment.as_deref())?;
                        stats.namespaces_created += 1;
                    }
                    tx.namespace(name)?.map(row).into_iter().collect()
                }
                NamespaceOp::Alter { name, comment } => {
                    if tx.namespace(name)?.is_none() {
                        return Err(grafeno::Error::NamespaceNotFound(name.clone()).into());
                    }
                    tx.describe_namespace(name, comment.as_deref())?;
                    tx.namespace(name)?.map(row).into_iter().collect()
                }
                NamespaceOp::Drop { name, if_exists } => {
                    if tx.namespace(name)?.is_none() {
                        if *if_exists {
                            Vec::new()
                        } else {
                            return Err(grafeno::Error::NamespaceNotFound(name.clone()).into());
                        }
                    } else {
                        let dropped = tx.drop_namespace(name)?;
                        stats.namespaces_dropped += 1;
                        stats.nodes_deleted += dropped.nodes;
                        stats.edges_deleted += dropped.edges;
                        vec![vec![
                            Value::Str(name.clone()),
                            Value::Int(dropped.nodes as i64),
                            Value::Int(dropped.edges as i64),
                            Value::Null,
                            Value::Null,
                        ]]
                    }
                }
            };
            Ok(QueryResult {
                columns,
                rows,
                stats,
                profile: None,
                trace: None,
            })
        }
        Schema::Events(op) => run_events(read, tx, ns, op, params),
        Schema::ShowNamespaces => Ok(QueryResult {
            columns: ["name", "nodes", "edges", "created_at", "description"]
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
            rows: read
                .namespaces()?
                .into_iter()
                .filter(|info| {
                    limits
                        .visible
                        .as_ref()
                        .is_none_or(|filter| filter.allows(&info.name))
                })
                .map(|info| {
                    // Counts of a namespace seen record by record would tell
                    // how much is hidden (14.4).
                    let hidden = limits
                        .view
                        .as_ref()
                        .is_some_and(|v| v.per_record(&info.name));
                    let count = |n: u64| {
                        if hidden {
                            Value::Null
                        } else {
                            Value::Int(n as i64)
                        }
                    };
                    vec![
                        Value::Str(info.name),
                        count(info.nodes),
                        count(info.edges),
                        Value::Time(info.created_at),
                        info.description.map_or(Value::Null, Value::Str),
                    ]
                })
                .collect(),
            stats: WriteStats::default(),
            profile: None,
            trace: None,
        }),
        Schema::Show { constraints: true } => {
            let mut rows: Vec<Vec<Value>> = read
                .index_defs(ns)?
                .iter()
                .filter(|def| def.unique)
                .map(key_row)
                .collect();
            for constraint in read.constraints(ns)? {
                rows.push(constraint_row(&constraint));
            }
            Ok(QueryResult {
                columns: CONSTRAINT_COLUMNS.map(String::from).to_vec(),
                rows,
                stats: WriteStats::default(),
                profile: None,
                trace: None,
            })
        }
        Schema::Show { constraints: false } => Ok(table(
            read.indexes(ns)?
                .into_iter()
                .map(|info| index_row(&info.def, info.entries))
                .collect(),
        )),
        Schema::Constrain {
            constraint,
            if_not_exists,
        } => {
            let tx = tx.expect("schema writes run with a write transaction");
            let mut constraint = constraint.clone();
            if constraint.name().is_empty() {
                constraint.set_name(constraint.default_name());
            }
            if *if_not_exists
                && let Some(existing) = tx
                    .constraints(ns)?
                    .into_iter()
                    .find(|other| other.name() == constraint.name())
            {
                return Ok(QueryResult {
                    columns: CONSTRAINT_COLUMNS.map(String::from).to_vec(),
                    rows: vec![constraint_row(&existing)],
                    stats: WriteStats::default(),
                    profile: None,
                    trace: None,
                });
            }
            let flagged = tx.create_constraint(ns, constraint.clone())?;
            let mut row = constraint_row(&constraint);
            row.push(Value::Int(flagged as i64));
            let mut columns = CONSTRAINT_COLUMNS.map(String::from).to_vec();
            columns.push("contradictions".into());
            Ok(QueryResult {
                columns,
                rows: vec![row],
                stats: WriteStats::default(),
                profile: None,
                trace: None,
            })
        }
        Schema::Create { def, if_not_exists } => {
            let tx = tx.expect("schema writes run with a write transaction");
            let mut def = def.clone();
            if def.name.is_empty() {
                def.name = def.generated_name();
            }
            if *if_not_exists
                && let Some(existing) = tx.index_defs(ns)?.into_iter().find(|other| {
                    other.name == def.name
                        || (other.same_shape(&def) && other.required == def.required)
                })
            {
                return Ok(if def.unique {
                    constraint_table(vec![key_row(&existing)])
                } else {
                    table(vec![index_row(&existing, 0)])
                });
            }
            let entries = tx.create_index(ns, def.clone())?;
            Ok(if def.unique {
                let mut row = key_row(&def);
                row.push(Value::Int(entries as i64));
                let mut result = constraint_table(vec![row]);
                result.columns.push("entries".into());
                result
            } else {
                table(vec![index_row(&def, entries)])
            })
        }
        Schema::Drop {
            name,
            constraint,
            if_exists,
        } => {
            let tx = tx.expect("schema writes run with a write transaction");
            if *constraint
                && let Some(found) = tx.constraints(ns)?.into_iter().find(|c| c.name() == name)
            {
                tx.drop_constraint(ns, name)?;
                return Ok(QueryResult {
                    columns: CONSTRAINT_COLUMNS.map(String::from).to_vec(),
                    rows: vec![constraint_row(&found)],
                    stats: WriteStats::default(),
                    profile: None,
                    trace: None,
                });
            }
            let existing = tx.index_defs(ns)?.into_iter().find(|def| def.name == *name);
            match existing {
                None if *if_exists => Ok(table(Vec::new())),
                None => Err(QueryError::runtime(format!(
                    "there is no {} named {name:?} (SHOW INDEXES lists them)",
                    if *constraint { "constraint" } else { "index" }
                ))),
                Some(def) if *constraint && !def.unique => Err(QueryError::runtime(format!(
                    "{name:?} is an index, not a constraint: use DROP INDEX"
                ))),
                Some(def) => {
                    tx.drop_index(ns, name)?;
                    Ok(if *constraint {
                        constraint_table(vec![key_row(&def)])
                    } else {
                        table(vec![index_row(&def, 0)])
                    })
                }
            }
        }
    }
}

/// Whether the statement writes when run (`EXPLAIN` never does).
pub fn is_write(query: &str) -> Result<bool, QueryError> {
    let statement = crate::cache::parsed(query)?;
    Ok(statement.mode != Mode::Explain && statement.writes())
}

/// What a statement does, for deciding who may run it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementKind {
    /// Reads only (also `EXPLAIN` of anything, `SHOW …`).
    Read,
    /// Writes nodes and facts.
    Write,
    /// Creates or drops indexes and constraints.
    Schema,
    /// Manages or lists users, roles and grants: the statement checks what
    /// the caller may manage itself ([`Limits::access`]).
    Access,
    /// Creates, changes or drops a namespace: needs
    /// [`Requirements::namespace`] on the namespace it names, not on the
    /// one it runs in (16.3).
    Namespace,
    /// Creates, drops or lists event subscriptions: needs the `events`
    /// privilege on the namespace (16.10).
    Events,
}

pub fn statement_kind(query: &str) -> Result<StatementKind, QueryError> {
    let statement = crate::cache::parsed(query)?;
    if statement.mode != Mode::Explain && matches!(statement.schema, Some(Schema::Access(_))) {
        return Ok(StatementKind::Access);
    }
    if statement.mode != Mode::Explain && matches!(statement.schema, Some(Schema::Namespace(_))) {
        return Ok(StatementKind::Namespace);
    }
    if statement.mode != Mode::Explain && matches!(statement.schema, Some(Schema::Events(_))) {
        return Ok(StatementKind::Events);
    }
    Ok(if statement.mode == Mode::Explain || !statement.writes() {
        StatementKind::Read
    } else if statement.schema.is_some() {
        StatementKind::Schema
    } else {
        StatementKind::Write
    })
}

/// What a statement would need if it ran, `EXPLAIN` aside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirements {
    /// It is an `EXPLAIN`: it describes, and runs nothing.
    pub explain: bool,
    /// The kind of the statement under the `EXPLAIN`, if any.
    pub kind: StatementKind,
    /// For an access statement, the server privilege `context` lacks.
    pub lacking: Option<&'static str>,
    /// For a namespace statement: the namespace it names and the action it
    /// takes on it (`CreateNamespace`, `Schema` or `Drop`).
    pub namespace: Option<(String, grafeno::Act)>,
}

/// [`Requirements`] of a statement, for a caller described by `context`.
pub fn requirements(
    query: &str,
    context: Option<&crate::access::AccessContext>,
) -> Result<Requirements, QueryError> {
    let statement = crate::cache::parsed(query)?;
    let namespace = match &statement.schema {
        Some(Schema::Namespace(op)) => Some((op.name().to_owned(), op.act())),
        _ => None,
    };
    let (kind, lacking) = match &statement.schema {
        Some(Schema::Access(access)) => (StatementKind::Access, access.lacking(context)),
        Some(Schema::Namespace(_)) => (StatementKind::Namespace, None),
        Some(Schema::Events(_)) => (StatementKind::Events, None),
        Some(_) if statement.writes() => (StatementKind::Schema, None),
        _ if statement.writes() => (StatementKind::Write, None),
        _ => (StatementKind::Read, None),
    };
    Ok(Requirements {
        explain: statement.mode == Mode::Explain,
        kind,
        lacking,
        namespace,
    })
}

fn validate(statement: &Statement) -> Result<(), QueryError> {
    if statement.schema.is_some() {
        if matches!(statement.mode, Mode::Profile | Mode::Trace) {
            return Err(QueryError::runtime(
                "PROFILE and TRACE run queries, not index commands",
            ));
        }
        return Ok(());
    }
    if statement
        .union_all
        .windows(2)
        .any(|pair| pair[0] != pair[1])
    {
        return Err(QueryError::runtime(
            "a statement cannot mix UNION and UNION ALL",
        ));
    }
    let union = statement.parts.len() > 1;
    for part in &statement.parts {
        let last = part.clauses.len() - 1;
        for (index, clause) in part.clauses.iter().enumerate() {
            match clause {
                Clause::Return(_) if index != last => {
                    return Err(QueryError::runtime("RETURN must be the last clause"));
                }
                Clause::With(_) if index == last => {
                    return Err(QueryError::runtime(
                        "a query cannot end with WITH: add RETURN, or a write",
                    ));
                }
                Clause::Subquery(query) => validate_subquery(query)?,
                Clause::With(projection) => {
                    for item in &projection.items {
                        if !item.aliased && !matches!(item.expr, Expr::Var(_)) {
                            return Err(QueryError::runtime(format!(
                                "WITH needs a name for `{}`: add AS name",
                                item.alias
                            )));
                        }
                    }
                }
                _ => {}
            }
        }
        let ends = &part.clauses[last];
        if union && !matches!(ends, Clause::Return(_)) {
            return Err(QueryError::runtime(
                "each part of a UNION must end with RETURN",
            ));
        }
        if !matches!(ends, Clause::Return(_) | Clause::Call(_)) && !ends.writes() {
            return Err(QueryError::runtime(format!(
                "a query cannot end with {}: add RETURN, or end with a write (CREATE, MERGE, SET, REMOVE, DELETE, RETRACT, FOREACH)",
                ends.name()
            )));
        }
    }
    Ok(())
}

/// A `CALL { … }` body: RETURN only last; it ends with RETURN or a write.
fn validate_subquery(query: &Query) -> Result<(), QueryError> {
    let last = query.clauses.len().saturating_sub(1);
    for (index, clause) in query.clauses.iter().enumerate() {
        match clause {
            Clause::Return(_) if index != last => {
                return Err(QueryError::runtime(
                    "RETURN must be the last clause of a CALL { } subquery",
                ));
            }
            Clause::Subquery(inner) => validate_subquery(inner)?,
            _ => {}
        }
    }
    match query.clauses.last() {
        Some(Clause::Return(_)) => Ok(()),
        Some(clause) if clause.writes() => Ok(()),
        _ => Err(QueryError::runtime(
            "a CALL { } subquery ends with RETURN, or with a write",
        )),
    }
}

// ---------------------------------------------------------------- runner

struct Runner<'a> {
    read: &'a ReadTx,
    tx: Option<&'a mut WriteTx>,
    ns: &'a str,
    params: &'a Params,
    stats: WriteStats,
    deleted_nodes: HashSet<NodeId>,
    deleted_edges: HashSet<EdgeId>,
    profile: Option<Vec<PlanStep>>,
    /// The walk being recorded, under `TRACE`.
    trace: Option<crate::trace::Trace>,
    /// Clauses entered so far (the trace's step numbers).
    steps_entered: usize,
    info: PlanInfo,
    budget: Budget,
    /// Groups a MATCH folded for the RETURN or WITH right after it.
    folded: Option<Vec<(Vec<Value>, Row)>>,
    /// Records read when the last PROFILE step ended.
    reads_seen: u64,
    /// On a part of a split query, its share of the first match.
    slice: Option<crate::eval::Slice>,
    /// What the caller may see and change (14.4).
    view: Option<std::sync::Arc<grafeno::View>>,
    /// Whether the caller may read the audit log (14.5).
    may_audit: bool,
    /// The time of the latest MATCH (its `VALID AT` and `KNOWN AT`), which
    /// the patterns inside expressions after it (`COUNT { … }`, `EXISTS
    /// { … }`, a pattern in `WHERE` or `RETURN`) follow as well. Any time
    /// when the latest MATCH named none, or a time per row.
    time: EdgeFilter,
}

/// The rows flowing between clauses and the variables they bind, in order.
struct Flow {
    rows: Vec<Row>,
    vars: Vec<String>,
}

impl Flow {
    fn bind(&mut self, names: impl IntoIterator<Item = impl AsRef<str>>) {
        for name in names {
            let name = name.as_ref();
            if !self.vars.iter().any(|var| var == name) {
                self.vars.push(name.to_owned());
            }
        }
    }
}

impl<'a> Runner<'a> {
    fn new(
        read: &'a ReadTx,
        tx: Option<&'a mut WriteTx>,
        ns: &'a str,
        params: &'a Params,
        mode: Mode,
        limits: &Limits,
    ) -> Result<Self, QueryError> {
        let info = plan_info_with(read, ns, tx.is_none())?;
        Ok(Self {
            read,
            tx,
            ns,
            params,
            stats: WriteStats::default(),
            deleted_nodes: HashSet::new(),
            deleted_edges: HashSet::new(),
            profile: (mode == Mode::Profile).then(Vec::new),
            trace: (mode == Mode::Trace).then(|| {
                crate::trace::Trace::new(crate::trace::TRACE_EVENTS, crate::trace::TRACE_BYTES)
            }),
            steps_entered: 0,
            info,
            budget: Budget::new(limits),
            folded: None,
            reads_seen: 0,
            slice: None,
            time: EdgeFilter::any(),
            view: limits.view.clone(),
            may_audit: limits
                .access
                .as_ref()
                .is_none_or(|access| access.read_audit),
        })
    }

    /// A runner for a read-only part of a split query, planning with the
    /// coordinator's `info` so that every part makes the same plan.
    fn with_info(
        read: &'a ReadTx,
        ns: &'a str,
        params: &'a Params,
        limits: &Limits,
        info: PlanInfo,
    ) -> Self {
        Self {
            read,
            tx: None,
            ns,
            params,
            stats: WriteStats::default(),
            deleted_nodes: HashSet::new(),
            deleted_edges: HashSet::new(),
            profile: None,
            trace: None,
            steps_entered: 0,
            info,
            budget: Budget::new(limits),
            folded: None,
            reads_seen: 0,
            slice: None,
            time: EdgeFilter::any(),
            view: limits.view.clone(),
            may_audit: limits
                .access
                .as_ref()
                .is_none_or(|access| access.read_audit),
        }
    }

    fn ctx<'b>(&'b self, edges: &'b EdgeFilter) -> Context<'b> {
        Context {
            read: self.read,
            write: self.tx.as_deref(),
            ns: self.ns,
            params: self.params,
            edges,
            info: &self.info,
            budget: &self.budget,
            view: self.view.as_ref(),
            may_audit: self.may_audit,
            trace: self.trace.as_ref(),
        }
    }

    fn run(mut self, statement: &Statement) -> Result<QueryResult, QueryError> {
        if let Some(max) = self.budget.max_cost() {
            let params = self.params;
            let value_of = |expr: &Expr| constant(expr, Some(params));
            let (_, cost) = crate::cost::estimate(statement, &self.info, &value_of);
            if cost > max {
                return Err(QueryError::limit(format!(
                    "the query's estimated cost {} is above {} (limit max_cost); EXPLAIN shows where it comes from",
                    show_cost(cost),
                    show_cost(max)
                )));
            }
        }
        // Property rules (not null, types, keys) hold at the statement's
        // end, not between its clauses: `MERGE … ON CREATE SET` may create
        // a node and then give it its required property.
        if let Some(tx) = self.tx.as_deref_mut() {
            tx.defer_checks();
        }
        let mut columns: Option<Vec<String>> = None;
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for (index, part) in statement.parts.iter().enumerate() {
            let (part_columns, part_rows) = self.run_query(part)?;
            match &columns {
                Some(existing) if *existing != part_columns => {
                    return Err(QueryError::runtime(format!(
                        "every part of a UNION must return the same columns: {} and {}",
                        existing.join(", "),
                        part_columns.join(", ")
                    )));
                }
                _ => columns = Some(part_columns),
            }
            rows.extend(part_rows);
            if index > 0 && !statement.union_all[index - 1] {
                let started = Instant::now();
                let mut seen = HashSet::new();
                rows.retain(|row| seen.insert(row.iter().map(Value::key).collect::<Vec<_>>()));
                self.record("UNION", "distinct rows of both parts", rows.len(), started);
            }
        }
        if let Some(tx) = self.tx.as_deref_mut() {
            tx.check_deferred()?;
        }
        if let Some(trace) = &self.trace {
            for (index, row) in rows.iter().enumerate() {
                trace.row(index, &row.iter().collect::<Vec<_>>());
            }
        }
        Ok(QueryResult {
            columns: columns.unwrap_or_default(),
            rows,
            stats: self.stats,
            profile: self.profile,
            trace: self.trace.map(crate::trace::Trace::finish),
        })
    }

    fn record(
        &mut self,
        operation: &str,
        details: impl Into<String>,
        rows: usize,
        started: Instant,
    ) {
        let reads = self.budget.reads();
        if let Some(profile) = &mut self.profile {
            profile.push(PlanStep {
                step: profile.len() + 1,
                operation: operation.to_owned(),
                details: details.into(),
                rows: Some(rows as u64),
                time_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
                reads: Some(reads - self.reads_seen),
            });
        }
        self.reads_seen = reads;
    }

    fn run_query(&mut self, query: &Query) -> Result<(Vec<String>, Vec<Vec<Value>>), QueryError> {
        let mut flow = Flow {
            rows: vec![Row::new()],
            vars: Vec::new(),
        };
        let mut bound: HashSet<String> = HashSet::new();
        for (index, clause) in query.clauses.iter().enumerate() {
            let started = Instant::now();
            if let Some(trace) = &self.trace {
                self.steps_entered += 1;
                trace.enter(self.steps_entered, clause.name());
            }
            let details = self.profile.is_some().then(|| {
                let params = self.params;
                describe(clause, &bound, &self.info, &|expr: &Expr| {
                    constant(expr, Some(params))
                })
            });
            if let Clause::Return(projection) = clause {
                let folded = self.folded.take();
                let edges = self.time.clone();
                let ctx = self.ctx(&edges);
                let (columns, rows) =
                    project_folded(&ctx, flow.rows, projection, &flow.vars, folded)?;
                self.record("RETURN", details.unwrap_or_default(), rows.len(), started);
                return Ok((columns, rows));
            }
            // A MATCH right before an aggregating RETURN or WITH folds its
            // rows as it makes them.
            if let Some(spec) = self.folding_spec(clause, query.clauses.get(index + 1), &flow)? {
                let mut groups = Groups::default();
                flow = self.apply_folding(clause, flow, None, Some((&spec, &mut groups)))?;
                let rows = groups.rows as usize;
                let edges = self.time.clone();
                let ctx = self.ctx(&edges);
                self.folded = Some(groups.finish(&spec, &ctx)?);
                bound.extend(flow.vars.iter().cloned());
                self.record(clause.name(), details.unwrap_or_default(), rows, started);
                continue;
            }
            // A MATCH right before a plain RETURN … LIMIT n stops after
            // the rows it needs.
            let cap = match (clause, query.clauses.get(index + 1)) {
                (Clause::Match { .. }, Some(Clause::Return(projection))) => {
                    self.row_cap(projection)?
                }
                _ => None,
            };
            flow = self.apply(clause, flow, cap)?;
            self.budget.held(&flow.rows, |row| Box::new(row.values()))?;
            bound.extend(flow.vars.iter().cloned());
            self.record(
                clause.name(),
                details.unwrap_or_default(),
                flow.rows.len(),
                started,
            );
        }
        // A query that ends with CALL returns what the procedure yields.
        if let Some(Clause::Call(call)) = query.clauses.last() {
            let columns = yielded(call)?
                .into_iter()
                .map(|(_, alias)| alias)
                .collect::<Vec<_>>();
            let rows = flow
                .rows
                .into_iter()
                .map(|row| {
                    columns
                        .iter()
                        .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect();
            return Ok((columns, rows));
        }
        Ok((Vec::new(), Vec::new()))
    }

    /// What to fold while matching, when `clause` is a (non-optional)
    /// `MATCH` and `next` a RETURN or WITH with aggregates.
    fn folding_spec(
        &self,
        clause: &Clause,
        next: Option<&Clause>,
        flow: &Flow,
    ) -> Result<Option<Spec>, QueryError> {
        let (
            Clause::Match {
                optional: false,
                patterns,
                ..
            },
            Some(Clause::Return(projection) | Clause::With(projection)),
        ) = (clause, next)
        else {
            return Ok(None);
        };
        if !projection.star
            && !projection
                .items
                .iter()
                .any(|item| item.expr.has_aggregate())
        {
            return Ok(None);
        }
        // The variables after the match, in the order it binds them.
        let mut vars = flow.vars.clone();
        for var in patterns.iter().flat_map(Pattern::variables) {
            if !vars.iter().any(|known| known == var) {
                vars.push(var.to_owned());
            }
        }
        let (items, _) = projection_items(projection, &vars)?;
        Ok(Spec::new(&items))
    }

    /// How many rows a RETURN needs from the clause before it: `SKIP + LIMIT`
    /// when it neither sorts, groups nor drops duplicates.
    fn row_cap(&self, projection: &Projection) -> Result<Option<usize>, QueryError> {
        let Some(limit) = &projection.limit else {
            return Ok(None);
        };
        if projection.distinct
            || !projection.order.is_empty()
            || projection
                .items
                .iter()
                .any(|item| item.expr.has_aggregate())
        {
            return Ok(None);
        }
        let count = |expr: &Expr| -> Result<Option<usize>, QueryError> {
            Ok(match self.eval(&Row::new(), expr)? {
                Value::Int(n) if n >= 0 => Some(n as usize),
                _ => None,
            })
        };
        let skip = match &projection.skip {
            Some(skip) => count(skip)?,
            None => Some(0),
        };
        Ok(skip
            .zip(count(limit)?)
            .map(|(skip, limit)| skip.saturating_add(limit)))
    }

    /// Runs one clause (not RETURN) on the rows; `cap` is how many rows the
    /// rest of the query needs, when that is known.
    fn apply(
        &mut self,
        clause: &Clause,
        flow: Flow,
        cap: Option<usize>,
    ) -> Result<Flow, QueryError> {
        self.apply_folding(clause, flow, cap, None)
    }

    /// [`Runner::apply`]; a `MATCH` given `fold` folds its rows into those
    /// groups as it makes them instead of keeping them (batched
    /// aggregation for the `RETURN` or `WITH` after it).
    fn apply_folding(
        &mut self,
        clause: &Clause,
        mut flow: Flow,
        cap: Option<usize>,
        mut fold: Option<(&Spec, &mut Groups)>,
    ) -> Result<Flow, QueryError> {
        match clause {
            Clause::Match {
                optional,
                patterns,
                valid_at,
                known_at,
                filter,
            } => {
                let new_vars: Vec<String> = patterns
                    .iter()
                    .flat_map(Pattern::variables)
                    .map(str::to_owned)
                    .collect();
                // `VALID AT at` may name a variable (one time per row, as
                // with `temporal.series`); otherwise it is computed once.
                let per_row = [valid_at, known_at]
                    .into_iter()
                    .flatten()
                    .any(mentions_variables);
                let shared = if per_row {
                    EdgeFilter::any()
                } else {
                    self.time_filter(valid_at.as_ref(), known_at.as_ref(), &Row::new())?
                };
                // What comes after this MATCH reads the graph at its time.
                self.time = shared.clone();
                let conditions: Vec<&Expr> = filter.as_ref().map(conjuncts).unwrap_or_default();
                // Cheapest start first; the variables are the same in every row.
                let params = self.params;
                let order = order_patterns(
                    patterns,
                    &|var| flow.vars.iter().any(|bound| bound == var),
                    &conditions,
                    &self.info,
                    &|expr: &Expr| constant(expr, Some(params)),
                );
                let mut out = Vec::new();
                for row in flow.rows {
                    if cap.is_some_and(|cap| out.len() >= cap) {
                        break;
                    }
                    let edges = if per_row {
                        self.time_filter(valid_at.as_ref(), known_at.as_ref(), &row)?
                    } else {
                        shared.clone()
                    };
                    let ctx = self.ctx(&edges);
                    let mut rows = vec![row.clone()];
                    for (position, &index) in order.iter().enumerate() {
                        let pattern = &patterns[index];
                        let last = position + 1 == order.len();
                        // A part of a split query cuts the candidates of
                        // the first pattern of its first match.
                        let slice = if position == 0 {
                            self.slice.as_ref().and_then(crate::eval::Slice::take)
                        } else {
                            None
                        };
                        let matcher = Matcher {
                            slice,
                            conditions: &conditions,
                            filter: if last { filter.as_ref() } else { None },
                            cap: if last {
                                cap.map(|cap| cap - out.len())
                            } else {
                                None
                            },
                        };
                        if last
                            && !*optional
                            && let Some((spec, groups)) = &mut fold
                        {
                            let spec: &Spec = spec;
                            match_pattern_into(
                                &ctx,
                                rows,
                                pattern,
                                &matcher,
                                &Emit {
                                    prepare: &|ctx, rows| spec.columns(ctx, rows),
                                },
                                &mut |columns| groups.fold(spec, columns),
                            )?;
                            rows = Vec::new();
                        } else {
                            rows = match_pattern(&ctx, rows, pattern, &matcher)?;
                        }
                    }
                    if rows.is_empty() && *optional {
                        let mut row = row;
                        for var in &new_vars {
                            row.entry(var.clone()).or_insert(Value::Null);
                        }
                        out.push(row);
                    } else {
                        out.extend(rows);
                    }
                    self.budget.rows(out.len())?;
                }
                flow.rows = out;
                flow.bind(&new_vars);
            }
            Clause::Unwind { list, var } => {
                let edges = self.time.clone();
                let ctx = self.ctx(&edges);
                let mut out = Vec::new();
                for row in flow.rows {
                    let items = match eval(&ctx, list, Scope::Row(&row))? {
                        Value::List(items) => items,
                        Value::Null => Vec::new(),
                        other => vec![other],
                    };
                    let next_len = row.len();
                    for item in items {
                        let mut next = row.clone();
                        next.insert(var.clone(), item);
                        out.push(next);
                        self.budget.work(1 + next_len as u64)?;
                        if out.len() % 1024 == 0 {
                            self.budget.held(&out, |row| Box::new(row.values()))?;
                        }
                    }
                }
                flow.rows = out;
                flow.bind([var]);
            }
            Clause::With(projection) => {
                let folded = self.folded.take();
                let edges = self.time.clone();
                let ctx = self.ctx(&edges);
                let (columns, values) =
                    project_folded(&ctx, flow.rows, projection, &flow.vars, folded)?;
                let mut rows: Vec<Row> = values
                    .into_iter()
                    .map(|values| columns.iter().cloned().zip(values).collect())
                    .collect();
                if let Some(filter) = &projection.filter {
                    rows = keep(&ctx, rows, filter)?;
                }
                flow = Flow {
                    rows,
                    vars: columns,
                };
            }
            Clause::Call(call) => {
                let yields = yielded(call)?;
                let mode = call.mode();
                let columns = call_columns(call)?;
                let edges = EdgeFilter::any();
                let procedure = procedures::find(&call.name)?;
                let mut out = Vec::new();
                // What a write mode puts on nodes: (node, property, value).
                let mut writes: Vec<(NodeId, String, Value)> = Vec::new();
                {
                    let ctx = self.ctx(&edges);
                    for row in flow.rows {
                        let args = call
                            .args
                            .iter()
                            .map(|arg| eval(&ctx, arg, Scope::Row(&row)))
                            .collect::<Result<Vec<_>, _>>()?;
                        // A mode passed in a parameter would be ignored:
                        // whether the call writes is decided before it runs.
                        if call.name.starts_with("algo.")
                            && let Some(Value::Map(settings)) = args.last()
                        {
                            let stats = settings
                                .get("mode")
                                .is_some_and(|mode| *mode == Value::Str("stats".into()));
                            let write = settings.get("write").is_some_and(|w| !w.is_null());
                            if (write && !matches!(mode, crate::ast::CallMode::Write(_)))
                                || (stats && mode == crate::ast::CallMode::Stream)
                            {
                                return Err(QueryError::runtime(format!(
                                    "{}(): write and mode: 'stats' go in the settings map written in the query ({}({{write: 'rank'}}), {}({{mode: 'stats', weight: $w}})), not in a parameter: whether a query writes is known before it runs",
                                    procedure.name, procedure.name, procedure.name
                                )));
                            }
                        }
                        let results = procedures::call(&ctx, &call.name, &args)?;
                        let results = match &mode {
                            crate::ast::CallMode::Stream => results,
                            crate::ast::CallMode::Stats => stats_rows(procedure.columns, &results),
                            crate::ast::CallMode::Write(targets) => {
                                let node_at = results.first().and_then(|first| {
                                    first
                                        .iter()
                                        .position(|value| matches!(value, Value::Node(_)))
                                });
                                let mut written = Vec::new();
                                for (column, property) in targets {
                                    let at = match column {
                                        Some(column) => procedure
                                            .columns
                                            .iter()
                                            .position(|name| name.eq_ignore_ascii_case(column))
                                            .ok_or_else(|| {
                                                QueryError::runtime(format!(
                                                    "{}() yields {}: it has no column {column} to write",
                                                    procedure.name,
                                                    procedure.columns.join(", ")
                                                ))
                                            })?,
                                        None => procedure
                                            .columns
                                            .iter()
                                            .enumerate()
                                            .position(|(at, _)| Some(at) != node_at)
                                            .unwrap_or(0),
                                    };
                                    let Some(node_at) = node_at else {
                                        if !results.is_empty() {
                                            return Err(QueryError::runtime(format!(
                                                "{}() gives no node to write to",
                                                procedure.name
                                            )));
                                        }
                                        written.push(vec![
                                            Value::Str(property.clone()),
                                            Value::Str(procedure.columns[at].to_owned()),
                                            Value::Int(0),
                                        ]);
                                        continue;
                                    };
                                    let mut count = 0;
                                    for result in &results {
                                        if let Value::Node(node) = &result[node_at] {
                                            writes.push((
                                                node.id,
                                                property.clone(),
                                                result[at].clone(),
                                            ));
                                            count += 1;
                                        }
                                    }
                                    written.push(vec![
                                        Value::Str(property.clone()),
                                        Value::Str(procedure.columns[at].to_owned()),
                                        Value::Int(count),
                                    ]);
                                }
                                written
                            }
                        };
                        for result in results {
                            let mut next = row.clone();
                            for (column, alias) in &yields {
                                let at = columns
                                    .iter()
                                    .position(|name| name.eq_ignore_ascii_case(column))
                                    .expect("checked by yielded()");
                                next.insert(alias.clone(), result[at].clone());
                            }
                            out.push(next);
                        }
                    }
                }
                if !writes.is_empty() {
                    let ns = self.ns;
                    let Some(tx) = self.tx.as_deref_mut() else {
                        return Err(QueryError::runtime("write: needs a writable database"));
                    };
                    for (node, property, value) in writes {
                        // `write: '@embedding'` stores the node's vector, as
                        // `SET node.@embedding = …` does.
                        if property == "@embedding" {
                            tx.set_embedding(ns, node, &embedding_from(&value)?)?;
                            self.stats.properties_set += 1;
                            continue;
                        }
                        if property.starts_with('@') {
                            return Err(QueryError::runtime(format!(
                                "write: stores a property or @embedding, not {property}"
                            )));
                        }
                        let value = value.to_property().map_err(QueryError::runtime)?;
                        let mut props = grafeno::Props::new();
                        props.insert(property, value);
                        tx.set_node_props(ns, node, props)?;
                        self.stats.properties_set += 1;
                    }
                }
                let edges = self.time.clone();
                let ctx = self.ctx(&edges);
                if let Some(filter) = &call.filter {
                    out = keep(&ctx, out, filter)?;
                }
                flow.rows = out;
                flow.bind(yields.iter().map(|(_, alias)| alias));
            }
            Clause::Foreach { var, list, body } => {
                for row in &flow.rows {
                    let items = {
                        let edges = self.time.clone();
                        let ctx = self.ctx(&edges);
                        match eval(&ctx, list, Scope::Row(row))? {
                            Value::List(items) => items,
                            Value::Null => Vec::new(),
                            other => {
                                return Err(QueryError::runtime(format!(
                                    "FOREACH needs a list, got a {}",
                                    other.type_name()
                                )));
                            }
                        }
                    };
                    for item in items {
                        let mut inner = row.clone();
                        inner.insert(var.clone(), item);
                        let mut inner = Flow {
                            rows: vec![inner],
                            vars: flow.vars.iter().cloned().chain([var.clone()]).collect(),
                        };
                        for clause in body {
                            inner = self.apply(clause, inner, None)?;
                        }
                    }
                }
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => {
                let mut out = Vec::new();
                for row in flow.rows {
                    // Only facts true now count as existing.
                    let edges = EdgeFilter::any().valid_at(Timestamp::now());
                    let found = {
                        let ctx = self.ctx(&edges);
                        match_pattern(&ctx, vec![row.clone()], pattern, &Matcher::default())?
                    };
                    if found.is_empty() {
                        let mut row = row;
                        self.create(&mut row, pattern, true)?;
                        for item in on_create {
                            self.set(&mut row, item)?;
                        }
                        out.push(row);
                    } else {
                        for mut row in found {
                            for item in on_match {
                                self.set(&mut row, item)?;
                            }
                            out.push(row);
                        }
                    }
                }
                flow.rows = out;
                flow.bind(pattern.variables());
            }
            Clause::Create(patterns) => {
                for row in &mut flow.rows {
                    for pattern in patterns {
                        self.create(row, pattern, false)?;
                    }
                }
                flow.bind(patterns.iter().flat_map(Pattern::variables));
            }
            Clause::Set(items) => {
                for row in &mut flow.rows {
                    for item in items {
                        self.set(row, item)?;
                    }
                }
            }
            Clause::Remove(items) => {
                for row in &mut flow.rows {
                    for item in items {
                        self.remove(row, item)?;
                    }
                }
            }
            Clause::Delete(exprs) => {
                for row in &flow.rows {
                    for expr in exprs {
                        self.delete(row, expr)?;
                    }
                }
            }
            Clause::Retract(exprs) => {
                for row in &flow.rows {
                    for expr in exprs {
                        self.retract(row, expr)?;
                    }
                }
            }
            Clause::Subquery(query) => {
                let mut out = Vec::new();
                let mut added: Vec<String> = Vec::new();
                for row in flow.rows {
                    let mut inner = Flow {
                        rows: vec![row.clone()],
                        vars: flow.vars.clone(),
                    };
                    let mut returned = None;
                    for clause in &query.clauses {
                        if let Clause::Return(projection) = clause {
                            let edges = self.time.clone();
                            let ctx = self.ctx(&edges);
                            returned = Some(project(
                                &ctx,
                                std::mem::take(&mut inner.rows),
                                projection,
                                &inner.vars,
                            )?);
                            break;
                        }
                        inner = self.apply(clause, inner, None)?;
                    }
                    match returned {
                        Some((columns, values)) => {
                            if let Some(clash) = columns.iter().find(|c| flow.vars.contains(c)) {
                                return Err(QueryError::runtime(format!(
                                    "the CALL {{ }} subquery returns `{clash}`, which is already a variable outside it"
                                )));
                            }
                            for values in values {
                                let mut next = row.clone();
                                next.extend(columns.iter().cloned().zip(values));
                                out.push(next);
                            }
                            added = columns;
                        }
                        // A subquery without RETURN (writes) keeps the row.
                        None => out.push(row),
                    }
                    self.budget.rows(out.len())?;
                }
                flow.rows = out;
                flow.bind(added);
            }
            Clause::Return(_) => unreachable!("RETURN ends the query"),
        }
        Ok(flow)
    }

    fn time_filter(
        &self,
        valid_at: Option<&Expr>,
        known_at: Option<&Expr>,
        row: &Row,
    ) -> Result<EdgeFilter, QueryError> {
        let any = EdgeFilter::any();
        let ctx = self.ctx(&any);
        let mut filter = EdgeFilter::any();
        if let Some(at) = valid_at {
            filter = filter.valid_at(to_time(&eval(&ctx, at, Scope::Row(row))?)?);
        }
        if let Some(at) = known_at {
            filter = filter.known_at(to_time(&eval(&ctx, at, Scope::Row(row))?)?);
        }
        Ok(filter)
    }

    /// The time the latest MATCH of `clauses` reads the graph at, as
    /// [`Runner::time`] would hold it after running them: what the merge of
    /// a split query evaluates its expressions at.
    fn time_of(&self, clauses: &[Clause]) -> Result<EdgeFilter, QueryError> {
        let latest = clauses.iter().rev().find_map(|clause| match clause {
            Clause::Match {
                valid_at, known_at, ..
            } => Some((valid_at, known_at)),
            _ => None,
        });
        match latest {
            Some((valid_at, known_at))
                if ![valid_at, known_at]
                    .into_iter()
                    .flatten()
                    .any(mentions_variables) =>
            {
                self.time_filter(valid_at.as_ref(), known_at.as_ref(), &Row::new())
            }
            _ => Ok(EdgeFilter::any()),
        }
    }

    fn tx(&mut self) -> &mut WriteTx {
        self.tx
            .as_deref_mut()
            .expect("writing queries run with a write transaction")
    }

    fn eval(&self, row: &Row, expr: &Expr) -> Result<Value, QueryError> {
        let edges = self.time.clone();
        eval(&self.ctx(&edges), expr, Scope::Row(row))
    }
}

/// Whether an expression reads a variable of the row.
fn mentions_variables(expr: &Expr) -> bool {
    let mut found = false;
    expr.visit(&mut |inner| found |= matches!(inner, Expr::Var(_)));
    found
}

fn keep(ctx: &Context, rows: Vec<Row>, filter: &Expr) -> Result<Vec<Row>, QueryError> {
    let mut kept = Vec::with_capacity(rows.len());
    for row in rows {
        if truth(&eval(ctx, filter, Scope::Row(&row))?)? == Some(true) {
            kept.push(row);
        } else if let Some(trace) = ctx.trace {
            trace.drop_row(&row_values(&row), filter);
        }
    }
    Ok(kept)
}

/// A row's values in the order of its variables' names.
pub(crate) fn row_values(row: &Row) -> Vec<&Value> {
    let mut names: Vec<&String> = row.keys().collect();
    names.sort();
    names.into_iter().map(|name| &row[name]).collect()
}

/// The `(column, alias)` pairs a CALL binds.
/// The columns a call gives back: its procedure's, or those of its mode.
fn call_columns(call: &ProcedureCall) -> Result<Vec<&'static str>, QueryError> {
    let procedure = procedures::find(&call.name)?;
    Ok(match call.mode() {
        crate::ast::CallMode::Stream => procedure.columns.to_vec(),
        crate::ast::CallMode::Stats => crate::ast::STATS_COLUMNS.to_vec(),
        crate::ast::CallMode::Write(_) => crate::ast::WRITE_COLUMNS.to_vec(),
    })
}

fn yielded(call: &ProcedureCall) -> Result<Vec<(String, String)>, QueryError> {
    let procedure = procedures::find(&call.name)?;
    let columns = call_columns(call)?;
    match &call.yields {
        None => Ok(columns
            .iter()
            .map(|column| ((*column).to_owned(), (*column).to_owned()))
            .collect()),
        Some(items) => {
            for (column, _) in items {
                if !columns.iter().any(|name| name.eq_ignore_ascii_case(column)) {
                    return Err(QueryError::runtime(format!(
                        "{}() yields {}, not {column}",
                        procedure.name,
                        columns.join(", ")
                    )));
                }
            }
            Ok(items.clone())
        }
    }
}

/// An algorithm's rows summed up: per numeric column, how many values,
/// the least, the most, the mean and the sum; first, the rows themselves.
fn stats_rows(columns: &[&str], rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let mut out = vec![vec![
        Value::Str("rows".into()),
        Value::Int(rows.len() as i64),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
    ]];
    for (at, column) in columns.iter().enumerate() {
        let numbers: Vec<f64> = rows
            .iter()
            .filter_map(|row| match row.get(at) {
                Some(Value::Int(n)) => Some(*n as f64),
                Some(Value::Float(f)) => Some(*f),
                _ => None,
            })
            .collect();
        if numbers.is_empty() {
            continue;
        }
        let sum: f64 = numbers.iter().sum();
        out.push(vec![
            Value::Str((*column).to_owned()),
            Value::Int(numbers.len() as i64),
            Value::Float(numbers.iter().copied().fold(f64::INFINITY, f64::min)),
            Value::Float(numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
            Value::Float(sum / numbers.len() as f64),
            Value::Float(sum),
        ]);
    }
    out
}

/// Whether a pattern predicate matches from `row` at least once.
/// Every match of the patterns from `row` that passes `filter` (for
/// `COUNT { }` and pattern comprehensions).
pub(crate) fn pattern_rows(
    ctx: &Context,
    row: &Row,
    patterns: &[Pattern],
    filter: Option<&Expr>,
) -> Result<Vec<Row>, QueryError> {
    let mut rows = vec![row.clone()];
    for pattern in patterns {
        rows = match_pattern(ctx, rows, pattern, &Matcher::default())?;
        if rows.is_empty() {
            return Ok(rows);
        }
    }
    if let Some(filter) = filter {
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            if truth(&eval(ctx, filter, Scope::Row(&row))?)? == Some(true) {
                kept.push(row);
            }
        }
        rows = kept;
    }
    Ok(rows)
}

pub(crate) fn exists(
    ctx: &Context,
    row: &Row,
    patterns: &[Pattern],
    filter: Option<&Expr>,
) -> Result<bool, QueryError> {
    let mut rows = vec![row.clone()];
    for pattern in patterns {
        rows = match_pattern(ctx, rows, pattern, &Matcher::default())?;
        if rows.is_empty() {
            return Ok(false);
        }
    }
    match filter {
        None => Ok(true),
        Some(filter) => {
            for row in &rows {
                if truth(&eval(ctx, filter, Scope::Row(row))?)? == Some(true) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------- matching

/// What a match may use besides the pattern: the `WHERE` conditions (to
/// pick an index and to drop anchors early), the filter for complete rows,
/// and how many rows are needed.
#[derive(Default)]
struct Matcher<'q> {
    conditions: &'q [&'q Expr],
    filter: Option<&'q Expr>,
    cap: Option<usize>,
    /// On a part of a split query, its share `(part, parts)` of this
    /// match's candidates (the query's first pattern only).
    slice: Option<(usize, usize)>,
}

/// Candidates are read in pages, so a scan with a LIMIT stops early.
const PAGE: usize = 512;

fn match_pattern(
    ctx: &Context,
    rows: Vec<Row>,
    pattern: &Pattern,
    matcher: &Matcher,
) -> Result<Vec<Row>, QueryError> {
    let mut out = Vec::new();
    let budget = ctx.budget;
    match_pattern_into(
        ctx,
        rows,
        pattern,
        matcher,
        &Emit {
            prepare: &|_, rows| Ok(rows),
        },
        &mut |rows: Vec<Row>| {
            out.extend(rows);
            // Every row so far is held.
            budget.rows(out.len())
        },
    )?;
    Ok(out)
}

/// Turns a batch of rows into what a consumer takes.
type Prepare<'a, T> = dyn Fn(&Context, Vec<Row>) -> Result<T, QueryError> + Sync + 'a;

/// Where the rows of a match go: `prepare` turns each batch into what
/// `take` consumes, on the thread that matched it (a worker of a parallel
/// scan included); `take` receives the batches in row order.
struct Emit<'e, T> {
    prepare: &'e Prepare<'e, T>,
}

/// Every match of `pattern` from each of `rows`, handed over in batches of
/// up to [`crate::aggregate::BATCH`] rows (or all at once with a row cap).
fn match_pattern_into<T: Send>(
    ctx: &Context,
    rows: Vec<Row>,
    pattern: &Pattern,
    matcher: &Matcher,
    emit: &Emit<T>,
    take: &mut (dyn FnMut(T) -> Result<(), QueryError> + Send),
) -> Result<(), QueryError> {
    let nodes = pattern.nodes();
    // Rows of the serial path not yet handed over.
    let mut out = Vec::new();
    for row in rows {
        let value_of = |expr: &Expr| eval(ctx, expr, Scope::Row(&row)).ok();
        let (anchor, source, _) = plan_anchor(
            pattern,
            &|var| row.contains_key(var),
            matcher.conditions,
            ctx.info,
            &value_of,
        );
        let anchor_var = nodes[anchor].var.as_deref();
        // Conditions on the anchor alone run before expanding from it.
        let early: Vec<&Expr> = matcher
            .conditions
            .iter()
            .copied()
            .filter(|condition| {
                free_vars(condition).is_some_and(|vars| {
                    vars.iter()
                        .all(|var| row.contains_key(var) || Some(var.as_str()) == anchor_var)
                })
            })
            .collect();
        let start = Start {
            pattern,
            nodes: &nodes,
            anchor,
            early: &early,
            filter: matcher.filter,
        };
        let ops = start.ops(ctx);
        let caches = Caches::new(ops.len());
        // A part of a split query takes its share of the candidates, in
        // their order, so the parts' rows put end to end are one server's.
        if let Some((part, parts)) = matcher.slice {
            if !out.is_empty() {
                take((emit.prepare)(ctx, std::mem::take(&mut out))?)?;
            }
            let ids = all_candidates(ctx, nodes[anchor], &source, &row)?;
            let (from, to) = (ids.len() * part / parts, ids.len() * (part + 1) / parts);
            let ids = &ids[from..to];
            let anchors_of =
                |ctx: &Context, ids: &[NodeId]| -> Result<Vec<Arc<Node>>, QueryError> {
                    let mut anchors = Vec::with_capacity(ids.len());
                    for id in ids {
                        let Some(node) = ctx.node(*id)? else {
                            continue;
                        };
                        let node = Arc::new(node);
                        if node_fits(ctx, &node, nodes[anchor], &row)? {
                            anchors.push(node);
                        }
                    }
                    Ok(anchors)
                };
            if matcher.cap.is_none()
                && ctx.write.is_none()
                && ctx.trace.is_none()
                && ids.len() >= crate::parallel::MIN_CANDIDATES
                && crate::parallel::available()
            {
                crate::parallel::map_chunks(
                    ctx,
                    ids,
                    |ctx, chunk: &[NodeId]| -> Result<T, QueryError> {
                        let mut rows = Vec::new();
                        for ids in chunk.chunks(ANCHORS) {
                            let anchors = anchors_of(ctx, ids)?;
                            start
                                .extend_batch(ctx, &row, &ops, anchors, &mut rows, None, &caches)?;
                        }
                        ctx.budget.rows(rows.len())?;
                        (emit.prepare)(ctx, rows)
                    },
                    take,
                )?;
            } else {
                let batch = if matcher.cap.is_some() { 1 } else { ANCHORS };
                for ids in ids.chunks(batch) {
                    let anchors = anchors_of(ctx, ids)?;
                    if start.extend_batch(
                        ctx,
                        &row,
                        &ops,
                        anchors,
                        &mut out,
                        matcher.cap,
                        &caches,
                    )? {
                        return take((emit.prepare)(ctx, out)?);
                    }
                    if matcher.cap.is_none() && out.len() >= crate::aggregate::BATCH {
                        take((emit.prepare)(ctx, std::mem::take(&mut out))?)?;
                    }
                    ctx.budget.rows(out.len())?;
                }
            }
            continue;
        }
        if let Some(ids) = parallel_candidates(ctx, &source, &row, matcher)? {
            if !out.is_empty() {
                take((emit.prepare)(ctx, std::mem::take(&mut out))?)?;
            }
            crate::parallel::map_chunks(
                ctx,
                &ids,
                |ctx, chunk: &[NodeId]| -> Result<T, QueryError> {
                    let mut rows = Vec::new();
                    for ids in chunk.chunks(ANCHORS) {
                        let mut anchors = Vec::with_capacity(ids.len());
                        for id in ids {
                            let Some(node) = ctx.node(*id)? else {
                                continue;
                            };
                            let node = Arc::new(node);
                            if node_fits(ctx, &node, nodes[anchor], &row)? {
                                anchors.push(node);
                            }
                        }
                        start.extend_batch(ctx, &row, &ops, anchors, &mut rows, None, &caches)?;
                    }
                    ctx.budget.rows(rows.len())?;
                    (emit.prepare)(ctx, rows)
                },
                take,
            )?;
            continue;
        }
        let mut cursor = None;
        // With a row cap, one anchor at a time, so that no more is read
        // than the rows need.
        let batch = if matcher.cap.is_some() { 1 } else { ANCHORS };
        loop {
            let (page, next) = candidates(ctx, nodes[anchor], &source, &row, cursor)?;
            let mut page = page.into_iter().peekable();
            while page.peek().is_some() {
                let anchors: Vec<Arc<Node>> = page.by_ref().take(batch).collect();
                if start.extend_batch(ctx, &row, &ops, anchors, &mut out, matcher.cap, &caches)? {
                    return take((emit.prepare)(ctx, out)?);
                }
                if matcher.cap.is_none() && out.len() >= crate::aggregate::BATCH {
                    take((emit.prepare)(ctx, std::mem::take(&mut out))?)?;
                }
                ctx.budget.rows(out.len())?;
            }
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
    }
    if !out.is_empty() {
        take((emit.prepare)(ctx, out)?)?;
    }
    Ok(())
}

/// The candidates of a start worth splitting across threads: a read-only
/// query, no row cap (a `LIMIT` wants the first rows, in order), and a
/// label or an index lookup estimated to find at least
/// [`crate::parallel::MIN_CANDIDATES`] nodes. `None` otherwise, or when
/// the query runs on one thread.
fn parallel_candidates(
    ctx: &Context,
    source: &Source,
    row: &Row,
    matcher: &Matcher,
) -> Result<Option<Vec<NodeId>>, QueryError> {
    // A traced walk keeps one order: no threads.
    if ctx.write.is_some()
        || ctx.trace.is_some()
        || matcher.cap.is_some()
        || source.rows() < crate::parallel::MIN_CANDIDATES as f64
        || !crate::parallel::available()
    {
        return Ok(None);
    }
    let ids = match source {
        Source::Label { label, .. } => {
            let ids = ctx.read.nodes_with_label(ctx.ns, label)?;
            ctx.budget.read(ids.len().max(1))?;
            ids
        }
        Source::Index { .. } => match index_candidates(ctx, source, row)? {
            Some(ids) => ids,
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    Ok(Some(ids))
}

/// The nodes an index start finds for this row, each once, in index
/// order; `None` when its values cannot be looked up (a time, a list…).
fn index_candidates(
    ctx: &Context,
    source: &Source,
    row: &Row,
) -> Result<Option<Vec<NodeId>>, QueryError> {
    let Source::Index {
        name,
        prefix,
        seek,
        edges,
        ..
    } = source
    else {
        return Ok(None);
    };
    let Some(ranges) = seek_ranges(ctx, *seek, row)? else {
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
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for range in &ranges {
        for id in ctx.index_seek(name, &values, range, *edges)? {
            if seen.insert(id) {
                ids.push(id);
            }
        }
    }
    Ok(Some(ids))
}

/// Every candidate of a start, in the order a serial match visits them
/// (the parts of a split query cut this list).
fn all_candidates(
    ctx: &Context,
    pattern: &NodePattern,
    source: &Source,
    row: &Row,
) -> Result<Vec<NodeId>, QueryError> {
    let mut ids = Vec::new();
    let mut cursor = None;
    match source {
        Source::Label { label, .. } => {
            ids = ctx.read.nodes_with_label(ctx.ns, label)?;
            ctx.budget.read(ids.len().max(1))?;
        }
        Source::Index { .. } if index_candidates(ctx, source, row)?.is_some() => {
            ids = index_candidates(ctx, source, row)?.unwrap_or_default();
        }
        _ => loop {
            // Scans, an `@id`: page through as the serial match does.
            let (page, next) = candidates(ctx, pattern, source, row, cursor)?;
            ids.extend(page.iter().map(|node| node.id));
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        },
    }
    Ok(ids)
}

pub(crate) fn all_true(ctx: &Context, conditions: &[&Expr], row: &Row) -> Result<bool, QueryError> {
    for condition in conditions {
        if truth(&eval(ctx, condition, Scope::Row(row))?)? != Some(true) {
            if let Some(trace) = ctx.trace {
                trace.drop_row(&row_values(row), condition);
            }
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn reverse(direction: Direction) -> Direction {
    match direction {
        Direction::Out => Direction::In,
        Direction::In => Direction::Out,
        Direction::Both => Direction::Both,
    }
}

/// Where the next page of candidates starts.
enum Cursor {
    /// A scan: the last node of the previous page.
    After(NodeId),
    /// An index lookup: the ids it found, and how many were loaded.
    Ids(Vec<NodeId>, usize),
}

/// Nodes found through an index are loaded this many at a time.
const INDEX_PAGE: usize = 64;

/// One page of the nodes a pattern can start from, and where the next page
/// starts.
fn candidates(
    ctx: &Context,
    pattern: &NodePattern,
    source: &Source,
    row: &Row,
    cursor: Option<Cursor>,
) -> Result<(Vec<Arc<Node>>, Option<Cursor>), QueryError> {
    let via = ctx.trace.map(|_| match source {
        Source::Bound => "bound".to_owned(),
        Source::Id(_) => "id".to_owned(),
        Source::Index { name, edges, .. } => match edges {
            Some(_) => format!("edge index {name}"),
            None => format!("index {name}"),
        },
        Source::Label { label, .. } => format!("label {label}"),
        Source::Scan { .. } => "scan".to_owned(),
    });
    let fitting = |found: Vec<Node>| -> Result<Vec<Arc<Node>>, QueryError> {
        let mut out = Vec::new();
        for node in found {
            let node = Arc::new(node);
            let fits = node_fits(ctx, &node, pattern, row)?;
            if let (Some(trace), Some(via)) = (ctx.trace, &via) {
                if fits {
                    trace.anchor(node.id, via);
                } else {
                    trace.drop_node(node.id, "pattern");
                }
            }
            if fits {
                out.push(node);
            }
        }
        Ok(out)
    };
    let load =
        |ids: Vec<NodeId>, from: usize| -> Result<(Vec<Arc<Node>>, Option<Cursor>), QueryError> {
            let to = (from + INDEX_PAGE).min(ids.len());
            let mut found = Vec::with_capacity(to - from);
            for id in &ids[from..to] {
                if let Some(node) = ctx.node(*id)? {
                    found.push(node);
                }
            }
            let more = to < ids.len();
            let next = more.then_some(Cursor::Ids(ids, to));
            Ok((fitting(found)?, next))
        };
    let after = match cursor {
        Some(Cursor::Ids(ids, from)) => return load(ids, from),
        Some(Cursor::After(id)) => Some(id),
        None => None,
    };
    let scan = |label: Option<&str>| -> Result<(Vec<Arc<Node>>, Option<Cursor>), QueryError> {
        let page = ctx.nodes_page(label, after, PAGE)?;
        let next = (page.len() == PAGE).then(|| Cursor::After(page[PAGE - 1].id));
        Ok((fitting(page)?, next))
    };
    match source {
        Source::Bound => {
            let var = pattern
                .var
                .as_deref()
                .expect("a bound pattern has a variable");
            match row.get(var) {
                Some(Value::Node(node)) if node_fits(ctx, node, pattern, row)? => {
                    Ok((vec![node.clone()], None))
                }
                Some(Value::Node(_) | Value::Null) | None => Ok((Vec::new(), None)),
                Some(other) => Err(QueryError::runtime(format!(
                    "`{var}` is a {}, not a node",
                    other.type_name()
                ))),
            }
        }
        Source::Id(expr) => {
            let found = match eval(ctx, expr, Scope::Row(row))? {
                Value::Str(text) => match uuid_from_text(&text) {
                    Some(id) => ctx.node(id)?.into_iter().collect(),
                    None => Vec::new(),
                },
                _ => Vec::new(),
            };
            Ok((fitting(found)?, None))
        }
        Source::Index { .. } => match index_candidates(ctx, source, row)? {
            Some(ids) => load(ids, 0),
            // Values the index cannot look up: scan the pattern's label.
            None => scan(pattern.labels.first().map(String::as_str)),
        },
        Source::Label { label, .. } => scan(Some(label)),
        Source::Scan { .. } => scan(None),
    }
}

pub(crate) fn uuid_from_text(text: &str) -> Option<NodeId> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(NodeId::from_u128)
}

pub(crate) fn props_fit(
    ctx: &Context,
    target: &Value,
    props: &[(Key, Expr)],
    row: &Row,
) -> Result<bool, QueryError> {
    for (key, expr) in props {
        let expected = eval(ctx, expr, Scope::Row(row))?;
        let actual = property(ctx, target, key)?;
        if equals(&actual, &expected) != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn node_fits(
    ctx: &Context,
    node: &Arc<Node>,
    pattern: &NodePattern,
    row: &Row,
) -> Result<bool, QueryError> {
    if !pattern.labels.iter().all(|label| node.has_label(label)) {
        return Ok(false);
    }
    props_fit(ctx, &Value::Node(node.clone()), &pattern.props, row)
}

// ---------------------------------------------------------------- writes

fn to_props(entries: impl IntoIterator<Item = (String, Value)>) -> Result<Props, QueryError> {
    let mut props = Props::new();
    for (key, value) in entries {
        props.insert(key, value.to_property().map_err(QueryError::runtime)?);
    }
    Ok(props)
}

fn ids_from(value: &Value, what: &str) -> Result<Vec<NodeId>, QueryError> {
    let Value::List(items) = value else {
        return Err(QueryError::runtime(format!("{what} needs a list of ids")));
    };
    items
        .iter()
        .map(|item| match item {
            Value::Str(text) => uuid_from_text(text)
                .ok_or_else(|| QueryError::runtime(format!("invalid id {text:?}"))),
            Value::Node(node) => Ok(node.id),
            other => Err(QueryError::runtime(format!(
                "{what} needs ids, got a {}",
                other.type_name()
            ))),
        })
        .collect()
}

fn embedding_from(value: &Value) -> Result<Vec<f32>, QueryError> {
    let Value::List(items) = value else {
        return Err(QueryError::runtime("@embedding needs a list of numbers"));
    };
    items
        .iter()
        .map(|item| match item {
            Value::Int(i) => Ok(*i as f32),
            Value::Float(f) => Ok(*f as f32),
            other => Err(QueryError::runtime(format!(
                "@embedding needs numbers, got a {}",
                other.type_name()
            ))),
        })
        .collect()
}

enum Target {
    Node(NodeId),
    Edge(EdgeId),
}

fn target_of(row: &Row, var: &str) -> Result<Option<Target>, QueryError> {
    match row.get(var) {
        Some(Value::Node(node)) => Ok(Some(Target::Node(node.id))),
        Some(Value::Edge(edge)) => Ok(Some(Target::Edge(edge.id))),
        Some(Value::Null) => Ok(None),
        Some(other) => Err(QueryError::runtime(format!(
            "`{var}` is a {}, not a node or edge",
            other.type_name()
        ))),
        None => Err(QueryError::runtime(format!("unknown variable `{var}`"))),
    }
}

impl Runner<'_> {
    /// Creates the parts of `pattern` that are not bound in `row`. MERGE
    /// (`undirected`) may create an undirected edge, from left to right.
    fn create(
        &mut self,
        row: &mut Row,
        pattern: &Pattern,
        undirected: bool,
    ) -> Result<(), QueryError> {
        let ns = self.ns;
        let mut ids = Vec::new();
        for node in pattern.nodes() {
            if let Some(var) = &node.var
                && let Some(bound) = row.get(var)
            {
                match bound {
                    Value::Node(existing) => {
                        ids.push(existing.id);
                        continue;
                    }
                    other => {
                        return Err(QueryError::runtime(format!(
                            "`{var}` is a {}, not a node",
                            other.type_name()
                        )));
                    }
                }
            }
            if node.labels.is_empty() {
                return Err(QueryError::runtime(
                    "CREATE and MERGE need at least one label for each new node",
                ));
            }
            let mut new = NewNode {
                labels: node.labels.clone(),
                ..NewNode::default()
            };
            let mut props = Vec::new();
            for (key, expr) in &node.props {
                let value = self.eval(row, expr)?;
                match key {
                    Key::Prop(name) => props.push((name.clone(), value)),
                    Key::Meta(name) => match name.as_str() {
                        "text" => match value {
                            Value::Str(text) => new.text = Some(text),
                            Value::Null => {}
                            other => {
                                return Err(QueryError::runtime(format!(
                                    "@text needs a string, got a {}",
                                    other.type_name()
                                )));
                            }
                        },
                        "embedding" if !value.is_null() => {
                            new.embedding = Some(embedding_from(&value)?)
                        }
                        "sources" if !value.is_null() => {
                            new.sources = ids_from(&value, "@sources")?
                        }
                        "embedding" | "sources" => {}
                        other => {
                            return Err(QueryError::runtime(format!(
                                "a new node can get @text, @embedding and @sources, not @{other}"
                            )));
                        }
                    },
                }
            }
            new.props = to_props(props)?;
            let tx = self.tx();
            let id = tx.create_node(ns, new)?;
            self.stats.nodes_created += 1;
            if let Some(var) = &node.var {
                let created = self.tx().node(ns, id)?.expect("just created");
                row.insert(var.clone(), Value::Node(Arc::new(created)));
            }
            ids.push(id);
        }

        for (index, (rel, _)) in pattern.steps.iter().enumerate() {
            let [label] = rel.labels.as_slice() else {
                return Err(QueryError::runtime("a new edge needs exactly one label"));
            };
            if rel.length.is_some() {
                return Err(QueryError::runtime(
                    "CREATE and MERGE cannot use variable-length edges",
                ));
            }
            // An undirected edge is created from left to right.
            let (from, to) = match rel.direction {
                Direction::Out | Direction::Both => (ids[index], ids[index + 1]),
                Direction::In => (ids[index + 1], ids[index]),
            };
            if rel.direction == Direction::Both && !undirected {
                return Err(QueryError::runtime(
                    "CREATE needs a direction for each edge (`->` or `<-`)",
                ));
            }
            if let Some(var) = &rel.var
                && row.contains_key(var)
            {
                return Err(QueryError::runtime(format!("`{var}` is already bound")));
            }
            let mut edge = NewEdge::new(from, label.clone(), to);
            let mut props = Vec::new();
            for (key, expr) in &rel.props {
                let value = self.eval(row, expr)?;
                match key {
                    Key::Prop(name) => props.push((name.clone(), value)),
                    Key::Meta(_) if value.is_null() => {}
                    Key::Meta(name) => match name.as_str() {
                        "valid_from" => edge.valid_from = Some(to_time(&value)?),
                        "valid_to" => edge.valid_to = Some(to_time(&value)?),
                        "sources" => edge.sources = ids_from(&value, "@sources")?,
                        other => {
                            return Err(QueryError::runtime(format!(
                                "a new edge can get @valid_from, @valid_to and @sources, not @{other}"
                            )));
                        }
                    },
                }
            }
            edge.props = to_props(props)?;
            let id = self.tx().create_edge(ns, edge)?;
            self.stats.edges_created += 1;
            for event in self.tx().take_constraint_events() {
                match event.action {
                    grafeno::Action::Flagged => self.stats.conflicts_flagged += 1,
                    _ => self.stats.facts_invalidated += 1,
                }
            }
            if let Some(var) = &rel.var {
                let created = self.tx().edge(ns, id)?.expect("just created");
                row.insert(var.clone(), Value::Edge(Arc::new(created)));
            }
        }
        Ok(())
    }

    /// Puts the current record of `var` back into the row after a write.
    fn refresh(&mut self, row: &mut Row, var: &str) -> Result<(), QueryError> {
        let ns = self.ns;
        let value = match target_of(row, var)? {
            Some(Target::Node(id)) => self
                .tx()
                .node(ns, id)?
                .map_or(Value::Null, |node| Value::Node(Arc::new(node))),
            Some(Target::Edge(id)) => self
                .tx()
                .edge(ns, id)?
                .map_or(Value::Null, |edge| Value::Edge(Arc::new(edge))),
            None => return Ok(()),
        };
        row.insert(var.to_owned(), value);
        Ok(())
    }

    fn set_props(&mut self, target: &Target, props: Props) -> Result<(), QueryError> {
        let ns = self.ns;
        match target {
            Target::Node(id) => self.tx().set_node_props(ns, *id, props)?,
            Target::Edge(id) => self.tx().set_edge_props(ns, *id, props)?,
        }
        Ok(())
    }

    fn remove_props(&mut self, target: &Target, keys: &[&str]) -> Result<(), QueryError> {
        let ns = self.ns;
        match target {
            Target::Node(id) => self.tx().remove_node_props(ns, *id, keys)?,
            Target::Edge(id) => self.tx().remove_edge_props(ns, *id, keys)?,
        }
        Ok(())
    }

    fn set(&mut self, row: &mut Row, item: &SetItem) -> Result<(), QueryError> {
        let ns = self.ns;
        let var = match item {
            SetItem::Property { var, .. }
            | SetItem::Merge { var, .. }
            | SetItem::Replace { var, .. }
            | SetItem::Labels { var, .. } => var,
        };
        let Some(target) = target_of(row, var)? else {
            return Ok(());
        };
        match item {
            SetItem::Property {
                key: Key::Prop(name),
                value,
                ..
            } => {
                let value = self.eval(row, value)?;
                self.set_props(&target, to_props([(name.clone(), value)])?)?;
                self.stats.properties_set += 1;
            }
            SetItem::Property {
                key: Key::Meta(name),
                value,
                ..
            } => {
                let value = self.eval(row, value)?;
                let tx = self.tx();
                match (&target, name.as_str(), &value) {
                    (Target::Node(id), "text", Value::Str(text)) => tx.set_text(ns, *id, text)?,
                    (Target::Node(id), "text", Value::Null) => {
                        tx.remove_text(ns, *id)?;
                    }
                    (Target::Node(id), "embedding", Value::Null) => {
                        tx.remove_embedding(ns, *id)?;
                    }
                    (Target::Node(id), "embedding", value) => {
                        tx.set_embedding(ns, *id, &embedding_from(value)?)?
                    }
                    (Target::Node(id), "importance", Value::Int(_) | Value::Float(_)) => {
                        let importance = match value {
                            Value::Int(i) => i as f32,
                            Value::Float(f) => f as f32,
                            _ => unreachable!(),
                        };
                        tx.set_importance(ns, *id, importance)?
                    }
                    (Target::Edge(id), "valid_to", value) => {
                        tx.invalidate_edge(ns, *id, to_time(value)?)?
                    }
                    (Target::Node(_), other, _) => {
                        return Err(QueryError::runtime(format!(
                            "SET can change @text, @embedding and @importance of a node, not @{other} (labels: SET n:Label)"
                        )));
                    }
                    (Target::Edge(_), other, _) => {
                        return Err(QueryError::runtime(format!(
                            "SET can change @valid_to of an edge (it invalidates the fact), not @{other}"
                        )));
                    }
                }
                self.stats.properties_set += 1;
            }
            SetItem::Merge { map, .. } | SetItem::Replace { map, .. } => {
                let Value::Map(entries) = self.eval(row, map)? else {
                    return Err(QueryError::runtime("SET with `+=` or `=` needs a map"));
                };
                if matches!(item, SetItem::Replace { .. }) {
                    // `=` replaces: the keys the map does not have go.
                    let current = match &target {
                        Target::Node(id) => self.tx().node(ns, *id)?.map(|n| n.props),
                        Target::Edge(id) => self.tx().edge(ns, *id)?.map(|e| e.props),
                    };
                    let gone: Vec<String> = current
                        .unwrap_or_default()
                        .keys()
                        .filter(|key| !entries.contains_key(*key))
                        .cloned()
                        .collect();
                    if !gone.is_empty() {
                        let gone: Vec<&str> = gone.iter().map(String::as_str).collect();
                        self.remove_props(&target, &gone)?;
                    }
                }
                self.stats.properties_set += entries.len() as u64;
                self.set_props(&target, to_props(entries)?)?;
            }
            SetItem::Labels { labels, .. } => {
                let Target::Node(id) = target else {
                    return Err(QueryError::runtime("only nodes have labels"));
                };
                for label in labels {
                    if !self
                        .tx()
                        .node(ns, id)?
                        .is_some_and(|node| node.has_label(label))
                    {
                        self.tx().add_label(ns, id, label)?;
                        self.stats.labels_added += 1;
                    }
                }
            }
        }
        self.refresh(row, var)
    }

    fn remove(&mut self, row: &mut Row, item: &RemoveItem) -> Result<(), QueryError> {
        let ns = self.ns;
        let var = match item {
            RemoveItem::Property { var, .. } | RemoveItem::Labels { var, .. } => var,
        };
        let Some(target) = target_of(row, var)? else {
            return Ok(());
        };
        match item {
            RemoveItem::Property {
                key: Key::Prop(name),
                ..
            } => {
                self.remove_props(&target, &[name.as_str()])?;
                self.stats.properties_set += 1;
            }
            RemoveItem::Property {
                key: Key::Meta(name),
                ..
            } => match (&target, name.as_str()) {
                (Target::Node(id), "text") => {
                    self.tx().remove_text(ns, *id)?;
                }
                (Target::Node(id), "embedding") => {
                    self.tx().remove_embedding(ns, *id)?;
                }
                (_, other) => {
                    return Err(QueryError::runtime(format!(
                        "REMOVE can drop @text and @embedding of a node, not @{other}"
                    )));
                }
            },
            RemoveItem::Labels { labels, .. } => {
                let Target::Node(id) = target else {
                    return Err(QueryError::runtime("only nodes have labels"));
                };
                for label in labels {
                    if self
                        .tx()
                        .node(ns, id)?
                        .is_some_and(|node| node.has_label(label))
                    {
                        self.tx().remove_label(ns, id, label)?;
                        self.stats.labels_removed += 1;
                    }
                }
            }
        }
        self.refresh(row, var)
    }

    fn delete(&mut self, row: &Row, expr: &Expr) -> Result<(), QueryError> {
        let ns = self.ns;
        let values = match self.eval(row, expr)? {
            Value::List(items) => items,
            other => vec![other],
        };
        for value in values {
            match value {
                Value::Null => {}
                Value::Node(node) => {
                    if self.deleted_nodes.insert(node.id) && self.tx().node(ns, node.id)?.is_some()
                    {
                        self.tx().delete_node(ns, node.id)?;
                        self.stats.nodes_deleted += 1;
                    }
                }
                Value::Edge(edge) => {
                    if self.deleted_edges.insert(edge.id) && self.tx().edge(ns, edge.id)?.is_some()
                    {
                        self.tx().delete_edge(ns, edge.id)?;
                        self.stats.edges_deleted += 1;
                    }
                }
                other => {
                    return Err(QueryError::runtime(format!(
                        "DELETE needs nodes or edges, got a {}",
                        other.type_name()
                    )));
                }
            }
        }
        Ok(())
    }

    fn retract(&mut self, row: &Row, expr: &Expr) -> Result<(), QueryError> {
        let ns = self.ns;
        let values = match self.eval(row, expr)? {
            Value::List(items) => items,
            other => vec![other],
        };
        for value in values {
            match value {
                Value::Null => {}
                Value::Edge(edge) => {
                    if self
                        .tx()
                        .edge(ns, edge.id)?
                        .is_some_and(|e| !e.is_retracted())
                    {
                        self.tx().retract_edge(ns, edge.id)?;
                        self.stats.edges_retracted += 1;
                    }
                }
                other => {
                    return Err(QueryError::runtime(format!(
                        "RETRACT needs edges, got a {}",
                        other.type_name()
                    )));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- projection

/// A projected expression and its column name.
type Item = (Expr, String);

/// The items of a RETURN or WITH (`*` expanded to the variables) and their
/// column names.
fn projection_items(
    projection: &Projection,
    variables: &[String],
) -> Result<(Vec<Item>, Vec<String>), QueryError> {
    let mut items: Vec<Item> = Vec::new();
    if projection.star {
        if variables.is_empty() && projection.items.is_empty() {
            return Err(QueryError::runtime("`*` needs variables in scope"));
        }
        items.extend(
            variables
                .iter()
                .map(|name| (Expr::Var(name.clone()), name.clone())),
        );
    }
    for item in &projection.items {
        items.push((item.expr.clone(), item.alias.clone()));
    }
    let columns: Vec<String> = items.iter().map(|(_, alias)| alias.clone()).collect();
    let mut seen_columns = HashSet::new();
    for column in &columns {
        if !seen_columns.insert(column) {
            return Err(QueryError::runtime(format!(
                "the column `{column}` appears twice: rename one with AS"
            )));
        }
    }
    Ok((items, columns))
}

/// Evaluates a RETURN or WITH: grouping when there are aggregates, then
/// DISTINCT, ORDER BY, SKIP and LIMIT.
fn project(
    ctx: &Context,
    rows: Vec<Row>,
    projection: &Projection,
    variables: &[String],
) -> Result<(Vec<String>, Vec<Vec<Value>>), QueryError> {
    project_folded(ctx, rows, projection, variables, None)
}

/// [`project`], with the groups already folded while the rows were matched
/// (`folded`), or folded here from `rows`.
fn project_folded(
    ctx: &Context,
    rows: Vec<Row>,
    projection: &Projection,
    variables: &[String],
    folded: Option<Vec<(Vec<Value>, Row)>>,
) -> Result<(Vec<String>, Vec<Vec<Value>>), QueryError> {
    let (items, columns) = projection_items(projection, variables)?;
    // Each output row keeps a row of variables (plus the aliases) for ORDER BY.
    let mut output: Vec<(Vec<Value>, Row)> = Vec::new();
    if let Some(folded) = folded {
        output = folded;
    } else if let Some(spec) = Spec::new(&items) {
        let mut groups = Groups::default();
        let mut rows = rows.into_iter().peekable();
        while rows.peek().is_some() {
            let batch: Vec<Row> = rows.by_ref().take(crate::aggregate::BATCH).collect();
            groups.fold(&spec, spec.columns(ctx, batch)?)?;
        }
        output = groups.finish(&spec, ctx)?;
    } else {
        for row in rows {
            let values = items
                .iter()
                .map(|(expr, _)| eval(ctx, expr, Scope::Row(&row)))
                .collect::<Result<Vec<_>, _>>()?;
            output.push((values, row));
        }
    }

    if projection.distinct {
        let mut seen = HashSet::new();
        output.retain(|(values, _)| seen.insert(values.iter().map(Value::key).collect::<Vec<_>>()));
    }

    if !projection.order.is_empty() {
        let mut keyed = order_keys(ctx, output, projection, &items, &columns)?;
        keyed.sort_by(|(a, _), (b, _)| compare_keys(a, b, &projection.order));
        output = keyed
            .into_iter()
            .map(|(_, values)| (values, Row::new()))
            .collect();
    }

    let (skip, limit) = skip_limit(ctx, projection)?;
    let rows = output
        .into_iter()
        .skip(skip)
        .take(limit)
        .map(|(values, _)| values)
        .collect();
    Ok((columns, rows))
}

/// An output row's ORDER BY keys, then its values.
pub(crate) type Keyed = (Vec<Value>, Vec<Value>);

/// Each output row's ORDER BY keys, with its values.
fn order_keys(
    ctx: &Context,
    output: Vec<(Vec<Value>, Row)>,
    projection: &Projection,
    items: &[Item],
    columns: &[String],
) -> Result<Vec<Keyed>, QueryError> {
    let mut keyed = Vec::with_capacity(output.len());
    for (values, mut source) in output {
        for (column, value) in columns.iter().zip(&values) {
            source.insert(column.clone(), value.clone());
        }
        let mut keys = Vec::with_capacity(projection.order.len());
        for (expr, _) in &projection.order {
            // An ORDER BY item that repeats a projected item (aggregates
            // included) uses its value.
            let key = match items.iter().position(|(item, _)| item == expr) {
                Some(at) => values[at].clone(),
                None => eval(ctx, expr, Scope::Row(&source))?,
            };
            keys.push(key);
        }
        keyed.push((keys, values));
    }
    Ok(keyed)
}

/// How two rows' ORDER BY keys compare.
fn compare_keys(a: &[Value], b: &[Value], order: &[(Expr, bool)]) -> std::cmp::Ordering {
    for ((left, right), (_, descending)) in a.iter().zip(b).zip(order) {
        let ordering = sort_order(left, right);
        let ordering = if *descending {
            ordering.reverse()
        } else {
            ordering
        };
        if !ordering.is_eq() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// A projection's SKIP (0 without one) and LIMIT (`usize::MAX`).
fn skip_limit(ctx: &Context, projection: &Projection) -> Result<(usize, usize), QueryError> {
    let empty = Row::new();
    let count = |expr: &Option<Expr>, what: &str| -> Result<Option<usize>, QueryError> {
        let Some(expr) = expr else { return Ok(None) };
        match eval(ctx, expr, Scope::Row(&empty))? {
            Value::Int(n) if n >= 0 => Ok(Some(n as usize)),
            other => Err(QueryError::runtime(format!(
                "{what} needs a non-negative integer, got {}",
                other.to_json()
            ))),
        }
    };
    Ok((
        count(&projection.skip, "SKIP")?.unwrap_or(0),
        count(&projection.limit, "LIMIT")?.unwrap_or(usize::MAX),
    ))
}

// ---------------------------------------------------------------- EXPLAIN

/// The plan as a table: one row per step.
/// Whether a plan must not show counts: they come from every record of the
/// namespace, and some are hidden from the caller (14.4).
fn hides(limits: &Limits, ns: &str) -> bool {
    limits.view.as_ref().is_some_and(|view| view.per_record(ns))
}

/// `details` without the counts of nodes the planner read in statistics
/// (`…, 1200 nodes)` and `…, ~40 nodes)` become `…)`).
fn without_counts(details: &str) -> String {
    let mut out = String::with_capacity(details.len());
    let mut rest = details;
    while let Some(at) = rest.find(", ") {
        let (before, after) = rest.split_at(at);
        out.push_str(before);
        let tail = &after[2..];
        let digits = tail.trim_start_matches('~');
        let number = digits.len()
            - digits
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .len();
        if number > 0 && digits[number..].starts_with(" nodes)") {
            rest = &digits[number + " nodes".len()..];
        } else {
            out.push_str(", ");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn explain(
    statement: &Statement,
    info: &PlanInfo,
    params: &Params,
    hide: bool,
) -> Result<QueryResult, QueryError> {
    let value_of = |expr: &Expr| constant(expr, Some(params));
    if let Some(schema) = &statement.schema {
        let details = match schema {
            Schema::Create { def, .. } => format!(
                "{} on {}, filled from the {} with the label{}{}",
                if def.required {
                    "key constraint"
                } else if def.unique {
                    "unique constraint"
                } else if def.fuzzy {
                    "fuzzy index"
                } else if def.fulltext.is_some() {
                    "full-text index"
                } else if def.vector.is_some() {
                    "vector index"
                } else {
                    "index"
                },
                def.describe(),
                if def.edges { "edges" } else { "nodes" },
                if def.labels().len() > 1 { "s" } else { "" },
                if def.unique {
                    ", refused if they break it"
                } else {
                    ""
                }
            ),
            Schema::Constrain { constraint, .. } => format!(
                "constraint on {}{}, checked against the current {}",
                if constraint.on_edges() { "[:" } else { ":" },
                constraint.label(),
                if constraint.on_edges() {
                    "facts"
                } else {
                    "nodes"
                }
            ),
            Schema::Drop { name, .. } => format!("drop {name}"),
            Schema::Show { .. } => "list the indexes".into(),
            Schema::ShowNamespaces => "list the namespaces".into(),
            Schema::Events(EventsOp::Show) => "list the event subscriptions".into(),
            Schema::Events(EventsOp::Create { name, .. }) => {
                format!("create the event subscription {name:?}")
            }
            Schema::Events(EventsOp::Drop { name, .. }) => {
                format!("drop the event subscription {name:?}")
            }
            Schema::Namespace(op) => match op {
                NamespaceOp::Create { name, .. } => format!("create the namespace {name:?}"),
                NamespaceOp::Alter { name, .. } => format!("change the comment of {name:?}"),
                NamespaceOp::Drop { name, .. } => {
                    format!("drop the namespace {name:?} and everything in it")
                }
            },
            Schema::Access(access) => access.describe(),
        };
        return Ok(QueryResult {
            columns: vec!["step".into(), "operation".into(), "details".into()],
            rows: vec![vec![
                Value::Int(1),
                Value::Str("SCHEMA".into()),
                Value::Str(details),
            ]],
            stats: WriteStats::default(),
            profile: None,
            trace: None,
        });
    }
    let (estimates, _) = crate::cost::estimate(statement, info, &value_of);
    let mut steps: Vec<(String, String)> = Vec::new();
    for (index, part) in statement.parts.iter().enumerate() {
        if index > 0 {
            let all = statement.union_all[index - 1];
            steps.push((
                if all { "UNION ALL" } else { "UNION" }.into(),
                if all {
                    "rows of both parts".into()
                } else {
                    "distinct rows of both parts".into()
                },
            ));
        }
        let mut bound = HashSet::new();
        for clause in &part.clauses {
            steps.push((
                clause.name().into(),
                describe(clause, &bound, info, &value_of),
            ));
            bound.extend(clause_variables(clause));
        }
    }
    Ok(QueryResult {
        columns: ["step", "operation", "details", "rows", "cost"]
            .map(String::from)
            .to_vec(),
        rows: steps
            .into_iter()
            .zip(estimates)
            .enumerate()
            .map(|(index, ((operation, details), estimate))| {
                let details = if hide {
                    without_counts(&details)
                } else {
                    details
                };
                vec![
                    Value::Int(index as i64 + 1),
                    Value::Str(operation),
                    Value::Str(details),
                    if hide {
                        Value::Null
                    } else {
                        Value::Float(estimate.rows.round())
                    },
                    if hide {
                        Value::Null
                    } else {
                        Value::Float(estimate.cost.round())
                    },
                ]
            })
            .collect(),
        stats: WriteStats::default(),
        profile: None,
        trace: None,
    })
}

/// A cost as people read it: `12 400`, `3.1e9`.
fn show_cost(cost: f64) -> String {
    if cost < 1e7 {
        format!("{cost:.0}")
    } else {
        format!("{cost:.1e}")
    }
}

/// Variables a clause binds, for planning the clauses after it.
fn clause_variables(clause: &Clause) -> Vec<String> {
    match clause {
        Clause::Match { patterns, .. } | Clause::Create(patterns) => patterns
            .iter()
            .flat_map(Pattern::variables)
            .map(str::to_owned)
            .collect(),
        Clause::Merge { pattern, .. } => {
            pattern.variables().into_iter().map(str::to_owned).collect()
        }
        Clause::Unwind { var, .. } => vec![var.clone()],
        Clause::With(projection) => projection
            .items
            .iter()
            .map(|item| item.alias.clone())
            .collect(),
        Clause::Call(call) => yielded(call)
            .map(|items| items.into_iter().map(|(_, alias)| alias).collect())
            .unwrap_or_default(),
        Clause::Subquery(query) => match query.clauses.last() {
            Some(Clause::Return(projection)) => projection
                .items
                .iter()
                .map(|item| item.alias.clone())
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn node_text(node: &NodePattern) -> String {
    let mut text = String::from("(");
    text.push_str(node.var.as_deref().unwrap_or(""));
    for label in &node.labels {
        text.push(':');
        text.push_str(label);
    }
    if !node.props.is_empty() {
        let keys: Vec<String> = node
            .props
            .iter()
            .map(|(key, _)| match key {
                Key::Prop(name) => name.clone(),
                Key::Meta(name) => format!("@{name}"),
            })
            .collect();
        text.push_str(&format!(" {{{}}}", keys.join(", ")));
    }
    text.push(')');
    text
}

fn rel_text(rel: &RelPattern, direction: Direction) -> String {
    let mut inner = rel.var.clone().unwrap_or_default();
    if !rel.labels.is_empty() {
        inner.push(':');
        inner.push_str(&rel.labels.join("|"));
    }
    if let Some((min, max)) = rel.length {
        inner.push_str(&match max {
            Some(max) => format!("*{min}..{max}"),
            None => format!("*{min}.."),
        });
    }
    let body = if inner.is_empty() {
        String::new()
    } else {
        format!("[{inner}]")
    };
    match direction {
        Direction::Out => format!("-{body}->"),
        Direction::In => format!("<-{body}-"),
        Direction::Both => format!("-{body}-"),
    }
}

/// How a pattern is matched: where it starts, then each expansion.
fn describe_pattern(
    pattern: &Pattern,
    bound: &HashSet<String>,
    conditions: &[&Expr],
    info: &PlanInfo,
    value_of: ValueOf,
) -> String {
    let nodes = pattern.nodes();
    let is_bound = |var: &str| bound.contains(var);
    let (anchor, source, _) = plan_anchor(pattern, &is_bound, conditions, info, value_of);
    let mut parts = vec![source.describe(&node_text(nodes[anchor]))];
    for step in anchor..pattern.steps.len() {
        let (rel, target) = &pattern.steps[step];
        parts.push(format!(
            "expand {}{}",
            rel_text(rel, rel.direction),
            node_text(target)
        ));
    }
    for step in (0..anchor).rev() {
        let (rel, _) = &pattern.steps[step];
        parts.push(format!(
            "expand {}{}",
            rel_text(rel, reverse(rel.direction)),
            node_text(nodes[step])
        ));
    }
    parts.join(", ")
}

fn describe(
    clause: &Clause,
    bound: &HashSet<String>,
    info: &PlanInfo,
    value_of: ValueOf,
) -> String {
    let projection_text = |projection: &Projection| {
        let mut parts = Vec::new();
        if projection.star {
            parts.push("*".to_owned());
        }
        parts.extend(projection.items.iter().map(|item| item.alias.clone()));
        let mut text = parts.join(", ");
        if projection
            .items
            .iter()
            .any(|item| item.expr.has_aggregate())
        {
            text.push_str("; aggregates");
        }
        if projection.distinct {
            text.push_str("; distinct");
        }
        if !projection.order.is_empty() {
            text.push_str("; sort");
        }
        if projection.skip.is_some() || projection.limit.is_some() {
            text.push_str("; skip/limit");
        }
        if projection.filter.is_some() {
            text.push_str("; filter");
        }
        text
    };
    match clause {
        Clause::Match {
            patterns,
            valid_at,
            known_at,
            filter,
            ..
        } => {
            let mut bound = bound.clone();
            let mut parts = Vec::new();
            let conditions: Vec<&Expr> = filter.as_ref().map(conjuncts).unwrap_or_default();
            let order = order_patterns(
                patterns,
                &|var| bound.contains(var),
                &conditions,
                info,
                value_of,
            );
            for index in order {
                let pattern = &patterns[index];
                parts.push(describe_pattern(
                    pattern,
                    &bound,
                    &conditions,
                    info,
                    value_of,
                ));
                bound.extend(pattern.variables().into_iter().map(str::to_owned));
            }
            let mut text = parts.join("; ");
            if valid_at.is_some() {
                text.push_str("; edges valid at a time");
            }
            if known_at.is_some() {
                text.push_str("; edges known at a time");
            }
            if filter.is_some() {
                text.push_str("; filter (WHERE)");
            }
            text
        }
        Clause::Unwind { var, .. } => format!("one row per item, as {var}"),
        Clause::With(projection) | Clause::Return(projection) => projection_text(projection),
        Clause::Call(call) => {
            let columns = yielded(call)
                .map(|items| {
                    items
                        .into_iter()
                        .map(|(_, alias)| alias)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let mut text = format!("{}() yields {columns}", call.name);
            if call.filter.is_some() {
                text.push_str("; filter (WHERE)");
            }
            text
        }
        Clause::Create(patterns) => format!("{} pattern(s)", patterns.len()),
        Clause::Merge {
            pattern,
            on_create,
            on_match,
        } => format!(
            "match or create: {}; {} ON CREATE, {} ON MATCH item(s)",
            describe_pattern(pattern, bound, &[], info, value_of),
            on_create.len(),
            on_match.len()
        ),
        Clause::Set(items) => format!("{} item(s)", items.len()),
        Clause::Remove(items) => format!("{} item(s)", items.len()),
        Clause::Delete(items) | Clause::Retract(items) => format!("{} value(s)", items.len()),
        Clause::Foreach { var, body, .. } => format!(
            "for each {var}: {}",
            body.iter().map(Clause::name).collect::<Vec<_>>().join(", ")
        ),
        Clause::Subquery(query) => format!(
            "once per row: {}",
            query
                .clauses
                .iter()
                .map(Clause::name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod hiding {
    #[test]
    fn plans_lose_their_counts() {
        assert_eq!(
            super::without_counts("start at n (scan of :Person, 1200 nodes)"),
            "start at n (scan of :Person)"
        );
        assert_eq!(
            super::without_counts("start at n (index i: n.k = …, ~40 nodes), then a, b"),
            "start at n (index i: n.k = …), then a, b"
        );
    }
}
