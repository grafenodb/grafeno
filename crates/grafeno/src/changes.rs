//! The change log: every commit appends what it changed, per namespace,
//! with a sequence number, so agents and services can follow the writes
//! (`changes(ns, since)`) instead of polling the graph. Entries say what
//! changed (a node or an edge, created, updated or deleted, its labels,
//! which parts), not the content: a follower reads the record itself.
//!
//! Changes of a transaction to the same record are folded into one entry,
//! written at commit, and are lost with an aborted transaction. The log
//! keeps a window ([`Retention`]): at most so many entries per namespace,
//! none older than so many seconds. A follower that falls behind the
//! window learns it (`ChangePage::gap`) and starts over from the graph.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use grafeno_core::{Edge, EdgeId, Node, NodeId, Result, Timestamp};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::db::{ReadTx, WriteTx};
use crate::store::{decode, encode, storage};

/// `(ns, seq) -> Change`.
pub(crate) const CHANGES: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("changes");

/// At most this many entries pruned per commit, so a commit stays short
/// after the window shrinks.
const PRUNE_BATCH: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Created,
    Updated,
    Deleted,
}

/// Which parts of a node changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// Labels, properties, validity: the record.
    Record,
    Text,
    Embedding,
}

/// One entry of the log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub seq: u64,
    /// When it was committed (the commit's clock, the same for its entries).
    pub at: Timestamp,
    pub op: ChangeOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<EdgeId>,
    /// The node's labels, or the edge's label.
    #[serde(default)]
    pub labels: Vec<String>,
    /// An edge's ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Part>,
    /// The principal that wrote it, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The key or session it wrote with (`key:name`, `session:id`), 14.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// The values event subscriptions asked for (16.10); none otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<crate::subscriptions::ChangeValues>,
}

/// What [`ReadTx::changes`] returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangePage {
    pub changes: Vec<Change>,
    /// The last sequence number of the namespace (0 before any change).
    pub last: u64,
    /// The oldest sequence number still kept (0 when the log is empty).
    pub oldest: u64,
    /// Changes after `since` were pruned before they could be read: the
    /// follower missed some and should read the graph again.
    pub gap: bool,
    /// The last sequence number this page read, changes hidden from its
    /// reader included (14.4): where to continue.
    #[serde(default)]
    pub through: u64,
}

impl ChangePage {
    /// Where to continue: the last change returned, or `since` if none.
    pub fn next(&self, since: u64) -> u64 {
        self.changes
            .last()
            .map_or(since, |change| change.seq)
            .max(self.through)
    }
}

/// How much of the log is kept, per namespace.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    /// Off: commits append nothing (what is there stays until pruned).
    pub enabled: bool,
    /// Entries kept at most; 0 for no limit.
    pub max_entries: u64,
    /// Seconds an entry is kept at most; 0 for no limit.
    pub max_age_secs: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 1_000_000,
            max_age_secs: 7 * 24 * 3600,
        }
    }
}

type Listener = Arc<dyn Fn(&[(String, u64)]) + Send + Sync>;

/// The retention window and the commit listeners of a database.
#[derive(Default)]
pub(crate) struct Hub {
    retention: Mutex<Retention>,
    listeners: RwLock<Vec<Listener>>,
    /// Event values for subscriptions (16.10), off by default.
    events: std::sync::atomic::AtomicBool,
}

impl Hub {
    pub(crate) fn events(&self) -> bool {
        self.events.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn set_events(&self, enabled: bool) {
        self.events
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn retention(&self) -> Retention {
        *self.retention.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_retention(&self, retention: Retention) {
        *self.retention.lock().unwrap_or_else(|e| e.into_inner()) = retention;
    }

    pub(crate) fn listen(&self, listener: Listener) {
        self.listeners
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(listener);
    }

    pub(crate) fn notify(&self, touched: &[(String, u64)]) {
        let listeners = self.listeners.read().unwrap_or_else(|e| e.into_inner());
        for listener in listeners.iter() {
            listener(touched);
        }
    }
}

/// The changes of a transaction, folded per record.
#[derive(Default)]
pub(crate) struct Pending {
    entries: Vec<(String, Change)>,
    /// `(id, is_edge)` to its entry (ids are unique; a namespace that
    /// differs gets an entry of its own, unfolded).
    index: HashMap<(u128, bool), usize>,
    /// Records as they were before the transaction, for subscriptions.
    befores: crate::subscriptions::Befores,
    /// The values were added (at commit, or for the replicated entry).
    pub(crate) filled: bool,
}

impl Pending {
    /// The entries so far, in order (for the replication log).
    pub(crate) fn entries(&self) -> &[(String, Change)] {
        &self.entries
    }

    /// Pending entries copied from another database's commit (an applied
    /// replication log entry), in place of those derived here.
    pub(crate) fn from_entries(entries: Vec<(String, Change)>) -> Self {
        Self {
            entries,
            index: HashMap::new(),
            befores: Default::default(),
            filled: true,
        }
    }

    /// Keeps what a record was before its first change in the transaction.
    pub(crate) fn remember(
        &mut self,
        ns: &str,
        id: u128,
        is_edge: bool,
        before: crate::subscriptions::Before,
    ) {
        self.befores
            .entry((ns.to_owned(), id, is_edge))
            .or_insert(before);
    }

    pub(crate) fn before(
        &self,
        ns: &str,
        id: u128,
        is_edge: bool,
    ) -> Option<&crate::subscriptions::Before> {
        self.befores.get(&(ns.to_owned(), id, is_edge))
    }

    pub(crate) fn set_values(
        &mut self,
        at: usize,
        values: Option<crate::subscriptions::ChangeValues>,
    ) {
        self.entries[at].1.values = values;
    }

    /// The entry of this record in this namespace, if any.
    fn find(&self, ns: &str, id: u128, is_edge: bool) -> Option<usize> {
        self.index
            .get(&(id, is_edge))
            .copied()
            .filter(|at| self.entries[*at].0 == ns)
    }

    /// Adds a part to the record's entry, if it has one.
    fn add_part(&mut self, ns: &str, id: u128, part: Part) -> bool {
        let Some(at) = self.find(ns, id, false) else {
            return false;
        };
        let entry = &mut self.entries[at].1;
        if entry.op != ChangeOp::Deleted && !entry.parts.contains(&part) {
            entry.parts.push(part);
            entry.parts.sort();
        }
        true
    }

    fn add(&mut self, ns: &str, id: u128, is_edge: bool, change: Change) {
        let Some(at) = self.find(ns, id, is_edge) else {
            self.index
                .entry((id, is_edge))
                .or_insert(self.entries.len());
            self.entries.push((ns.to_owned(), change));
            return;
        };
        let entry = &mut self.entries[at].1;
        match (entry.op, change.op) {
            (_, ChangeOp::Deleted) => {
                entry.op = ChangeOp::Deleted;
                entry.parts.clear();
            }
            (ChangeOp::Deleted, op) => {
                entry.op = op;
                entry.parts = change.parts;
            }
            _ => {
                for part in change.parts {
                    if !entry.parts.contains(&part) {
                        entry.parts.push(part);
                    }
                }
                entry.parts.sort();
            }
        }
        if !change.labels.is_empty() {
            entry.labels = change.labels;
        }
    }
}

impl WriteTx {
    fn logging(&self) -> bool {
        self.log_changes
    }

    pub(crate) fn log_node(&mut self, ns: &str, node: &Node, op: ChangeOp, part: Option<Part>) {
        if !self.logging() {
            return;
        }
        let change = Change {
            seq: 0,
            at: Timestamp::from_micros(0),
            op,
            node: Some(node.id),
            edge: None,
            labels: node.labels.clone(),
            from: None,
            to: None,
            parts: part.into_iter().collect(),
            by: self.actor.as_ref().map(|actor| actor.name.clone()),
            via: self.origin.as_ref().and_then(|origin| origin.via.clone()),
            values: None,
        };
        self.pending_changes
            .add(ns, node.id.as_u128(), false, change);
    }

    pub(crate) fn log_edge(&mut self, ns: &str, edge: &Edge, op: ChangeOp) {
        if !self.logging() {
            return;
        }
        let change = Change {
            seq: 0,
            at: Timestamp::from_micros(0),
            op,
            node: None,
            edge: Some(edge.id),
            labels: vec![edge.label.clone()],
            from: Some(edge.from),
            to: Some(edge.to),
            parts: if op == ChangeOp::Deleted {
                Vec::new()
            } else {
                vec![Part::Record]
            },
            by: self.actor.as_ref().map(|actor| actor.name.clone()),
            via: self.origin.as_ref().and_then(|origin| origin.via.clone()),
            values: None,
        };
        self.pending_changes
            .add(ns, edge.id.as_u128(), true, change);
    }

    /// A node's text or embedding changed (looked up for its labels).
    pub(crate) fn log_node_part(&mut self, ns: &str, id: NodeId, part: Part) -> Result<()> {
        if !self.logging() {
            return Ok(());
        }
        // Usually the record changed in this transaction too: no read.
        if self.pending_changes.add_part(ns, id.as_u128(), part) {
            return Ok(());
        }
        if let Some(node) = self.node(ns, id)? {
            self.log_node(ns, &node, ChangeOp::Updated, Some(part));
        }
        Ok(())
    }

    /// Appends the folded changes with their sequence numbers and prunes
    /// what fell out of the window. Returns each namespace written with its
    /// last sequence number.
    pub(crate) fn write_changes(&mut self) -> Result<Vec<(String, u64)>> {
        self.fill_values()?;
        let pending = std::mem::take(&mut self.pending_changes);
        if pending.entries.is_empty() {
            return Ok(Vec::new());
        }
        let retention = self.shared.changes.retention();
        let now = self.commit_at.unwrap_or_else(Timestamp::now);
        let mut table = self.tx.open_table(CHANGES).map_err(storage)?;
        let mut last: HashMap<String, u64> = HashMap::new();
        for (ns, mut change) in pending.entries {
            let seq = match last.get(&ns) {
                Some(seq) => *seq,
                None => table
                    .range((ns.as_str(), 0)..=(ns.as_str(), u64::MAX))
                    .map_err(storage)?
                    .next_back()
                    .transpose()
                    .map_err(storage)?
                    .map_or(0, |(key, _)| key.value().1),
            } + 1;
            change.seq = seq;
            change.at = now;
            table
                .insert((ns.as_str(), seq), encode(&change)?.as_slice())
                .map_err(storage)?;
            last.insert(ns, seq);
        }
        for (ns, seq) in &last {
            let mut doomed: Vec<u64> = Vec::new();
            for item in table
                .range((ns.as_str(), 0)..=(ns.as_str(), *seq))
                .map_err(storage)?
            {
                let (key, value) = item.map_err(storage)?;
                let old = key.value().1;
                let too_many = retention.max_entries > 0 && seq - old >= retention.max_entries;
                let too_old = retention.max_age_secs > 0 && {
                    let change: Change = decode(value.value())?;
                    now.as_micros() - change.at.as_micros()
                        > retention.max_age_secs as i64 * 1_000_000
                };
                if !(too_many || too_old) || doomed.len() >= PRUNE_BATCH {
                    break;
                }
                doomed.push(old);
            }
            for old in doomed {
                table.remove((ns.as_str(), old)).map_err(storage)?;
            }
        }
        let mut touched: Vec<(String, u64)> = last.into_iter().collect();
        touched.sort();
        Ok(touched)
    }
}

/// A page of the log as it is stored, values included.
pub(crate) fn raw_page(
    table: &impl ReadableTable<(&'static str, u64), &'static [u8]>,
    ns: &str,
    since: u64,
    limit: usize,
) -> Result<ChangePage> {
    page(table, ns, since, limit)
}

fn page(
    table: &impl ReadableTable<(&'static str, u64), &'static [u8]>,
    ns: &str,
    since: u64,
    limit: usize,
) -> Result<ChangePage> {
    let mut range = table.range((ns, 0)..=(ns, u64::MAX)).map_err(storage)?;
    let oldest = range
        .next()
        .transpose()
        .map_err(storage)?
        .map_or(0, |(key, _)| key.value().1);
    let last = range
        .next_back()
        .transpose()
        .map_err(storage)?
        .map_or(oldest, |(key, _)| key.value().1);
    let mut changes = Vec::new();
    if since < last {
        for item in table
            .range((ns, since.saturating_add(1))..=(ns, u64::MAX))
            .map_err(storage)?
        {
            if changes.len() >= limit {
                break;
            }
            let (_, value) = item.map_err(storage)?;
            changes.push(decode(value.value())?);
        }
    }
    Ok(ChangePage {
        through: changes.last().map_or(since, |change: &Change| change.seq),
        changes,
        last,
        oldest,
        gap: oldest > 0 && since + 1 < oldest,
    })
}

impl ReadTx {
    /// Up to `limit` changes of the namespace after `since` (0 for the
    /// start of what is kept), oldest first.
    pub fn changes(&self, ns: &str, since: u64, limit: usize) -> Result<ChangePage> {
        let mut found = page(&self.changes, ns, since, limit)?;
        // Values are for the subscriptions that asked (`events`).
        for change in &mut found.changes {
            change.values = None;
        }
        let Some(view) = self.view() else {
            return Ok(found);
        };
        if !view.reads(ns) {
            found.changes.clear();
            return Ok(found);
        }
        if !view.per_record(ns) {
            return Ok(found);
        }
        // A change is seen when its record is, as it is now; a deleted
        // one by its labels, unless policies (which need the record) apply.
        let policies = view.has_policies(ns);
        let mut kept = Vec::with_capacity(found.changes.len());
        for change in found.changes {
            let seen = match (change.node, change.edge) {
                (_, Some(edge)) => match self.edge(ns, edge)? {
                    Some(_) => true,
                    None => {
                        !policies
                            && view
                                .rights
                                .on_labels(crate::Act::Read, ns, &change.labels, true)
                            && change
                                .from
                                .is_none_or(|id| self.sees(ns, id).unwrap_or(false))
                            && change
                                .to
                                .is_none_or(|id| self.sees(ns, id).unwrap_or(false))
                            && !self.raw_edge_exists(ns, edge)?
                    }
                },
                (Some(node), None) => match self.node(ns, node)? {
                    Some(_) => true,
                    None => {
                        !policies
                            && !self.raw_node_exists(ns, node)?
                            && view
                                .rights
                                .on_labels(crate::Act::Read, ns, &change.labels, false)
                    }
                },
                (None, None) => true,
            };
            if seen {
                kept.push(change);
            }
        }
        found.changes = kept;
        Ok(found)
    }
}

impl crate::Grafeno {
    /// How much of the change log is kept (the default: a million entries
    /// and seven days per namespace). Applies from the next commit.
    pub fn set_change_retention(&self, retention: Retention) {
        self.shared.changes.set_retention(retention);
    }

    pub fn change_retention(&self) -> Retention {
        self.shared.changes.retention()
    }

    /// Calls `listener` after every commit that appended changes, with each
    /// namespace written and its last sequence number. It runs on the
    /// committing thread: it should only signal (a channel, a condvar).
    pub fn on_commit(&self, listener: impl Fn(&[(String, u64)]) + Send + Sync + 'static) {
        self.shared.changes.listen(Arc::new(listener));
    }
}
