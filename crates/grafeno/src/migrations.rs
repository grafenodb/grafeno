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

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use grafeno_core::{Error, Result, Timestamp};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

use crate::db::{Grafeno, ReadTx, WriteTx};
use crate::store::{Codec, envelope, is_enveloped, storage, unwrap_envelope};

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
    /// The versioned envelope (its payload JSON, uncompressed for now):
    /// read by this build and later ones only.
    Envelope,
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
}

/// What a step does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
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

/// The tables a reencode step goes through, and their names.
const RECORD_TABLES: [(
    &str,
    redb::TableDefinition<'static, crate::store::RecordKey, &'static [u8]>,
); 3] = [
    ("nodes", crate::store::NODES),
    ("edges", crate::store::EDGES),
    ("hyperedges", crate::hyperedge::HYPEREDGES),
];

fn decode_migration(bytes: &[u8]) -> Result<Migration> {
    serde_json::from_slice(bytes).map_err(|e| Error::Serialization(e.to_string()))
}

/// Whether a stored record is in `encoding`.
fn in_encoding(bytes: &[u8], encoding: Encoding) -> bool {
    is_enveloped(bytes) == (encoding == Encoding::Envelope)
}

/// `bytes` in `encoding`.
fn reencoded(bytes: &[u8], encoding: Encoding) -> Result<Vec<u8>> {
    let json = unwrap_envelope(bytes)?;
    Ok(match encoding {
        Encoding::Json => json.into_owned(),
        Encoding::Envelope => envelope(&json, Codec::Plain),
    })
}

impl ReadTx {
    /// Every migration recorded in this file, oldest first.
    pub fn migrations(&self) -> Result<Vec<Migration>> {
        self.state_entries(MIGRATIONS_NS, KIND, "", usize::MAX)?
            .into_iter()
            .map(|(_, bytes)| decode_migration(&bytes))
            .collect()
    }

    /// What a namespace holds, with a digest of its records' values.
    pub fn namespace_digest(&self, ns: &str) -> Result<NamespaceDigest> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        let mut counts = [0u64; 3];
        for (slot, (_, table)) in RECORD_TABLES.iter().enumerate() {
            let table = self.tx.open_table(*table).map_err(storage)?;
            let end = format!("{ns}\0");
            for item in table
                .range((ns, 0u128)..(end.as_str(), 0u128))
                .map_err(storage)?
            {
                let (key, value) = item.map_err(storage)?;
                counts[slot] += 1;
                key.value().1.hash(&mut hasher);
                let json: serde_json::Value = crate::store::decode(value.value())?;
                serde_json::to_vec(&json)
                    .map_err(|e| Error::Serialization(e.to_string()))?
                    .hash(&mut hasher);
            }
        }
        Ok(NamespaceDigest {
            namespace: ns.to_owned(),
            nodes: counts[0],
            edges: counts[1],
            hyperedges: counts[2],
            digest: format!("{:016x}", hasher.finish()),
        })
    }

    /// Records of `ns` not stored in `encoding`, from a sample of up to
    /// `sample` per table, scaled to the table's records: an estimate.
    fn records_to_reencode(&self, ns: &str, encoding: Encoding, sample: usize) -> Result<u64> {
        let mut total = 0u64;
        for (_, table) in RECORD_TABLES {
            let table = self.tx.open_table(table).map_err(storage)?;
            let end = format!("{ns}\0");
            let (mut seen, mut other, mut all) = (0usize, 0usize, 0u64);
            for item in table
                .range((ns, 0u128)..(end.as_str(), 0u128))
                .map_err(storage)?
            {
                let (_, value) = item.map_err(storage)?;
                all += 1;
                if seen < sample {
                    seen += 1;
                    if !in_encoding(value.value(), encoding) {
                        other += 1;
                    }
                }
            }
            if seen > 0 {
                total += (all as f64 * other as f64 / seen as f64).round() as u64;
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
            // Rebuilding writes records the way every write does: it goes
            // first, so that the reencoding after it stays.
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
        })
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
            let current = migration.clone();
            let encoding = current.options.encoding.unwrap_or_default();
            let work: Work = Arc::new(move |tx: &mut WriteTx| {
                let mut migration = current.clone();
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

/// Rewrites up to `batch` records of `ns` after the step's cursor.
fn reencode_batch(
    tx: &mut WriteTx,
    step: &mut MigrationStep,
    ns: &str,
    batch: usize,
    encoding: Encoding,
) -> Result<bool> {
    let (mut table_name, mut after) = match &step.cursor {
        Some((table, id)) => (
            table.clone(),
            Some(
                u128::from_str_radix(id, 16)
                    .map_err(|_| Error::Serialization("a migration's cursor".into()))?,
            ),
        ),
        None => (RECORD_TABLES[0].0.to_owned(), None),
    };
    let mut left = batch;
    let mut slot = RECORD_TABLES
        .iter()
        .position(|(name, _)| *name == table_name)
        .unwrap_or(0);
    while slot < RECORD_TABLES.len() {
        let (name, definition) = RECORD_TABLES[slot];
        let mut table = tx.tx.open_table(definition).map_err(storage)?;
        let end = format!("{ns}\0");
        let lower = match after {
            Some(id) => std::ops::Bound::Excluded((ns, id)),
            None => std::ops::Bound::Included((ns, 0u128)),
        };
        let upper = std::ops::Bound::Excluded((end.as_str(), 0u128));
        let mut pending: Vec<(u128, Vec<u8>)> = Vec::new();
        let mut last = None;
        for item in table.range((lower, upper)).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let id = key.value().1;
            last = Some(id);
            if !in_encoding(value.value(), encoding) {
                pending.push((id, value.value().to_vec()));
            }
            left -= 1;
            if left == 0 {
                break;
            }
        }
        for (id, old) in &pending {
            let new = reencoded(old, encoding)?;
            // The same value, decoded: or nothing is written.
            let before: serde_json::Value = crate::store::decode(old)?;
            let after_value: serde_json::Value = crate::store::decode(&new)?;
            if before != after_value {
                return Err(Error::Invalid(format!(
                    "record {id:032x} of {ns} would not read the same reencoded"
                )));
            }
            table.insert((ns, *id), new.as_slice()).map_err(storage)?;
        }
        step.done += pending.len() as u64;
        if left == 0 {
            step.cursor = last.map(|id| (name.to_owned(), format!("{id:032x}")));
            return Ok(false);
        }
        slot += 1;
        after = None;
        table_name = RECORD_TABLES
            .get(slot)
            .map(|(n, _)| (*n).to_owned())
            .unwrap_or_default();
        step.cursor = None;
    }
    let _ = table_name;
    Ok(true)
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
    /// `migration` (paused, in progress, failed), `compact` (free pages),
    /// `pack` (space lost inside pages).
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
