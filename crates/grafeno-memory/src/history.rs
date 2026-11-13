//! Knowledge over time (phase 10): what held at one time and not at
//! another, and how a memory was replaced, version after version, with the
//! reasons recorded for each change.
//!
//! Both read only, and both answer with records: every fact and version
//! names its id, so nothing here is more than what was stored.

use std::collections::{BTreeMap, HashSet};

use grafeno::{Direction, Edge, EdgeFilter, EdgeId, Node, NodeId, ReadTx, Result, Timestamp};
use serde::Serialize;

use crate::knowledge::{SUPERSEDES, SUPPORTED_BY};
use crate::palace::{ANCHORED_AT, HAS_LOCUS, HAS_ROOM};

/// The property of a `supersedes` fact that says why the newer memory
/// replaced the older one.
pub const REASON: &str = "reason";

/// A fact as one state shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StateFact {
    pub fact: EdgeId,
    pub from: NodeId,
    pub label: String,
    pub to: NodeId,
    /// The fact as a sentence.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<Timestamp>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<NodeId>,
}

/// One end and label whose other end changed between the two times: what
/// was `before` became `after`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Change {
    pub before: Vec<StateFact>,
    pub after: Vec<StateFact>,
}

/// What held at `then` and what holds at `at`, for the facts of some nodes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comparison {
    pub then: Timestamp,
    pub at: Timestamp,
    /// Facts true then and not at `at`, whose end and label kept no fact.
    pub ended: Vec<StateFact>,
    /// Facts true at `at` and not then, on an end and label that had none.
    pub began: Vec<StateFact>,
    /// An end and label whose facts were others then: a change of state,
    /// told apart from a fact that simply ended or began.
    pub changed: Vec<Change>,
    /// Facts true at both times.
    pub kept: usize,
}

fn holds(edge: &Edge, at: Timestamp) -> bool {
    edge.valid_from.is_none_or(|from| from <= at) && edge.valid_to.is_none_or(|to| to > at)
}

pub(crate) fn name(read: &ReadTx, ns: &str, id: NodeId) -> Result<String> {
    match read.node(ns, id)? {
        Some(node) => name_of(read, ns, &node),
        None => Ok(id.to_string()),
    }
}

/// A node's name, or the start of its text, quoted, when it has none.
pub(crate) fn name_of(read: &ReadTx, ns: &str, node: &Node) -> Result<String> {
    let id = node.id;
    if node.explicit_name().is_none()
        && let Some(text) = read.text(ns, id)?
    {
        let line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let mut short: String = line.chars().take(60).collect();
        if line.chars().count() > 60 {
            short.push('…');
        }
        return Ok(format!("“{short}”"));
    }
    Ok(node.display_name())
}

pub(crate) fn state_fact(read: &ReadTx, ns: &str, edge: &Edge) -> Result<StateFact> {
    Ok(StateFact {
        fact: edge.id,
        from: edge.from,
        label: edge.label.clone(),
        to: edge.to,
        text: format!(
            "{} {} {}",
            name(read, ns, edge.from)?,
            edge.label,
            name(read, ns, edge.to)?
        ),
        valid_from: edge.valid_from,
        valid_to: edge.valid_to,
        sources: edge.sources.clone(),
    })
}

/// The facts of `nodes` (those starting or ending at any of them) that held
/// at `then` against those that hold at `at`, by valid time. Facts taken
/// back as mistakes never held and are not counted.
///
/// A change is an end and a label (the facts leaving a node, the facts
/// arriving at it) with facts at both times but not the same ones, such as
/// a decision that chose one option then and another now. Edges that
/// arrange a palace are left out.
pub fn compare(
    read: &ReadTx,
    ns: &str,
    nodes: &[NodeId],
    then: Timestamp,
    at: Timestamp,
) -> Result<Comparison> {
    let scope: HashSet<NodeId> = nodes.iter().copied().collect();
    let mut seen = HashSet::new();
    // (node, outgoing, label) -> (facts then, facts at `at`).
    type Side = (NodeId, bool, String);
    let mut sides: BTreeMap<Side, (Vec<Edge>, Vec<Edge>)> = BTreeMap::new();
    for node in nodes {
        for neighbor in read.neighbors(ns, *node, Direction::Both, &EdgeFilter::any())? {
            let edge = neighbor.edge;
            if [ANCHORED_AT, HAS_ROOM, HAS_LOCUS].contains(&edge.label.as_str())
                || !seen.insert(edge.id)
            {
                continue;
            }
            // The side is the end in the scope; with both in it, the start.
            let side = if scope.contains(&edge.from) {
                (edge.from, true, edge.label.clone())
            } else {
                (edge.to, false, edge.label.clone())
            };
            let (before, after) = sides.entry(side).or_default();
            if holds(&edge, then) {
                before.push(edge.clone());
            }
            if holds(&edge, at) {
                after.push(edge);
            }
        }
    }
    let mut comparison = Comparison {
        then,
        at,
        ended: Vec::new(),
        began: Vec::new(),
        changed: Vec::new(),
        kept: 0,
    };
    for (before, after) in sides.into_values() {
        let before_ids: HashSet<EdgeId> = before.iter().map(|edge| edge.id).collect();
        let after_ids: HashSet<EdgeId> = after.iter().map(|edge| edge.id).collect();
        comparison.kept += before_ids.intersection(&after_ids).count();
        let gone: Vec<&Edge> = before
            .iter()
            .filter(|edge| !after_ids.contains(&edge.id))
            .collect();
        let new: Vec<&Edge> = after
            .iter()
            .filter(|edge| !before_ids.contains(&edge.id))
            .collect();
        match (gone.is_empty(), new.is_empty()) {
            (true, true) => {}
            (false, true) => {
                for edge in gone {
                    comparison.ended.push(state_fact(read, ns, edge)?);
                }
            }
            (true, false) => {
                for edge in new {
                    comparison.began.push(state_fact(read, ns, edge)?);
                }
            }
            (false, false) => comparison.changed.push(Change {
                before: gone
                    .into_iter()
                    .map(|edge| state_fact(read, ns, edge))
                    .collect::<Result<_>>()?,
                after: new
                    .into_iter()
                    .map(|edge| state_fact(read, ns, edge))
                    .collect::<Result<_>>()?,
            }),
        }
    }
    Ok(comparison)
}

/// One version of a memory that was replaced, or replaced another.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Version {
    pub id: NodeId,
    /// Its name, or the start of its text.
    pub text: String,
    pub recorded_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
    /// The `supersedes` fact by which this version replaced the one before
    /// it; none for the first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes_fact: Option<EdgeId>,
    /// Why it replaced the one before, as recorded on that fact
    /// ([`REASON`]); none when nothing was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What supports the change: the sources of that fact and what this
    /// version is `supported_by`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<NodeId>,
}

/// A memory's line of versions, oldest first; the last is the current one.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct History {
    pub current: NodeId,
    pub versions: Vec<Version>,
    /// Versions replaced by more than one newer memory: the line followed
    /// is the newest, and these others are named for an agent to settle.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<NodeId>,
}

/// Newest first, by when they were recorded.
fn newest(read: &ReadTx, ns: &str, ids: Vec<(EdgeId, NodeId)>) -> Result<Vec<(EdgeId, Node)>> {
    let mut nodes = Vec::new();
    for (edge, id) in ids {
        if let Some(node) = read.node(ns, id)? {
            nodes.push((edge, node));
        }
    }
    nodes.sort_by_key(|(_, node)| std::cmp::Reverse((node.created_at, node.id)));
    Ok(nodes)
}

/// The line of versions `node` belongs to, through `supersedes` facts
/// (newer `supersedes` older): from the first version to the current one,
/// each with the reason and evidence recorded for its change.
pub fn history(read: &ReadTx, ns: &str, node: NodeId) -> Result<History> {
    if read.node(ns, node)?.is_none() {
        return Err(grafeno::Error::NodeNotFound(node));
    }
    let replaces = EdgeFilter::any().label(SUPERSEDES);
    let mut branches = Vec::new();
    // Up to the current one: whatever supersedes it, the newest.
    let mut current = node;
    let mut seen = HashSet::from([node]);
    loop {
        let newer = newest(
            read,
            ns,
            read.neighbor_ids(ns, current, Direction::In, &replaces)?,
        )?;
        if newer.len() > 1 {
            branches.extend(newer[1..].iter().map(|(_, node)| node.id));
        }
        match newer.into_iter().next() {
            Some((_, next)) if seen.insert(next.id) => current = next.id,
            _ => break,
        }
    }
    // Down to the first one, from the current.
    let mut versions = Vec::new();
    let mut at = current;
    let mut walked = HashSet::from([current]);
    loop {
        let record = read.node(ns, at)?.ok_or(grafeno::Error::NodeNotFound(at))?;
        let older = newest(
            read,
            ns,
            read.neighbor_ids(ns, at, Direction::Out, &replaces)?,
        )?;
        let mut version = Version {
            id: at,
            text: name(read, ns, at)?,
            recorded_at: record.created_at,
            recorded_by: record.recorded_by.clone(),
            supersedes_fact: None,
            reason: None,
            evidence: read
                .neighbor_ids(
                    ns,
                    at,
                    Direction::Out,
                    &EdgeFilter::any().label(SUPPORTED_BY),
                )?
                .into_iter()
                .map(|(_, id)| id)
                .collect(),
        };
        let next = older.into_iter().next();
        if let Some((edge, _)) = &next
            && let Some(fact) = read.edge(ns, *edge)?
        {
            version.supersedes_fact = Some(fact.id);
            version.reason = fact
                .props
                .get(REASON)
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            for source in fact.sources {
                if !version.evidence.contains(&source) {
                    version.evidence.push(source);
                }
            }
        }
        versions.push(version);
        match next {
            Some((_, older)) if walked.insert(older.id) => at = older.id,
            _ => break,
        }
    }
    versions.reverse();
    branches.sort();
    branches.dedup();
    Ok(History {
        current,
        versions,
        branches,
    })
}
