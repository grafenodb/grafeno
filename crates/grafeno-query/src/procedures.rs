//! Built-in procedures for `CALL name(args) YIELD …`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::QueryError;
use crate::algorithms::{self, ALGORITHMS};
use crate::eval::{Context, to_time};
use crate::temporal::{self, Duration};
use crate::value::Value;

/// `temporal.series` stops at this many points.
const MAX_SERIES: usize = 10_000;

/// A procedure: its name, arguments, the columns it yields, and what it does.
pub(crate) struct Procedure {
    pub(crate) name: &'static str,
    pub(crate) args: &'static str,
    pub(crate) columns: &'static [&'static str],
    pub(crate) description: &'static str,
}

pub(crate) const PROCEDURES: &[Procedure] = &[
    Procedure {
        name: "db.labels",
        args: "",
        columns: &["label", "count"],
        description: "Node labels of the namespace and how many nodes have each.",
    },
    Procedure {
        name: "db.edgeLabels",
        args: "",
        columns: &["label", "count"],
        description: "Edge labels of the namespace and how many edges have each (also db.relationshipTypes).",
    },
    Procedure {
        name: "db.propertyKeys",
        args: "",
        columns: &["key"],
        description: "Property names used by nodes and edges (reads every record).",
    },
    Procedure {
        name: "db.indexes",
        args: "",
        columns: &["name", "label", "property", "unique", "entries", "fuzzy"],
        description: "Property indexes, fuzzy indexes and unique constraints (also SHOW INDEXES); a fuzzy index counts trigram entries.",
    },
    Procedure {
        name: "db.match",
        args: "label, properties, limit = 10",
        columns: &["node", "score", "decision", "fields"],
        description: "Nodes that may be the record described by the properties, by the label's match rule: score, decision (same, review, different) and each field's similarity.",
    },
    Procedure {
        name: "db.searchFuzzy",
        args: "label, property, text, limit = 10, measure = 'dice'",
        columns: &["node", "score"],
        description: "Nodes whose property looks like the text (typos, word order), through the fuzzy index on it; measure: dice, jaro_winkler, edit, tokens, …",
    },
    Procedure {
        name: "db.audit",
        args: "since = 0, limit = 1000",
        columns: &[
            "seq", "at", "kind", "account", "via", "address", "target", "outcome", "detail",
        ],
        description: "The audit log after `since` (14.5); needs the audit privilege.",
    },
    Procedure {
        name: "db.stats",
        args: "",
        columns: &["nodes", "edges", "texts", "vectors", "dimension"],
        description: "Counts of the namespace.",
    },
    Procedure {
        name: "db.analyze",
        args: "",
        columns: &[
            "kind",
            "name",
            "count",
            "distinct",
            "out_degree",
            "in_degree",
            "common",
        ],
        description: "Rebuilds the planner's statistics now and lists them: per index its entries, distinct values and most common values; per edge label its edges and average degree each way. They are otherwise rebuilt on their own when the data drifts.",
    },
    Procedure {
        name: "db.search",
        args: "text, limit = 10",
        columns: &["node", "score"],
        description: "Full-text search (BM25), best first.",
    },
    Procedure {
        name: "db.searchVector",
        args: "embedding, limit = 10",
        columns: &["node", "score"],
        description: "Nearest nodes by embedding (cosine similarity), best first.",
    },
    Procedure {
        name: "temporal.series",
        args: "start, end, step",
        columns: &["at"],
        description: "Times from start to end (included) every step (a duration such as 'P1M'), for counting what was valid at each one.",
    },
    Procedure {
        name: "dbms.procedures",
        args: "",
        columns: &["name", "arguments", "columns", "description"],
        description: "The procedures CALL can run.",
    },
];

/// Accepted alternative names.
fn canonical(name: &str) -> &str {
    match name {
        "db.relationshiptypes" => "db.edgelabels",
        other => other,
    }
}

pub(crate) fn find(name: &str) -> Result<&'static Procedure, QueryError> {
    let name = canonical(name);
    PROCEDURES
        .iter()
        .chain(ALGORITHMS)
        .find(|procedure| procedure.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            QueryError::runtime(format!(
                "unknown procedure {name}() (CALL dbms.procedures() lists them)"
            ))
        })
}

fn limit_arg(args: &[Value], at: usize) -> Result<usize, QueryError> {
    match args.get(at) {
        None | Some(Value::Null) => Ok(10),
        Some(Value::Int(n)) if *n > 0 => Ok(*n as usize),
        Some(other) => Err(QueryError::runtime(format!(
            "the limit must be a positive integer, got {}",
            other.to_json()
        ))),
    }
}

/// Runs a procedure: one row per result, in the order of its columns.
pub(crate) fn call(
    ctx: &Context,
    name: &str,
    args: &[Value],
) -> Result<Vec<Vec<Value>>, QueryError> {
    let procedure = find(name)?;
    if procedure.name.starts_with("algo.") {
        return algorithms::call(ctx, procedure, args);
    }
    let read = ctx.read;
    let ns = ctx.ns;
    // Under a view decided record by record (14.4), what the catalog
    // procedures count comes from the visible records.
    if ctx.per_record().is_some()
        && let Some(rows) = visible_catalog(ctx, procedure.name)?
    {
        return Ok(rows);
    }
    let counted = |counts: std::collections::BTreeMap<String, u64>| -> Vec<Vec<Value>> {
        counts
            .into_iter()
            .map(|(label, count)| vec![Value::Str(label), Value::Int(count as i64)])
            .collect()
    };
    Ok(match procedure.name {
        "db.audit" => {
            if !ctx.may_audit {
                return Err(QueryError {
                    forbidden: true,
                    ..QueryError::runtime("reading the audit log needs the audit privilege")
                });
            }
            let since = match args.first() {
                None | Some(Value::Null) => 0,
                Some(Value::Int(n)) if *n >= 0 => *n as u64,
                Some(other) => {
                    return Err(QueryError::runtime(format!(
                        "db.audit(since, limit): since is a sequence number, not {}",
                        other.to_json()
                    )));
                }
            };
            let limit = match args.get(1) {
                None | Some(Value::Null) => 1000,
                _ => limit_arg(args, 1)?,
            };
            read.audit(since, limit)?
                .events
                .into_iter()
                .map(|event| {
                    let text = |value: Option<String>| value.map_or(Value::Null, Value::Str);
                    vec![
                        Value::Int(event.seq as i64),
                        Value::Time(event.at),
                        Value::Str(event.kind),
                        text(event.account),
                        text(event.via),
                        text(event.address),
                        text(event.target),
                        Value::Str(
                            serde_json::to_value(event.outcome)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .unwrap_or_default(),
                        ),
                        Value::from_json(&event.detail),
                    ]
                })
                .collect()
        }
        "db.labels" => counted(read.stats(ns)?.node_labels),
        "db.edgeLabels" => counted(read.stats(ns)?.edge_labels),
        "db.propertyKeys" => {
            let mut keys = BTreeSet::new();
            for node in read.nodes(ns)? {
                keys.extend(node.props.keys().cloned());
            }
            for edge in read.edges(ns)? {
                keys.extend(edge.props.keys().cloned());
            }
            keys.into_iter().map(|key| vec![Value::Str(key)]).collect()
        }
        "db.indexes" => read
            .indexes(ns)?
            .into_iter()
            .map(|info| {
                vec![
                    Value::Str(info.def.name),
                    Value::Str(info.def.label),
                    Value::Str(info.def.key),
                    Value::Bool(info.def.unique),
                    Value::Int(info.entries as i64),
                ]
            })
            .collect(),
        "db.stats" => {
            let stats = read.stats(ns)?;
            vec![vec![
                Value::Int(stats.nodes as i64),
                Value::Int(stats.edges as i64),
                Value::Int(stats.texts as i64),
                Value::Int(stats.vectors as i64),
                stats
                    .vector_dimension
                    .map_or(Value::Null, |dimension| Value::Int(dimension as i64)),
            ]]
        }
        "db.analyze" => {
            let stats = read.analyze(ns)?;
            let mut rows = Vec::new();
            for index in &stats.indexes {
                rows.push(vec![
                    Value::Str("index".into()),
                    Value::Str(index.name.clone()),
                    Value::Int(index.entries as i64),
                    Value::Int(index.distinct as i64),
                    Value::Null,
                    Value::Null,
                    Value::List(
                        index
                            .common
                            .iter()
                            .take(10)
                            .map(|(value, count)| {
                                Value::List(vec![
                                    Value::from_json(value),
                                    Value::Int(*count as i64),
                                ])
                            })
                            .collect(),
                    ),
                ]);
            }
            for edge in &stats.edge_labels {
                rows.push(vec![
                    Value::Str("edge".into()),
                    Value::Str(edge.label.clone()),
                    Value::Int(edge.edges as i64),
                    Value::Null,
                    Value::Float(edge.out_degree()),
                    Value::Float(edge.in_degree()),
                    Value::Null,
                ]);
            }
            rows
        }
        "db.search" | "db.searchVector" => {
            let limit = limit_arg(args, 1)?;
            let found = if procedure.name == "db.search" {
                let Some(Value::Str(text)) = args.first() else {
                    return Err(QueryError::runtime("db.search() needs the text to search"));
                };
                ctx.visible_hits(limit, |ask| Ok(read.search_text(ns, text, ask)?))?
            } else {
                let Some(Value::List(items)) = args.first() else {
                    return Err(QueryError::runtime(
                        "db.searchVector() needs an embedding (a list of numbers)",
                    ));
                };
                let mut embedding = Vec::with_capacity(items.len());
                for item in items {
                    embedding.push(match item {
                        Value::Int(i) => *i as f32,
                        Value::Float(f) => *f as f32,
                        other => {
                            return Err(QueryError::runtime(format!(
                                "an embedding holds numbers, not a {}",
                                other.type_name()
                            )));
                        }
                    });
                }
                ctx.visible_hits(limit, |ask| Ok(read.search_vector(ns, &embedding, ask)?))?
            };
            found
                .into_iter()
                .map(|(node, score)| {
                    vec![Value::Node(Arc::new(node)), Value::Float(f64::from(score))]
                })
                .collect()
        }
        "db.match" => {
            let (Some(Value::Str(label)), Some(Value::Map(props))) = (args.first(), args.get(1))
            else {
                return Err(QueryError::runtime(
                    "db.match(label, properties, limit) needs a label and a map of properties",
                ));
            };
            let probe = grafeno::Probe {
                label: label.clone(),
                props: props
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_json()))
                    .collect(),
                neighbors: Vec::new(),
            };
            let limit = limit_arg(args, 2)?;
            refuse_resolution(ctx)?;
            let mut rows = Vec::new();
            for found in read.find_matches(ns, &probe, None, limit, None)? {
                let Some(node) = ctx.node(found.node)? else {
                    continue;
                };
                let decision = serde_json::to_value(found.decision)
                    .map_err(|e| QueryError::runtime(e.to_string()))?;
                let fields = serde_json::to_value(&found.fields)
                    .map_err(|e| QueryError::runtime(e.to_string()))?;
                rows.push(vec![
                    Value::Node(Arc::new(node)),
                    Value::Float(found.score),
                    Value::from_json(&decision),
                    Value::from_json(&fields),
                ]);
            }
            rows
        }
        "db.searchFuzzy" => {
            let text_at = |at: usize, what: &str| -> Result<String, QueryError> {
                match args.get(at) {
                    Some(Value::Str(s)) => Ok(s.clone()),
                    _ => Err(QueryError::runtime(format!(
                        "db.searchFuzzy(label, property, text, limit, measure) needs {what}"
                    ))),
                }
            };
            let (label, key, text) = (
                text_at(0, "a label")?,
                text_at(1, "a property")?,
                text_at(2, "the text to look for")?,
            );
            let limit = limit_arg(args, 3)?;
            let measure = match args.get(4) {
                None | Some(Value::Null) => grafeno::textsim::Measure::Dice { n: 3 },
                Some(Value::Str(name)) => {
                    grafeno::textsim::Measure::by_name(name).ok_or_else(|| {
                        QueryError::runtime(format!("db.searchFuzzy(): unknown measure {name:?}"))
                    })?
                }
                Some(other) => {
                    return Err(QueryError::runtime(format!(
                        "db.searchFuzzy(): the measure is a name, not a {}",
                        other.type_name()
                    )));
                }
            };
            // A property the caller may not read finds nothing.
            if ctx
                .per_record()
                .is_some_and(|view| view.property_hidden(ns, std::slice::from_ref(&label), &key))
            {
                return Ok(Vec::new());
            }
            let mut rows = Vec::new();
            let mut ask = limit;
            loop {
                let hits = read.fuzzy_search(ns, &label, &key, &text, measure, ask)?;
                let exhausted = hits.len() < ask;
                rows.clear();
                for hit in hits {
                    if let Some(node) = ctx.node(hit.node)? {
                        rows.push(vec![Value::Node(Arc::new(node)), Value::Float(hit.score)]);
                        if rows.len() == limit {
                            break;
                        }
                    }
                }
                if rows.len() == limit || exhausted || ctx.per_record().is_none() || ask >= 1 << 20
                {
                    break;
                }
                ask *= 4;
            }
            rows
        }
        "temporal.series" => {
            let [start, end, step] = args else {
                return Err(QueryError::runtime(
                    "temporal.series() takes a start, an end and a step (such as 'P1M')",
                ));
            };
            let (start, end) = (to_time(start)?, to_time(end)?);
            let step = match step {
                Value::Duration(d) => *d,
                Value::Str(text) => Duration::parse(text)?,
                other => {
                    return Err(QueryError::runtime(format!(
                        "temporal.series() needs a duration as step, not a {}",
                        other.type_name()
                    )));
                }
            };
            if step.approx_micros() <= 0.0 {
                return Err(QueryError::runtime(
                    "temporal.series() needs a positive step",
                ));
            }
            let mut rows = Vec::new();
            // Each point is start + n × step, so month steps stay on the
            // calendar (Jan 31, Feb 29, Mar 31, …).
            loop {
                let at = temporal::add(start, step.scale(rows.len() as f64)?)?;
                if at > end {
                    break;
                }
                if rows.len() == MAX_SERIES {
                    return Err(QueryError::runtime(format!(
                        "temporal.series() would give more than {MAX_SERIES} points: use a larger step"
                    )));
                }
                rows.push(vec![Value::Time(at)]);
            }
            rows
        }
        "dbms.procedures" => PROCEDURES
            .iter()
            .chain(ALGORITHMS)
            .map(|procedure| {
                vec![
                    Value::Str(procedure.name.into()),
                    Value::Str(procedure.args.into()),
                    Value::List(
                        procedure
                            .columns
                            .iter()
                            .map(|column| Value::Str((*column).into()))
                            .collect(),
                    ),
                    Value::Str(procedure.description.into()),
                ]
            })
            .collect(),
        other => unreachable!("procedure {other} without a body"),
    })
}

/// Entity resolution compares every property its rules name, hidden or not:
/// under a view decided record by record it would tell what the caller may
/// not read, so it is refused there (14.4).
fn refuse_resolution(ctx: &Context) -> Result<(), QueryError> {
    if ctx.per_record().is_some() {
        return Err(QueryError {
            forbidden: true,
            ..QueryError::runtime(
                "entity resolution compares properties the caller may not read here: it needs read on every record and property of the namespace",
            )
        });
    }
    Ok(())
}

/// `db.labels`, `db.edgeLabels`, `db.propertyKeys` and `db.stats` from the
/// records the caller sees.
fn visible_catalog(ctx: &Context, name: &str) -> Result<Option<Vec<Vec<Value>>>, QueryError> {
    if !matches!(
        name,
        "db.labels" | "db.edgeLabels" | "db.propertyKeys" | "db.stats"
    ) {
        return Ok(None);
    }
    let mut labels: BTreeMap<String, u64> = BTreeMap::new();
    let mut edge_labels: BTreeMap<String, u64> = BTreeMap::new();
    let mut keys = BTreeSet::new();
    let (mut nodes, mut edges) = (0u64, 0u64);
    let filter = grafeno::EdgeFilter::default();
    let mut after = None;
    loop {
        let page = ctx.nodes_page(None, after, 1024)?;
        let done = page.len() < 1024;
        after = page.last().map(|node| node.id);
        for node in page {
            nodes += 1;
            for label in &node.labels {
                *labels.entry(label.clone()).or_default() += 1;
            }
            keys.extend(node.props.keys().cloned());
            for (edge, _) in ctx.neighbor_ids(node.id, grafeno::Direction::Out, &filter)? {
                if let Some(edge) = ctx.edge(edge)? {
                    edges += 1;
                    *edge_labels.entry(edge.label.clone()).or_default() += 1;
                    keys.extend(edge.props.keys().cloned());
                }
            }
        }
        if done {
            break;
        }
    }
    let counted = |counts: BTreeMap<String, u64>| -> Vec<Vec<Value>> {
        counts
            .into_iter()
            .map(|(label, count)| vec![Value::Str(label), Value::Int(count as i64)])
            .collect()
    };
    Ok(Some(match name {
        "db.labels" => counted(labels),
        "db.edgeLabels" => counted(edge_labels),
        "db.propertyKeys" => keys.into_iter().map(|key| vec![Value::Str(key)]).collect(),
        _ => vec![vec![
            Value::Int(nodes as i64),
            Value::Int(edges as i64),
            Value::Null,
            Value::Null,
            Value::Null,
        ]],
    }))
}
