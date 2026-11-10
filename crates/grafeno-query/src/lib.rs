//! GQ, the Grafeno query language: graph patterns, filters, aggregates,
//! subqueries and writes, with Grafeno's time and memory features.
//!
//! ```text
//! MATCH (a:Person {name: 'Ana'})-[w:works_at]->(c:Company)
//! VALID AT '2024-06-01'
//! WHERE c.name STARTS WITH 'Ac'
//! RETURN a.name, c.name, w.@valid_from
//! ORDER BY c.name LIMIT 10
//! ```
//!
//! - **Reading**: `MATCH` and `OPTIONAL MATCH` patterns
//!   (`(n:Label {key: value})`, `-[r:a|b]->`, `<-`, `-`, `*min..max`),
//!   `VALID AT` / `KNOWN AT`, `WHERE` (with pattern predicates), `WITH`,
//!   `UNWIND`, `UNION [ALL]`, `CALL procedure() YIELD …`, and `RETURN` with
//!   `DISTINCT`, aggregates (`count`, `sum`, `avg`, `min`, `max`,
//!   `collect`), `ORDER BY`, `SKIP` and `LIMIT`.
//! - **Writing**: `CREATE`, `MERGE` (with `ON CREATE` / `ON MATCH`), `SET`,
//!   `REMOVE`, `DELETE` (deletes a node with its edges), `RETRACT` (marks an
//!   edge as a mistake, keeping its history) and `FOREACH`. A statement with
//!   writes runs in one transaction, and its later clauses see its writes.
//! - **Expressions**: `CASE`, list comprehensions, `all`/`any`/`none`/
//!   `single`, `reduce`, slices, `^`.
//! - **Statistics and time**: `stdev`, `variance`, `median`, percentiles,
//!   `mode`; durations (`duration('P1M')`, calendar arithmetic,
//!   `duration.between`), date parts (`t.year`), `date.truncate`, and
//!   `temporal.series` with `VALID AT` per row.
//! - **Graph algorithms**: `CALL algo.<name>(…, {settings})` for paths
//!   (Dijkstra, A*, Yen, all shortest paths), centrality (degree,
//!   PageRank, betweenness, closeness), communities (components, label
//!   propagation, Louvain, triangles), similarity, link prediction and
//!   FastRP embeddings, plus `algo.*` pair functions.
//! - `EXPLAIN` returns the plan; `PROFILE` adds rows and time per step.
//! - **Pseudo-properties** (`@name`) expose what is not a property: `@id`,
//!   `@labels`, `@text`, `@embedding`, `@sources`, `@created_at`, `@recorded_by` on nodes;
//!   `@id`, `@label`, `@from`, `@to`, `@valid_from`, `@valid_to`,
//!   `@recorded_at`, `@expired_at`, `@recorded_by` on edges.
//!
//! The full reference is in `docs/query-language.md`.

mod access;
mod aggregate;
mod algorithms;
mod ast;
mod cache;
mod cost;
mod eval;
mod exec;
mod lexer;
mod limits;
mod morefns;
mod parallel;
mod parser;
mod plan;
mod procedures;
mod temporal;
mod textfns;
mod traverse;
mod value;

use std::fmt;

pub use access::{AccessContext, GrantRequest, PasswordHasher, manage_grants};
pub use cache::{
    STATEMENT_CACHE, StatementCacheStats, clear_statement_cache, statement_cache_stats,
};
pub use eval::Params;
pub use exec::split::{Partial, SplitPlan, execute_part, merge_parts, split_plan};
pub use exec::{
    MAX_PATH_LENGTH, PlanStep, QueryResult, Requirements, StatementKind, WriteStats, execute,
    execute_in, execute_read, execute_read_with, execute_with, is_write, requirements,
    statement_kind,
};
pub use limits::{Limits, NamespaceFilter};
pub use parallel::{MIN_CANDIDATES, parallelism, set_parallelism};
pub use temporal::Duration;
pub use value::Value;

/// A query that could not be parsed or run. Syntax errors carry the line
/// and column (1-based) where they were found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// The engine failed (storage or serialization), not the query.
    pub internal: bool,
    /// The query went over one of its [`Limits`] (time, reads, rows) or
    /// was cancelled.
    pub limit: bool,
    /// The writer may not change what the query touched (an `own_only`
    /// principal and someone else's record).
    pub forbidden: bool,
}

impl QueryError {
    pub(crate) fn at(source: &str, offset: usize, message: impl Into<String>) -> Self {
        let before = &source[..offset.min(source.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        Self {
            message: message.into(),
            line: Some(line),
            column: Some(column),
            internal: false,
            limit: false,
            forbidden: false,
        }
    }

    pub(crate) fn runtime(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            line: None,
            column: None,
            internal: false,
            limit: false,
            forbidden: false,
        }
    }

    pub(crate) fn limit(message: impl Into<String>) -> Self {
        Self {
            limit: true,
            ..Self::runtime(message)
        }
    }
}

impl From<grafeno::Error> for QueryError {
    fn from(error: grafeno::Error) -> Self {
        let internal = matches!(
            error,
            grafeno::Error::Storage(_) | grafeno::Error::Serialization(_)
        );
        Self {
            internal,
            forbidden: matches!(error, grafeno::Error::Forbidden(_)),
            ..Self::runtime(error.to_string())
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(column)) => {
                write!(f, "{} (line {line}, column {column})", self.message)
            }
            _ => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for QueryError {}

/// A graph algorithm `CALL algo.<name>` can run: its name (as written in
/// queries), arguments and the columns it yields.
#[derive(Clone, Copy, Debug)]
pub struct Algorithm {
    pub name: &'static str,
    pub args: &'static str,
    pub columns: &'static [&'static str],
    pub description: &'static str,
}

/// Every graph algorithm.
pub fn algorithms() -> impl Iterator<Item = Algorithm> {
    algorithms::ALGORITHMS.iter().map(|procedure| Algorithm {
        name: procedure.name,
        args: procedure.args,
        columns: procedure.columns,
        description: procedure.description,
    })
}

/// Parameters from a JSON object (`{"name": "Ana"}` → `$name`).
pub fn params_from_json(object: &serde_json::Map<String, serde_json::Value>) -> Params {
    object
        .iter()
        .map(|(key, value)| (key.clone(), Value::from_json(value)))
        .collect()
}
