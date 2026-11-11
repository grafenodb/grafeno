//! The syntax tree of a query.

use grafeno::Direction;

use crate::Value;

/// One statement: query parts joined by `UNION`, maybe under `EXPLAIN` or
/// `PROFILE`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Statement {
    pub(crate) mode: Mode,
    pub(crate) parts: Vec<Query>,
    /// `UNION ALL` (keep duplicates) or `UNION`; one entry per join.
    pub(crate) union_all: Vec<bool>,
    /// A schema command instead of a query (`parts` is then empty).
    pub(crate) schema: Option<Schema>,
}

/// Index and constraint commands.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Schema {
    /// `CREATE [FUZZY] INDEX`, or `CREATE CONSTRAINT … IS UNIQUE | KEY`
    /// (an index with `unique`, and `required` for a key). Its name is
    /// empty when not given.
    Create {
        def: grafeno::IndexDef,
        if_not_exists: bool,
    },
    /// `DROP INDEX` / `DROP CONSTRAINT`.
    Drop {
        name: String,
        constraint: bool,
        if_exists: bool,
    },
    /// `CREATE CONSTRAINT … REQUIRE ONE PER SOURCE`, `… IS FUNCTIONAL`,
    /// `… IS NOT NULL` or `… IS :: type` (the name is empty when not
    /// given).
    Constrain {
        constraint: grafeno::Constraint,
        if_not_exists: bool,
    },
    /// `SHOW INDEXES` / `SHOW CONSTRAINTS`.
    Show { constraints: bool },
    /// `SHOW NAMESPACES`.
    ShowNamespaces,
    /// `CREATE`, `ALTER` and `DROP NAMESPACE`.
    Namespace(NamespaceOp),
    /// Users, roles and grants (14.3).
    Access(crate::access::Access),
    /// Event subscriptions (16.10).
    Events(EventsOp),
}

/// `CREATE EVENT SUBSCRIPTION`, `DROP EVENT SUBSCRIPTION` and `SHOW EVENT
/// SUBSCRIPTIONS` (16.10).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EventsOp {
    Create {
        name: String,
        if_not_exists: bool,
        on: Vec<grafeno::EventTarget>,
        labels: Vec<String>,
        keys: Vec<String>,
        /// `WHERE key = value AND …`: values are literals or parameters.
        equal: Vec<(String, Expr)>,
        vector_values: bool,
        /// `DELIVER TO WEBHOOK url [SECRET secret]`; a stream when absent.
        webhook: Option<Box<(Expr, Option<Expr>)>>,
    },
    Drop {
        name: String,
        if_exists: bool,
    },
    Show,
}

/// A statement on a namespace as a whole (16.3).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NamespaceOp {
    /// `CREATE NAMESPACE name [IF NOT EXISTS] [COMMENT 'text']`
    Create {
        name: String,
        if_not_exists: bool,
        comment: Option<String>,
    },
    /// `ALTER NAMESPACE name SET COMMENT 'text' | REMOVE COMMENT`
    Alter {
        name: String,
        comment: Option<String>,
    },
    /// `DROP NAMESPACE name [IF EXISTS]`
    Drop { name: String, if_exists: bool },
}

impl NamespaceOp {
    pub(crate) fn name(&self) -> &str {
        match self {
            NamespaceOp::Create { name, .. }
            | NamespaceOp::Alter { name, .. }
            | NamespaceOp::Drop { name, .. } => name,
        }
    }

    /// What the caller needs on the namespace it names: as the HTTP routes.
    pub(crate) fn act(&self) -> grafeno::Act {
        match self {
            NamespaceOp::Create { .. } => grafeno::Act::CreateNamespace,
            NamespaceOp::Alter { .. } => grafeno::Act::Schema,
            NamespaceOp::Drop { .. } => grafeno::Act::Drop,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    Run,
    /// Describe the plan without running it.
    Explain,
    /// Run, and report rows and time per step.
    Profile,
    /// Run, and record the walk: anchors, edges followed, what each
    /// pattern and condition kept and dropped, rows (16.9).
    Trace,
}

impl Statement {
    pub(crate) fn writes(&self) -> bool {
        match &self.schema {
            Some(Schema::Show { .. } | Schema::ShowNamespaces | Schema::Events(EventsOp::Show)) => {
                false
            }
            Some(Schema::Access(access)) => access.writes(),
            Some(_) => true,
            None => self
                .parts
                .iter()
                .any(|part| part.clauses.iter().any(Clause::writes)),
        }
    }
}

/// A sequence of clauses: rows flow from each clause into the next.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Query {
    pub(crate) clauses: Vec<Clause>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Clause {
    Match {
        optional: bool,
        patterns: Vec<Pattern>,
        valid_at: Option<Expr>,
        known_at: Option<Expr>,
        filter: Option<Expr>,
    },
    /// `UNWIND list AS x`
    Unwind {
        list: Expr,
        var: String,
    },
    /// `WITH …`: a projection between steps.
    With(Projection),
    /// `CALL name(args) YIELD …`
    Call(ProcedureCall),
    Create(Vec<Pattern>),
    Merge {
        pattern: Pattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    Set(Vec<SetItem>),
    Remove(Vec<RemoveItem>),
    Delete(Vec<Expr>),
    Retract(Vec<Expr>),
    /// `FOREACH (x IN list | clauses)`: writes once per item.
    Foreach {
        var: String,
        list: Expr,
        body: Vec<Clause>,
    },
    Return(Projection),
    /// `CALL { … }`: the clauses run once per row, which sees the row's
    /// variables; what the inner RETURN gives is added to it (a subquery
    /// without RETURN keeps the rows as they were).
    Subquery(Query),
}

impl Clause {
    pub(crate) fn writes(&self) -> bool {
        matches!(
            self,
            Clause::Create(_)
                | Clause::Merge { .. }
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete(_)
                | Clause::Retract(_)
                | Clause::Foreach { .. }
        ) || matches!(self, Clause::Subquery(query) if query.clauses.iter().any(Clause::writes))
            || matches!(self, Clause::Call(call) if call.writes())
    }

    /// The keyword, for plans and error messages.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Clause::Match { optional: true, .. } => "OPTIONAL MATCH",
            Clause::Match { .. } => "MATCH",
            Clause::Unwind { .. } => "UNWIND",
            Clause::With(_) => "WITH",
            Clause::Call(_) => "CALL",
            Clause::Create(_) => "CREATE",
            Clause::Merge { .. } => "MERGE",
            Clause::Set(_) => "SET",
            Clause::Remove(_) => "REMOVE",
            Clause::Delete(_) => "DELETE",
            Clause::Retract(_) => "RETRACT",
            Clause::Foreach { .. } => "FOREACH",
            Clause::Return(_) => "RETURN",
            Clause::Subquery(_) => "CALL { }",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProcedureCall {
    /// Lowercase, with dots: `db.labels`.
    pub(crate) name: String,
    pub(crate) args: Vec<Expr>,
    /// `YIELD a, b AS c` (`None`: every column; `YIELD *` too).
    pub(crate) yields: Option<Vec<(String, String)>>,
    pub(crate) filter: Option<Expr>,
}

/// How an algorithm's rows are given back (16.8): as they are, summed up
/// per column, or written to the nodes' properties.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CallMode {
    Stream,
    Stats,
    /// Columns (`None`: the first one that is not a node) and the property
    /// each goes to.
    Write(Vec<(Option<String>, String)>),
}

impl ProcedureCall {
    /// The mode its settings ask for: they must be a literal map, so that
    /// whether the statement writes is known before it runs.
    pub(crate) fn mode(&self) -> CallMode {
        if !self.name.starts_with("algo.") {
            return CallMode::Stream;
        }
        let Some(Expr::Map(entries)) = self.args.last() else {
            return CallMode::Stream;
        };
        for (key, value) in entries {
            if key == "write" {
                match value {
                    Expr::Literal(Value::Str(property)) => {
                        return CallMode::Write(vec![(None, property.clone())]);
                    }
                    Expr::Map(columns) => {
                        let mut out = Vec::new();
                        for (column, property) in columns {
                            if let Expr::Literal(Value::Str(property)) = property {
                                out.push((Some(column.clone()), property.clone()));
                            }
                        }
                        return CallMode::Write(out);
                    }
                    _ => {}
                }
            }
        }
        if entries.iter().any(|(key, value)| {
            key == "mode" && *value == Expr::Literal(Value::Str("stats".into()))
        }) {
            return CallMode::Stats;
        }
        CallMode::Stream
    }

    pub(crate) fn writes(&self) -> bool {
        matches!(self.mode(), CallMode::Write(_))
    }
}

/// The columns of an algorithm run with `mode: 'stats'`.
pub(crate) const STATS_COLUMNS: [&str; 6] = ["column", "count", "min", "max", "mean", "sum"];
/// The columns of an algorithm run with `write: …`.
pub(crate) const WRITE_COLUMNS: [&str; 3] = ["property", "column", "nodes"];

/// `(a)-[r]->(b)-[s]-(c)`: a start node and the steps after it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pattern {
    pub(crate) start: NodePattern,
    pub(crate) steps: Vec<(RelPattern, NodePattern)>,
    /// `p = (…)`: the variable that holds the matched path.
    pub(crate) path: Option<String>,
    /// `shortestPath(…)` (`Some(false)`) or `allShortestPaths(…)`
    /// (`Some(true)`).
    pub(crate) shortest: Option<bool>,
}

impl Pattern {
    pub(crate) fn nodes(&self) -> Vec<&NodePattern> {
        std::iter::once(&self.start)
            .chain(self.steps.iter().map(|(_, node)| node))
            .collect()
    }

    /// Every variable the pattern names, in order (the path's last).
    pub(crate) fn variables(&self) -> Vec<&str> {
        std::iter::once(self.start.var.as_deref())
            .chain(
                self.steps
                    .iter()
                    .flat_map(|(rel, node)| [rel.var.as_deref(), node.var.as_deref()]),
            )
            .chain(std::iter::once(self.path.as_deref()))
            .flatten()
            .collect()
    }
}

/// A property key: a regular property, or a pseudo-property such as `@text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    Prop(String),
    Meta(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NodePattern {
    pub(crate) var: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) props: Vec<(Key, Expr)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelPattern {
    pub(crate) var: Option<String>,
    /// Any of these labels (`[:a|b]`); empty means any label.
    pub(crate) labels: Vec<String>,
    pub(crate) props: Vec<(Key, Expr)>,
    /// From the left node's point of view.
    pub(crate) direction: Direction,
    /// `*min..max` (`max` `None`: up to the engine's limit).
    pub(crate) length: Option<(u32, Option<u32>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SetItem {
    /// `n.key = value` (a `null` value removes the property).
    Property { var: String, key: Key, value: Expr },
    /// `n += {…}`
    Merge { var: String, map: Expr },
    /// `n = {…}`: replaces every property.
    Replace { var: String, map: Expr },
    /// `n:Label:Other`
    Labels { var: String, labels: Vec<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RemoveItem {
    Property { var: String, key: Key },
    Labels { var: String, labels: Vec<String> },
}

/// What `RETURN` and `WITH` share.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Projection {
    pub(crate) distinct: bool,
    /// `*`: every variable in scope, then `items`.
    pub(crate) star: bool,
    pub(crate) items: Vec<ReturnItem>,
    pub(crate) order: Vec<(Expr, bool)>,
    pub(crate) skip: Option<Expr>,
    pub(crate) limit: Option<Expr>,
    /// `WITH … WHERE`: filters the projected rows.
    pub(crate) filter: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReturnItem {
    pub(crate) expr: Expr,
    /// The column name: the `AS` alias, or the expression as written.
    pub(crate) alias: String,
    /// Whether the alias was written with `AS`.
    pub(crate) aliased: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinOp {
    Or,
    Xor,
    And,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    Contains,
    StartsWith,
    EndsWith,
    In,
    /// `=~`: the whole string matches the regular expression.
    Regex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnOp {
    Not,
    Neg,
}

/// `all`, `any`, `none` and `single`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Quantifier {
    All,
    Any,
    None,
    Single,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    Param(String),
    Var(String),
    Property(Box<Expr>, Key),
    Index(Box<Expr>, Box<Expr>),
    /// `list[from..to]` (either end may be missing).
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// `x IS NULL` (`negated`: `IS NOT NULL`).
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `n:Label:Other`
    HasLabels(Box<Expr>, Vec<String>),
    /// A function call; `name` is lowercase.
    Call {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
    },
    /// `count(*)`
    CountStar,
    /// `CASE [subject] WHEN … THEN … [ELSE …] END`
    Case {
        subject: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
    /// `[x IN list WHERE filter | map]`
    Comprehension {
        var: String,
        list: Box<Expr>,
        filter: Option<Box<Expr>>,
        map: Option<Box<Expr>>,
    },
    /// `all(x IN list WHERE predicate)` and the like.
    Quantified {
        quantifier: Quantifier,
        var: String,
        list: Box<Expr>,
        predicate: Box<Expr>,
    },
    /// `reduce(acc = init, x IN list | expr)`
    Reduce {
        acc: String,
        init: Box<Expr>,
        var: String,
        list: Box<Expr>,
        expr: Box<Expr>,
    },
    /// A pattern used as a condition: `(a)-[:knows]->(b)`, `exists(…)`,
    /// `EXISTS { MATCH … WHERE … }`. True when it matches at least once.
    Exists {
        patterns: Vec<Pattern>,
        filter: Option<Box<Expr>>,
    },
    /// `COUNT { MATCH … WHERE … }`: how many times the patterns match;
    /// `[(a)-->(b) WHERE … | b.name]`: a list with `map` for each match.
    PatternQuery {
        kind: PatternKind,
        patterns: Vec<Pattern>,
        filter: Option<Box<Expr>>,
        map: Option<Box<Expr>>,
    },
    /// `n {.name, .*, alias: expr, var}`: a map built from a node, an
    /// edge or a map.
    MapProjection {
        base: Box<Expr>,
        items: Vec<MapItem>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PatternKind {
    Count,
    Collect,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MapItem {
    /// `.key`
    Property(String),
    /// `.*`
    All,
    /// `var`: the variable under its own name.
    Var(String),
    /// `name: expr`
    Entry(String, Expr),
}

pub(crate) const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "collect",
    "stdev",
    "stdevp",
    "variance",
    "variancep",
    "median",
    "percentilecont",
    "percentiledisc",
    "mode",
];

impl Expr {
    pub(crate) fn has_aggregate(&self) -> bool {
        let mut found = false;
        self.visit(&mut |expr| {
            if matches!(expr, Expr::CountStar)
                || matches!(expr, Expr::Call { name, .. } if AGGREGATES.contains(&name.as_str()))
            {
                found = true;
            }
        });
        found
    }

    /// Calls `f` on this expression and every expression inside it (not
    /// inside pattern predicates, whose expressions run per match).
    pub(crate) fn visit(&self, f: &mut impl FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Literal(_)
            | Expr::Param(_)
            | Expr::Var(_)
            | Expr::CountStar
            | Expr::Exists { .. }
            | Expr::PatternQuery { .. } => {}
            Expr::MapProjection { base, items } => {
                base.visit(f);
                for item in items {
                    match item {
                        MapItem::Var(name) => f(&Expr::Var(name.clone())),
                        MapItem::Entry(_, value) => value.visit(f),
                        MapItem::Property(_) | MapItem::All => {}
                    }
                }
            }
            Expr::Property(inner, _) | Expr::Unary(_, inner) | Expr::HasLabels(inner, _) => {
                inner.visit(f)
            }
            Expr::IsNull { expr, .. } => expr.visit(f),
            Expr::Index(a, b) | Expr::Binary(_, a, b) => {
                a.visit(f);
                b.visit(f);
            }
            Expr::Slice(list, from, to) => {
                list.visit(f);
                for bound in [from, to].into_iter().flatten() {
                    bound.visit(f);
                }
            }
            Expr::List(items) => items.iter().for_each(|item| item.visit(f)),
            Expr::Map(entries) => entries.iter().for_each(|(_, value)| value.visit(f)),
            Expr::Call { args, .. } => args.iter().for_each(|arg| arg.visit(f)),
            Expr::Case {
                subject,
                branches,
                otherwise,
            } => {
                if let Some(subject) = subject {
                    subject.visit(f);
                }
                for (when, then) in branches {
                    when.visit(f);
                    then.visit(f);
                }
                if let Some(otherwise) = otherwise {
                    otherwise.visit(f);
                }
            }
            Expr::Comprehension {
                list, filter, map, ..
            } => {
                list.visit(f);
                for inner in [filter, map].into_iter().flatten() {
                    inner.visit(f);
                }
            }
            Expr::Quantified {
                list, predicate, ..
            } => {
                list.visit(f);
                predicate.visit(f);
            }
            Expr::Reduce {
                init, list, expr, ..
            } => {
                init.visit(f);
                list.visit(f);
                expr.visit(f);
            }
        }
    }
}
