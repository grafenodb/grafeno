//! Merging two nodes that are the same entity, safely: identity rules
//! (strong keys and labels must agree), contradictions reported instead of
//! dropped silently, and a record of every merge that can undo it.

use std::collections::BTreeSet;

use grafeno_core::{Edge, EdgeId, Error, MergeId, Node, NodeId, Result, Timestamp, Value};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::db::{ReadTx, WriteTx};
use crate::memory::{MemoryState, NAME_KEYS, names_of, normalize_name};
use crate::store::{ADJ_IN, ADJ_OUT, RecordKey, adjacency, decode, encode, storage};

/// Merge records by namespace and merge id.
pub(crate) const MERGES: TableDefinition<RecordKey, &[u8]> = TableDefinition::new("merges");

/// Properties that identify an entity by default, besides those under a
/// unique constraint: two nodes with different values are not the same.
pub const DEFAULT_STRONG_KEYS: [&str; 2] = ["email", "external_id"];

/// How to merge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeOptions {
    /// Merge in spite of conflicts: the kept node's values win, and the
    /// conflicts are reported and recorded.
    pub force: bool,
    /// Properties that identify an entity (with the keys of unique
    /// constraints on either node's labels).
    pub strong_keys: Vec<String>,
    /// Edge labels a node has at most one of at a time (`born_in`,
    /// `married_to`): different current targets conflict.
    pub functional: Vec<String>,
    /// Labels left out when comparing labels (`Archived`).
    pub ignored_labels: Vec<String>,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            force: false,
            strong_keys: DEFAULT_STRONG_KEYS.map(String::from).to_vec(),
            functional: Vec::new(),
            ignored_labels: vec!["Archived".into()],
        }
    }
}

/// What kind of disagreement two nodes have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Different values of an identifying property: two entities.
    StrongKey,
    /// Each node has a label the other lacks (a person and a company).
    Labels,
    /// Different values of the same property (two birth dates).
    Property,
    /// Different current targets of a functional edge label.
    Edge,
}

/// A disagreement between two nodes, with both sides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    pub kind: ConflictKind,
    /// The property or edge label (`labels` for labels).
    pub key: String,
    pub kept: Value,
    pub merged: Value,
}

impl Conflict {
    /// Whether it says the two are different entities (strong keys,
    /// labels), rather than that their facts disagree.
    pub fn is_identity(&self) -> bool {
        matches!(self.kind, ConflictKind::StrongKey | ConflictKind::Labels)
    }

    pub fn describe(&self) -> String {
        match self.kind {
            ConflictKind::Labels => format!("different labels: {} and {}", self.kept, self.merged),
            ConflictKind::StrongKey => {
                format!("different {}: {} and {}", self.key, self.kept, self.merged)
            }
            ConflictKind::Property => {
                format!(
                    "conflicting {}: {} and {}",
                    self.key, self.kept, self.merged
                )
            }
            ConflictKind::Edge => format!(
                "conflicting {} facts: to {} and to {}",
                self.key, self.kept, self.merged
            ),
        }
    }
}

/// A value as compared for identity: strings trimmed and lowercased;
/// numbers as numbers.
fn comparable(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(s.trim().to_lowercase()),
        Value::Number(n) => n
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or(value.clone(), Value::Number),
        other => other.clone(),
    }
}

fn same_value(a: &Value, b: &Value) -> bool {
    a == b || comparable(a) == comparable(b)
}

/// Where two nodes disagree, given the strong keys to use. Names and
/// aliases never conflict (they are how duplicates differ); a strong key
/// counts once, as a strong key.
pub fn node_conflicts(
    a: &Node,
    b: &Node,
    strong_keys: &[String],
    ignored_labels: &[String],
) -> Vec<Conflict> {
    let mut out = Vec::new();
    let labels = |node: &Node| -> BTreeSet<String> {
        node.labels
            .iter()
            .filter(|label| !ignored_labels.contains(label))
            .cloned()
            .collect()
    };
    let (la, lb) = (labels(a), labels(b));
    if !la.is_subset(&lb) && !lb.is_subset(&la) {
        out.push(Conflict {
            kind: ConflictKind::Labels,
            key: "labels".into(),
            kept: Value::from(la.into_iter().collect::<Vec<_>>()),
            merged: Value::from(lb.into_iter().collect::<Vec<_>>()),
        });
    }
    for (key, value) in &a.props {
        if key == "aliases" || NAME_KEYS.contains(&key.as_str()) {
            continue;
        }
        let Some(other) = b.props.get(key) else {
            continue;
        };
        if value.is_null() || other.is_null() || same_value(value, other) {
            continue;
        }
        out.push(Conflict {
            kind: if strong_keys.contains(key) {
                ConflictKind::StrongKey
            } else {
                ConflictKind::Property
            },
            key: key.clone(),
            kept: value.clone(),
            merged: other.clone(),
        });
    }
    out
}

/// The strong keys for two nodes: the options' keys and those of unique
/// constraints on any of their labels.
fn strong_keys_for(
    tx: &WriteTx,
    ns: &str,
    a: &Node,
    b: &Node,
    options: &MergeOptions,
) -> Result<Vec<String>> {
    let mut keys = options.strong_keys.clone();
    for def in tx.index_defs(ns)? {
        if def.unique
            && def.is_simple()
            && (a.has_label(&def.label) || b.has_label(&def.label))
            && !keys.contains(&def.key)
        {
            keys.push(def.key);
        }
    }
    Ok(keys)
}

/// What [`WriteTx::merge_nodes`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MergeReport {
    /// The record of the merge, for [`WriteTx::undo_merge`].
    pub id: MergeId,
    pub kept: NodeId,
    pub merged: NodeId,
    /// Edges of the merged node now attached to the kept one.
    pub edges_moved: u64,
    /// Edges dropped: between the two nodes, or the same as one the kept
    /// node already had.
    pub edges_dropped: u64,
    /// The conflicts merged over with `force` (the kept node's values won).
    pub conflicts: Vec<Conflict>,
}

/// A node as it was: record, text, embedding and memory state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub node: Node,
    pub text: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub memory: MemoryState,
}

/// One merge, as recorded: who, when, what conflicts were overridden, and
/// everything needed to undo it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeRecord {
    pub id: MergeId,
    pub kept: NodeId,
    pub merged: NodeId,
    pub at: Timestamp,
    /// The principal that merged, when known.
    pub by: Option<String>,
    pub forced: bool,
    pub conflicts: Vec<Conflict>,
    /// Both nodes before the merge.
    pub kept_before: NodeSnapshot,
    pub merged_before: NodeSnapshot,
    /// Edges moved to the kept node, as they were.
    pub moved: Vec<Edge>,
    /// Edges dropped (between the two, or repeating one of the kept node).
    pub dropped: Vec<Edge>,
    /// The kept node's `updated_at` right after the merge: undoing needs it
    /// unchanged since.
    pub kept_updated_at: Timestamp,
    /// When and by whom it was undone.
    pub undone_at: Option<Timestamp>,
    pub undone_by: Option<String>,
}

impl ReadTx {
    /// The merges of a namespace, newest first; with `node`, those that kept
    /// or merged it.
    pub fn merges(&self, ns: &str, node: Option<NodeId>) -> Result<Vec<MergeRecord>> {
        list(&self.merges, ns, node)
    }

    pub fn merge_record(&self, ns: &str, id: MergeId) -> Result<Option<MergeRecord>> {
        get(&self.merges, ns, id)
    }
}

fn list(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    node: Option<NodeId>,
) -> Result<Vec<MergeRecord>> {
    let mut out = Vec::new();
    for item in table.range((ns, 0u128)..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns {
            break;
        }
        let record: MergeRecord = decode(value.value())?;
        if node.is_none_or(|id| record.kept == id || record.merged == id) {
            out.push(record);
        }
    }
    out.reverse();
    Ok(out)
}

fn get(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    id: MergeId,
) -> Result<Option<MergeRecord>> {
    table
        .get((ns, id.as_u128()))
        .map_err(storage)?
        .map(|guard| decode(guard.value()))
        .transpose()
}

impl WriteTx {
    pub fn merges(&self, ns: &str, node: Option<NodeId>) -> Result<Vec<MergeRecord>> {
        list(&self.tx.open_table(MERGES).map_err(storage)?, ns, node)
    }

    fn snapshot(&self, ns: &str, node: &Node) -> Result<NodeSnapshot> {
        Ok(NodeSnapshot {
            node: node.clone(),
            text: self.text(ns, node.id)?,
            embedding: self.embedding(ns, node.id)?,
            memory: self.memory(ns, node)?,
        })
    }

    /// Edges current now from `node` with `label`, by target.
    fn current_targets(&self, ns: &str, node: NodeId, label: &str) -> Result<BTreeSet<NodeId>> {
        let entries = {
            let table = self.tx.open_table(ADJ_OUT).map_err(storage)?;
            adjacency(&table, ns, node, Some(label))?
        };
        let now = Timestamp::now();
        let mut out = BTreeSet::new();
        for entry in entries {
            if let Some(edge) = self.edge(ns, entry.edge)?
                && !edge.is_retracted()
                && edge.holds_at(now)
            {
                out.insert(edge.to);
            }
        }
        Ok(out)
    }

    /// Where `kept` and `merged` disagree: strong keys, labels, properties
    /// and functional edges. Changes nothing.
    pub fn merge_conflicts(
        &self,
        ns: &str,
        kept: NodeId,
        merged: NodeId,
        options: &MergeOptions,
    ) -> Result<Vec<Conflict>> {
        let keep = self.node(ns, kept)?.ok_or(Error::NodeNotFound(kept))?;
        let gone = self.node(ns, merged)?.ok_or(Error::NodeNotFound(merged))?;
        self.conflicts(ns, &keep, &gone, options)
    }

    fn conflicts(
        &self,
        ns: &str,
        keep: &Node,
        gone: &Node,
        options: &MergeOptions,
    ) -> Result<Vec<Conflict>> {
        let strong = strong_keys_for(self, ns, keep, gone, options)?;
        let mut conflicts = node_conflicts(keep, gone, &strong, &options.ignored_labels);
        for label in &options.functional {
            let (a, b) = (
                self.current_targets(ns, keep.id, label)?,
                self.current_targets(ns, gone.id, label)?,
            );
            // Pointing at each other is not a disagreement.
            let a: BTreeSet<NodeId> = a.into_iter().filter(|id| *id != gone.id).collect();
            let b: BTreeSet<NodeId> = b.into_iter().filter(|id| *id != keep.id).collect();
            if !a.is_empty() && !b.is_empty() && a != b {
                let ids = |set: &BTreeSet<NodeId>| {
                    Value::from(set.iter().map(|id| id.to_string()).collect::<Vec<_>>())
                };
                conflicts.push(Conflict {
                    kind: ConflictKind::Edge,
                    key: label.clone(),
                    kept: ids(&a),
                    merged: ids(&b),
                });
            }
        }
        Ok(conflicts)
    }

    /// [`WriteTx::merge_nodes_with`] with the default options: refuses to
    /// merge nodes that conflict.
    pub fn merge_nodes(&mut self, ns: &str, kept: NodeId, merged: NodeId) -> Result<MergeReport> {
        self.merge_nodes_with(ns, kept, merged, &MergeOptions::default())
    }

    /// Merges node `merged` into node `kept` and deletes `merged`:
    ///
    /// - it refuses, with [`Error::Conflict`], when the two disagree (see
    ///   [`WriteTx::merge_conflicts`]) unless `options.force`;
    /// - its edges move to `kept` (edges between the two, and edges `kept`
    ///   already has with the same label, other end and times, are dropped);
    /// - labels and sources are joined; properties of `kept` win, the others
    ///   are added; the names of `merged` become `aliases` of `kept`;
    /// - texts are joined; `kept`'s embedding stays (or `merged`'s, if
    ///   `kept` had none);
    /// - importance and last access take the higher value, accesses add up;
    /// - the merge is recorded, with both nodes as they were, for
    ///   [`WriteTx::merges`] and [`WriteTx::undo_merge`].
    pub fn merge_nodes_with(
        &mut self,
        ns: &str,
        kept: NodeId,
        merged: NodeId,
        options: &MergeOptions,
    ) -> Result<MergeReport> {
        if kept == merged {
            return Err(Error::Invalid("cannot merge a node into itself".into()));
        }
        let keep = self.node(ns, kept)?.ok_or(Error::NodeNotFound(kept))?;
        let gone = self.node(ns, merged)?.ok_or(Error::NodeNotFound(merged))?;
        self.guard_node_for(crate::Act::Merge, ns, &keep)?;
        self.guard_node_for(crate::Act::Merge, ns, &gone)?;
        let conflicts = self.conflicts(ns, &keep, &gone, options)?;
        if !conflicts.is_empty() && !options.force {
            let listed: Vec<String> = conflicts.iter().map(Conflict::describe).collect();
            return Err(Error::Conflict(format!(
                "the nodes disagree, so they may not be the same: {}; merge with force to keep the kept node's values",
                listed.join("; ")
            )));
        }
        let kept_before = self.snapshot(ns, &keep)?;
        let merged_before = self.snapshot(ns, &gone)?;
        let mut report = MergeReport {
            id: MergeId::new(),
            kept,
            merged,
            conflicts: if options.force {
                conflicts.clone()
            } else {
                Vec::new()
            },
            ..MergeReport::default()
        };
        let (mut moved, mut dropped) = (Vec::new(), Vec::new());

        // Move the edges first, while both nodes exist.
        let mut edge_ids: Vec<EdgeId> = Vec::new();
        for definition in [ADJ_OUT, ADJ_IN] {
            let table = self.tx.open_table(definition).map_err(storage)?;
            edge_ids.extend(
                adjacency(&table, ns, merged, None)?
                    .into_iter()
                    .map(|entry| entry.edge),
            );
        }
        edge_ids.sort();
        edge_ids.dedup();
        let same = |a: &Edge, b: &Edge| {
            a.label == b.label
                && a.from == b.from
                && a.to == b.to
                && a.valid_from == b.valid_from
                && a.valid_to == b.valid_to
                && a.is_retracted() == b.is_retracted()
        };
        let mut kept_edges: Vec<Edge> = Vec::new();
        for definition in [ADJ_OUT, ADJ_IN] {
            let entries = {
                let table = self.tx.open_table(definition).map_err(storage)?;
                adjacency(&table, ns, kept, None)?
            };
            for entry in entries {
                if let Some(edge) = self.edge(ns, entry.edge)? {
                    kept_edges.push(edge);
                }
            }
        }
        for edge_id in edge_ids {
            let Some(original) = self.edge(ns, edge_id)? else {
                continue;
            };
            let mut edge = original.clone();
            let between = (edge.from == kept && edge.to == merged)
                || (edge.from == merged && edge.to == kept);
            if edge.from == merged {
                edge.from = kept;
            }
            if edge.to == merged {
                edge.to = kept;
            }
            if between || kept_edges.iter().any(|existing| same(existing, &edge)) {
                self.delete_edge(ns, edge_id)?;
                dropped.push(original);
                report.edges_dropped += 1;
            } else {
                self.restore_edge(ns, edge.clone())?;
                kept_edges.push(edge);
                moved.push(original);
                report.edges_moved += 1;
            }
        }

        // Then the node itself.
        let mut node = keep.clone();
        for label in &gone.labels {
            if !node.labels.contains(label) {
                node.labels.push(label.clone());
            }
        }
        node.labels.sort();
        for (key, value) in &gone.props {
            if key != "aliases" && !NAME_KEYS.contains(&key.as_str()) {
                node.props
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
        let own_names = names_of(&keep);
        let mut aliases: Vec<Value> = match keep.props.get("aliases") {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        let mut alias_candidates: Vec<String> = NAME_KEYS
            .iter()
            .filter_map(|key| {
                gone.props
                    .get(*key)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect();
        if let Some(Value::Array(items)) = gone.props.get("aliases") {
            alias_candidates.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        let mut seen: BTreeSet<String> = aliases
            .iter()
            .filter_map(Value::as_str)
            .map(normalize_name)
            .chain(own_names.iter().cloned())
            .collect();
        for alias in alias_candidates {
            if seen.insert(normalize_name(&alias)) {
                aliases.push(Value::String(alias));
            }
        }
        if !aliases.is_empty() {
            node.props.insert("aliases".into(), Value::Array(aliases));
        }
        for source in &gone.sources {
            if !node.sources.contains(source) && *source != kept {
                node.sources.push(*source);
            }
        }
        node.created_at = node.created_at.min(gone.created_at);
        node.updated_at = Timestamp::now();

        let text = match (&kept_before.text, &merged_before.text) {
            (Some(a), Some(b)) if a.trim() == b.trim() || a.contains(b.trim()) => Some(a.clone()),
            (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
            (a, b) => a.clone().or_else(|| b.clone()),
        };
        let embedding = kept_before
            .embedding
            .clone()
            .or_else(|| merged_before.embedding.clone());
        let (keep_state, gone_state) = (kept_before.memory, merged_before.memory);
        let state = MemoryState {
            importance: keep_state.importance.max(gone_state.importance),
            accesses: keep_state.accesses.saturating_add(gone_state.accesses),
            last_access: keep_state.last_access.max(gone_state.last_access),
        };
        let kept_updated_at = node.updated_at;

        self.delete_node(ns, merged)?;
        self.restore_node(ns, node, text.as_deref(), embedding.as_deref())?;
        self.put_memory(ns, kept, state)?;

        let record = MergeRecord {
            id: report.id,
            kept,
            merged,
            at: kept_updated_at,
            by: self.actor().map(|actor| actor.name.clone()),
            forced: options.force && !conflicts.is_empty(),
            conflicts: report.conflicts.clone(),
            kept_before,
            merged_before,
            moved,
            dropped,
            kept_updated_at,
            undone_at: None,
            undone_by: None,
        };
        self.put_merge(ns, &record)?;
        Ok(report)
    }

    fn put_merge(&mut self, ns: &str, record: &MergeRecord) -> Result<()> {
        self.note(crate::replication::Touch::Merge(
            ns.to_owned(),
            record.id.as_u128(),
        ));
        let bytes = encode(record)?;
        self.tx
            .open_table(MERGES)
            .map_err(storage)?
            .insert((ns, record.id.as_u128()), bytes.as_slice())
            .map_err(storage)?;
        Ok(())
    }

    /// Undoes a merge: the merged node comes back as it was (same id, text,
    /// embedding and memory), the kept node returns to its state before the
    /// merge, moved edges return to the merged node and dropped ones are
    /// restored. Edges added to the kept node since stay with it.
    ///
    /// Refuses with [`Error::Conflict`] if the kept node changed after the
    /// merge (its later changes would be lost) unless `force`.
    pub fn undo_merge(&mut self, ns: &str, id: MergeId, force: bool) -> Result<MergeRecord> {
        let mut record = get(&self.tx.open_table(MERGES).map_err(storage)?, ns, id)?
            .ok_or_else(|| Error::Invalid(format!("no merge {id} in {ns:?}")))?;
        if record.undone_at.is_some() {
            return Err(Error::Conflict(format!("merge {id} was undone already")));
        }
        let keep = self
            .node(ns, record.kept)?
            .ok_or(Error::NodeNotFound(record.kept))?;
        self.guard_node_for(crate::Act::Merge, ns, &keep)?;
        if self.node(ns, record.merged)?.is_some() {
            return Err(Error::Conflict(format!(
                "node {} exists again, so the merge cannot be undone",
                record.merged
            )));
        }
        if keep.updated_at != record.kept_updated_at && !force {
            return Err(Error::Conflict(format!(
                "node {} changed after the merge; undo with force to lose those changes",
                record.kept
            )));
        }
        let restore = |tx: &mut WriteTx, snapshot: &NodeSnapshot| -> Result<()> {
            tx.restore_node(
                ns,
                snapshot.node.clone(),
                snapshot.text.as_deref(),
                snapshot.embedding.as_deref(),
            )?;
            tx.put_memory(ns, snapshot.node.id, snapshot.memory)
        };
        restore(self, &record.merged_before)?;
        restore(self, &record.kept_before)?;
        for edge in record.moved.iter().chain(&record.dropped) {
            // An edge deleted since, or whose other end is gone, stays gone.
            let ends_exist =
                self.node(ns, edge.from)?.is_some() && self.node(ns, edge.to)?.is_some();
            let moved_still_there =
                record.moved.contains(edge) && self.edge(ns, edge.id)?.is_some();
            let dropped = record.dropped.contains(edge);
            if ends_exist && (moved_still_there || dropped) {
                self.restore_edge(ns, edge.clone())?;
            }
        }
        record.undone_at = Some(Timestamp::now());
        record.undone_by = self.actor().map(|actor| actor.name.clone());
        self.put_merge(ns, &record)?;
        Ok(record)
    }
}
