//! The replication log (phase 13.1): every committed write transaction, as
//! the effects it had, so that other servers can apply it and hold the same
//! data.
//!
//! A write transaction notes the keys of the primary records it touches
//! (nodes, edges, texts, embeddings, memory states, index and constraint
//! definitions, principals, namespaces, match rules, state entries, merge
//! records). At commit their final values are read and appended to the log
//! as one [`Entry`], in the same redb transaction as the writes: values,
//! not requests, so ids, clock readings and everything a GQ statement or a
//! constraint decided are already resolved. Work that rewrites a whole
//! namespace (dropping it, rebuilding its indexes, compacting its vectors)
//! is logged as that operation instead of record by record.
//!
//! [`WriteTx::apply_entry`] performs an entry on another database: it writes
//! the records and derives everything else from them (adjacency, labels,
//! names, property indexes, postings, the HNSW graph, counters), writes the
//! change log entries the leader's commit wrote (so every member's change
//! log is the same), runs none of the rules that decided them (constraints,
//! property history), and records the position it reached in the same
//! transaction, so an entry is never applied twice nor half applied.
//!
//! The log is off until [`crate::Grafeno::enable_replication_log`] turns it
//! on for good (it is kept in the file): a single server pays nothing.

use std::collections::{BTreeSet, HashSet};

use grafeno_core::{Edge, EdgeId, Error, Node, NodeId, Result, Timestamp};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::db::{ReadTx, SCHEMA_VERSION, WriteTx};
use crate::store::{EDGES, MEMORY, META, NODES, decode, encode, storage};

/// `index -> Entry` (JSON).
pub(crate) const REPLICATION_LOG: TableDefinition<u64, &[u8]> =
    TableDefinition::new("replication_log");

/// The state kind and key that fence a namespace moving to another cluster
/// group: once written, the namespace refuses every other write here.
pub const FENCE: (&str, &str) = ("cluster", "moved");

/// The newest version of [`Entry`]'s format this build reads and writes.
/// 1: records and bulk work (13.1). 2: also change log entries copied as
/// they are ([`Op::Change`], 13.5). 3: accounts, API keys and sessions
/// instead of principals (14.1). 4: roles (14.2). 5: record policies and
/// account attributes (14.4). 6: audit events (14.5). An entry carries the oldest format that
/// can hold it, so a member of the version before reads every entry that
/// does not need the new one.
pub const ENTRY_FORMAT: u32 = 6;

pub(crate) const ENABLED: &str = "replication_enabled";
const FIRST: &str = "replication_first";
const LAST: &str = "replication_last";
const APPLIED: &str = "replication_applied";

/// A key of a primary record, noted when a transaction writes it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Touch {
    Node(String, u128),
    Edge(String, u128),
    Text(String, u128),
    Vector(String, u128),
    Memory(String, u128),
    Index(String, String),
    Constraint(String, String),
    Account(String),
    ApiKey(String),
    Session(String),
    Role(String),
    /// An audit event (14.5).
    Audit(u64),
    Namespace(String),
    MatchRule(String, String),
    State(String, String, String),
    Merge(String, u128),
    /// A change log entry copied as it is (a namespace moving between
    /// cluster groups keeps its sequence numbers).
    Change(String, u64),
    Bulk(Bulk),
}

/// Work logged as itself rather than record by record.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Bulk {
    DropNamespace { ns: String },
    RebuildIndexes { ns: String },
    CompactVectors { ns: String },
}

/// What a write transaction touched, in the order it first touched it.
#[derive(Default)]
pub(crate) struct Capture {
    keys: Vec<Touch>,
    seen: HashSet<Touch>,
    /// Inside bulk work, which is logged as itself.
    paused: u32,
}

/// One effect: a record's value after the transaction (`None`: it is gone),
/// or bulk work.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Node {
        ns: String,
        id: NodeId,
        node: Option<Node>,
    },
    Edge {
        ns: String,
        id: EdgeId,
        edge: Option<Edge>,
    },
    Text {
        ns: String,
        id: NodeId,
        text: Option<String>,
    },
    Vector {
        ns: String,
        id: NodeId,
        embedding: Option<Vec<f32>>,
    },
    Memory {
        ns: String,
        id: NodeId,
        state: Option<(f32, u32, i64)>,
    },
    Index {
        ns: String,
        name: String,
        def: Option<Json>,
    },
    Constraint {
        ns: String,
        name: String,
        constraint: Option<Json>,
    },
    /// A key of layout 14 and before (from a member of that version): an
    /// account and a key now.
    Principal {
        name: String,
        principal: Option<Json>,
    },
    Account {
        name: String,
        account: Option<Json>,
    },
    ApiKey {
        name: String,
        key: Option<Json>,
    },
    Session {
        id: String,
        session: Option<Json>,
    },
    Role {
        name: String,
        role: Option<Json>,
    },
    /// An audit event as it is (or `None`: pruned), 14.5.
    Audit {
        seq: u64,
        event: Option<Json>,
    },
    Namespace {
        ns: String,
        meta: Option<Json>,
    },
    MatchRule {
        ns: String,
        label: String,
        rule: Option<Json>,
    },
    State {
        ns: String,
        kind: String,
        key: String,
        value: Option<Vec<u8>>,
    },
    Merge {
        ns: String,
        /// The merge id, in decimal (JSON has no 128-bit integers).
        id: String,
        record: Option<Json>,
    },
    /// A change log entry with its sequence number.
    Change {
        ns: String,
        seq: u64,
        change: Option<crate::changes::Change>,
    },
    Bulk(Bulk),
}

impl Op {
    /// The namespace the effect belongs to (`None` for principals).
    pub fn namespace(&self) -> Option<&str> {
        match self {
            Op::Node { ns, .. }
            | Op::Edge { ns, .. }
            | Op::Text { ns, .. }
            | Op::Vector { ns, .. }
            | Op::Memory { ns, .. }
            | Op::Index { ns, .. }
            | Op::Constraint { ns, .. }
            | Op::Namespace { ns, .. }
            | Op::MatchRule { ns, .. }
            | Op::State { ns, .. }
            | Op::Merge { ns, .. }
            | Op::Change { ns, .. } => Some(ns),
            Op::Bulk(Bulk::DropNamespace { ns })
            | Op::Bulk(Bulk::RebuildIndexes { ns })
            | Op::Bulk(Bulk::CompactVectors { ns }) => Some(ns),
            Op::Principal { .. }
            | Op::Account { .. }
            | Op::ApiKey { .. }
            | Op::Session { .. }
            | Op::Role { .. }
            | Op::Audit { .. } => None,
        }
    }

    /// The key it writes, to note for this database's own log.
    fn touch(&self) -> Touch {
        match self {
            Op::Node { ns, id, .. } => Touch::Node(ns.clone(), id.as_u128()),
            Op::Edge { ns, id, .. } => Touch::Edge(ns.clone(), id.as_u128()),
            Op::Text { ns, id, .. } => Touch::Text(ns.clone(), id.as_u128()),
            Op::Vector { ns, id, .. } => Touch::Vector(ns.clone(), id.as_u128()),
            Op::Memory { ns, id, .. } => Touch::Memory(ns.clone(), id.as_u128()),
            Op::Index { ns, name, .. } => Touch::Index(ns.clone(), name.clone()),
            Op::Constraint { ns, name, .. } => Touch::Constraint(ns.clone(), name.clone()),
            Op::Principal { name, .. } | Op::Account { name, .. } => Touch::Account(name.clone()),
            Op::ApiKey { name, .. } => Touch::ApiKey(name.clone()),
            Op::Session { id, .. } => Touch::Session(id.clone()),
            Op::Role { name, .. } => Touch::Role(name.clone()),
            Op::Audit { seq, .. } => Touch::Audit(*seq),
            Op::Namespace { ns, .. } => Touch::Namespace(ns.clone()),
            Op::MatchRule { ns, label, .. } => Touch::MatchRule(ns.clone(), label.clone()),
            Op::State { ns, kind, key, .. } => Touch::State(ns.clone(), kind.clone(), key.clone()),
            Op::Merge { ns, id, .. } => Touch::Merge(ns.clone(), id.parse().unwrap_or(0)),
            Op::Change { ns, seq, .. } => Touch::Change(ns.clone(), *seq),
            Op::Bulk(bulk) => Touch::Bulk(bulk.clone()),
        }
    }
}

/// A committed write transaction, as its effects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub format: u32,
    /// The layout version of the database that wrote it.
    pub layout: u64,
    /// The commit's clock: the change log entries it makes carry it.
    pub at: Timestamp,
    pub ops: Vec<Op>,
    /// The change log entries the commit wrote, per namespace (without
    /// their sequence numbers, given again in the same order on apply), so
    /// that every member's change log is the same.
    #[serde(default)]
    pub changes: Vec<(String, crate::changes::Change)>,
}

impl Entry {
    /// The effects on namespace `ns`, in order.
    pub fn ops_of(&self, ns: &str) -> Vec<Op> {
        self.ops
            .iter()
            .filter(|op| op.namespace() == Some(ns))
            .cloned()
            .collect()
    }

    /// The change log entries the commit wrote in `ns`, in order.
    pub fn changes_of(&self, ns: &str) -> Vec<(String, crate::changes::Change)> {
        self.changes
            .iter()
            .filter(|(of, _)| of == ns)
            .cloned()
            .collect()
    }
}

/// Where a database stands in its replication log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LogPosition {
    pub enabled: bool,
    /// The first entry still kept (0 when none is).
    pub first: u64,
    /// The last entry written.
    pub last: u64,
    /// The last entry applied to the data (equal to `last` on the server
    /// that wrote them).
    pub applied: u64,
}

fn meta_get(table: &impl ReadableTable<&'static str, u64>, key: &str) -> Result<u64> {
    Ok(table
        .get(key)
        .map_err(storage)?
        .map_or(0, |guard| guard.value()))
}

fn json_of(bytes: &[u8]) -> Result<Json> {
    serde_json::from_slice(bytes).map_err(|e| Error::Serialization(e.to_string()))
}

fn bytes_of(value: &Json) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| Error::Serialization(e.to_string()))
}

impl Touch {
    fn ns(&self) -> Option<&str> {
        match self {
            Touch::Node(ns, _)
            | Touch::Edge(ns, _)
            | Touch::Text(ns, _)
            | Touch::Vector(ns, _)
            | Touch::Memory(ns, _)
            | Touch::Index(ns, _)
            | Touch::Constraint(ns, _)
            | Touch::Namespace(ns)
            | Touch::MatchRule(ns, _)
            | Touch::State(ns, _, _)
            | Touch::Merge(ns, _)
            | Touch::Change(ns, _) => Some(ns),
            Touch::Account(_)
            | Touch::ApiKey(_)
            | Touch::Session(_)
            | Touch::Role(_)
            | Touch::Audit(_)
            | Touch::Bulk(_) => None,
        }
    }
}

impl WriteTx {
    /// Notes that this transaction wrote a primary record (when the log is
    /// on, and outside bulk work).
    pub(crate) fn note(&mut self, touch: Touch) {
        if let Some(capture) = self.capture.as_mut()
            && capture.paused == 0
            && capture.seen.insert(touch.clone())
        {
            capture.keys.push(touch);
        }
    }

    /// Runs bulk work, logged as `bulk` instead of the records it writes.
    pub(crate) fn bulk<T>(
        &mut self,
        bulk: Bulk,
        work: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if let Some(capture) = self.capture.as_mut() {
            if capture.paused == 0 {
                capture.keys.push(Touch::Bulk(bulk));
            }
            capture.paused += 1;
        }
        let result = work(self);
        if let Some(capture) = self.capture.as_mut() {
            capture.paused -= 1;
        }
        result
    }

    /// The final value of a touched record.
    fn after_image(&self, touch: &Touch) -> Result<Op> {
        let tx = &self.tx;
        Ok(match touch {
            Touch::Node(ns, id) => Op::Node {
                ns: ns.clone(),
                id: NodeId::from_u128(*id),
                node: tx
                    .open_table(NODES)
                    .map_err(storage)?
                    .get((ns.as_str(), *id))
                    .map_err(storage)?
                    .map(|guard| decode(guard.value()))
                    .transpose()?,
            },
            Touch::Edge(ns, id) => Op::Edge {
                ns: ns.clone(),
                id: EdgeId::from_u128(*id),
                edge: tx
                    .open_table(EDGES)
                    .map_err(storage)?
                    .get((ns.as_str(), *id))
                    .map_err(storage)?
                    .map(|guard| decode(guard.value()))
                    .transpose()?,
            },
            Touch::Text(ns, id) => Op::Text {
                ns: ns.clone(),
                id: NodeId::from_u128(*id),
                text: self.text(ns, NodeId::from_u128(*id))?,
            },
            Touch::Vector(ns, id) => Op::Vector {
                ns: ns.clone(),
                id: NodeId::from_u128(*id),
                embedding: self.embedding(ns, NodeId::from_u128(*id))?,
            },
            Touch::Memory(ns, id) => Op::Memory {
                ns: ns.clone(),
                id: NodeId::from_u128(*id),
                state: tx
                    .open_table(MEMORY)
                    .map_err(storage)?
                    .get((ns.as_str(), *id))
                    .map_err(storage)?
                    .map(|guard| guard.value()),
            },
            Touch::Index(ns, name) => Op::Index {
                ns: ns.clone(),
                name: name.clone(),
                def: tx
                    .open_table(crate::index::INDEXES)
                    .map_err(storage)?
                    .get((ns.as_str(), name.as_str()))
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::Constraint(ns, name) => Op::Constraint {
                ns: ns.clone(),
                name: name.clone(),
                constraint: tx
                    .open_table(crate::constraints::CONSTRAINTS)
                    .map_err(storage)?
                    .get((ns.as_str(), name.as_str()))
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::Account(name) => Op::Account {
                name: name.clone(),
                account: tx
                    .open_table(crate::access::ACCOUNTS)
                    .map_err(storage)?
                    .get(name.as_str())
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::ApiKey(name) => Op::ApiKey {
                name: name.clone(),
                key: tx
                    .open_table(crate::access::API_KEYS)
                    .map_err(storage)?
                    .get(name.as_str())
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::Audit(seq) => Op::Audit {
                seq: *seq,
                event: tx
                    .open_table(crate::audit::AUDIT)
                    .map_err(storage)?
                    .get(*seq)
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::Role(name) => Op::Role {
                name: name.clone(),
                role: tx
                    .open_table(crate::rights::ROLES)
                    .map_err(storage)?
                    .get(name.as_str())
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::Session(id) => Op::Session {
                id: id.clone(),
                session: tx
                    .open_table(crate::access::SESSIONS)
                    .map_err(storage)?
                    .get(id.as_str())
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::Namespace(ns) => Op::Namespace {
                ns: ns.clone(),
                meta: tx
                    .open_table(crate::namespace::NAMESPACES)
                    .map_err(storage)?
                    .get(ns.as_str())
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::MatchRule(ns, label) => Op::MatchRule {
                ns: ns.clone(),
                label: label.clone(),
                rule: tx
                    .open_table(crate::resolve::MATCH_RULES)
                    .map_err(storage)?
                    .get((ns.as_str(), label.as_str()))
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::State(ns, kind, key) => Op::State {
                ns: ns.clone(),
                kind: kind.clone(),
                key: key.clone(),
                value: self.state(ns, kind, key)?,
            },
            Touch::Merge(ns, id) => Op::Merge {
                ns: ns.clone(),
                id: id.to_string(),
                record: tx
                    .open_table(crate::merge::MERGES)
                    .map_err(storage)?
                    .get((ns.as_str(), *id))
                    .map_err(storage)?
                    .map(|guard| json_of(guard.value()))
                    .transpose()?,
            },
            Touch::Change(ns, seq) => Op::Change {
                ns: ns.clone(),
                seq: *seq,
                change: tx
                    .open_table(crate::changes::CHANGES)
                    .map_err(storage)?
                    .get((ns.as_str(), *seq))
                    .map_err(storage)?
                    .map(|guard| decode(guard.value()))
                    .transpose()?,
            },
            Touch::Bulk(bulk) => Op::Bulk(bulk.clone()),
        })
    }

    /// Writes effects copied from another database (a namespace moving
    /// between cluster groups): records as they are, derived data rebuilt,
    /// no rules run and no change log entries made but those copied
    /// (`Op::Change`) and `changes` (appended as this commit's, numbered
    /// after the copied ones, stamped `at`: the copied commit's clock).
    /// Noted for this database's own log, so a cluster's leader replicates
    /// them as any write.
    pub fn import_ops(
        &mut self,
        ops: &[Op],
        changes: Vec<(String, crate::changes::Change)>,
        at: Option<Timestamp>,
    ) -> Result<()> {
        if at.is_some() {
            self.commit_at = at;
        }
        self.past_fences = true;
        let logging = self.log_changes;
        self.log_changes = false;
        self.applying = true;
        let result = ops.iter().try_for_each(|op| {
            self.note(op.touch());
            self.apply_op(op)
        });
        self.applying = false;
        self.log_changes = logging;
        result?;
        if !changes.is_empty() {
            self.pending_changes = crate::changes::Pending::from_entries(changes);
        }
        Ok(())
    }

    /// Ends a transaction begun with [`crate::Grafeno::begin_captured`]
    /// without committing it, and returns what it did as a log entry
    /// (`None`: nothing). The entry is what a cluster replicates.
    pub fn into_entry(mut self) -> Result<Option<Entry>> {
        let entry = self.build_entry();
        self.abort()?;
        entry
    }

    /// What this transaction did, as a log entry (`None`: nothing).
    pub(crate) fn build_entry(&mut self) -> Result<Option<Entry>> {
        let Some(capture) = self.capture.take() else {
            return Ok(None);
        };
        if capture.keys.is_empty() {
            return Ok(None);
        }
        let mut ops = Vec::with_capacity(capture.keys.len());
        let mut namespaces = BTreeSet::new();
        for touch in &capture.keys {
            if let Some(ns) = touch.ns() {
                namespaces.insert(ns.to_owned());
            }
            ops.push(self.after_image(touch)?);
        }
        // A namespace fenced for a move takes no more writes here: they
        // would miss the copy.
        if !self.past_fences {
            for ns in &namespaces {
                if self.state(ns, FENCE.0, FENCE.1)?.is_some() {
                    return Err(Error::Busy(format!(
                        "namespace {ns:?} is moving to another cluster group: try again"
                    )));
                }
            }
        }
        // Namespaces registered on the way (by a first write to them) keep
        // the creation time they got here.
        for ns in namespaces {
            if !capture.seen.contains(&Touch::Namespace(ns.clone())) {
                ops.push(self.after_image(&Touch::Namespace(ns))?);
            }
        }
        // Record policies and account attributes (14.4) would be dropped,
        // unseen, by a member that does not know them.
        let nonempty = |value: &Option<Json>, field: &str| {
            value.as_ref().and_then(|v| v.get(field)).is_some_and(|v| {
                v.as_array().is_some_and(|a| !a.is_empty())
                    || v.as_object().is_some_and(|o| !o.is_empty())
            })
        };
        let format = if ops.iter().any(|op| matches!(op, Op::Audit { .. })) {
            6
        } else if ops.iter().any(|op| match op {
            Op::Role { role, .. } => nonempty(role, "policies"),
            Op::Account { account, .. } => nonempty(account, "attributes"),
            _ => false,
        }) {
            5
        } else if ops.iter().any(|op| matches!(op, Op::Role { .. })) {
            4
        } else if ops.iter().any(|op| {
            matches!(
                op,
                Op::Account { .. } | Op::ApiKey { .. } | Op::Session { .. }
            )
        }) {
            3
        } else if ops.iter().any(|op| matches!(op, Op::Change { .. })) {
            2
        } else {
            1
        };
        Ok(Some(Entry {
            format,
            layout: SCHEMA_VERSION,
            // An import keeps the clock of the commit it copies.
            at: self.commit_at.unwrap_or_else(Timestamp::now),
            ops,
            changes: self.pending_changes.entries().to_vec(),
        }))
    }

    /// Appends what this transaction did to the log, as the entry after
    /// the last one, and returns the commit clock to stamp its changes with.
    pub(crate) fn log_entry(&mut self) -> Result<Option<Timestamp>> {
        let Some(entry) = self.build_entry()? else {
            return Ok(None);
        };
        let at = entry.at;
        let mut meta = self.tx.open_table(META).map_err(storage)?;
        let index = meta_get(&meta, LAST)? + 1;
        if meta_get(&meta, FIRST)? == 0 {
            meta.insert(FIRST, index).map_err(storage)?;
        }
        meta.insert(LAST, index).map_err(storage)?;
        meta.insert(APPLIED, index).map_err(storage)?;
        drop(meta);
        self.tx
            .open_table(REPLICATION_LOG)
            .map_err(storage)?
            .insert(index, encode(&entry)?.as_slice())
            .map_err(storage)?;
        Ok(Some(at))
    }

    /// Applies the entry at `index` of another database's log: writes its
    /// records, derives the rest, keeps the entry in this database's log
    /// and records the position. An entry already applied is skipped
    /// (`false`); one past the next is an error (entries are missing).
    pub fn apply_entry(&mut self, index: u64, entry: &Entry) -> Result<bool> {
        // The format decides, not the layout of the database that wrote it:
        // entries are records, whatever layout stores them (a layout that
        // stores records differently comes with a new format).
        if entry.format > ENTRY_FORMAT {
            return Err(Error::Invalid(format!(
                "log entry {index} has format {}, newer than this Grafeno reads ({ENTRY_FORMAT})",
                entry.format
            )));
        }
        let applied = meta_get(&self.tx.open_table(META).map_err(storage)?, APPLIED)?;
        if index <= applied {
            return Ok(false);
        }
        if index != applied + 1 {
            return Err(Error::Invalid(format!(
                "log entry {index} cannot follow {applied}: entries are missing"
            )));
        }
        // What this applies is logged as the entry itself, not again.
        let capture = self.capture.take();
        self.applying = true;
        self.commit_at = Some(entry.at);
        let result = entry.ops.iter().try_for_each(|op| self.apply_op(op));
        // The change log gets the leader's entries, not those derived here.
        self.pending_changes = crate::changes::Pending::from_entries(entry.changes.clone());
        self.applying = false;
        self.capture = None;
        drop(capture);
        result?;
        let mut meta = self.tx.open_table(META).map_err(storage)?;
        if meta_get(&meta, FIRST)? == 0 {
            meta.insert(FIRST, index).map_err(storage)?;
        }
        let last = meta_get(&meta, LAST)?.max(index);
        meta.insert(LAST, last).map_err(storage)?;
        meta.insert(APPLIED, index).map_err(storage)?;
        meta.insert(ENABLED, 1).map_err(storage)?;
        drop(meta);
        self.tx
            .open_table(REPLICATION_LOG)
            .map_err(storage)?
            .insert(index, encode(entry)?.as_slice())
            .map_err(storage)?;
        Ok(true)
    }

    fn apply_op(&mut self, op: &Op) -> Result<()> {
        match op {
            Op::Node { ns, id, node } => {
                let previous = self.node(ns, *id)?;
                match (node, previous) {
                    (Some(node), previous) => self.put_node(ns, node, previous.as_ref())?,
                    (None, Some(previous)) => self.remove_node_record(ns, &previous)?,
                    (None, None) => {}
                }
            }
            Op::Edge { ns, id, edge } => {
                let existed = self.edge(ns, *id)?.is_some();
                match edge {
                    Some(edge) => {
                        self.put_edge(ns, edge)?;
                        if !existed {
                            crate::stats::bump_namespace(&self.tx, ns, 0, 1)?;
                            crate::stats::bump_edge_label(&self.tx, ns, &edge.label, 1)?;
                        }
                    }
                    None if existed => self.delete_edge(ns, *id)?,
                    None => {}
                }
            }
            Op::Text { ns, id, text } => {
                if self.text(ns, *id)? != *text {
                    match text {
                        Some(text) => crate::text::index(&self.tx, ns, *id, text)?,
                        None => {
                            crate::text::remove(&self.tx, ns, *id)?;
                        }
                    }
                    self.log_node_part(ns, *id, crate::changes::Part::Text)?;
                }
            }
            Op::Vector { ns, id, embedding } => {
                if self.embedding(ns, *id)? != *embedding {
                    match embedding {
                        Some(embedding) => {
                            crate::vector::insert(&self.tx, &mut self.vectors, ns, *id, embedding)?
                        }
                        None => {
                            crate::vector::remove(&self.tx, &mut self.vectors, ns, *id)?;
                        }
                    }
                    self.log_node_part(ns, *id, crate::changes::Part::Embedding)?;
                }
            }
            Op::Memory { ns, id, state } => {
                let mut table = self.tx.open_table(MEMORY).map_err(storage)?;
                match state {
                    Some(state) => {
                        table
                            .insert((ns.as_str(), id.as_u128()), *state)
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove((ns.as_str(), id.as_u128())).map_err(storage)?;
                    }
                }
            }
            Op::Index { ns, name, def } => {
                let current = self
                    .index_defs(ns)?
                    .into_iter()
                    .find(|existing| existing.name == *name);
                let wanted: Option<crate::IndexDef> = def
                    .as_ref()
                    .map(|def| serde_json::from_value(def.clone()))
                    .transpose()
                    .map_err(|e| Error::Serialization(e.to_string()))?;
                if current != wanted {
                    if current.is_some() {
                        self.drop_index(ns, name)?;
                    }
                    if let Some(wanted) = wanted {
                        self.create_index(ns, wanted)?;
                    }
                }
            }
            Op::Constraint {
                ns,
                name,
                constraint,
            } => {
                self.constraint_cache = None;
                let mut table = self
                    .tx
                    .open_table(crate::constraints::CONSTRAINTS)
                    .map_err(storage)?;
                match constraint {
                    Some(value) => {
                        table
                            .insert((ns.as_str(), name.as_str()), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table
                            .remove((ns.as_str(), name.as_str()))
                            .map_err(storage)?;
                    }
                }
            }
            Op::Principal { name, principal } => match principal {
                // A member of layout 14 or before wrote a key: an account
                // and a key here.
                Some(value) => {
                    let principal: crate::access::Principal = serde_json::from_value(value.clone())
                        .map_err(|e| Error::Serialization(e.to_string()))?;
                    let (account, key) = principal.split();
                    self.put_account(&account)?;
                    self.put_api_key(&key)?;
                }
                None => {
                    self.delete_api_key(name)?;
                    self.delete_account(name)?;
                }
            },
            Op::Account { name, account } => {
                let mut table = self
                    .tx
                    .open_table(crate::access::ACCOUNTS)
                    .map_err(storage)?;
                match account {
                    Some(value) => {
                        table
                            .insert(name.as_str(), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove(name.as_str()).map_err(storage)?;
                    }
                }
                drop(table);
                self.bump_access()?;
            }
            Op::Audit { seq, event } => {
                let bytes = event.as_ref().map(bytes_of).transpose()?;
                self.apply_audit(*seq, bytes.as_deref())?;
            }
            Op::Role { name, role } => {
                let mut table = self.tx.open_table(crate::rights::ROLES).map_err(storage)?;
                match role {
                    Some(value) => {
                        table
                            .insert(name.as_str(), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove(name.as_str()).map_err(storage)?;
                    }
                }
                drop(table);
                self.bump_access()?;
            }
            Op::ApiKey { name, key } => {
                let mut table = self
                    .tx
                    .open_table(crate::access::API_KEYS)
                    .map_err(storage)?;
                match key {
                    Some(value) => {
                        table
                            .insert(name.as_str(), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove(name.as_str()).map_err(storage)?;
                    }
                }
            }
            Op::Session { id, session } => {
                let mut table = self
                    .tx
                    .open_table(crate::access::SESSIONS)
                    .map_err(storage)?;
                match session {
                    Some(value) => {
                        table
                            .insert(id.as_str(), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove(id.as_str()).map_err(storage)?;
                    }
                }
            }
            Op::Namespace { ns, meta } => {
                let mut table = self
                    .tx
                    .open_table(crate::namespace::NAMESPACES)
                    .map_err(storage)?;
                match meta {
                    Some(value) => {
                        table
                            .insert(ns.as_str(), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove(ns.as_str()).map_err(storage)?;
                    }
                }
            }
            Op::MatchRule { ns, label, rule } => {
                let mut table = self
                    .tx
                    .open_table(crate::resolve::MATCH_RULES)
                    .map_err(storage)?;
                match rule {
                    Some(value) => {
                        table
                            .insert((ns.as_str(), label.as_str()), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table
                            .remove((ns.as_str(), label.as_str()))
                            .map_err(storage)?;
                    }
                }
            }
            Op::State {
                ns,
                kind,
                key,
                value,
            } => match value {
                Some(value) => self.put_state(ns, kind, key, value)?,
                None => {
                    self.delete_state(ns, kind, key)?;
                }
            },
            Op::Merge { ns, id, record } => {
                let id: u128 = id
                    .parse()
                    .map_err(|_| Error::Serialization(format!("bad merge id {id:?}")))?;
                let mut table = self.tx.open_table(crate::merge::MERGES).map_err(storage)?;
                match record {
                    Some(value) => {
                        table
                            .insert((ns.as_str(), id), bytes_of(value)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove((ns.as_str(), id)).map_err(storage)?;
                    }
                }
            }
            Op::Change { ns, seq, change } => {
                let mut table = self
                    .tx
                    .open_table(crate::changes::CHANGES)
                    .map_err(storage)?;
                match change {
                    Some(change) => {
                        table
                            .insert((ns.as_str(), *seq), encode(change)?.as_slice())
                            .map_err(storage)?;
                    }
                    None => {
                        table.remove((ns.as_str(), *seq)).map_err(storage)?;
                    }
                }
            }
            Op::Bulk(Bulk::DropNamespace { ns }) => {
                if self.namespace(ns)?.is_some() {
                    self.drop_namespace(ns)?;
                }
            }
            Op::Bulk(Bulk::RebuildIndexes { ns }) => {
                self.rebuild_indexes(ns)?;
            }
            Op::Bulk(Bulk::CompactVectors { ns }) => {
                self.compact_vectors(ns)?;
            }
        }
        Ok(())
    }

    /// Drops the log's entries up to `through` (kept in a snapshot): a
    /// member that needs them catches up from a backup instead.
    pub fn truncate_log(&mut self, through: u64) -> Result<u64> {
        let mut meta = self.tx.open_table(META).map_err(storage)?;
        let (first, last) = (meta_get(&meta, FIRST)?, meta_get(&meta, LAST)?);
        let through = through.min(last);
        if first == 0 || through < first {
            return Ok(0);
        }
        let mut log = self.tx.open_table(REPLICATION_LOG).map_err(storage)?;
        let mut dropped = 0;
        for index in first..=through {
            if log.remove(index).map_err(storage)?.is_some() {
                dropped += 1;
            }
        }
        let next = if through == last { 0 } else { through + 1 };
        meta.insert(FIRST, next).map_err(storage)?;
        Ok(dropped)
    }
}

impl ReadTx {
    /// Where this database stands in its replication log.
    pub fn log_position(&self) -> Result<LogPosition> {
        let meta = self.tx.open_table(META).map_err(storage)?;
        Ok(LogPosition {
            enabled: meta_get(&meta, ENABLED)? == 1,
            first: meta_get(&meta, FIRST)?,
            last: meta_get(&meta, LAST)?,
            applied: meta_get(&meta, APPLIED)?,
        })
    }

    /// Every primary record of `ns` and its change log, as effects, handed
    /// to `take` about `chunk` at a time: what copying the namespace to
    /// another database takes ([`WriteTx::import_ops`] writes them there).
    pub fn namespace_ops(
        &self,
        ns: &str,
        chunk: usize,
        mut take: impl FnMut(Vec<Op>) -> Result<()>,
    ) -> Result<()> {
        let chunk = chunk.max(1);
        let mut ops: Vec<Op> = Vec::with_capacity(chunk);
        let mut push = |op: Op, ops: &mut Vec<Op>| -> Result<()> {
            ops.push(op);
            if ops.len() >= chunk {
                take(std::mem::take(ops))?;
            }
            Ok(())
        };
        let owned = |key: &str| key.to_owned();
        if let Some(meta) = self.registry.get(ns).map_err(storage)? {
            push(
                Op::Namespace {
                    ns: owned(ns),
                    meta: Some(json_of(meta.value())?),
                },
                &mut ops,
            )?;
        }
        // Definitions first, so records are indexed as they arrive.
        for item in self.indexes.range((ns, "")..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let (of, name) = key.value();
            if of != ns {
                break;
            }
            push(
                Op::Index {
                    ns: owned(ns),
                    name: owned(name),
                    def: Some(json_of(value.value())?),
                },
                &mut ops,
            )?;
        }
        for item in self.constraints.range((ns, "")..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let (of, name) = key.value();
            if of != ns {
                break;
            }
            push(
                Op::Constraint {
                    ns: owned(ns),
                    name: owned(name),
                    constraint: Some(json_of(value.value())?),
                },
                &mut ops,
            )?;
        }
        for item in self.match_rules.range((ns, "")..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let (of, label) = key.value();
            if of != ns {
                break;
            }
            push(
                Op::MatchRule {
                    ns: owned(ns),
                    label: owned(label),
                    rule: Some(json_of(value.value())?),
                },
                &mut ops,
            )?;
        }
        let records = (ns, 0u128)..=(ns, u128::MAX);
        for item in self.nodes.range(records.clone()).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            push(
                Op::Node {
                    ns: owned(ns),
                    id: NodeId::from_u128(key.value().1),
                    node: Some(decode(value.value())?),
                },
                &mut ops,
            )?;
        }
        for item in self.edges.range(records.clone()).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            push(
                Op::Edge {
                    ns: owned(ns),
                    id: EdgeId::from_u128(key.value().1),
                    edge: Some(decode(value.value())?),
                },
                &mut ops,
            )?;
        }
        for item in self.texts.range(records.clone()).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            push(
                Op::Text {
                    ns: owned(ns),
                    id: NodeId::from_u128(key.value().1),
                    text: Some(value.value().to_owned()),
                },
                &mut ops,
            )?;
        }
        let vector_ids: Vec<u128> = self
            .vectors
            .range(records.clone())
            .map_err(storage)?
            .map(|item| item.map(|(key, _)| key.value().1).map_err(storage))
            .collect::<Result<_>>()?;
        for id in vector_ids {
            let id = NodeId::from_u128(id);
            push(
                Op::Vector {
                    ns: owned(ns),
                    id,
                    embedding: self.embedding(ns, id)?,
                },
                &mut ops,
            )?;
        }
        for item in self.memory.range(records.clone()).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            push(
                Op::Memory {
                    ns: owned(ns),
                    id: NodeId::from_u128(key.value().1),
                    state: Some(value.value()),
                },
                &mut ops,
            )?;
        }
        for item in self.merges.range(records).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            push(
                Op::Merge {
                    ns: owned(ns),
                    id: key.value().1.to_string(),
                    record: Some(json_of(value.value())?),
                },
                &mut ops,
            )?;
        }
        for item in self.state.range((ns, "", "")..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let (of, kind, name) = key.value();
            if of != ns {
                break;
            }
            push(
                Op::State {
                    ns: owned(ns),
                    kind: owned(kind),
                    key: owned(name),
                    value: Some(value.value().to_vec()),
                },
                &mut ops,
            )?;
        }
        for item in self
            .changes
            .range((ns, 0u64)..=(ns, u64::MAX))
            .map_err(storage)?
        {
            let (key, value) = item.map_err(storage)?;
            let change: crate::changes::Change = decode(value.value())?;
            push(
                Op::Change {
                    ns: owned(ns),
                    seq: key.value().1,
                    change: Some(change),
                },
                &mut ops,
            )?;
        }
        if !ops.is_empty() {
            take(ops)?;
        }
        Ok(())
    }

    /// Up to `limit` log entries after `after`, in order.
    pub fn log_entries(&self, after: u64, limit: usize) -> Result<Vec<(u64, Entry)>> {
        let table = self.tx.open_table(REPLICATION_LOG).map_err(storage)?;
        let mut out = Vec::new();
        for item in table.range(after.saturating_add(1)..).map_err(storage)? {
            if out.len() >= limit {
                break;
            }
            let (index, value) = item.map_err(storage)?;
            out.push((index.value(), decode(value.value())?));
        }
        Ok(out)
    }
}

impl WriteTx {
    /// Fences `ns` for a move to cluster group `to`: from this write on,
    /// every other write to it here is refused (busy), so the entries up
    /// to this one hold all it will ever get here.
    pub fn fence_namespace(&mut self, ns: &str, to: &str) -> Result<()> {
        self.past_fences = true;
        self.put_state(ns, FENCE.0, FENCE.1, to.as_bytes())
    }

    /// Lifts the fence (a move given up).
    pub fn unfence_namespace(&mut self, ns: &str) -> Result<()> {
        self.past_fences = true;
        self.delete_state(ns, FENCE.0, FENCE.1).map(|_| ())
    }

    /// Drops what a moved namespace left here and keeps its fence, so a
    /// late write meant for it is refused rather than lost.
    pub fn drop_moved_namespace(&mut self, ns: &str, to: &str) -> Result<()> {
        self.past_fences = true;
        if self.namespace(ns)?.is_some() {
            self.drop_namespace(ns)?;
        }
        self.put_state(ns, FENCE.0, FENCE.1, to.as_bytes())
    }
}
