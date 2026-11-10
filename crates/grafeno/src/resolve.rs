//! Entity resolution: deciding whether a record and a node (or two nodes)
//! are the same thing, by rules declared per label.
//!
//! A [`MatchRule`] says which properties identify (strong keys: equal
//! means the same, different means not), which properties compare and
//! how ([`Measure`]) and how much each weighs, and two thresholds: from
//! `same` on, a pair is the same thing; from `review` on, someone (a
//! person, an agent) should look. Shared neighbors can count as one more
//! field. Every score comes with its fields, so a decision can be
//! explained and checked.
//!
//! Candidates come from the indexes on the rule's properties: a fuzzy
//! index finds similar text, an exact index equal values, so a record is
//! compared with a few nodes, not with every node of the label.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use grafeno_core::{Error, Node, NodeId, Props, Result, Value};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::db::{ReadTx, WriteTx};
use crate::index::IndexRange;
use crate::store::{decode, encode, storage};
use crate::textsim::Measure;
use crate::traverse::{Direction, EdgeFilter};

/// `(ns, label) -> MatchRule` (JSON).
pub(crate) const MATCH_RULES: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("match_rules");

/// Nodes of a label compared at most when no index finds candidates.
pub const SCAN_LIMIT: usize = 20_000;

/// How one property compares.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldRule {
    pub key: String,
    #[serde(flatten)]
    pub measure: Measure,
    /// Its share of the score, against the other fields present in both.
    #[serde(default = "one")]
    pub weight: f64,
}

fn one() -> f64 {
    1.0
}

fn same_default() -> f64 {
    0.9
}

fn review_default() -> f64 {
    0.75
}

/// How to tell whether two nodes of a label are the same thing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchRule {
    pub label: String,
    /// Properties that identify: equal values decide "same", different
    /// values "different", whatever the other fields say.
    #[serde(default)]
    pub strong_keys: Vec<String>,
    #[serde(default)]
    pub fields: Vec<FieldRule>,
    /// The weight of shared neighbors (Jaccard of the neighbor sets) as one
    /// more field, when both sides have neighbors; 0 leaves the graph out.
    #[serde(default)]
    pub neighbors: f64,
    /// From this score on: the same thing.
    #[serde(default = "same_default")]
    pub same: f64,
    /// From this score on (below `same`): to review.
    #[serde(default = "review_default")]
    pub review: f64,
}

impl MatchRule {
    fn check(&self) -> Result<()> {
        crate::store::check_label(&self.label)?;
        if self.fields.is_empty() && self.strong_keys.is_empty() {
            return Err(Error::Invalid(
                "a match rule needs fields to compare or strong keys".into(),
            ));
        }
        let weights = self
            .fields
            .iter()
            .map(|field| field.weight)
            .chain([self.neighbors]);
        if weights.clone().any(|w| !w.is_finite() || w < 0.0) {
            return Err(Error::Invalid("weights are numbers from 0 up".into()));
        }
        if !(0.0..=1.0).contains(&self.review)
            || !(0.0..=1.0).contains(&self.same)
            || self.review > self.same
        {
            return Err(Error::Invalid(
                "thresholds go from 0 to 1, with review at most same".into(),
            ));
        }
        Ok(())
    }
}

/// What a comparison decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Same,
    Review,
    Different,
}

/// How one field compared.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FieldScore {
    pub key: String,
    #[serde(flatten)]
    pub measure: Measure,
    pub record: Value,
    pub node: Value,
    pub similarity: f64,
    pub weight: f64,
}

/// A node compared with a record, and why it scored what it did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Match {
    pub node: NodeId,
    pub score: f64,
    pub decision: Decision,
    /// The strong key that decided, if one did.
    pub decided_by: Option<String>,
    pub fields: Vec<FieldScore>,
    /// Shared neighbors (0 to 1), when the rule counts them.
    pub neighbors: Option<f64>,
}

/// What to look for: a label, properties, and the record's neighbors
/// (nodes it is linked to), if known.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    pub label: String,
    #[serde(default)]
    pub props: Props,
    #[serde(default)]
    pub neighbors: Vec<NodeId>,
}

/// A value as text for a measure: strings as they are, numbers and
/// booleans written out; lists and maps are not compared.
fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn texts(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().filter_map(as_text).collect(),
        other => as_text(other).into_iter().collect(),
    }
}

fn same_key(a: &Value, b: &Value) -> bool {
    let norm = |v: &Value| as_text(v).map(|s| s.trim().to_lowercase());
    a == b || (norm(a).is_some() && norm(a) == norm(b))
}

/// Scores `node` against the record, under `rule`.
pub fn score(
    rule: &MatchRule,
    record: &Props,
    record_neighbors: Option<&HashSet<NodeId>>,
    node: &Node,
    node_neighbors: Option<&HashSet<NodeId>>,
) -> Match {
    let decide = |score: f64| {
        if score >= rule.same {
            Decision::Same
        } else if score >= rule.review {
            Decision::Review
        } else {
            Decision::Different
        }
    };
    for key in &rule.strong_keys {
        if let (Some(a), Some(b)) = (record.get(key), node.props.get(key))
            && !a.is_null()
            && !b.is_null()
        {
            let same = same_key(a, b);
            return Match {
                node: node.id,
                score: if same { 1.0 } else { 0.0 },
                decision: if same {
                    Decision::Same
                } else {
                    Decision::Different
                },
                decided_by: Some(key.clone()),
                fields: Vec::new(),
                neighbors: None,
            };
        }
    }
    let mut fields = Vec::new();
    let (mut total, mut weights) = (0.0, 0.0);
    for field in &rule.fields {
        let (Some(a), Some(b)) = (record.get(&field.key), node.props.get(&field.key)) else {
            continue;
        };
        let (xs, ys) = (texts(a), texts(b));
        if xs.is_empty() || ys.is_empty() {
            continue;
        }
        // The best pair, for list properties such as aliases.
        let similarity = xs
            .iter()
            .flat_map(|x| ys.iter().map(move |y| field.measure.compare(x, y)))
            .fold(0.0, f64::max);
        total += similarity * field.weight;
        weights += field.weight;
        fields.push(FieldScore {
            key: field.key.clone(),
            measure: field.measure,
            record: a.clone(),
            node: b.clone(),
            similarity,
            weight: field.weight,
        });
    }
    let mut neighbors = None;
    if rule.neighbors > 0.0
        && let (Some(x), Some(y)) = (record_neighbors, node_neighbors)
        // No neighbors on one side says nothing either way.
        && !x.is_empty()
        && !y.is_empty()
    {
        let shared = x.intersection(y).count() as f64 / x.union(y).count() as f64;
        total += shared * rule.neighbors;
        weights += rule.neighbors;
        neighbors = Some(shared);
    }
    let score = if weights > 0.0 { total / weights } else { 0.0 };
    Match {
        node: node.id,
        score,
        decision: decide(score),
        decided_by: None,
        fields,
        neighbors,
    }
}

fn rules_in(
    table: &impl ReadableTable<(&'static str, &'static str), &'static [u8]>,
    ns: &str,
) -> Result<Vec<MatchRule>> {
    let mut out = Vec::new();
    for item in table.range((ns, "")..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns {
            break;
        }
        out.push(decode(value.value())?);
    }
    Ok(out)
}

impl ReadTx {
    /// The match rules of a namespace, by label.
    pub fn match_rules(&self, ns: &str) -> Result<Vec<MatchRule>> {
        rules_in(&self.match_rules, ns)
    }

    pub fn match_rule(&self, ns: &str, label: &str) -> Result<Option<MatchRule>> {
        self.match_rules
            .get((ns, label))
            .map_err(storage)?
            .map(|guard| decode(guard.value()))
            .transpose()
    }

    fn neighbor_set(&self, ns: &str, node: NodeId) -> Result<HashSet<NodeId>> {
        Ok(self
            .neighbor_ids(ns, node, Direction::Both, &EdgeFilter::any())?
            .into_iter()
            .map(|(_, other)| other)
            .collect())
    }

    /// Nodes that may be the record of `probe`, best first, each with its
    /// score, decision and fields, under the rule of `probe.label` (or
    /// `rule`, to try one without storing it). Candidates come from the
    /// indexes on the rule's keys: equal strong keys and field values
    /// (exact indexes) and similar texts (fuzzy indexes); without any, the
    /// first [`SCAN_LIMIT`] nodes of the label are compared. `exclude`
    /// leaves a node out (the probe itself, for deduplication).
    pub fn find_matches(
        &self,
        ns: &str,
        probe: &Probe,
        rule: Option<&MatchRule>,
        limit: usize,
        exclude: Option<NodeId>,
    ) -> Result<Vec<Match>> {
        let stored;
        let rule = match rule {
            Some(rule) => rule,
            None => {
                stored = self.match_rule(ns, &probe.label)?.ok_or_else(|| {
                    Error::Invalid(format!(
                        "no match rule for :{}; declare one first",
                        probe.label
                    ))
                })?;
                &stored
            }
        };
        let defs = self.index_defs(ns)?;
        let index = |key: &str, fuzzy: bool| {
            defs.iter().find(|def| {
                def.is_simple() && def.label == rule.label && def.key == key && def.fuzzy == fuzzy
            })
        };
        let mut candidates: BTreeSet<NodeId> = BTreeSet::new();
        let mut indexed = false;
        let keys = rule
            .strong_keys
            .iter()
            .chain(rule.fields.iter().map(|field| &field.key));
        for key in keys {
            let Some(value) = probe.props.get(key) else {
                continue;
            };
            if let Some(def) = index(key, false) {
                indexed = true;
                for v in match value {
                    Value::Array(items) => items.clone(),
                    other => vec![other.clone()],
                } {
                    candidates.extend(self.index_lookup(
                        ns,
                        &def.name,
                        &IndexRange::Equal(v),
                        1_000,
                    )?);
                }
            }
            if index(key, true).is_some() {
                indexed = true;
                for text in texts(value) {
                    for hit in self.fuzzy_search(
                        ns,
                        &rule.label,
                        key,
                        &text,
                        Measure::Dice { n: 3 },
                        limit.max(10) * 5,
                    )? {
                        candidates.insert(hit.node);
                    }
                }
            }
        }
        let nodes: Vec<Node> = if indexed {
            let mut nodes = Vec::new();
            for id in candidates {
                if let Some(node) = self.node(ns, id)?
                    && node.has_label(&rule.label)
                {
                    nodes.push(node);
                }
            }
            nodes
        } else {
            self.nodes_page(ns, Some(&rule.label), None, SCAN_LIMIT)?
        };
        let probe_neighbors: HashSet<NodeId> = probe.neighbors.iter().copied().collect();
        let mut out = Vec::new();
        for node in nodes {
            if Some(node.id) == exclude {
                continue;
            }
            let node_neighbors = if rule.neighbors > 0.0 {
                Some(self.neighbor_set(ns, node.id)?)
            } else {
                None
            };
            out.push(score(
                rule,
                &probe.props,
                Some(&probe_neighbors),
                &node,
                node_neighbors.as_ref(),
            ));
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        out.truncate(limit);
        Ok(out)
    }

    /// Pairs of nodes of `label` that the rule calls the same or to review,
    /// best first: each node is matched against the others, through the
    /// indexes. Pairs joined by a `distinct_from` edge are left out.
    pub fn match_pairs(&self, ns: &str, label: &str, limit: usize) -> Result<Vec<(NodeId, Match)>> {
        let rule = self.match_rule(ns, label)?.ok_or_else(|| {
            Error::Invalid(format!("no match rule for :{label}; declare one first"))
        })?;
        let mut pairs: BTreeMap<(NodeId, NodeId), Match> = BTreeMap::new();
        let mut after = None;
        loop {
            let page = self.nodes_page(ns, Some(label), after, 1_000)?;
            let Some(last) = page.last() else {
                break;
            };
            after = Some(last.id);
            for node in page {
                let probe = Probe {
                    label: label.to_owned(),
                    neighbors: if rule.neighbors > 0.0 {
                        self.neighbor_set(ns, node.id)?.into_iter().collect()
                    } else {
                        Vec::new()
                    },
                    props: node.props.clone(),
                };
                for found in self.find_matches(ns, &probe, Some(&rule), 10, Some(node.id))? {
                    if found.decision == Decision::Different {
                        continue;
                    }
                    let key = if node.id < found.node {
                        (node.id, found.node)
                    } else {
                        (found.node, node.id)
                    };
                    pairs.entry(key).or_insert(found);
                }
            }
        }
        let distinct = EdgeFilter::any().label("distinct_from");
        let mut out = Vec::new();
        for ((a, b), found) in pairs {
            let marked = self
                .neighbor_ids(ns, a, Direction::Both, &distinct)?
                .iter()
                .any(|(_, other)| *other == b);
            if !marked {
                let other = if found.node == a { b } else { a };
                out.push((other, found));
            }
        }
        out.sort_by(|x, y| y.1.score.total_cmp(&x.1.score));
        out.truncate(limit);
        Ok(out)
    }
}

impl WriteTx {
    /// Declares (or replaces) the match rule of a label.
    pub fn set_match_rule(&mut self, ns: &str, rule: &MatchRule) -> Result<()> {
        rule.check()?;
        self.note(crate::replication::Touch::MatchRule(
            ns.to_owned(),
            rule.label.clone(),
        ));
        crate::namespace::ensure(&self.tx, ns)?;
        self.tx
            .open_table(MATCH_RULES)
            .map_err(storage)?
            .insert((ns, rule.label.as_str()), encode(rule)?.as_slice())
            .map_err(storage)?;
        Ok(())
    }

    /// Removes the match rule of a label; whether there was one.
    pub fn delete_match_rule(&mut self, ns: &str, label: &str) -> Result<bool> {
        self.note(crate::replication::Touch::MatchRule(
            ns.to_owned(),
            label.to_owned(),
        ));
        Ok(self
            .tx
            .open_table(MATCH_RULES)
            .map_err(storage)?
            .remove((ns, label))
            .map_err(storage)?
            .is_some())
    }

    pub fn match_rules(&self, ns: &str) -> Result<Vec<MatchRule>> {
        rules_in(&self.tx.open_table(MATCH_RULES).map_err(storage)?, ns)
    }
}
