//! Bringing a database's storage up to date (docs/roadmap/
//! STORAGE_AND_INDEXING_OPTIMIZATION.md, phase 4): a plan of steps per
//! namespace, run in batches, each batch a transaction that also records
//! how far the migration got, so that an interrupted one resumes where it
//! stopped and a finished one, run again, changes nothing.
//!
//! The steps:
//!
//! - **reencode**: rewrite the node, edge and hyperedge records whose
//!   stored form is not the one asked for (the JSON of every layout so
//!   far, or the versioned envelope), in key order. Only the bytes change:
//!   each record is decoded again after rewriting and compared with what
//!   it was, in the same transaction.
//! - **reindex**: rebuild the namespace's indexes (their entries inserted
//!   in key order: the space earlier inserts left half empty in pages
//!   comes back with a compaction).
//! - **verify**: `check` the namespace, and on an exclusive run (a file no
//!   server has open) compare a digest of every record's logical value
//!   with the one taken when the plan was made.
//!
//! Migrations are physical: they change how this member stores its file,
//! not what it holds, so they are not replicated; each member of a cluster
//! migrates its own file. The registry lives with the file, in the state
//! store, under a name no namespace can have.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use grafeno_core::{Error, Result, Timestamp};
use redb::{ReadableTable, ReadableTableMetadata};
use serde::{Deserialize, Serialize};

use crate::compress::StoredKind;
use crate::db::{Grafeno, ReadTx, WriteTx};
use crate::store::{Codec, RecordKey, envelope, is_enveloped, storage, unwrap_envelope};

/// Where the registry is kept in the state store.
pub(crate) const MIGRATIONS_NS: &str = "\u{1}migrations";
const KIND: &str = "migration";

/// How records are stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// JSON as every layout so far wrote it; what this build writes, and
    /// what every earlier build reads.
    #[default]
    Json,
    /// The versioned envelope, uncompressed: read by this build and later
    /// ones only.
    Envelope,
    /// Compressed where it pays (Zstandard, with a dictionary per
    /// namespace and kind of value, trained first): what this build writes
    /// with compression on.
    Compressed,
}

/// What a migration should do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationOptions {
    /// Namespaces to migrate; empty: every one.
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// Rewrite records stored otherwise into this encoding.
    #[serde(default)]
    pub encoding: Option<Encoding>,
    /// Rebuild every index.
    #[serde(default)]
    pub reindex: bool,
    /// Check each namespace at the end.
    #[serde(default)]
    pub verify: bool,
    /// Records per transaction (10,000 when 0).
    #[serde(default)]
    pub batch_size: usize,
    /// With [`Encoding::Compressed`]: train new dictionaries even where
    /// there are some.
    #[serde(default)]
    pub retrain: bool,
}

/// What a step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Train the dictionaries the namespace (or the file's logs) has none
    /// of yet.
    Train,
    Reencode,
    Reindex,
    Verify,
}

/// Where a migration or a step stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationState {
    Pending,
    InProgress,
    Paused,
    Completed,
    Failed,
    RollbackRequired,
}

/// One step of a plan, and how far it got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStep {
    pub kind: StepKind,
    pub namespace: String,
    /// Records it is estimated to touch (exact for counts, sampled for
    /// what needs rewriting).
    pub estimate: u64,
    pub state: MigrationState,
    /// Records rewritten, or indexes rebuilt.
    pub done: u64,
    /// The last key handled (table and id), for resuming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For a verify step: what `check` and the digest said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// A migration: its plan, its state and what it did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Migration {
    pub id: String,
    pub options: MigrationOptions,
    pub engine: String,
    pub layout: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub state: MigrationState,
    pub steps: Vec<MigrationStep>,
    /// Per namespace: nodes, edges and hyperedges, and a digest of their
    /// logical values, when the plan was made.
    pub before: Vec<NamespaceDigest>,
    /// The file's layout once finished (a migration back to JSON of the
    /// whole file brings back the one before envelopes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_after: Option<u64>,
}

/// What a namespace held when a plan was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceDigest {
    pub namespace: String,
    pub nodes: u64,
    pub edges: u64,
    pub hyperedges: u64,
    /// Over every record's decoded value, in key order (hex).
    pub digest: String,
}

/// How a run is driven.
#[derive(Clone, Default)]
pub struct RunControl {
    /// Set to pause the run at the end of the batch in progress.
    pub stop: Option<Arc<AtomicBool>>,
    /// Stop after this many batches (pausing), for tests.
    pub max_batches: Option<usize>,
    /// Nothing else has the file open: the digest can be compared.
    pub exclusive: bool,
}

/// The tables a reencode step goes through: a namespace's records, texts
/// and change log, or (the step of namespace "") the file's logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Nodes,
    Edges,
    HyperEdges,
    Texts,
    TextValues,
    Changes,
    Audit,
    Replication,
}

impl Slot {
    const NAMESPACE: [Slot; 6] = [
        Slot::Nodes,
        Slot::Edges,
        Slot::HyperEdges,
        Slot::Texts,
        Slot::TextValues,
        Slot::Changes,
    ];
    const FILE: [Slot; 2] = [Slot::Audit, Slot::Replication];

    fn name(self) -> &'static str {
        match self {
            Slot::Nodes => "nodes",
            Slot::Edges => "edges",
            Slot::HyperEdges => "hyperedges",
            Slot::Texts => "texts",
            Slot::TextValues => "text_values",
            Slot::Changes => "changes",
            Slot::Audit => "audit",
            Slot::Replication => "replication_log",
        }
    }

    fn kind(self) -> StoredKind {
        match self {
            Slot::Nodes => StoredKind::Node,
            Slot::Edges => StoredKind::Edge,
            Slot::HyperEdges => StoredKind::HyperEdge,
            Slot::Texts | Slot::TextValues => StoredKind::Text,
            Slot::Changes => StoredKind::Change,
            Slot::Audit => StoredKind::Audit,
            Slot::Replication => StoredKind::Replication,
        }
    }

    fn records(self) -> Option<redb::TableDefinition<'static, RecordKey, &'static [u8]>> {
        match self {
            Slot::Nodes => Some(crate::store::NODES),
            Slot::Edges => Some(crate::store::EDGES),
            Slot::HyperEdges => Some(crate::hyperedge::HYPEREDGES),
            _ => None,
        }
    }

    fn sequence(self) -> Option<redb::TableDefinition<'static, u64, &'static [u8]>> {
        match self {
            Slot::Audit => Some(crate::audit::AUDIT),
            Slot::Replication => Some(crate::replication::REPLICATION_LOG),
            _ => None,
        }
    }
}

fn decode_migration(bytes: &[u8]) -> Result<Migration> {
    serde_json::from_slice(bytes).map_err(|e| Error::Serialization(e.to_string()))
}

/// The codec of a stored value's envelope, `None` without one.
fn codec_of(bytes: &[u8]) -> Option<u8> {
    is_enveloped(bytes).then(|| bytes.get(2).copied().unwrap_or(0))
}

/// Whether a stored value may need rewriting into `encoding` (for the
/// plan's estimate: a compressed one is rewritten when it was not
/// compressed with a dictionary).
fn may_need(bytes: &[u8], encoding: Encoding) -> bool {
    match (encoding, codec_of(bytes)) {
        (Encoding::Json, codec) => codec.is_some(),
        (Encoding::Envelope, codec) => codec != Some(0),
        (Encoding::Compressed, codec) => codec != Some(3),
    }
}

/// `raw` (a value's decoded bytes) as `encoding` stores it in `tx`.
fn target(
    tx: &WriteTx,
    ns: &str,
    kind: StoredKind,
    raw: &[u8],
    encoding: Encoding,
    always: bool,
) -> Vec<u8> {
    match encoding {
        Encoding::Json => raw.to_vec(),
        Encoding::Envelope => {
            tx.wrote_envelope();
            envelope(raw, Codec::Plain)
        }
        Encoding::Compressed => {
            let (bytes, enveloped) =
                tx.shared
                    .compression
                    .pack_as(ns, kind, raw.to_vec(), always, true);
            if enveloped {
                tx.wrote_envelope();
            }
            bytes
        }
    }
}

/// A stable 64-bit hash (FNV-1a), for digests kept in the file.
fn fnv(parts: &[&[u8]]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0xFF;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl ReadTx {
    /// Every migration recorded in this file, oldest first.
    pub fn migrations(&self) -> Result<Vec<Migration>> {
        self.state_entries(MIGRATIONS_NS, KIND, "", usize::MAX)?
            .into_iter()
            .map(|(_, bytes)| decode_migration(&bytes))
            .collect()
    }

    /// What a namespace holds, with a digest of its records', texts' and
    /// change log entries' decoded values (the same whatever encoding
    /// stores them).
    pub fn namespace_digest(&self, ns: &str) -> Result<NamespaceDigest> {
        let end = format!("{ns}\0");
        let records = (ns, 0u128)..(end.as_str(), 0u128);
        let mut digest = 0u64;
        let mut counts = [0u64; 3];
        let mut add = |slot: u8, key: &[u8], value: &[u8]| {
            digest = digest.wrapping_add(fnv(&[&[slot], key, value]));
        };
        for (at, slot) in [Slot::Nodes, Slot::Edges, Slot::HyperEdges]
            .into_iter()
            .enumerate()
        {
            let table = self
                .tx
                .open_table(slot.records().expect("a record table"))
                .map_err(storage)?;
            for item in table.range(records.clone()).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                counts[at] += 1;
                add(
                    at as u8,
                    &key.value().1.to_le_bytes(),
                    &unwrap_envelope(value.value())?,
                );
            }
        }
        for item in self.texts.range(records.clone()).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            add(3, &key.value().1.to_le_bytes(), value.value().as_bytes());
        }
        if let Some(values) = &self.text_values {
            for item in values.range(records.clone()).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                add(
                    3,
                    &key.value().1.to_le_bytes(),
                    &unwrap_envelope(value.value())?,
                );
            }
        }
        let changes = self
            .tx
            .open_table(crate::changes::CHANGES)
            .map_err(storage)?;
        for item in changes
            .range((ns, 0u64)..=(ns, u64::MAX))
            .map_err(storage)?
        {
            let (key, value) = item.map_err(storage)?;
            add(
                4,
                &key.value().1.to_le_bytes(),
                &unwrap_envelope(value.value())?,
            );
        }
        Ok(NamespaceDigest {
            namespace: ns.to_owned(),
            nodes: counts[0],
            edges: counts[1],
            hyperedges: counts[2],
            digest: format!("{digest:016x}"),
        })
    }

    /// Values of `ns` (or the file's logs, for "") that may need rewriting
    /// into `encoding`, from a sample of up to `sample` per table, scaled
    /// to the table's values: an estimate.
    fn records_to_reencode(&self, ns: &str, encoding: Encoding, sample: usize) -> Result<u64> {
        let end = format!("{ns}\0");
        let records = (ns, 0u128)..(end.as_str(), 0u128);
        let mut total = 0u64;
        let count = |values: &mut dyn Iterator<Item = Result<Vec<u8>>>| -> Result<u64> {
            let (mut seen, mut other, mut all) = (0usize, 0usize, 0u64);
            for value in values {
                let value = value?;
                all += 1;
                if seen < sample {
                    seen += 1;
                    other += usize::from(may_need(&value, encoding));
                }
            }
            Ok(if seen > 0 {
                (all as f64 * other as f64 / seen as f64).round() as u64
            } else {
                0
            })
        };
        let slots: &[Slot] = if ns.is_empty() {
            &Slot::FILE
        } else {
            &Slot::NAMESPACE
        };
        for slot in slots {
            match slot {
                Slot::Texts => {
                    if encoding != Encoding::Json {
                        // Every text there moves to `text_values`.
                        total += self.texts.range(records.clone()).map_err(storage)?.count() as u64;
                    }
                }
                Slot::TextValues => {
                    if let Some(values) = &self.text_values {
                        if encoding == Encoding::Json {
                            // Every text there moves back to `texts`.
                            total += values.range(records.clone()).map_err(storage)?.count() as u64;
                        } else {
                            total += count(
                                &mut values.range(records.clone()).map_err(storage)?.map(|item| {
                                    let (_, value) = item.map_err(storage)?;
                                    Ok(value.value().to_vec())
                                }),
                            )?;
                        }
                    }
                }
                Slot::Changes => {
                    let table = self
                        .tx
                        .open_table(crate::changes::CHANGES)
                        .map_err(storage)?;
                    total += count(
                        &mut table
                            .range((ns, 0u64)..=(ns, u64::MAX))
                            .map_err(storage)?
                            .map(|item| {
                                let (_, value) = item.map_err(storage)?;
                                Ok(value.value().to_vec())
                            }),
                    )?;
                }
                slot => {
                    if let Some(definition) = slot.records() {
                        let table = self.tx.open_table(definition).map_err(storage)?;
                        total += count(&mut table.range(records.clone()).map_err(storage)?.map(
                            |item| {
                                let (_, value) = item.map_err(storage)?;
                                Ok(value.value().to_vec())
                            },
                        ))?;
                    } else if let Some(definition) = slot.sequence() {
                        let table = self.tx.open_table(definition).map_err(storage)?;
                        total += count(&mut table.iter().map_err(storage)?.map(|item| {
                            let (_, value) = item.map_err(storage)?;
                            Ok(value.value().to_vec())
                        }))?;
                    }
                }
            }
        }
        Ok(total)
    }
}

/// Less space than this is not worth a compaction.
const WORTH_IT: u64 = 1 << 20;

impl Grafeno {
    /// What `options` would do, without changing anything (a dry run): the
    /// steps with their estimates, and what each namespace holds now.
    pub fn plan_migration(&self, options: &MigrationOptions) -> Result<Migration> {
        let read = self.begin_read()?;
        let known: Vec<String> = read
            .namespaces()?
            .into_iter()
            .map(|info| info.name)
            .collect();
        let namespaces = if options.namespaces.is_empty() {
            known.clone()
        } else {
            for ns in &options.namespaces {
                if !known.contains(ns) {
                    return Err(Error::NamespaceNotFound(ns.clone()));
                }
            }
            options.namespaces.clone()
        };
        let mut steps = Vec::new();
        let mut before = Vec::new();
        for ns in &namespaces {
            let digest = read.namespace_digest(ns)?;
            let step = |kind, estimate| MigrationStep {
                kind,
                namespace: ns.clone(),
                estimate,
                state: MigrationState::Pending,
                done: 0,
                cursor: None,
                error: None,
                result: None,
            };
            // Dictionaries first: what the steps after it write uses them.
            if options.encoding == Some(Encoding::Compressed) {
                steps.push(step(
                    StepKind::Train,
                    self.kinds_to_train(ns, options.retrain) as u64,
                ));
            }
            // Rebuilding writes records the way every write does: it goes
            // before the reencoding, so that what that writes stays.
            if options.reindex {
                steps.push(step(StepKind::Reindex, read.index_defs(ns)?.len() as u64));
            }
            if let Some(encoding) = options.encoding {
                let estimate = read.records_to_reencode(ns, encoding, 1_000)?;
                steps.push(step(StepKind::Reencode, estimate));
            }
            if options.verify {
                steps.push(step(
                    StepKind::Verify,
                    digest.nodes + digest.edges + digest.hyperedges,
                ));
            }
            before.push(digest);
        }
        // The file's logs (audit, replication), with the whole file.
        if let Some(encoding) = options.encoding
            && options.namespaces.is_empty()
        {
            let step = |kind, estimate| MigrationStep {
                kind,
                namespace: String::new(),
                estimate,
                state: MigrationState::Pending,
                done: 0,
                cursor: None,
                error: None,
                result: None,
            };
            if encoding == Encoding::Compressed {
                steps.push(step(
                    StepKind::Train,
                    self.kinds_to_train("", options.retrain) as u64,
                ));
            }
            steps.push(step(
                StepKind::Reencode,
                read.records_to_reencode("", encoding, 1_000)?,
            ));
        }
        let now = Timestamp::now();
        Ok(Migration {
            id: format!("m{:x}", now.as_micros()),
            options: options.clone(),
            engine: env!("CARGO_PKG_VERSION").to_owned(),
            layout: self.layout()?,
            created_at: now,
            updated_at: now,
            state: MigrationState::Pending,
            steps,
            before,
            layout_after: None,
        })
    }

    /// The kinds of values of `ns` ("" for the file's logs) a train step
    /// would train a dictionary for, at most.
    fn kinds_to_train(&self, ns: &str, retrain: bool) -> usize {
        StoredKind::ALL
            .into_iter()
            .filter(|kind| kind.global() == ns.is_empty())
            .filter(|kind| retrain || !self.shared.compression.has_dictionary(ns, *kind))
            .count()
    }

    /// Records a plan so that it can be run (and resumed).
    pub fn start_migration(&self, plan: &Migration) -> Result<()> {
        if self
            .begin_read()?
            .migrations()?
            .iter()
            .any(|m| matches!(m.state, MigrationState::InProgress | MigrationState::Paused))
        {
            return Err(Error::Conflict(
                "a migration is in progress or paused: resume it, or let it end, first".into(),
            ));
        }
        let plan = plan.clone();
        self.write(move |tx| tx.save_migration(&plan))
    }

    /// The migration `id`, if it is recorded.
    pub fn migration(&self, id: &str) -> Result<Option<Migration>> {
        Ok(self
            .begin_read()?
            .migrations()?
            .into_iter()
            .find(|m| m.id == id))
    }

    /// Runs (or resumes) migration `id` in this database's own
    /// transactions; see [`crate::Writer::run_migration`] for a server.
    pub fn run_migration(&self, id: &str, control: &RunControl) -> Result<Migration> {
        run(self, &|work| self.write(|tx| work(tx)), id, control)
    }
}

impl crate::Writer {
    /// Runs (or resumes) migration `id` through this writer's bulk lane:
    /// each batch a transaction of its own, after the interactive writes.
    pub fn run_migration(&self, id: &str, control: &RunControl) -> Result<Migration> {
        let db = self.database();
        let outcome = run(
            db,
            &|work| {
                let work = work.clone();
                self.write_bulk(move |tx| work(tx))
            },
            id,
            control,
        );
        self.flush()?;
        outcome
    }
}

/// A batch: it does its work and records the migration in `tx`.
type Work = Arc<dyn Fn(&mut WriteTx) -> Result<bool> + Send + Sync>;

fn run(
    db: &Grafeno,
    commit: &dyn Fn(&Work) -> Result<bool>,
    id: &str,
    control: &RunControl,
) -> Result<Migration> {
    let mut migration = db
        .migration(id)?
        .ok_or_else(|| Error::Invalid(format!("there is no migration {id:?}")))?;
    if migration.state == MigrationState::Completed {
        return Ok(migration);
    }
    let batch = if migration.options.batch_size == 0 {
        10_000
    } else {
        migration.options.batch_size
    };
    let mut batches = 0usize;
    let stopped = |batches: usize| {
        control
            .stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Relaxed))
            || control.max_batches.is_some_and(|max| batches >= max)
    };
    for index in 0..migration.steps.len() {
        if migration.steps[index].state == MigrationState::Completed {
            continue;
        }
        loop {
            if stopped(batches) {
                migration.state = MigrationState::Paused;
                let paused = migration.clone();
                db.write(move |tx| tx.save_migration(&paused))?;
                return db.migration(id).map(|m| m.unwrap_or(migration));
            }
            if migration.steps[index].kind == StepKind::Verify {
                let outcome = verify(db, &migration, index, control.exclusive);
                let step = &mut migration.steps[index];
                match outcome {
                    Ok(result) => {
                        step.done = step.estimate;
                        step.result = Some(result);
                    }
                    Err((state, error)) => {
                        step.state = state;
                        step.error = Some(error.to_string());
                        migration.state = state;
                        migration.updated_at = Timestamp::now();
                        let failed = migration.clone();
                        db.write(move |tx| tx.save_migration(&failed))?;
                        return Ok(migration);
                    }
                }
            }
            // Dictionaries are trained on a snapshot, outside the write;
            // the batch stores them, and writes use them once committed.
            let trained = if migration.steps[index].kind == StepKind::Train {
                db.train_for(&migration.steps[index].namespace, migration.options.retrain)?
            } else {
                Vec::new()
            };
            let adopting = trained.clone();
            let current = migration.clone();
            let encoding = current.options.encoding.unwrap_or_default();
            let work: Work = Arc::new(move |tx: &mut WriteTx| {
                let mut migration = current.clone();
                if migration.steps[index].kind == StepKind::Train {
                    tx.save_dictionaries(&trained)?;
                    migration.steps[index].done = trained.len() as u64;
                }
                let finished = migration_batch(tx, &mut migration, index, batch, encoding);
                let finished = match finished {
                    Ok(finished) => finished,
                    Err(error) => {
                        migration.steps[index].state = MigrationState::Failed;
                        migration.steps[index].error = Some(error.to_string());
                        migration.state = MigrationState::Failed;
                        tx.save_migration(&migration)?;
                        return Ok(true);
                    }
                };
                tx.save_migration(&migration)?;
                Ok(finished)
            });
            let finished = commit(&work)?;
            db.adopt_dictionaries(&adopting);
            batches += 1;
            migration = db
                .migration(id)?
                .ok_or_else(|| Error::Storage(format!("migration {id:?} went away")))?;
            if migration.state == MigrationState::Failed {
                return Ok(migration);
            }
            if finished {
                break;
            }
        }
    }
    // Back to JSON, the whole file, with no write compressing meanwhile:
    // the layout before envelopes comes back.
    if migration.options.encoding == Some(Encoding::Json)
        && migration.options.namespaces.is_empty()
        && db.compression() == crate::Compression::Off
    {
        match db.write(|tx| tx.leave_envelope_layout())? {
            Ok(_) => db.shared.compression.forget(),
            Err(why) => {
                if let Some(step) = migration.steps.last_mut() {
                    let note = format!("the layout stays {}: {why}", crate::SCHEMA_VERSION);
                    step.result = Some(match &step.result {
                        Some(result) => format!("{result}; {note}"),
                        None => note,
                    });
                }
            }
        }
    }
    migration.layout_after = Some(db.layout()?);
    migration.state = MigrationState::Completed;
    migration.updated_at = Timestamp::now();
    let done = migration.clone();
    db.write(move |tx| tx.save_migration(&done))?;
    Ok(migration)
}

/// One batch of step `index`; whether the step is finished.
fn migration_batch(
    tx: &mut WriteTx,
    migration: &mut Migration,
    index: usize,
    batch: usize,
    encoding: Encoding,
) -> Result<bool> {
    migration.state = MigrationState::InProgress;
    migration.updated_at = Timestamp::now();
    let step = &mut migration.steps[index];
    step.state = MigrationState::InProgress;
    let ns = step.namespace.clone();
    let finished = match step.kind {
        StepKind::Reencode => reencode_batch(tx, step, &ns, batch, encoding)?,
        StepKind::Reindex => {
            tx.rebuild_indexes(&ns)?;
            step.done = tx.index_defs(&ns)?.len() as u64;
            true
        }
        // Checked in `run`, on a snapshot: here it only records the result.
        StepKind::Verify => true,
        // Stored by the batch before this.
        StepKind::Train => true,
    };
    let step = &mut migration.steps[index];
    if finished {
        step.state = MigrationState::Completed;
    }
    Ok(finished)
}

/// What a verify step finds: `check` on a snapshot, and with `exclusive`
/// the namespace's digest against the plan's.
fn verify(
    db: &Grafeno,
    migration: &Migration,
    index: usize,
    exclusive: bool,
) -> std::result::Result<String, (MigrationState, Error)> {
    let ns = &migration.steps[index].namespace;
    let read = db.begin_read().map_err(|e| (MigrationState::Failed, e))?;
    let report = read.check(ns).map_err(|e| (MigrationState::Failed, e))?;
    if !report.is_ok() {
        return Err((
            MigrationState::Failed,
            Error::Invalid(format!(
                "{ns} does not check out after the migration: {}",
                report.problems.join("; ")
            )),
        ));
    }
    let mut result = format!("check: ok ({} nodes, {} edges)", report.nodes, report.edges);
    if exclusive {
        let now = read
            .namespace_digest(ns)
            .map_err(|e| (MigrationState::Failed, e))?;
        if let Some(before) = migration.before.iter().find(|d| &d.namespace == ns) {
            if *before != now {
                return Err((
                    MigrationState::RollbackRequired,
                    Error::Invalid(format!(
                        "{ns} holds something else than before the migration ({} nodes, {} edges, digest {} then; {} nodes, {} edges, digest {} now): restore the backup taken before it",
                        before.nodes, before.edges, before.digest, now.nodes, now.edges, now.digest
                    )),
                ));
            }
            result.push_str("; every record's value as before");
        }
    }
    Ok(result)
}

/// Rewrites up to `batch` values of `ns` (the file's logs for "") after the
/// step's cursor. Every value written is decoded again and compared with
/// what it held, in the same transaction.
fn reencode_batch(
    tx: &mut WriteTx,
    step: &mut MigrationStep,
    ns: &str,
    batch: usize,
    encoding: Encoding,
) -> Result<bool> {
    let tx: &WriteTx = tx;
    let slots: &[Slot] = if ns.is_empty() {
        &Slot::FILE
    } else {
        &Slot::NAMESPACE
    };
    let (mut at, mut after) = match &step.cursor {
        Some((table, key)) => (
            slots.iter().position(|s| s.name() == table).unwrap_or(0),
            Some(
                u128::from_str_radix(key, 16)
                    .map_err(|_| Error::Serialization("a migration's cursor".into()))?,
            ),
        ),
        None => (0, None),
    };
    let mut left = batch;
    while at < slots.len() {
        let slot = slots[at];
        let (rewritten, last) = match slot {
            Slot::Texts => texts_batch(tx, ns, after, &mut left, encoding)?,
            Slot::TextValues => text_values_batch(tx, ns, after, &mut left, encoding)?,
            Slot::Changes => changes_batch(tx, ns, after, &mut left, encoding)?,
            slot if slot.records().is_some() => {
                records_batch(tx, slot, ns, after, &mut left, encoding)?
            }
            slot => sequence_batch(tx, slot, after, &mut left, encoding)?,
        };
        step.done += rewritten;
        if left == 0 {
            step.cursor = last.map(|key| (slot.name().to_owned(), format!("{key:032x}")));
            return Ok(false);
        }
        at += 1;
        after = None;
        step.cursor = None;
    }
    Ok(true)
}

fn same(raw: &[u8], new: &[u8], what: &str) -> Result<()> {
    if unwrap_envelope(new)?.as_ref() != raw {
        return Err(Error::Invalid(format!(
            "{what} would not read the same reencoded"
        )));
    }
    Ok(())
}

type Batch = (u64, Option<u128>);

fn lower_bound<K>(after: Option<u128>, key: impl Fn(u128) -> K, first: K) -> std::ops::Bound<K> {
    match after {
        Some(id) => std::ops::Bound::Excluded(key(id)),
        None => std::ops::Bound::Included(first),
    }
}

fn records_batch(
    tx: &WriteTx,
    slot: Slot,
    ns: &str,
    after: Option<u128>,
    left: &mut usize,
    encoding: Encoding,
) -> Result<Batch> {
    let mut table = tx
        .tx
        .open_table(slot.records().expect("a record table"))
        .map_err(storage)?;
    let end = format!("{ns}\0");
    let lower = lower_bound(after, |id| (ns, id), (ns, 0u128));
    let upper = std::ops::Bound::Excluded((end.as_str(), 0u128));
    let mut pending: Vec<(u128, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut last = None;
    for item in table.range((lower, upper)).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let id = key.value().1;
        last = Some(id);
        let raw = unwrap_envelope(value.value())?.into_owned();
        let new = target(tx, ns, slot.kind(), &raw, encoding, false);
        if new != value.value() {
            pending.push((id, raw, new));
        }
        *left -= 1;
        if *left == 0 {
            break;
        }
    }
    for (id, raw, new) in &pending {
        same(raw, new, &format!("record {id:032x} of {ns}"))?;
        table.insert((ns, *id), new.as_slice()).map_err(storage)?;
    }
    Ok((pending.len() as u64, last))
}

/// Texts in `texts`: into `text_values` unless going back to JSON.
fn texts_batch(
    tx: &WriteTx,
    ns: &str,
    after: Option<u128>,
    left: &mut usize,
    encoding: Encoding,
) -> Result<Batch> {
    if encoding == Encoding::Json {
        return Ok((0, None));
    }
    let mut texts = tx.tx.open_table(crate::store::TEXTS).map_err(storage)?;
    let end = format!("{ns}\0");
    let lower = lower_bound(after, |id| (ns, id), (ns, 0u128));
    let upper = std::ops::Bound::Excluded((end.as_str(), 0u128));
    let mut moving: Vec<(u128, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut last = None;
    for item in texts.range((lower, upper)).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let id = key.value().1;
        last = Some(id);
        let raw = value.value().as_bytes().to_vec();
        let new = target(
            tx,
            ns,
            StoredKind::Text,
            &raw,
            encoding,
            encoding == Encoding::Envelope,
        );
        // Not worth compressing: it stays where it is.
        if is_enveloped(&new) {
            moving.push((id, raw, new));
        }
        *left -= 1;
        if *left == 0 {
            break;
        }
    }
    if moving.is_empty() {
        return Ok((0, last));
    }
    let mut values = tx
        .tx
        .open_table(crate::store::TEXT_VALUES)
        .map_err(storage)?;
    for (id, raw, new) in &moving {
        same(raw, new, &format!("the text of {id:032x} in {ns}"))?;
        values.insert((ns, *id), new.as_slice()).map_err(storage)?;
        texts.remove((ns, *id)).map_err(storage)?;
    }
    Ok((moving.len() as u64, last))
}

/// Texts in `text_values`: rewritten, or back into `texts` for JSON.
fn text_values_batch(
    tx: &WriteTx,
    ns: &str,
    after: Option<u128>,
    left: &mut usize,
    encoding: Encoding,
) -> Result<Batch> {
    if !crate::compress::has_table(&tx.tx, redb::TableHandle::name(&crate::store::TEXT_VALUES))? {
        return Ok((0, None));
    }
    let mut values = tx
        .tx
        .open_table(crate::store::TEXT_VALUES)
        .map_err(storage)?;
    let end = format!("{ns}\0");
    let lower = lower_bound(after, |id| (ns, id), (ns, 0u128));
    let upper = std::ops::Bound::Excluded((end.as_str(), 0u128));
    let mut pending: Vec<(u128, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut last = None;
    for item in values.range((lower, upper)).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let id = key.value().1;
        last = Some(id);
        let raw = unwrap_envelope(value.value())?.into_owned();
        let new = if encoding == Encoding::Json {
            raw.clone()
        } else {
            target(
                tx,
                ns,
                StoredKind::Text,
                &raw,
                encoding,
                encoding == Encoding::Envelope,
            )
        };
        if !is_enveloped(&new) || new != value.value() {
            pending.push((id, raw, new));
        }
        *left -= 1;
        if *left == 0 {
            break;
        }
    }
    let mut texts = tx.tx.open_table(crate::store::TEXTS).map_err(storage)?;
    for (id, raw, new) in &pending {
        if is_enveloped(new) {
            same(raw, new, &format!("the text of {id:032x} in {ns}"))?;
            values.insert((ns, *id), new.as_slice()).map_err(storage)?;
        } else {
            // Back to `texts`, as it is.
            let text = std::str::from_utf8(raw)
                .map_err(|_| Error::Serialization("a stored text is not UTF-8".into()))?;
            texts.insert((ns, *id), text).map_err(storage)?;
            values.remove((ns, *id)).map_err(storage)?;
        }
    }
    Ok((pending.len() as u64, last))
}

fn changes_batch(
    tx: &WriteTx,
    ns: &str,
    after: Option<u128>,
    left: &mut usize,
    encoding: Encoding,
) -> Result<Batch> {
    let mut table = tx.tx.open_table(crate::changes::CHANGES).map_err(storage)?;
    let lower = lower_bound(after, |seq| (ns, seq as u64), (ns, 0u64));
    let upper = std::ops::Bound::Included((ns, u64::MAX));
    let mut pending: Vec<(u64, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut last = None;
    for item in table.range((lower, upper)).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let seq = key.value().1;
        last = Some(u128::from(seq));
        let raw = unwrap_envelope(value.value())?.into_owned();
        let new = target(tx, ns, StoredKind::Change, &raw, encoding, false);
        if new != value.value() {
            pending.push((seq, raw, new));
        }
        *left -= 1;
        if *left == 0 {
            break;
        }
    }
    for (seq, raw, new) in &pending {
        same(raw, new, &format!("change {seq} of {ns}"))?;
        table.insert((ns, *seq), new.as_slice()).map_err(storage)?;
    }
    Ok((pending.len() as u64, last))
}

fn sequence_batch(
    tx: &WriteTx,
    slot: Slot,
    after: Option<u128>,
    left: &mut usize,
    encoding: Encoding,
) -> Result<Batch> {
    let mut table = tx
        .tx
        .open_table(slot.sequence().expect("a log table"))
        .map_err(storage)?;
    let lower = lower_bound(after, |seq| seq as u64, 0u64);
    let mut pending: Vec<(u64, Vec<u8>, Vec<u8>)> = Vec::new();
    let mut last = None;
    for item in table
        .range((lower, std::ops::Bound::Unbounded))
        .map_err(storage)?
    {
        let (key, value) = item.map_err(storage)?;
        let seq = key.value();
        last = Some(u128::from(seq));
        let raw = unwrap_envelope(value.value())?.into_owned();
        let new = target(tx, "", slot.kind(), &raw, encoding, false);
        if new != value.value() {
            pending.push((seq, raw, new));
        }
        *left -= 1;
        if *left == 0 {
            break;
        }
    }
    for (seq, raw, new) in &pending {
        same(raw, new, &format!("entry {seq} of {}", slot.name()))?;
        table.insert(*seq, new.as_slice()).map_err(storage)?;
    }
    Ok((pending.len() as u64, last))
}

impl WriteTx {
    /// After a migration back to JSON of the whole file: when no value is
    /// left in an envelope, the tables of layout 20 go and the file gets
    /// back the layout it had, which earlier releases open. Returns the
    /// layout, or why it stays.
    pub(crate) fn leave_envelope_layout(&mut self) -> Result<std::result::Result<u64, String>> {
        for slot in [Slot::Nodes, Slot::Edges, Slot::HyperEdges] {
            let table = self
                .tx
                .open_table(slot.records().expect("records"))
                .map_err(storage)?;
            for item in table.iter().map_err(storage)? {
                let (_, value) = item.map_err(storage)?;
                if is_enveloped(value.value()) {
                    return Ok(Err(format!(
                        "{} still holds values in envelopes",
                        slot.name()
                    )));
                }
            }
        }
        for definition in [crate::audit::AUDIT, crate::replication::REPLICATION_LOG] {
            let table = self.tx.open_table(definition).map_err(storage)?;
            for item in table.iter().map_err(storage)? {
                let (_, value) = item.map_err(storage)?;
                if is_enveloped(value.value()) {
                    return Ok(Err("a log still holds values in envelopes".into()));
                }
            }
        }
        let changes = self
            .tx
            .open_table(crate::changes::CHANGES)
            .map_err(storage)?;
        for item in changes.iter().map_err(storage)? {
            let (_, value) = item.map_err(storage)?;
            if is_enveloped(value.value()) {
                return Ok(Err("the change log still holds values in envelopes".into()));
            }
        }
        drop(changes);
        let values = redb::TableHandle::name(&crate::store::TEXT_VALUES);
        if crate::compress::has_table(&self.tx, values)? {
            if self
                .tx
                .open_table(crate::store::TEXT_VALUES)
                .map_err(storage)?
                .len()
                .map_err(storage)?
                > 0
            {
                return Ok(Err("text_values still holds texts".into()));
            }
            self.tx
                .delete_table(crate::store::TEXT_VALUES)
                .map_err(storage)?;
        }
        if crate::compress::has_table(
            &self.tx,
            redb::TableHandle::name(&crate::compress::DICTIONARIES),
        )? {
            self.tx
                .delete_table(crate::compress::DICTIONARIES)
                .map_err(storage)?;
        }
        let hyperedges = self
            .tx
            .open_table(crate::hyperedge::HYPEREDGES)
            .map_err(storage)?
            .len()
            .map_err(storage)?
            > 0;
        let mut meta = self.tx.open_table(crate::store::META).map_err(storage)?;
        let before = meta
            .remove(crate::db::LAYOUT_BEFORE_ENVELOPES)
            .map_err(storage)?
            .map_or(crate::BASE_LAYOUT, |guard| guard.value());
        let layout = if hyperedges {
            before.max(crate::HYPEREDGE_LAYOUT)
        } else {
            before
        }
        .min(crate::HYPEREDGE_LAYOUT);
        meta.insert(crate::db::SCHEMA_KEY, layout)
            .map_err(storage)?;
        Ok(Ok(layout))
    }
}

impl WriteTx {
    /// Records `migration` in the file (not replicated: a migration is
    /// this member's own).
    pub(crate) fn save_migration(&mut self, migration: &Migration) -> Result<()> {
        let bytes =
            serde_json::to_vec(migration).map_err(|e| Error::Serialization(e.to_string()))?;
        self.tx
            .open_table(crate::state::STATE)
            .map_err(storage)?
            .insert(
                (MIGRATIONS_NS, KIND, migration.id.as_str()),
                bytes.as_slice(),
            )
            .map_err(storage)?;
        Ok(())
    }
}

/// Something the storage could do, or a migration that waits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pending {
    /// `migration` (paused, in progress, failed), `train` (namespaces
    /// without compression dictionaries), `compact` (free pages), `pack`
    /// (space lost inside pages).
    pub kind: String,
    pub detail: String,
    /// How many bytes it would give back at most, when it is space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl Grafeno {
    /// What is pending: migrations not finished, and space a compaction
    /// would give back (free pages past a quarter of the file, space lost
    /// inside pages past a quarter of what they store). Nothing is run.
    pub fn pending_work(&self) -> Result<Vec<Pending>> {
        let mut out = Vec::new();
        for migration in self.begin_read()?.migrations()? {
            let what = match migration.state {
                MigrationState::Paused | MigrationState::InProgress => "is paused: resume it",
                MigrationState::Failed => "failed: see its steps",
                MigrationState::RollbackRequired => {
                    "found the data changed: restore the backup taken before it"
                }
                _ => continue,
            };
            out.push(Pending {
                kind: "migration".into(),
                detail: format!("migration {} {what}", migration.id),
                bytes: None,
            });
        }
        if self.compression() == crate::Compression::Auto {
            let read = self.begin_read()?;
            let mut waiting = Vec::new();
            for info in read.namespaces()? {
                if read.stats(&info.name)?.nodes >= 1_000
                    && !self
                        .shared
                        .compression
                        .has_dictionary(&info.name, StoredKind::Node)
                {
                    waiting.push(info.name);
                }
            }
            if !waiting.is_empty() {
                let shown: Vec<&str> = waiting.iter().take(3).map(String::as_str).collect();
                out.push(Pending {
                    kind: "train".into(),
                    detail: format!(
                        "{} namespace(s) have no compression dictionaries yet ({}{}): a server trains them in the background; `grafeno storage apply --encoding compressed` also rewrites what is stored",
                        waiting.len(),
                        shown.join(", "),
                        if waiting.len() > 3 { ", …" } else { "" }
                    ),
                    bytes: None,
                });
            }
        }
        let usage = self.storage_usage()?;
        // The file as it was before opening: what redb added when it
        // opened it goes away when it closes it, not by compacting.
        let file = usage
            .file_bytes
            .map(|now| self.opened_bytes.map_or(now, |opened| now.min(opened)));
        let free = file
            .zip(usage.allocated_bytes)
            .map(|(file, used)| file.saturating_sub(used));
        if let (Some(file), Some(free)) = (file, free)
            && free * 4 > file
            && free >= WORTH_IT
        {
            out.push(Pending {
                kind: "compact".into(),
                detail: format!(
                    "{} MiB of the file's {} MiB are free pages: `grafeno vacuum` gives them back",
                    free >> 20,
                    file >> 20
                ),
                bytes: Some(free),
            });
        }
        if usage.fragmented_bytes * 4 > usage.stored_bytes
            && usage.fragmented_bytes >= WORTH_IT
            && usage.file_bytes.is_some()
        {
            out.push(Pending {
                kind: "pack".into(),
                detail: format!(
                    "{} MiB are lost inside pages: `grafeno vacuum --maximum` packs them",
                    usage.fragmented_bytes >> 20
                ),
                bytes: Some(usage.fragmented_bytes),
            });
        }
        Ok(out)
    }
}
