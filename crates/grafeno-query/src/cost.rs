//! The planner's estimate of what a statement costs, from the namespace's
//! counts: how many rows each step makes, and how much work (rows touched,
//! records read, items computed) it takes to make them.
//!
//! The estimate is rough by design. Starts come from the planner (index
//! lookups estimated from the planner's statistics and the values the
//! query gives), expansions from each edge label's average degree in the
//! direction followed; a condition is assumed to keep half the rows. It is
//! good for telling a query that touches thousands of rows from one that
//! touches billions, which is what `max_cost` needs.

use std::collections::HashSet;

use crate::ast::{Clause, Expr, Pattern, ProcedureCall, Projection, Query, Statement};
use crate::exec::MAX_PATH_LENGTH;
use crate::plan::{PlanInfo, ValueOf, conjuncts, order_patterns, plan_anchor};
use crate::value::Value;

/// Rows and work, estimated, after one step.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Estimate {
    pub(crate) rows: f64,
    pub(crate) cost: f64,
}

/// Far beyond any limit, and still finite.
const CAP: f64 = 1e18;

fn capped(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, CAP)
    } else {
        CAP
    }
}

/// The estimate after each clause of each part, in order (a `UNION` adds
/// a step of its own), and the total cost.
pub(crate) fn estimate(
    statement: &Statement,
    info: &PlanInfo,
    value_of: ValueOf,
) -> (Vec<Estimate>, f64) {
    let mut steps = Vec::new();
    let mut total = 0.0;
    let mut union_rows = 0.0;
    for (index, part) in statement.parts.iter().enumerate() {
        if index > 0 {
            steps.push(Estimate {
                rows: union_rows,
                cost: capped(total),
            });
        }
        let mut model = Model::new(info, value_of);
        let part_steps = model.query(part);
        union_rows = capped(union_rows + model.rows);
        total = capped(total + model.cost);
        let base = total - model.cost;
        steps.extend(part_steps.into_iter().map(|step| Estimate {
            rows: step.rows,
            cost: capped(base + step.cost),
        }));
    }
    (steps, total)
}

struct Model<'i> {
    info: &'i PlanInfo,
    value_of: ValueOf<'i>,
    rows: f64,
    cost: f64,
    bound: HashSet<String>,
}

impl<'i> Model<'i> {
    fn new(info: &'i PlanInfo, value_of: ValueOf<'i>) -> Self {
        Self {
            info,
            value_of,
            rows: 1.0,
            cost: 0.0,
            bound: HashSet::new(),
        }
    }

    fn nodes(&self) -> f64 {
        (self.info.nodes as f64).max(1.0)
    }

    fn label_share(&self, labels: &[String]) -> f64 {
        labels
            .iter()
            .map(|label| {
                let count = self.info.label_counts.get(label).copied().unwrap_or(0) as f64;
                (count / self.nodes()).clamp(1.0 / self.nodes(), 1.0)
            })
            .fold(1.0, f64::min)
    }

    fn add(&mut self, work: f64) {
        self.cost = capped(self.cost + work);
    }

    fn step(&self) -> Estimate {
        Estimate {
            rows: self.rows,
            cost: self.cost,
        }
    }

    fn query(&mut self, query: &Query) -> Vec<Estimate> {
        let mut steps = Vec::new();
        for clause in &query.clauses {
            self.clause(clause);
            steps.push(self.step());
        }
        steps
    }

    fn clause(&mut self, clause: &Clause) {
        match clause {
            Clause::Match {
                optional,
                patterns,
                filter,
                ..
            } => {
                let conditions: Vec<&Expr> = filter.as_ref().map(conjuncts).unwrap_or_default();
                let before = self.rows;
                let mut rows = self.rows;
                let bound = self.bound.clone();
                let order = order_patterns(
                    patterns,
                    &|var| bound.contains(var),
                    &conditions,
                    self.info,
                    self.value_of,
                );
                for index in order {
                    rows = self.pattern(&patterns[index], rows, &conditions);
                }
                if let Some(filter) = filter {
                    self.add(rows * self.expr(filter));
                    rows *= 0.5f64.powi(conditions.len().min(8) as i32);
                }
                self.rows = capped(if *optional { rows.max(before) } else { rows });
            }
            Clause::Unwind { list, var } => {
                let per_row = self.expr(list);
                self.add(self.rows * per_row);
                self.rows = capped(self.rows * list_length(list));
                self.add(self.rows);
                self.bound.insert(var.clone());
            }
            Clause::With(projection) | Clause::Return(projection) => {
                self.projection(projection);
            }
            Clause::Call(call) => self.call(call),
            Clause::Create(patterns) => {
                let writes: usize = patterns.iter().map(|p| 1 + 2 * p.steps.len()).sum();
                self.add(self.rows * writes as f64);
                for pattern in patterns {
                    self.bind(pattern);
                }
            }
            Clause::Merge { pattern, .. } => {
                // Each row looks the pattern up, then may write it.
                let lookup = self.pattern(pattern, 1.0, &[]);
                self.add(self.rows * (lookup + 1.0 + pattern.steps.len() as f64));
                self.bind(pattern);
            }
            Clause::Set(items) => self.add(self.rows * items.len() as f64),
            Clause::Remove(items) => self.add(self.rows * items.len() as f64),
            Clause::Delete(items) | Clause::Retract(items) => {
                self.add(self.rows * items.len() as f64)
            }
            Clause::Subquery(query) => {
                let mut inner = Model::new(self.info, self.value_of);
                inner.bound = self.bound.clone();
                for clause in &query.clauses {
                    inner.clause(clause);
                }
                self.add(self.rows * (1.0 + inner.cost));
                self.rows = capped(self.rows * inner.rows.max(1.0));
                self.bound.extend(inner.bound);
            }
            Clause::Foreach { var, list, body } => {
                let items = self.rows * list_length(list);
                let mut inner = Model::new(self.info, self.value_of);
                inner.rows = items;
                inner.bound = self.bound.clone();
                inner.bound.insert(var.clone());
                for clause in body {
                    inner.clause(clause);
                }
                self.add(items + inner.cost);
            }
        }
    }

    fn bind(&mut self, pattern: &Pattern) {
        self.bound
            .extend(pattern.variables().into_iter().map(str::to_owned));
    }

    /// Rows after matching `pattern` from `rows` rows; adds the work.
    fn pattern(&mut self, pattern: &Pattern, rows: f64, conditions: &[&Expr]) -> f64 {
        let nodes = pattern.nodes();
        let bound = self.bound.clone();
        let is_bound = |var: &str| bound.contains(var);
        let (anchor, source, _) =
            plan_anchor(pattern, &is_bound, conditions, self.info, self.value_of);
        let start = source.rows();
        let mut rows = capped(rows * start);
        self.add(rows);
        // Steps away from the anchor, both ways: each multiplies the rows
        // by the edges followed and keeps the share of targets that fit.
        let order = (anchor..pattern.steps.len())
            .map(|step| (step, step + 1, false))
            .chain((0..anchor).rev().map(|step| (step, step, true)));
        for (step, target, backwards) in order {
            let rel = &pattern.steps[step].0;
            let node = nodes[target];
            let direction = match (rel.direction, backwards) {
                (grafeno::Direction::Out, true) => grafeno::Direction::In,
                (grafeno::Direction::In, true) => grafeno::Direction::Out,
                (direction, _) => direction,
            };
            let degree = self.info.degree(&rel.labels, direction);
            let reach = match rel.length {
                None => degree,
                Some((min, max)) => {
                    let max = max.unwrap_or(MAX_PATH_LENGTH);
                    (min..=max)
                        .map(|hops| degree.powi(hops as i32))
                        .sum::<f64>()
                }
            };
            // Expanding reads every edge followed, then checks its target.
            self.add(rows * reach.max(1.0));
            rows = capped(rows * reach);
            let mut keep = self.label_share(&node.labels);
            if node.var.as_deref().is_some_and(|var| bound.contains(var)) {
                keep /= self.nodes();
            }
            keep *= 0.1f64.powi(node.props.len().min(8) as i32);
            rows = capped(rows * keep);
        }
        self.bind(pattern);
        rows
    }

    fn projection(&mut self, projection: &Projection) {
        let mut per_row = 1.0;
        for item in &projection.items {
            per_row += self.expr(&item.expr);
        }
        self.add(self.rows * per_row);
        let aggregates = projection
            .items
            .iter()
            .any(|item| item.expr.has_aggregate());
        let grouped = projection
            .items
            .iter()
            .any(|item| !item.expr.has_aggregate());
        if aggregates && !grouped {
            self.rows = 1.0;
        }
        if !projection.order.is_empty() && self.rows > 1.0 {
            self.add(self.rows * self.rows.log2());
        }
        if projection.filter.is_some() {
            self.rows *= 0.5;
        }
        if let Some(Expr::Literal(Value::Int(limit))) = &projection.limit {
            self.rows = self.rows.min((*limit).max(0) as f64);
        }
        self.bound = projection
            .items
            .iter()
            .map(|item| item.alias.clone())
            .chain(
                projection
                    .star
                    .then(|| self.bound.clone())
                    .into_iter()
                    .flatten(),
            )
            .collect();
    }

    fn call(&mut self, call: &ProcedureCall) {
        let nodes = self.info.nodes as f64;
        let graph = nodes + self.info.edges as f64;
        let (work, rows) = match call.name.to_lowercase().as_str() {
            // One search from every node.
            "algo.betweenness" | "algo.closeness" => (nodes * graph, nodes),
            // Pairs of nodes compared.
            "algo.nodesimilarity" | "algo.knn" => (nodes * nodes, nodes),
            // A few searches between two nodes.
            name if name.contains("path") || name == "algo.astar" || name == "algo.distances" => {
                (graph * 4.0, nodes.sqrt().max(1.0))
            }
            name if name.starts_with("algo.") => (graph * 20.0, nodes),
            "db.search" | "db.searchvector" | "db.searchindex" | "db.similar" => (100.0, 10.0),
            "db.searchvectors" => (1_000.0, 100.0),
            _ => (10.0, 10.0),
        };
        self.add(self.rows * work);
        self.rows = capped(self.rows * rows);
        if let Some(yields) = &call.yields {
            self.bound
                .extend(yields.iter().map(|(_, alias)| alias.clone()));
        }
    }

    /// The work of evaluating `expr` once: loops over lists multiply.
    fn expr(&self, expr: &Expr) -> f64 {
        let inner = |items: &mut dyn Iterator<Item = &Expr>| -> f64 {
            items.map(|item| self.expr(item)).sum::<f64>()
        };
        let cost = match expr {
            Expr::Comprehension {
                list, filter, map, ..
            } => {
                let body = 1.0 + inner(&mut filter.iter().chain(map.iter()).map(|b| &**b));
                self.expr(list) + list_length(list) * body
            }
            Expr::Quantified {
                list, predicate, ..
            } => self.expr(list) + list_length(list) * (1.0 + self.expr(predicate)),
            Expr::Reduce {
                init, list, expr, ..
            } => self.expr(init) + self.expr(list) + list_length(list) * (1.0 + self.expr(expr)),
            Expr::Exists { patterns, .. } | Expr::PatternQuery { patterns, .. } => {
                let mut model = Model::new(self.info, self.value_of);
                model.bound = self.bound.clone();
                let mut rows = 1.0;
                for pattern in patterns {
                    rows = model.pattern(pattern, rows, &[]);
                }
                model.cost
            }
            Expr::Call { name, args, .. } if name == "range" => {
                inner(&mut args.iter()) + list_length(expr)
            }
            other => 1.0 + inner(&mut children(other).into_iter()),
        };
        capped(cost)
    }
}

/// The expressions right inside `expr`.
fn children(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Literal(_)
        | Expr::Param(_)
        | Expr::Var(_)
        | Expr::CountStar
        | Expr::Exists { .. }
        | Expr::PatternQuery { .. } => Vec::new(),
        Expr::MapProjection { base, items } => std::iter::once(&**base)
            .chain(items.iter().filter_map(|item| match item {
                crate::ast::MapItem::Entry(_, value) => Some(value),
                _ => None,
            }))
            .collect(),
        Expr::Property(inner, _) | Expr::Unary(_, inner) | Expr::HasLabels(inner, _) => {
            vec![inner]
        }
        Expr::IsNull { expr, .. } => vec![expr],
        Expr::Index(a, b) | Expr::Binary(_, a, b) => vec![a, b],
        Expr::Slice(list, from, to) => std::iter::once(&**list)
            .chain(from.iter().map(|b| &**b))
            .chain(to.iter().map(|b| &**b))
            .collect(),
        Expr::List(items) => items.iter().collect(),
        Expr::Map(entries) => entries.iter().map(|(_, value)| value).collect(),
        Expr::Call { args, .. } => args.iter().collect(),
        Expr::Case {
            subject,
            branches,
            otherwise,
        } => subject
            .iter()
            .map(|b| &**b)
            .chain(branches.iter().flat_map(|(when, then)| [when, then]))
            .chain(otherwise.iter().map(|b| &**b))
            .collect(),
        Expr::Comprehension {
            list, filter, map, ..
        } => std::iter::once(&**list)
            .chain(filter.iter().map(|b| &**b))
            .chain(map.iter().map(|b| &**b))
            .collect(),
        Expr::Quantified {
            list, predicate, ..
        } => vec![list, predicate],
        Expr::Reduce {
            init, list, expr, ..
        } => vec![init, list, expr],
    }
}

/// How many items a list expression gives, when it can be told without
/// running it: a literal list, `range()` of literals, a comprehension over
/// one of those. Ten otherwise.
fn list_length(expr: &Expr) -> f64 {
    let int = |expr: &Expr| match expr {
        Expr::Literal(Value::Int(i)) => Some(*i as f64),
        Expr::Unary(crate::ast::UnOp::Neg, inner) => match &**inner {
            Expr::Literal(Value::Int(i)) => Some(-(*i as f64)),
            _ => None,
        },
        _ => None,
    };
    match expr {
        Expr::List(items) => items.len() as f64,
        Expr::Literal(Value::List(items)) => items.len() as f64,
        Expr::Literal(Value::Null) => 0.0,
        Expr::Call { name, args, .. } if name == "range" && (2..=3).contains(&args.len()) => {
            match (int(&args[0]), int(&args[1]), args.get(2).map(int)) {
                (Some(start), Some(end), None) => (end - start + 1.0).max(0.0),
                (Some(start), Some(end), Some(Some(step))) if step != 0.0 => {
                    ((end - start) / step + 1.0).floor().max(0.0)
                }
                _ => 10.0,
            }
        }
        Expr::Comprehension { list, filter, .. } => {
            list_length(list) * if filter.is_some() { 0.5 } else { 1.0 }
        }
        _ => 10.0,
    }
}
