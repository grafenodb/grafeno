//! Queries split across cluster members (phase 13.4).
//!
//! Every member holds all the data, so a read query whose first `MATCH`
//! starts from many nodes can be cut into parts: part `k` of `n` takes the
//! `k`th share of that start's candidates, in the order one server visits
//! them, and runs the clauses up to the final `RETURN` on them. Clauses
//! that work row by row (`MATCH`, `OPTIONAL MATCH`, `UNWIND`, a plain
//! `WITH`) give, part after part, exactly the rows one server would. The
//! `RETURN` is where the parts meet:
//!
//! - with aggregates, each part folds its rows into groups and returns
//!   them unfinished; the coordinator merges them in part order (12.3's
//!   accumulators made mergeable: integer sums in 128 bits, float sums
//!   exact, so neither depends on the order) and finishes them as one
//!   server would;
//! - without, each part returns its projected rows (only the distinct
//!   ones with `DISTINCT`, sorted by their `ORDER BY` keys, which it sends
//!   along, and only `SKIP + LIMIT` of them when that is enough); the
//!   coordinator puts them end to end, drops duplicates, sorts them
//!   stably, and applies `SKIP` and `LIMIT`.
//!
//! Every part plans with the coordinator's statistics ([`SplitPlan`]) and
//! reads a snapshot at the same log position (the caller sees to that), so
//! the parts make the same plan over the same data and the result equals
//! one server's, row order included. Queries of any other shape (writes,
//! `UNION`, a `WITH` that aggregates, sorts or limits, procedures,
//! nondeterministic functions) are not split.

use std::collections::HashMap;
use std::sync::Arc;

use grafeno::{EdgeFilter, IndexDef, PlannerStats, ReadTx};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

use super::{Flow, Runner, compare_keys, order_keys, projection_items, skip_limit};
use crate::QueryError;
use crate::aggregate::{Groups, Spec};
use crate::ast::{Clause, Mode, Projection, Query, Statement};
use crate::eval::{Params, Row, Scope, Slice, eval};
use crate::limits::Limits;
use crate::plan::PlanInfo;
use crate::value::Value;
use crate::{QueryResult, WriteStats};

/// What every part of a split query plans with: the coordinator's plan
/// information (counts, indexes, statistics) as exact JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SplitPlan {
    info: Json,
    /// The planner's estimated cost of the whole query.
    cost: f64,
}

impl SplitPlan {
    /// The planner's estimate of the query's cost (as `EXPLAIN` shows it):
    /// splitting pays off for costly queries only.
    pub fn cost(&self) -> f64 {
        self.cost
    }
}

/// What one part of a split query returns to the coordinator.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Partial {
    /// The variables bound before the RETURN, in order (for `RETURN *`).
    vars: Vec<String>,
    /// `{"groups": …}` with aggregates, `{"rows": [[values, keys]…]}`
    /// without.
    data: Json,
}

/// Function names whose value changes from call to call: a query that
/// uses them is not split (its parts would disagree).
const VOLATILE: [&str; 2] = ["rand", "randomuuid"];
/// Functions that read the clock when called without arguments.
const CLOCK: [&str; 6] = [
    "timestamp",
    "datetime",
    "date",
    "time",
    "localtime",
    "localdatetime",
];

/// Whether the text calls a volatile function or reads the clock. Looks at
/// the text, so a string that merely contains such a call counts too
/// (the query then runs unsplit, which is always correct).
fn volatile(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let calls = |name: &str, empty_only: bool| {
        let mut from = 0;
        while let Some(at) = lower[from..].find(name) {
            let start = from + at;
            let end = start + name.len();
            from = end;
            let before_ok = start == 0
                || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
            let rest = lower[end..].trim_start();
            if !before_ok || !rest.starts_with('(') {
                continue;
            }
            if !empty_only || rest[1..].trim_start().starts_with(')') {
                return true;
            }
        }
        false
    };
    VOLATILE.iter().any(|name| calls(name, false)) || CLOCK.iter().any(|name| calls(name, true))
}

/// The final `RETURN` of a query that can be split, or `None`.
fn splittable<'s>(statement: &'s Statement, text: &str) -> Option<&'s Projection> {
    if statement.mode != Mode::Run
        || statement.schema.is_some()
        || statement.writes()
        || statement.parts.len() != 1
        || volatile(text)
    {
        return None;
    }
    let clauses = &statement.parts[0].clauses;
    let (
        Some(Clause::Match {
            optional: false, ..
        }),
        Some(Clause::Return(projection)),
    ) = (clauses.first(), clauses.last())
    else {
        return None;
    };
    let row_by_row = clauses[1..clauses.len() - 1]
        .iter()
        .all(|clause| match clause {
            Clause::Match { .. } | Clause::Unwind { .. } => true,
            Clause::With(with) => {
                !with.distinct
                    && with.order.is_empty()
                    && with.skip.is_none()
                    && with.limit.is_none()
                    && !with.items.iter().any(|item| item.expr.has_aggregate())
            }
            _ => false,
        });
    row_by_row.then_some(projection)
}

fn info_to_wire(info: &PlanInfo) -> Json {
    json!({
        "indexes": info.indexes,
        "label_counts": info.label_counts,
        "edge_labels": info.edge_labels,
        "nodes": info.nodes,
        "edges": info.edges,
        "stats": info.stats.as_ref().map(|stats| stats.to_wire()),
    })
}

fn info_from_wire(json: &Json) -> Result<PlanInfo, QueryError> {
    let bad = || QueryError::runtime("the split plan is malformed");
    let field = |name: &str| json.get(name).cloned().ok_or_else(bad);
    let indexes: Vec<IndexDef> = serde_json::from_value(field("indexes")?).map_err(|_| bad())?;
    let label_counts: HashMap<String, u64> =
        serde_json::from_value(field("label_counts")?).map_err(|_| bad())?;
    let edge_labels: HashMap<String, u64> =
        serde_json::from_value(field("edge_labels")?).map_err(|_| bad())?;
    let stats = match json.get("stats") {
        None | Some(Json::Null) => None,
        Some(wire) => Some(Arc::new(PlannerStats::from_wire(wire).ok_or_else(bad)?)),
    };
    Ok(PlanInfo {
        indexes,
        label_counts,
        edge_labels,
        nodes: json["nodes"].as_u64().ok_or_else(bad)?,
        edges: json["edges"].as_u64().ok_or_else(bad)?,
        stats,
    })
}

/// Whether `query` can be split across members, and if so the plan every
/// part must use (from this snapshot's statistics).
pub fn split_plan(
    read: &ReadTx,
    ns: &str,
    query: &str,
    params: &Params,
) -> Result<Option<SplitPlan>, QueryError> {
    let statement = crate::cache::parsed(query)?;
    super::validate(&statement)?;
    if splittable(&statement, query).is_none() {
        return Ok(None);
    }
    let info = super::plan_info(read, ns)?;
    let value_of = |expr: &crate::ast::Expr| crate::plan::constant(expr, Some(params));
    let (_, cost) = crate::cost::estimate(&statement, &info, &value_of);
    Ok(Some(SplitPlan {
        info: info_to_wire(&info),
        cost,
    }))
}

/// Runs part `part` of `parts` of a split query on `read` (a snapshot at
/// the coordinator's position).
#[allow(clippy::too_many_arguments)]
pub fn execute_part(
    read: &ReadTx,
    ns: &str,
    query: &str,
    params: &Params,
    limits: &Limits,
    plan: &SplitPlan,
    part: usize,
    parts: usize,
) -> Result<Partial, QueryError> {
    if parts == 0 || part >= parts {
        return Err(QueryError::runtime(format!(
            "there is no part {part} of {parts}"
        )));
    }
    let statement = crate::cache::parsed(query)?;
    super::validate(&statement)?;
    let Some(projection) = splittable(&statement, query) else {
        return Err(QueryError::runtime("this query cannot be split"));
    };
    let info = info_from_wire(&plan.info)?;
    let mut runner = Runner::with_info(read, ns, params, limits, info);
    runner.slice = Some(Slice::new(part, parts));
    let query: &Query = &statement.parts[0];
    let last = query.clauses.len() - 1;
    let ret = &query.clauses[last];
    let mut flow = Flow {
        rows: vec![Row::new()],
        vars: Vec::new(),
    };
    let mut folded: Option<Groups> = None;
    for (index, clause) in query.clauses[..last].iter().enumerate() {
        let before_return = index + 1 == last;
        if before_return && let Some(spec) = runner.folding_spec(clause, Some(ret), &flow)? {
            let mut groups = Groups::default();
            flow = runner.apply_folding(clause, flow, None, Some((&spec, &mut groups)))?;
            folded = Some(groups);
            continue;
        }
        let cap = match clause {
            Clause::Match { .. } if before_return => runner.row_cap(projection)?,
            _ => None,
        };
        flow = runner.apply(clause, flow, cap)?;
        runner
            .budget
            .held(&flow.rows, |row| Box::new(row.values()))?;
    }
    if runner.slice.as_ref().and_then(Slice::take).is_some() {
        // The first match never asked for candidates: every part would
        // return everything.
        return Err(QueryError::runtime("this query's start cannot be split"));
    }
    let (items, columns) = projection_items(projection, &flow.vars)?;
    let edges = EdgeFilter::any();
    let ctx = runner.ctx(&edges);
    let data = if let Some(spec) = Spec::new(&items) {
        let groups = match folded {
            Some(groups) => groups,
            None => {
                let mut groups = Groups::default();
                let mut rows = flow.rows.into_iter().peekable();
                while rows.peek().is_some() {
                    let batch: Vec<Row> = rows.by_ref().take(crate::aggregate::BATCH).collect();
                    groups.fold(&spec, spec.columns(&ctx, batch)?)?;
                }
                groups
            }
        };
        json!({ "groups": groups.to_wire() })
    } else {
        let mut output: Vec<(Vec<Value>, Row)> = Vec::with_capacity(flow.rows.len());
        for row in flow.rows {
            let values = items
                .iter()
                .map(|(expr, _)| eval(&ctx, expr, Scope::Row(&row)))
                .collect::<Result<Vec<_>, _>>()?;
            output.push((values, row));
        }
        if projection.distinct {
            let mut seen = std::collections::HashSet::new();
            output.retain(|(values, _)| {
                seen.insert(values.iter().map(Value::key).collect::<Vec<_>>())
            });
        }
        let mut keyed = if projection.order.is_empty() {
            output
                .into_iter()
                .map(|(values, _)| (Vec::new(), values))
                .collect()
        } else {
            let mut keyed = order_keys(&ctx, output, projection, &items, &columns)?;
            keyed.sort_by(|(a, _), (b, _)| compare_keys(a, b, &projection.order));
            keyed
        };
        // Without DISTINCT, the first SKIP + LIMIT rows of each part hold
        // every row the result can keep.
        if !projection.distinct {
            let (skip, limit) = skip_limit(&ctx, projection)?;
            keyed.truncate(skip.saturating_add(limit));
        }
        let rows: Vec<Json> = keyed
            .into_iter()
            .map(|(keys, values)| {
                json!([
                    values.iter().map(Value::to_wire).collect::<Vec<_>>(),
                    keys.iter().map(Value::to_wire).collect::<Vec<_>>(),
                ])
            })
            .collect();
        json!({ "rows": rows })
    };
    Ok(Partial {
        vars: flow.vars,
        data,
    })
}

/// Merges the parts of a split query, given in part order, into the result
/// one server would return. `read` is a snapshot at the same position as
/// the parts' (the coordinator's).
pub fn merge_parts(
    read: &ReadTx,
    ns: &str,
    query: &str,
    params: &Params,
    limits: &Limits,
    plan: &SplitPlan,
    partials: Vec<Partial>,
) -> Result<QueryResult, QueryError> {
    let statement = crate::cache::parsed(query)?;
    super::validate(&statement)?;
    let Some(projection) = splittable(&statement, query) else {
        return Err(QueryError::runtime("this query cannot be split"));
    };
    let Some(vars) = partials.first().map(|p| p.vars.clone()) else {
        return Err(QueryError::runtime("a split query needs at least one part"));
    };
    if partials.iter().any(|p| p.vars != vars) {
        return Err(QueryError::runtime("the parts of a split query disagree"));
    }
    let info = info_from_wire(&plan.info)?;
    let runner = Runner::with_info(read, ns, params, limits, info);
    let edges = EdgeFilter::any();
    let ctx = runner.ctx(&edges);
    let (items, columns) = projection_items(projection, &vars)?;
    let bad = || QueryError::runtime("a part of a split query is malformed");
    let (columns, rows) = if let Some(spec) = Spec::new(&items) {
        let mut total = Groups::default();
        for partial in &partials {
            let groups = Groups::from_wire(&spec, &partial.data["groups"])?;
            total.merge(&spec, groups)?;
        }
        let output = total.finish(&spec, &ctx)?;
        super::project_folded(&ctx, Vec::new(), projection, &vars, Some(output))?
    } else {
        let mut keyed: Vec<super::Keyed> = Vec::new();
        for partial in &partials {
            for row in partial.data["rows"].as_array().ok_or_else(bad)? {
                let decode = |json: &Json| -> Result<Vec<Value>, QueryError> {
                    json.as_array()
                        .ok_or_else(bad)?
                        .iter()
                        .map(Value::from_wire)
                        .collect()
                };
                keyed.push((decode(&row[1])?, decode(&row[0])?));
            }
        }
        if projection.distinct {
            let mut seen = std::collections::HashSet::new();
            keyed.retain(|(_, values)| {
                seen.insert(values.iter().map(Value::key).collect::<Vec<_>>())
            });
        }
        if !projection.order.is_empty() {
            // Each part's rows are sorted; a stable sort of them end to end
            // keeps ties in the order one server found them.
            keyed.sort_by(|(a, _), (b, _)| compare_keys(a, b, &projection.order));
        }
        let (skip, limit) = skip_limit(&ctx, projection)?;
        let rows = keyed
            .into_iter()
            .skip(skip)
            .take(limit)
            .map(|(_, values)| values)
            .collect();
        (columns, rows)
    };
    Ok(QueryResult {
        columns,
        rows,
        stats: WriteStats::default(),
        profile: None,
        trace: None,
    })
}

#[cfg(test)]
mod tests {
    use super::volatile;

    #[test]
    fn finds_volatile_calls() {
        assert!(volatile("RETURN rand()"));
        assert!(volatile("RETURN randomUUID ( )"));
        assert!(volatile("RETURN datetime()"));
        assert!(!volatile("RETURN datetime('2024-01-01')"));
        assert!(!volatile("MATCH (n) RETURN n.date"));
        assert!(!volatile("RETURN operand(1)"));
    }
}
