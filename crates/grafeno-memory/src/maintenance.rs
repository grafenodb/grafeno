//! Keeping a memory healthy: duplicate entities, weak memories, and groups
//! of memories to consolidate.
//!
//! Grafeno finds the candidates and applies the changes; deciding what is a
//! duplicate and writing summaries is the agent's (or a person's) job, so the
//! engine stays independent of any language model.

use std::collections::{BTreeSet, HashMap, HashSet};

use grafeno::{
    Conflict, DEFAULT_STRONG_KEYS, Direction, EdgeFilter, MemoryState, NewEdge, NewNode, Node,
    NodeId, ReadTx, Result, Timestamp, Value, WriteTx, names_of, node_conflicts,
};
use serde::Serialize;

use crate::recall::ARCHIVED;

/// The edge label that marks two nodes as known to be different: duplicate
/// detection does not propose them again.
pub const DISTINCT_FROM: &str = "distinct_from";

/// Words shared by more nodes than this are too common to suggest duplicates.
const COMMON_WORD: usize = 50;

#[derive(Clone, Debug)]
pub struct DuplicateOptions {
    /// Only nodes with this label (both nodes of a pair share a label anyway).
    pub label: Option<String>,
    /// Pairs scoring at least this, from 0 to 1.
    pub min_score: f32,
    /// Also compare embeddings, with this cosine similarity at least.
    pub vector_threshold: Option<f32>,
    pub limit: usize,
    /// Properties that identify an entity, besides the keys of unique
    /// constraints: equal values make a duplicate, different values rule
    /// one out (namesakes).
    pub strong_keys: Vec<String>,
}

impl Default for DuplicateOptions {
    fn default() -> Self {
        Self {
            label: None,
            min_score: 0.6,
            vector_threshold: Some(0.92),
            limit: 50,
            strong_keys: DEFAULT_STRONG_KEYS.map(String::from).to_vec(),
        }
    }
}

/// Two nodes that may be the same entity.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Duplicate {
    pub a: Node,
    pub b: Node,
    /// From 0 to 1: 1 for the same name.
    pub score: f32,
    pub reasons: Vec<String>,
    /// Facts the two disagree on (a merge needs `force` to override them).
    pub conflicts: Vec<Conflict>,
}

fn words(name: &str) -> BTreeSet<&str> {
    name.split(' ').filter(|word| !word.is_empty()).collect()
}

/// Whether two different words are likely one with a typo: letters only,
/// at least four, one edit apart (two from eight letters on). A different
/// or added last letter is not a typo but another name ("mario" and
/// "maria", "luis" and "luisa").
fn typo(a: &str, b: &str) -> bool {
    let letters = |w: &str| w.chars().count() >= 4 && w.chars().all(char::is_alphabetic);
    if a == b || !letters(a) || !letters(b) {
        return false;
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (short, long) = if ca.len() <= cb.len() {
        (&ca, &cb)
    } else {
        (&cb, &ca)
    };
    if short.len() < 8 {
        let ending = if short.len() == long.len() {
            short[..short.len() - 1] == long[..long.len() - 1]
        } else {
            long.starts_with(short)
        };
        if ending {
            return false;
        }
    }
    let max = if a.chars().count().min(b.chars().count()) >= 8 {
        2
    } else {
        1
    };
    grafeno::textsim::damerau_within(a, b, max).is_some()
}

/// The keys under which a word is filed to find typos of it: the word, and
/// the word without each one of its letters (two words one edit apart share
/// one of these).
fn typo_keys(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut keys = vec![word.to_owned()];
    if chars.len() >= 4 && chars.iter().all(|c| c.is_alphabetic()) {
        for skip in 0..chars.len() {
            keys.push(
                chars
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| *at != skip)
                    .map(|(_, c)| *c)
                    .collect(),
            );
        }
    }
    keys
}

/// How alike two sets of normalized names are, and why.
fn name_similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> Option<(f32, String)> {
    if let Some(name) = a.intersection(b).next() {
        return Some((1.0, format!("same name \"{name}\"")));
    }
    let mut best: Option<(f32, String)> = None;
    for x in a {
        for y in b {
            let (wx, wy) = (words(x), words(y));
            let shared = wx.intersection(&wy).count();
            // Words of one name that are a typo of a word of the other.
            let typos = wx
                .difference(&wy)
                .filter(|w| wy.difference(&wx).any(|v| typo(w, v)))
                .count();
            if shared == 0 && typos == 0 {
                continue;
            }
            let union = wx.union(&wy).count() - typos;
            let jaccard = (shared as f32 + 0.8 * typos as f32) / union as f32;
            // "ana garcia" and "ana garcia lopez": one name inside the other.
            let contained = typos == 0 && (wx.is_subset(&wy) || wy.is_subset(&wx));
            let score = if contained { jaccard.max(0.7) } else { jaccard };
            let reason = if contained {
                format!("\"{x}\" and \"{y}\": one name contains the other")
            } else if typos > 0 {
                format!("\"{x}\" and \"{y}\": alike but for a typo")
            } else {
                format!("\"{x}\" and \"{y}\" share {shared} word(s)")
            };
            if best.as_ref().is_none_or(|(top, _)| score > *top) {
                best = Some((score, reason));
            }
        }
    }
    best
}

fn shares_label(a: &Node, b: &Node) -> bool {
    a.labels
        .iter()
        .any(|label| label != ARCHIVED && b.has_label(label))
}

/// Pairs of nodes that may be the same entity, best first: by a strong key
/// (the same email), by name (equal, one inside the other, shared words,
/// typos) and, with `vector_threshold`, by meaning.
///
/// Names only suggest; identity rules decide against: pairs with
/// different values of a strong key (namesakes), with labels that do not
/// fit (each has one the other lacks, as a person and a company), or joined
/// by a [`DISTINCT_FROM`] edge are left out. Facts they disagree on lower
/// the score and are listed in `conflicts`.
pub fn find_duplicates(
    read: &ReadTx,
    ns: &str,
    options: &DuplicateOptions,
) -> Result<Vec<Duplicate>> {
    let nodes: Vec<Node> = read
        .nodes_page(ns, options.label.as_deref(), None, usize::MAX)?
        .into_iter()
        .filter(|node| !node.has_label(ARCHIVED))
        .collect();
    let by_id: HashMap<NodeId, &Node> = nodes.iter().map(|node| (node.id, node)).collect();
    let names: HashMap<NodeId, BTreeSet<String>> =
        nodes.iter().map(|node| (node.id, names_of(node))).collect();

    let mut strong_keys = options.strong_keys.clone();
    for def in read.index_defs(ns)? {
        if def.unique && def.is_simple() && !strong_keys.contains(&def.key) {
            strong_keys.push(def.key);
        }
    }

    // Nodes per name word (and per typo key of it), to compare only nodes
    // that share one.
    let mut by_word: HashMap<String, Vec<NodeId>> = HashMap::new();
    for (id, node_names) in &names {
        let mut seen = HashSet::new();
        for name in node_names {
            for word in words(name) {
                for key in typo_keys(word) {
                    if seen.insert(key.clone()) {
                        by_word.entry(key).or_default().push(*id);
                    }
                }
            }
        }
    }

    let mut pairs: HashMap<(NodeId, NodeId), (f32, Vec<String>)> = HashMap::new();
    let key = |a: NodeId, b: NodeId| if a < b { (a, b) } else { (b, a) };

    // The same value of a strong key: the same entity, whatever the name.
    let mut by_strong: HashMap<(String, String), Vec<NodeId>> = HashMap::new();
    for node in &nodes {
        for strong in &strong_keys {
            if let Some(value) = node.props.get(strong).filter(|value| !value.is_null()) {
                let value = match value {
                    Value::String(s) => s.trim().to_lowercase(),
                    other => other.to_string(),
                };
                by_strong
                    .entry((strong.clone(), value))
                    .or_default()
                    .push(node.id);
            }
        }
    }
    for ((strong, value), ids) in &by_strong {
        for (index, &a) in ids.iter().enumerate() {
            for &b in &ids[index + 1..] {
                let entry = pairs.entry(key(a, b)).or_insert((0.0, Vec::new()));
                entry.0 = 1.0;
                entry.1.push(format!("same {strong} \"{value}\""));
            }
        }
    }
    for ids in by_word
        .values()
        .filter(|ids| ids.len() > 1 && ids.len() <= COMMON_WORD)
    {
        for (index, &a) in ids.iter().enumerate() {
            for &b in &ids[index + 1..] {
                if a == b || !shares_label(by_id[&a], by_id[&b]) {
                    continue;
                }
                let entry = pairs.get(&key(a, b));
                if entry.is_some_and(|(_, reasons)| {
                    reasons
                        .iter()
                        .any(|r| r.starts_with('"') || r.starts_with("same name"))
                }) {
                    continue;
                }
                if let Some((score, reason)) = name_similarity(&names[&a], &names[&b]) {
                    let entry = pairs.entry(key(a, b)).or_insert((0.0, Vec::new()));
                    entry.0 = entry.0.max(score);
                    entry.1.push(reason);
                }
            }
        }
    }

    if let Some(threshold) = options.vector_threshold {
        for node in &nodes {
            let Some(embedding) = read.embedding(ns, node.id)? else {
                continue;
            };
            for hit in read.search_vector(ns, &embedding, 6)? {
                let Some(other) = by_id.get(&hit.node) else {
                    continue;
                };
                if hit.node == node.id || hit.score < threshold || !shares_label(node, other) {
                    continue;
                }
                let entry = pairs
                    .entry(key(node.id, hit.node))
                    .or_insert((0.0, Vec::new()));
                let reason = format!("similar meaning ({:.2})", hit.score);
                if !entry.1.contains(&reason) {
                    entry.0 = entry.0.max(hit.score);
                    entry.1.push(reason);
                }
            }
        }
    }

    let ignored = vec![ARCHIVED.to_owned()];
    let mut out = Vec::new();
    for ((a, b), (score, mut reasons)) in pairs {
        let conflicts = node_conflicts(by_id[&a], by_id[&b], &strong_keys, &ignored);
        if conflicts.iter().any(Conflict::is_identity) {
            continue;
        }
        // Each fact they disagree on makes them less likely the same.
        let score = score * 0.85f32.powi(conflicts.len() as i32);
        reasons.extend(conflicts.iter().map(Conflict::describe));
        if score < options.min_score {
            continue;
        }
        let distinct = read
            .neighbors(
                ns,
                a,
                Direction::Both,
                &EdgeFilter::any().label(DISTINCT_FROM),
            )?
            .iter()
            .any(|neighbor| neighbor.node == b);
        if !distinct {
            out.push(Duplicate {
                a: by_id[&a].clone(),
                b: by_id[&b].clone(),
                score,
                reasons,
                conflicts,
            });
        }
    }
    out.sort_by(|x, y| y.score.total_cmp(&x.score).then(x.a.id.cmp(&y.a.id)));
    out.truncate(options.limit);
    Ok(out)
}

#[derive(Clone, Debug)]
pub struct DecayOptions {
    /// Recency halves every this many seconds since the last access.
    pub half_life_secs: f64,
    /// Memories with a strength below this are weak (0 to 1).
    pub threshold: f32,
    /// Only nodes with one of these labels (empty: any).
    pub labels: Vec<String>,
    pub limit: usize,
    pub now: Timestamp,
}

impl Default for DecayOptions {
    fn default() -> Self {
        Self {
            half_life_secs: 30.0 * 86_400.0,
            threshold: 0.1,
            labels: Vec::new(),
            limit: 100,
            now: Timestamp::now(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WeakMemory {
    pub node: Node,
    pub strength: f32,
    pub memory: MemoryState,
}

/// Memories whose strength (importance decayed since the last access, see
/// [`MemoryState::strength`]) is below the threshold, weakest first.
/// Archived ones are left out.
pub fn weak_memories(read: &ReadTx, ns: &str, options: &DecayOptions) -> Result<Vec<WeakMemory>> {
    let mut out = Vec::new();
    for node in read.nodes_page(ns, None, None, usize::MAX)? {
        if node.has_label(ARCHIVED)
            || (!options.labels.is_empty()
                && !options.labels.iter().any(|label| node.has_label(label)))
        {
            continue;
        }
        let memory = read.memory(ns, &node)?;
        let strength = memory.strength(options.now, options.half_life_secs);
        if strength < options.threshold {
            out.push(WeakMemory {
                node,
                strength,
                memory,
            });
        }
    }
    out.sort_by(|a, b| {
        a.strength
            .total_cmp(&b.strength)
            .then(a.node.id.cmp(&b.node.id))
    });
    out.truncate(options.limit);
    Ok(out)
}

/// Adds the [`ARCHIVED`] label to these nodes (recall then skips them).
/// Returns how many were not archived yet. Nothing is deleted.
pub fn archive(tx: &mut WriteTx, ns: &str, ids: &[NodeId]) -> Result<usize> {
    let mut archived = 0;
    for &id in ids {
        if let Some(node) = tx.node(ns, id)?
            && !node.has_label(ARCHIVED)
        {
            tx.add_label(ns, id, ARCHIVED)?;
            archived += 1;
        }
    }
    Ok(archived)
}

#[derive(Clone, Debug)]
pub struct ConsolidationOptions {
    /// Labels of the memories to group (an entity is any node they connect to
    /// that has none of these labels).
    pub memory_labels: Vec<String>,
    /// Only memories not updated or used for this long.
    pub older_than_secs: i64,
    /// Groups of at least this many memories.
    pub min_size: usize,
    pub limit: usize,
    pub now: Timestamp,
}

impl Default for ConsolidationOptions {
    fn default() -> Self {
        Self {
            memory_labels: vec!["Memory".into(), "Episode".into(), "Note".into()],
            older_than_secs: 7 * 86_400,
            min_size: 3,
            limit: 20,
            now: Timestamp::now(),
        }
    }
}

/// Old memories about the same entity: a summary could replace them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsolidationGroup {
    pub entity: Node,
    pub memories: Vec<Node>,
}

/// Groups of old, unarchived memories connected to the same entity, largest
/// first.
pub fn consolidation_groups(
    read: &ReadTx,
    ns: &str,
    options: &ConsolidationOptions,
) -> Result<Vec<ConsolidationGroup>> {
    let is_memory = |node: &Node| {
        options
            .memory_labels
            .iter()
            .any(|label| node.has_label(label))
    };
    let cutoff = options.now.as_micros() - options.older_than_secs.saturating_mul(1_000_000);
    let mut groups: HashMap<NodeId, Vec<Node>> = HashMap::new();
    let mut entities: HashMap<NodeId, Node> = HashMap::new();
    for label in &options.memory_labels {
        for node in read.nodes_page(ns, Some(label), None, usize::MAX)? {
            if node.has_label(ARCHIVED) {
                continue;
            }
            let state = read.memory(ns, &node)?;
            if state
                .last_access
                .as_micros()
                .max(node.updated_at.as_micros())
                > cutoff
            {
                continue;
            }
            for neighbor in read.neighbors(ns, node.id, Direction::Both, &EdgeFilter::any())? {
                let Some(entity) = read.node(ns, neighbor.node)? else {
                    continue;
                };
                if is_memory(&entity) || entity.has_label(ARCHIVED) {
                    continue;
                }
                let members = groups.entry(entity.id).or_default();
                if !members.iter().any(|m| m.id == node.id) {
                    members.push(node.clone());
                }
                entities.entry(entity.id).or_insert(entity);
            }
        }
    }
    let mut out: Vec<ConsolidationGroup> = groups
        .into_iter()
        .filter(|(_, memories)| memories.len() >= options.min_size)
        .map(|(id, memories)| ConsolidationGroup {
            entity: entities.remove(&id).expect("entity of a group"),
            memories,
        })
        .collect();
    out.sort_by(|a, b| {
        b.memories
            .len()
            .cmp(&a.memories.len())
            .then(a.entity.id.cmp(&b.entity.id))
    });
    out.truncate(options.limit);
    Ok(out)
}

/// A summary written by an agent to replace several memories.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub text: String,
    pub name: Option<String>,
    /// Labels of the summary; `Summary` when empty.
    pub labels: Vec<String>,
    /// The memories it summarizes: its sources, linked by `summarizes`.
    pub sources: Vec<NodeId>,
    /// The entity it is about, linked by `about`.
    pub about: Option<NodeId>,
    pub embedding: Option<Vec<f32>>,
    /// Archive the summarized memories (the default for consolidation).
    pub archive_sources: bool,
}

/// Stores a summary with its provenance; it gets the highest importance of
/// its sources. Returns its id.
pub fn write_summary(tx: &mut WriteTx, ns: &str, summary: Summary) -> Result<NodeId> {
    if summary.sources.is_empty() {
        return Err(grafeno::Error::Invalid(
            "a summary needs the memories it summarizes".into(),
        ));
    }
    let mut importance = 0f32;
    for &source in &summary.sources {
        let node = tx
            .node(ns, source)?
            .ok_or(grafeno::Error::NodeNotFound(source))?;
        importance = importance.max(tx.memory(ns, &node)?.importance);
    }
    let mut new = NewNode {
        labels: if summary.labels.is_empty() {
            vec!["Summary".into()]
        } else {
            summary.labels
        },
        sources: summary.sources.clone(),
        text: Some(summary.text),
        embedding: summary.embedding,
        ..NewNode::default()
    };
    if let Some(name) = summary.name {
        new.props.insert("name".into(), name.into());
    }
    let id = tx.create_node(ns, new)?;
    for &source in &summary.sources {
        tx.create_edge(ns, NewEdge::new(id, "summarizes", source))?;
    }
    if let Some(about) = summary.about {
        tx.create_edge(ns, NewEdge::new(id, "about", about))?;
    }
    tx.set_importance(ns, id, importance)?;
    if summary.archive_sources {
        archive(tx, ns, &summary.sources)?;
    }
    Ok(id)
}
