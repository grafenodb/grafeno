use std::collections::HashMap;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use grafeno_core::{Edge, EdgeId, Error, NewEdge, NewNode, Node, NodeId, Props, Result, Timestamp};
use redb::{
    Database, ReadOnlyTable, ReadTransaction, ReadableDatabase, ReadableTable,
    ReadableTableMetadata, TableDefinition, WriteTransaction,
};

use crate::access::{ACCOUNTS, API_KEYS, SESSIONS};
use crate::algo::Shared;
use crate::changes::{CHANGES, ChangeOp, Part};
use crate::constraints::CONSTRAINTS;
use crate::index::{INDEX_ENTRIES, INDEXES, IndexKey};
use crate::memory::names_of;
use crate::merge::MERGES;
use crate::namespace::{NAMESPACES, NamespaceMeta};
use crate::replication::Touch;
use crate::resolve::MATCH_RULES;
use crate::rights::ROLES;
use crate::state::STATE;
use crate::stats::{
    NamespaceInfo, NamespaceStats, bump_edge_label, bump_namespace, bump_node_label, label_counts,
    namespaces,
};
use crate::store::{
    ADJ_IN, ADJ_OUT, Adj, AdjKey, AdjValue, CountKey, EDGE_LABEL_COUNTS, EDGES, HNSW_LINKS,
    LABEL_COUNTS, LabelKey, LinkKey, MEMORY, META, NAMES, NODE_LABELS, NODES, NS_COUNTS, POSTINGS,
    PostingKey, RecordKey, TEXT_LENGTHS, TEXT_STATS, TEXTS, VECTOR_META, VECTORS, VECTORS_Q8,
    adjacency, adjacency_value, check_label, check_namespace, decode, encode, label_page,
    nodes_with_label, read_record, scan_page, scan_records, storage,
};
use crate::{Hit, text, vector};

/// The version of the on-disk layout written by this build.
///
/// 1. Phases 1-3: graph, text and vector tables.
/// 2. Adds the `meta` table and the namespace and label counters.
/// 3. Adjacency entries carry the edge's times (`adjacency_out`/`_in`
///    replace `adj_out`/`_in`), and postings carry the text length
///    (`text_postings` replaces `postings`).
/// 4. Adds the `names` index (entity resolution) and the `memory` table
///    (importance and accesses).
/// 5. Adds property indexes (`indexes`, `index_entries`), empty until one
///    is declared.
/// 6. Adds the `namespaces` registry, so that empty namespaces exist.
/// 7. Adds `principals` (API keys), empty until one is created.
/// 8. Adds `merges` (merge records, for undo), empty until a merge.
/// 9. Allows fuzzy indexes (`IndexDef::fuzzy`), whose trigram entries an
///    older Grafeno would read as values, and adds `match_rules`.
/// 10. Adds `state` (what extraction read, its review queue), empty.
/// 11. Adds `constraints` (on facts and properties), empty.
/// 12. Adds `changes` (the change log), empty: it starts with the first
///     commit after the upgrade.
/// 13. Adds `vectors_q8` (int8 copies of vectors) and compact adjacency.
/// 14. Adds `replication_log`.
/// 15. Replaces `principals` with `accounts`, `api_keys` and `sessions`:
///     each principal becomes a service account and a key (14.1).
/// 16. Adds `roles` (14.2), empty until one is defined.
/// 17. Adds `audit` (14.5), the audit log, empty until something happens.
/// 18. Allows indexes on several properties, on edges and over several
///     labels (16.5) and full-text indexes (16.6), whose entries an older
///     Grafeno would read as values, and adds `fulltext_stats`.
pub const SCHEMA_VERSION: u64 = 18;

/// The tables replaced in version 3, by name (their types no longer matter).
const OLD_ADJ_OUT: TableDefinition<AdjKey, u128> = TableDefinition::new("adj_out");
const OLD_ADJ_IN: TableDefinition<AdjKey, u128> = TableDefinition::new("adj_in");
const OLD_POSTINGS: TableDefinition<PostingKey, u32> = TableDefinition::new("postings");

const SCHEMA_KEY: &str = "schema_version";

/// How much memory a database may use for caches. Everything else a
/// process holds is per request: rows of a query, a projection being
/// built, a transaction's changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenOptions {
    /// Pages of the file kept in memory (redb's cache), shared by readers
    /// and the writer. More makes repeated reads faster.
    pub cache_bytes: usize,
    /// Graph projections kept for algorithms between reads.
    pub projection_cache_bytes: usize,
    /// Decoded vectors and link lists a write transaction keeps while it
    /// inserts vectors.
    pub vector_cache_bytes: usize,
    /// Before an older file's layout is upgraded, copy it to
    /// `<name>.layout-<old>.redb` beside it (see
    /// [`Grafeno::copy_before_upgrade`]), so the previous version of
    /// Grafeno can still open the data. On by default.
    pub copy_before_upgrade: bool,
}

/// The default page cache: 256 MiB.
pub const DEFAULT_CACHE: usize = 256 * 1024 * 1024;

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            cache_bytes: DEFAULT_CACHE,
            projection_cache_bytes: crate::algo::DEFAULT_PROJECTION_CACHE,
            vector_cache_bytes: crate::vector::DEFAULT_VECTOR_CACHE,
            copy_before_upgrade: true,
        }
    }
}

impl OpenOptions {
    /// Every cache sized from one budget: half for pages, a quarter each
    /// for projections and vectors.
    pub fn with_memory(bytes: usize) -> Self {
        Self {
            cache_bytes: bytes / 2,
            projection_cache_bytes: bytes / 4,
            vector_cache_bytes: bytes / 4,
            ..Self::default()
        }
    }
}

/// Where [`Grafeno::copy_before_upgrade`] copies a file of layout `version`.
pub fn upgrade_copy_path(path: &FsPath, version: u64) -> std::path::PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{stem}.layout-{version}.redb"))
}

/// The copies [`Grafeno::copy_before_upgrade`] left beside the file.
pub fn upgrade_copies(path: &FsPath) -> Vec<std::path::PathBuf> {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let prefix = format!("{stem}.layout-");
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => FsPath::new("."),
    };
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|name| {
                name.strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(".redb"))
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .collect();
    found.sort();
    found
}

/// What checking a database file found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileIntegrity {
    /// Every page matches its checksum.
    Clean,
    /// Damage was found and redb repaired it (the last commits may be lost).
    Repaired,
}

/// When a commit reaches the disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// When `commit` returns (the default).
    #[default]
    Immediate,
    /// At the next [`Grafeno::flush`] (which a [`crate::Writer`] calls
    /// twice a second) or immediate commit. Much faster for many small
    /// writes; a crash loses the commits since the last flush, never part
    /// of one.
    Eventual,
}

/// A Grafeno database: one file (or memory) holding many namespaces.
///
/// It allows one writer and many readers at once; each reader sees a fixed
/// snapshot.
pub struct Grafeno {
    db: Arc<Database>,
    eventual: AtomicBool,
    /// Commits with eventual durability since the last flush.
    unflushed: AtomicBool,
    /// The commit counter and the projections cached for algorithms.
    pub(crate) shared: Arc<Shared>,
    vector_cache: usize,
}

impl Grafeno {
    /// Opens the file, creating it if it does not exist, and upgrades its
    /// layout if it was written by an older version.
    pub fn open(path: impl AsRef<FsPath>) -> Result<Self> {
        Self::open_with(path, &OpenOptions::default())
    }

    /// [`Grafeno::open`] with caches of these sizes.
    pub fn open_with(path: impl AsRef<FsPath>, options: &OpenOptions) -> Result<Self> {
        let path = path.as_ref();
        if options.copy_before_upgrade {
            Self::copy_before_upgrade(path)?;
        }
        let db = Database::builder()
            .set_cache_size(options.cache_bytes)
            .create(path)
            .map_err(storage)?;
        let grafeno = Self::init(db)?;
        Ok(grafeno.with_options(options))
    }

    /// The layout version of a database file, without upgrading it: `None`
    /// for a missing or empty file. No other program may have it open.
    pub fn file_layout(path: impl AsRef<FsPath>) -> Result<Option<u64>> {
        let path = path.as_ref();
        if std::fs::metadata(path).map_or(true, |meta| meta.len() == 0) {
            return Ok(None);
        }
        let db = Database::open(path).map_err(storage)?;
        let read = db.begin_read().map_err(storage)?;
        let version = match read.open_table(META) {
            Ok(meta) => meta.get(SCHEMA_KEY).map_err(storage)?.map(|v| v.value()),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(storage(error)),
        };
        if version.is_some() {
            return Ok(version);
        }
        // Before version 2 there was no `meta` table.
        let nodes = match read.open_table(NODES) {
            Ok(nodes) => nodes.len().map_err(storage)?,
            Err(redb::TableError::TableDoesNotExist(_)) => 0,
            Err(error) => return Err(storage(error)),
        };
        Ok((nodes > 0).then_some(1))
    }

    /// If the file at `path` has an older layout than this build writes,
    /// copies it to `<name>.layout-<old>.redb` beside it, as it is, and
    /// removes copies an earlier upgrade left (`<name>.layout-*.redb`): the
    /// previous version of Grafeno opens the copy, so going back to it
    /// keeps the data. Returns the copy's path. If the copy cannot be made
    /// (no room on the disk), the file is left untouched and opening it
    /// fails, rather than upgrading it without a way back. No other
    /// program may have the file open.
    pub fn copy_before_upgrade(path: impl AsRef<FsPath>) -> Result<Option<std::path::PathBuf>> {
        let path = path.as_ref();
        let Some(version) = Self::file_layout(path)? else {
            return Ok(None);
        };
        if version >= SCHEMA_VERSION {
            return Ok(None);
        }
        let copy = upgrade_copy_path(path, version);
        let partial = copy.with_extension("redb.partial");
        let result = std::fs::copy(path, &partial)
            .and_then(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&partial)?
                    .sync_all()
            })
            .and_then(|()| std::fs::rename(&partial, &copy));
        if let Err(error) = result {
            let _ = std::fs::remove_file(&partial);
            return Err(Error::Storage(format!(
                "{} has layout version {version} and this Grafeno writes {SCHEMA_VERSION}; \
                 it keeps a copy at {} before upgrading it, and could not ({error}). \
                 Free some disk space (the copy is as large as the file), or turn the copy off",
                path.display(),
                copy.display()
            )));
        }
        for older in upgrade_copies(path) {
            if older != copy {
                let _ = std::fs::remove_file(older);
            }
        }
        Ok(Some(copy))
    }

    fn with_options(mut self, options: &OpenOptions) -> Self {
        self.shared
            .set_projection_budget(options.projection_cache_bytes);
        self.vector_cache = options.vector_cache_bytes;
        self
    }

    /// The bytes the projection cache holds now.
    pub fn projection_cache_bytes(&self) -> usize {
        self.shared.projection_bytes()
    }

    /// Checks every page of a database file against its checksum, which
    /// normal reads do not (redb trusts its pages, and a damaged one can
    /// stop the program). Run it on a file that may have been damaged
    /// (copied badly, a failing disk, edited by something else) before
    /// using it; no other program may have it open. A file damaged beyond
    /// repair is an [`Error::Storage`] naming the damage.
    pub fn verify_file(path: impl AsRef<FsPath>) -> Result<FileIntegrity> {
        let path = path.as_ref();
        let damaged = |error: &dyn std::fmt::Display| {
            Error::Storage(format!("{} is damaged: {error}", path.display()))
        };
        let outcome = std::panic::catch_unwind(|| -> Result<FileIntegrity> {
            let mut db = Database::open(path).map_err(|error| damaged(&error))?;
            match db.check_integrity() {
                Ok(true) => Ok(FileIntegrity::Clean),
                Ok(false) => Ok(FileIntegrity::Repaired),
                Err(error) => Err(damaged(&error)),
            }
        });
        outcome.unwrap_or_else(|_| Err(damaged(&"its pages cannot be read")))
    }

    /// An in-memory database: for tests and short-lived agents.
    pub fn in_memory() -> Result<Self> {
        let db = Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(storage)?;
        Self::init(db)
    }

    fn init(db: Database) -> Result<Self> {
        let tx = db.begin_write().map_err(storage)?;
        // Layouts 3 to 12 stored adjacency values as fixed tuples: convert
        // them before the tables are opened with the compact type.
        let stored = tx
            .open_table(META)
            .map_err(storage)?
            .get(SCHEMA_KEY)
            .map_err(storage)?
            .map(|guard| guard.value());
        if stored.is_some_and(|version| (3..13).contains(&version)) {
            compact_adjacency(&tx)?;
        }
        // Tables must exist before a read transaction can open them.
        tx.open_table(NODES).map_err(storage)?;
        tx.open_table(EDGES).map_err(storage)?;
        tx.open_table(ADJ_OUT).map_err(storage)?;
        tx.open_table(ADJ_IN).map_err(storage)?;
        tx.open_table(NODE_LABELS).map_err(storage)?;
        tx.open_table(TEXTS).map_err(storage)?;
        tx.open_table(TEXT_LENGTHS).map_err(storage)?;
        tx.open_table(POSTINGS).map_err(storage)?;
        tx.open_table(TEXT_STATS).map_err(storage)?;
        tx.open_table(VECTORS).map_err(storage)?;
        tx.open_table(VECTORS_Q8).map_err(storage)?;
        tx.open_table(HNSW_LINKS).map_err(storage)?;
        tx.open_table(VECTOR_META).map_err(storage)?;
        tx.open_table(NS_COUNTS).map_err(storage)?;
        tx.open_table(LABEL_COUNTS).map_err(storage)?;
        tx.open_table(EDGE_LABEL_COUNTS).map_err(storage)?;
        tx.open_table(NAMES).map_err(storage)?;
        tx.open_table(MEMORY).map_err(storage)?;
        tx.open_table(INDEXES).map_err(storage)?;
        tx.open_table(INDEX_ENTRIES).map_err(storage)?;
        tx.open_table(crate::fulltext::FULLTEXT_STATS)
            .map_err(storage)?;
        tx.open_table(NAMESPACES).map_err(storage)?;
        tx.open_table(ACCOUNTS).map_err(storage)?;
        tx.open_table(API_KEYS).map_err(storage)?;
        tx.open_table(SESSIONS).map_err(storage)?;
        tx.open_table(ROLES).map_err(storage)?;
        tx.open_table(crate::audit::AUDIT).map_err(storage)?;
        tx.open_table(MERGES).map_err(storage)?;
        tx.open_table(MATCH_RULES).map_err(storage)?;
        tx.open_table(STATE).map_err(storage)?;
        tx.open_table(CONSTRAINTS).map_err(storage)?;
        tx.open_table(CHANGES).map_err(storage)?;
        tx.open_table(crate::replication::REPLICATION_LOG)
            .map_err(storage)?;

        let stored = tx
            .open_table(META)
            .map_err(storage)?
            .get(SCHEMA_KEY)
            .map_err(storage)?
            .map(|guard| guard.value());
        let version = match stored {
            Some(version) => version,
            // Before version 2 there was no `meta` table: a database with
            // nodes but no version is from version 1.
            None if tx
                .open_table(NODES)
                .map_err(storage)?
                .len()
                .map_err(storage)?
                > 0 =>
            {
                1
            }
            None => SCHEMA_VERSION,
        };
        if version > SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "the database uses layout version {version}, newer than this Grafeno \
                 (version {SCHEMA_VERSION}); upgrade Grafeno to open it"
            )));
        }
        if version < 2 {
            recount(&tx)?;
        }
        if version < 3 {
            migrate_to_3(&tx)?;
        }
        if version < 4 {
            migrate_to_4(&tx)?;
        }
        if version < 6 {
            migrate_to_6(&tx)?;
        }
        if version < 13 {
            quantize_vectors(&tx)?;
        }
        if version < 15 {
            crate::access::upgrade_principals(&tx)?;
        }
        tx.open_table(META)
            .map_err(storage)?
            .insert(SCHEMA_KEY, SCHEMA_VERSION)
            .map_err(storage)?;
        let replication = tx
            .open_table(META)
            .map_err(storage)?
            .get(crate::replication::ENABLED)
            .map_err(storage)?
            .is_some_and(|guard| guard.value() == 1);
        tx.commit().map_err(storage)?;
        let shared: Arc<Shared> = Arc::default();
        shared.replication.store(replication, Ordering::Relaxed);
        Ok(Self {
            db: Arc::new(db),
            eventual: AtomicBool::new(false),
            unflushed: AtomicBool::new(false),
            shared,
            vector_cache: crate::vector::DEFAULT_VECTOR_CACHE,
        })
    }

    pub fn durability(&self) -> Durability {
        if self.eventual.load(Ordering::Relaxed) {
            Durability::Eventual
        } else {
            Durability::Immediate
        }
    }

    /// Sets the durability of later commits.
    pub fn set_durability(&self, durability: Durability) {
        self.eventual
            .store(durability == Durability::Eventual, Ordering::Relaxed);
    }

    /// Makes every commit so far durable. Cheap when there is nothing to do.
    pub fn flush(&self) -> Result<()> {
        if !self.unflushed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        // An empty commit with immediate durability persists the ones before it.
        let tx = self.db.begin_write().map_err(storage)?;
        tx.commit().map_err(storage)
    }

    pub fn begin_read(&self) -> Result<ReadTx> {
        let (tx, generation) = self.shared.snapshot(|| self.db.begin_read());
        let tx = tx.map_err(storage)?;
        Ok(ReadTx {
            shared: self.shared.clone(),
            generation,
            nodes: tx.open_table(NODES).map_err(storage)?,
            edges: tx.open_table(EDGES).map_err(storage)?,
            out: tx.open_table(ADJ_OUT).map_err(storage)?,
            inn: tx.open_table(ADJ_IN).map_err(storage)?,
            labels: tx.open_table(NODE_LABELS).map_err(storage)?,
            texts: tx.open_table(TEXTS).map_err(storage)?,
            text_lengths: tx.open_table(TEXT_LENGTHS).map_err(storage)?,
            postings: tx.open_table(POSTINGS).map_err(storage)?,
            text_stats: tx.open_table(TEXT_STATS).map_err(storage)?,
            vectors: tx.open_table(VECTORS).map_err(storage)?,
            vectors_q8: tx.open_table(VECTORS_Q8).map_err(storage)?,
            hnsw_links: tx.open_table(HNSW_LINKS).map_err(storage)?,
            vector_meta: tx.open_table(VECTOR_META).map_err(storage)?,
            ns_counts: tx.open_table(NS_COUNTS).map_err(storage)?,
            label_counts: tx.open_table(LABEL_COUNTS).map_err(storage)?,
            edge_label_counts: tx.open_table(EDGE_LABEL_COUNTS).map_err(storage)?,
            names: tx.open_table(NAMES).map_err(storage)?,
            memory: tx.open_table(MEMORY).map_err(storage)?,
            indexes: tx.open_table(INDEXES).map_err(storage)?,
            index_entries: tx.open_table(INDEX_ENTRIES).map_err(storage)?,
            fulltext_stats: tx
                .open_table(crate::fulltext::FULLTEXT_STATS)
                .map_err(storage)?,
            registry: tx.open_table(NAMESPACES).map_err(storage)?,
            merges: tx.open_table(MERGES).map_err(storage)?,
            match_rules: tx.open_table(MATCH_RULES).map_err(storage)?,
            state: tx.open_table(STATE).map_err(storage)?,
            constraints: tx.open_table(CONSTRAINTS).map_err(storage)?,
            changes: tx.open_table(CHANGES).map_err(storage)?,
            tx,
            view: None,
        })
    }

    pub fn begin_write(&self) -> Result<WriteTx> {
        self.begin_write_lane(false)
    }

    /// A write transaction; with `eventual`, its commit has eventual
    /// durability whatever the database's setting (bulk loads).
    pub(crate) fn begin_write_lane(&self, eventual: bool) -> Result<WriteTx> {
        let mut tx = self.db.begin_write().map_err(storage)?;
        if eventual || self.eventual.load(Ordering::Relaxed) {
            tx.set_durability(redb::Durability::None).map_err(storage)?;
            self.unflushed.store(true, Ordering::Release);
        }
        Ok(WriteTx {
            tx,
            database: self.db.clone(),
            vectors: vector::Cache::new(self.vector_cache),
            shared: self.shared.clone(),
            actor: None,
            constraint_cache: None,
            constraint_log: Vec::new(),
            deferred_rules: None,
            subscription_cache: None,
            log_changes: self.shared.changes.retention().enabled,
            pending_changes: crate::changes::Pending::default(),
            capture: self
                .shared
                .replication
                .load(Ordering::Relaxed)
                .then(crate::replication::Capture::default),
            applying: false,
            commit_at: None,
            past_fences: false,
            origin: None,
        })
    }

    /// Turns the replication log on, for good: from the next commit, every
    /// write transaction is appended to it (see [`ReadTx::log_entries`]).
    /// Members that will apply it start from a backup taken after this.
    pub fn enable_replication_log(&self) -> Result<()> {
        let tx = self.db.begin_write().map_err(storage)?;
        tx.open_table(META)
            .map_err(storage)?
            .insert(crate::replication::ENABLED, 1)
            .map_err(storage)?;
        self.shared.committing(|| tx.commit().map_err(storage))?;
        self.shared.replication.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Replaces everything in this database with the contents of the backup
    /// at `path` (written by [`ReadTx::backup`] with this layout version), in
    /// one transaction: readers see the old data or the new, never a mix.
    /// For a cluster member that receives a snapshot. Returns the entries
    /// copied.
    pub fn restore_backup(&self, path: impl AsRef<FsPath>) -> Result<u64> {
        let source = Database::open(path.as_ref()).map_err(storage)?;
        let from = source.begin_read().map_err(storage)?;
        let version = from
            .open_table(META)
            .map_err(storage)?
            .get(SCHEMA_KEY)
            .map_err(storage)?
            .map_or(0, |guard| guard.value());
        if version != SCHEMA_VERSION {
            return Err(Error::Invalid(format!(
                "the backup has layout version {version}, this Grafeno {SCHEMA_VERSION}"
            )));
        }
        let tx = self.db.begin_write().map_err(storage)?;
        let entries = crate::backup::restore_into(&from, &tx)?;
        let replication = tx
            .open_table(META)
            .map_err(storage)?
            .get(crate::replication::ENABLED)
            .map_err(storage)?
            .is_some_and(|guard| guard.value() == 1);
        self.shared.committing(|| tx.commit().map_err(storage))?;
        self.shared
            .replication
            .store(replication, Ordering::Relaxed);
        Ok(entries)
    }

    /// Runs `f` in a write transaction and discards it, returning its value
    /// and what it would have done as a replication log entry (`None` when
    /// it wrote nothing). A cluster leader proposes the entry; every member,
    /// the leader included, applies it once it is committed.
    pub fn dry_run<T>(
        &self,
        f: impl FnOnce(&mut WriteTx) -> Result<T>,
    ) -> Result<(T, Option<crate::replication::Entry>)> {
        let mut tx = self.begin_write()?;
        tx.capture = Some(crate::replication::Capture::default());
        let outcome = f(&mut tx);
        let result = match outcome {
            Ok(value) => tx.build_entry().map(|entry| (value, entry)),
            Err(error) => Err(error),
        };
        tx.abort()?;
        result
    }

    /// A write transaction whose changes are captured as a replication
    /// entry instead of being committed: an explicit transaction on a
    /// cluster leader (16.4) runs its statements in it and, at `COMMIT`,
    /// proposes [`WriteTx::into_entry`] to the cluster.
    pub fn begin_captured(&self) -> Result<WriteTx> {
        let mut tx = self.begin_write()?;
        tx.capture = Some(crate::replication::Capture::default());
        Ok(tx)
    }

    /// Applies log entries of another database in order, each in its own
    /// transaction; returns how many were applied (those already applied
    /// are skipped).
    pub fn apply_entries(&self, entries: &[(u64, crate::replication::Entry)]) -> Result<usize> {
        let mut applied = 0;
        for (index, entry) in entries {
            let applied_now = self.write(|tx| tx.apply_entry(*index, entry))?;
            if applied_now {
                applied += 1;
            }
        }
        self.shared.replication.store(true, Ordering::Relaxed);
        Ok(applied)
    }

    /// Runs `f` in a write transaction: commits if it returns `Ok`, discards
    /// everything if it returns `Err`.
    pub fn write<T>(&self, f: impl FnOnce(&mut WriteTx) -> Result<T>) -> Result<T> {
        let mut tx = self.begin_write()?;
        match f(&mut tx) {
            Ok(value) => {
                tx.commit()?;
                Ok(value)
            }
            Err(error) => {
                tx.abort()?;
                Err(error)
            }
        }
    }
}

/// Version 6: registers every namespace that has data (nodes, edges or
/// indexes), as created now.
/// Layout 13: adjacency values in the compact encoding of [`Adj`]. Each
/// table is renamed, copied entry by entry into a new one under its name,
/// and dropped.
fn compact_adjacency(tx: &WriteTransaction) -> Result<()> {
    type Old = TableDefinition<'static, AdjKey, AdjValue>;
    for (name, aside) in [
        ("adjacency_out", "adjacency_out_v12"),
        ("adjacency_in", "adjacency_in_v12"),
    ] {
        let old: Old = TableDefinition::new(name);
        let renamed: Old = TableDefinition::new(aside);
        // Already compact (a file marked with an older version by hand).
        match tx.open_table(old) {
            Err(redb::TableError::TableTypeMismatch { .. }) => continue,
            other => drop(other.map_err(storage)?),
        }
        tx.rename_table(old, renamed).map_err(storage)?;
        {
            let from = tx.open_table(renamed).map_err(storage)?;
            let mut to = tx
                .open_table(TableDefinition::<AdjKey, Adj>::new(name))
                .map_err(storage)?;
            for item in from.iter().map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                to.insert(key.value(), value.value()).map_err(storage)?;
            }
        }
        tx.delete_table(renamed).map_err(storage)?;
    }
    Ok(())
}

/// Layout 13: a quantized copy of every vector, for searches.
fn quantize_vectors(tx: &WriteTransaction) -> Result<()> {
    let vectors = tx.open_table(VECTORS).map_err(storage)?;
    let mut quantized = tx.open_table(VECTORS_Q8).map_err(storage)?;
    for item in vectors.iter().map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let (chunks, _) = value.value().as_chunks::<4>();
        let vector: Vec<f32> = chunks.iter().map(|c| f32::from_le_bytes(*c)).collect();
        quantized
            .insert(key.value(), vector::quantize(&vector).as_slice())
            .map_err(storage)?;
    }
    Ok(())
}

fn migrate_to_6(tx: &WriteTransaction) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for item in tx
        .open_table(NS_COUNTS)
        .map_err(storage)?
        .iter()
        .map_err(storage)?
    {
        names.insert(item.map_err(storage)?.0.value().to_owned());
    }
    for item in tx
        .open_table(INDEXES)
        .map_err(storage)?
        .iter()
        .map_err(storage)?
    {
        names.insert(item.map_err(storage)?.0.value().0.to_owned());
    }
    let mut registry = tx.open_table(NAMESPACES).map_err(storage)?;
    let meta = encode(&NamespaceMeta {
        created_at: Timestamp::now(),
        description: None,
    })?;
    for name in &names {
        if registry.get(name.as_str()).map_err(storage)?.is_none() {
            registry
                .insert(name.as_str(), meta.as_slice())
                .map_err(storage)?;
        }
    }
    Ok(())
}

/// Version 4: builds the names index from the node records. (The memory
/// table starts empty: every node has the default state.)
fn migrate_to_4(tx: &WriteTransaction) -> Result<()> {
    let mut names = tx.open_table(NAMES).map_err(storage)?;
    names.retain(|_, _| false).map_err(storage)?;
    for item in tx
        .open_table(NODES)
        .map_err(storage)?
        .iter()
        .map_err(storage)?
    {
        let (key, value) = item.map_err(storage)?;
        let (ns, id) = key.value();
        let node: Node = decode(value.value())?;
        for name in names_of(&node) {
            names.insert((ns, name.as_str(), id), ()).map_err(storage)?;
        }
    }
    Ok(())
}

/// Version 3: rebuilds the adjacency tables from the edge records and the
/// postings from the stored texts, then drops the old tables.
fn migrate_to_3(tx: &WriteTransaction) -> Result<()> {
    let mut out = tx.open_table(ADJ_OUT).map_err(storage)?;
    let mut inn = tx.open_table(ADJ_IN).map_err(storage)?;
    out.retain(|_, _| false).map_err(storage)?;
    inn.retain(|_, _| false).map_err(storage)?;
    for item in tx
        .open_table(EDGES)
        .map_err(storage)?
        .iter()
        .map_err(storage)?
    {
        let (key, value) = item.map_err(storage)?;
        let ns = key.value().0;
        let edge: Edge = decode(value.value())?;
        let (from, to, id) = (edge.from.as_u128(), edge.to.as_u128(), edge.id.as_u128());
        out.insert(
            (ns, from, edge.label.as_str(), id),
            adjacency_value(&edge, edge.to),
        )
        .map_err(storage)?;
        inn.insert(
            (ns, to, edge.label.as_str(), id),
            adjacency_value(&edge, edge.from),
        )
        .map_err(storage)?;
    }
    drop((out, inn));

    let mut postings = tx.open_table(POSTINGS).map_err(storage)?;
    postings.retain(|_, _| false).map_err(storage)?;
    for item in tx
        .open_table(TEXTS)
        .map_err(storage)?
        .iter()
        .map_err(storage)?
    {
        let (key, value) = item.map_err(storage)?;
        let (ns, id) = key.value();
        let terms = text::tokenize(value.value());
        let length = terms.len() as u32;
        for (term, frequency) in text::term_frequencies(&terms) {
            postings
                .insert((ns, term, id), (frequency, length))
                .map_err(storage)?;
        }
    }
    drop(postings);

    tx.delete_table(OLD_ADJ_OUT).map_err(storage)?;
    tx.delete_table(OLD_ADJ_IN).map_err(storage)?;
    tx.delete_table(OLD_POSTINGS).map_err(storage)?;
    Ok(())
}

/// Rebuilds every counter from the node and edge records.
fn recount(tx: &WriteTransaction) -> Result<()> {
    let mut namespaces: HashMap<String, (u64, u64)> = HashMap::new();
    let mut node_labels: HashMap<(String, String), u64> = HashMap::new();
    let mut edge_labels: HashMap<(String, String), u64> = HashMap::new();
    {
        let nodes = tx.open_table(NODES).map_err(storage)?;
        for item in nodes.iter().map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let ns = key.value().0.to_owned();
            let node: Node = decode(value.value())?;
            namespaces.entry(ns.clone()).or_default().0 += 1;
            for label in node.labels {
                *node_labels.entry((ns.clone(), label)).or_default() += 1;
            }
        }
        let edges = tx.open_table(EDGES).map_err(storage)?;
        for item in edges.iter().map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let ns = key.value().0.to_owned();
            let edge: Edge = decode(value.value())?;
            namespaces.entry(ns.clone()).or_default().1 += 1;
            *edge_labels.entry((ns, edge.label)).or_default() += 1;
        }
    }
    let mut table = tx.open_table(NS_COUNTS).map_err(storage)?;
    table.retain(|_, _| false).map_err(storage)?;
    for (ns, counts) in &namespaces {
        table.insert(ns.as_str(), *counts).map_err(storage)?;
    }
    drop(table);
    for (definition, counts) in [
        (LABEL_COUNTS, &node_labels),
        (EDGE_LABEL_COUNTS, &edge_labels),
    ] {
        let mut table = tx.open_table(definition).map_err(storage)?;
        table.retain(|_, _| false).map_err(storage)?;
        for ((ns, label), count) in counts {
            table
                .insert((ns.as_str(), label.as_str()), *count)
                .map_err(storage)?;
        }
    }
    Ok(())
}

impl Drop for Grafeno {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// A read transaction: a fixed snapshot of the database.
pub struct ReadTx {
    pub(crate) nodes: ReadOnlyTable<RecordKey, &'static [u8]>,
    pub(crate) edges: ReadOnlyTable<RecordKey, &'static [u8]>,
    pub(crate) out: ReadOnlyTable<AdjKey, Adj>,
    pub(crate) inn: ReadOnlyTable<AdjKey, Adj>,
    pub(crate) labels: ReadOnlyTable<LabelKey, ()>,
    pub(crate) texts: ReadOnlyTable<RecordKey, &'static str>,
    pub(crate) text_lengths: ReadOnlyTable<RecordKey, u32>,
    pub(crate) postings: ReadOnlyTable<PostingKey, (u32, u32)>,
    pub(crate) text_stats: ReadOnlyTable<&'static str, (u64, u64)>,
    pub(crate) vectors: ReadOnlyTable<RecordKey, &'static [u8]>,
    pub(crate) vectors_q8: ReadOnlyTable<RecordKey, &'static [u8]>,
    pub(crate) hnsw_links: ReadOnlyTable<LinkKey, &'static [u8]>,
    pub(crate) vector_meta: ReadOnlyTable<&'static str, &'static [u8]>,
    pub(crate) ns_counts: ReadOnlyTable<&'static str, (u64, u64)>,
    pub(crate) label_counts: ReadOnlyTable<CountKey, u64>,
    pub(crate) edge_label_counts: ReadOnlyTable<CountKey, u64>,
    pub(crate) names: ReadOnlyTable<LabelKey, ()>,
    pub(crate) memory: ReadOnlyTable<RecordKey, (f32, u32, i64)>,
    pub(crate) indexes: ReadOnlyTable<(&'static str, &'static str), &'static [u8]>,
    pub(crate) index_entries: ReadOnlyTable<IndexKey, ()>,
    pub(crate) fulltext_stats: ReadOnlyTable<(&'static str, &'static str), &'static [u8]>,
    pub(crate) registry: ReadOnlyTable<&'static str, &'static [u8]>,
    pub(crate) merges: ReadOnlyTable<RecordKey, &'static [u8]>,
    pub(crate) match_rules: ReadOnlyTable<(&'static str, &'static str), &'static [u8]>,
    pub(crate) state: ReadOnlyTable<crate::state::StateKey, &'static [u8]>,
    pub(crate) constraints: ReadOnlyTable<(&'static str, &'static str), &'static [u8]>,
    pub(crate) changes: ReadOnlyTable<(&'static str, u64), &'static [u8]>,
    pub(crate) shared: Arc<Shared>,
    /// The commit this snapshot sees, when known (for the projection cache).
    pub(crate) generation: Option<u64>,
    pub(crate) tx: ReadTransaction,
    /// What its reader may see (14.4), set on a snapshot of its own with
    /// [`ReadTx::with_view`]: hidden records are absent from what it reads.
    pub(crate) view: Option<Arc<crate::View>>,
}

impl ReadTx {
    /// This snapshot as `view` sees it (14.4): every read leaves out the
    /// nodes, edges and properties hidden from it. It takes the snapshot
    /// itself, so a shared one cannot be given a reader's view.
    pub fn with_view(mut self, view: Option<Arc<crate::View>>) -> Self {
        self.view = view;
        self
    }

    /// The view this snapshot reads with, if any.
    pub fn view(&self) -> Option<&crate::View> {
        self.view.as_deref()
    }

    /// The view, when records of `ns` are checked one by one.
    fn per_record(&self, ns: &str) -> Option<&crate::View> {
        self.view.as_deref().filter(|view| view.per_record(ns))
    }

    /// Whether nothing of `ns` may be read through this snapshot.
    fn blind(&self, ns: &str) -> bool {
        self.view.as_deref().is_some_and(|view| !view.reads(ns))
    }

    /// Whether the node `id` exists and may be seen.
    pub fn sees(&self, ns: &str, id: NodeId) -> Result<bool> {
        if self.blind(ns) {
            return Ok(false);
        }
        let node: Option<Node> = read_record(&self.nodes, ns, id.as_u128())?;
        Ok(match (node, self.per_record(ns)) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(node), Some(view)) => view.node_visible(ns, &node),
        })
    }

    /// Whether the node exists, seen or not.
    pub(crate) fn raw_node_exists(&self, ns: &str, id: NodeId) -> Result<bool> {
        Ok(self
            .nodes
            .get((ns, id.as_u128()))
            .map_err(storage)?
            .is_some())
    }

    /// Whether the edge exists, seen or not.
    pub(crate) fn raw_edge_exists(&self, ns: &str, id: EdgeId) -> Result<bool> {
        Ok(self
            .edges
            .get((ns, id.as_u128()))
            .map_err(storage)?
            .is_some())
    }

    pub(crate) fn seen_node(&self, ns: &str, node: Node) -> Option<Node> {
        if self.blind(ns) {
            return None;
        }
        match self.per_record(ns) {
            Some(view) => view.node(ns, node),
            None => Some(node),
        }
    }

    pub(crate) fn seen_edge(&self, ns: &str, edge: Edge) -> Result<Option<Edge>> {
        if self.blind(ns) {
            return Ok(None);
        }
        match self.per_record(ns) {
            Some(view) => view.edge(ns, edge, |id| self.sees(ns, id)),
            None => Ok(Some(edge)),
        }
    }

    pub fn node(&self, ns: &str, id: NodeId) -> Result<Option<Node>> {
        if self.blind(ns) {
            return Ok(None);
        }
        let node: Option<Node> = read_record(&self.nodes, ns, id.as_u128())?;
        Ok(node.and_then(|node| self.seen_node(ns, node)))
    }

    pub fn edge(&self, ns: &str, id: EdgeId) -> Result<Option<Edge>> {
        if self.blind(ns) {
            return Ok(None);
        }
        match read_record(&self.edges, ns, id.as_u128())? {
            Some(edge) => self.seen_edge(ns, edge),
            None => Ok(None),
        }
    }

    /// Search hits the view may see, up to `limit`: `fetch(n)` gives the
    /// best `n`, asked again with more while hidden ones crowd out the
    /// visible (the decision 14.4 took about filtering vector search:
    /// after the graph walk, widening it, with recall measured).
    fn visible_hits(
        &self,
        ns: &str,
        limit: usize,
        fetch: impl Fn(usize) -> Result<Vec<Hit>>,
    ) -> Result<Vec<Hit>> {
        if self.blind(ns) {
            return Ok(Vec::new());
        }
        if self.per_record(ns).is_none() {
            return fetch(limit);
        }
        let mut ask = limit.max(1);
        loop {
            let hits = fetch(ask)?;
            let exhausted = hits.len() < ask;
            // As many as were found at most: the limit is the caller's, and
            // may be anything (a 10^12 once asked for that much memory).
            let mut out = Vec::with_capacity(limit.min(hits.len()));
            for hit in hits {
                if self.sees(ns, hit.node)? {
                    out.push(hit);
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
            if exhausted || ask >= 1 << 20 {
                return Ok(out);
            }
            ask = ask.saturating_mul(4);
        }
    }

    /// Reads several IDs in this snapshot, keeping order, duplicates and missing ones.
    pub fn nodes_by_id(&self, ns: &str, ids: &[NodeId]) -> Result<Vec<Option<Node>>> {
        ids.iter().map(|&id| self.node(ns, id)).collect()
    }

    /// Reads edges by ID, retracted ones included, like [`ReadTx::edge`].
    pub fn edges_by_id(&self, ns: &str, ids: &[EdgeId]) -> Result<Vec<Option<Edge>>> {
        ids.iter().map(|&id| self.edge(ns, id)).collect()
    }

    /// Every node of the namespace. Prefer [`ReadTx::nodes_page`] for large ones.
    pub fn nodes(&self, ns: &str) -> Result<Vec<Node>> {
        if self.blind(ns) {
            return Ok(Vec::new());
        }
        let nodes: Vec<Node> = scan_records(&self.nodes, ns)?;
        Ok(nodes
            .into_iter()
            .filter_map(|node| self.seen_node(ns, node))
            .collect())
    }

    /// Every edge of the namespace, including retracted ones. Prefer
    /// [`ReadTx::edges_page`] for large ones.
    pub fn edges(&self, ns: &str) -> Result<Vec<Edge>> {
        if self.blind(ns) {
            return Ok(Vec::new());
        }
        let edges: Vec<Edge> = scan_records(&self.edges, ns)?;
        let mut out = Vec::with_capacity(edges.len());
        for edge in edges {
            if let Some(edge) = self.seen_edge(ns, edge)? {
                out.push(edge);
            }
        }
        Ok(out)
    }

    /// Up to `limit` nodes after the id `after`, in id (creation) order, only
    /// those with `label` if given. Pass the last id back as `after` to get
    /// the next page.
    pub fn nodes_page(
        &self,
        ns: &str,
        label: Option<&str>,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<Node>> {
        if self.blind(ns) {
            return Ok(Vec::new());
        }
        if self.per_record(ns).is_none() {
            return self.raw_nodes_page(ns, label, after, limit);
        }
        // Fewer than `limit` only at the end, as without a view.
        let mut out = Vec::new();
        let mut after = after;
        loop {
            let page = self.raw_nodes_page(ns, label, after, limit)?;
            let full = page.len() == limit;
            after = page.last().map(|node| node.id).or(after);
            out.extend(page.into_iter().filter_map(|node| self.seen_node(ns, node)));
            if out.len() >= limit || !full {
                out.truncate(limit);
                return Ok(out);
            }
        }
    }

    fn raw_nodes_page(
        &self,
        ns: &str,
        label: Option<&str>,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<Node>> {
        let after = after.map(NodeId::as_u128);
        match label {
            None => scan_page(&self.nodes, ns, after, limit, |_: &Node| true),
            Some(label) => label_page(&self.labels, ns, label, after, limit)?
                .into_iter()
                .map(|id| self.node(ns, id)?.ok_or(Error::NodeNotFound(id)))
                .collect(),
        }
    }

    /// Up to `limit` edges after the id `after`, in id order, only those with
    /// `label` if given (filtering scans the namespace's edges).
    pub fn edges_page(
        &self,
        ns: &str,
        label: Option<&str>,
        after: Option<EdgeId>,
        limit: usize,
    ) -> Result<Vec<Edge>> {
        if self.blind(ns) {
            return Ok(Vec::new());
        }
        let raw = |after: Option<EdgeId>| -> Result<Vec<Edge>> {
            scan_page(
                &self.edges,
                ns,
                after.map(EdgeId::as_u128),
                limit,
                |edge: &Edge| label.is_none_or(|label| edge.label == label),
            )
        };
        if self.per_record(ns).is_none() {
            return raw(after);
        }
        let mut out = Vec::new();
        let mut after = after;
        loop {
            let page = raw(after)?;
            let full = page.len() == limit;
            after = page.last().map(|edge| edge.id).or(after);
            for edge in page {
                if let Some(edge) = self.seen_edge(ns, edge)? {
                    out.push(edge);
                }
            }
            if out.len() >= limit || !full {
                out.truncate(limit);
                return Ok(out);
            }
        }
    }

    pub fn nodes_with_label(&self, ns: &str, label: &str) -> Result<Vec<NodeId>> {
        if self.blind(ns) {
            return Ok(Vec::new());
        }
        let ids = nodes_with_label(&self.labels, ns, label)?;
        if self.per_record(ns).is_none() {
            return Ok(ids);
        }
        let mut visible = Vec::with_capacity(ids.len());
        for id in ids {
            if self.sees(ns, id)? {
                visible.push(id);
            }
        }
        Ok(visible)
    }

    /// Every namespace, empty ones included, in name order.
    pub fn namespaces(&self) -> Result<Vec<NamespaceInfo>> {
        namespaces(&self.registry, &self.ns_counts)
    }

    pub fn stats(&self, ns: &str) -> Result<NamespaceStats> {
        if self.blind(ns) {
            return Ok(NamespaceStats {
                name: ns.to_owned(),
                ..NamespaceStats::default()
            });
        }
        // Counted from what the view sees: the stored counts would tell how
        // much is hidden.
        if self.per_record(ns).is_some() {
            let mut stats = NamespaceStats {
                name: ns.to_owned(),
                ..NamespaceStats::default()
            };
            let mut after = None;
            loop {
                let page = self.nodes_page(ns, None, after, 1024)?;
                let done = page.len() < 1024;
                after = page.last().map(|node| node.id);
                for node in page {
                    stats.nodes += 1;
                    for label in &node.labels {
                        *stats.node_labels.entry(label.clone()).or_default() += 1;
                    }
                    stats.texts += u64::from(text::text(&self.texts, ns, node.id)?.is_some());
                    stats.vectors +=
                        u64::from(vector::embedding(&self.vectors, ns, node.id)?.is_some());
                }
                if done {
                    break;
                }
            }
            let mut after = None;
            loop {
                let page = self.edges_page(ns, None, after, 1024)?;
                let done = page.len() < 1024;
                after = page.last().map(|edge| edge.id);
                for edge in page {
                    if edge.is_retracted() {
                        continue;
                    }
                    stats.edges += 1;
                    *stats.edge_labels.entry(edge.label.clone()).or_default() += 1;
                }
                if done {
                    break;
                }
            }
            return Ok(stats);
        }
        let (nodes, edges) = self
            .ns_counts
            .get(ns)
            .map_err(storage)?
            .map(|guard| guard.value())
            .unwrap_or((0, 0));
        let (texts, text_terms) = self
            .text_stats
            .get(ns)
            .map_err(storage)?
            .map(|guard| guard.value())
            .unwrap_or((0, 0));
        let meta = vector::read_meta(&self.vector_meta, ns)?;
        let vectors = meta.as_ref().map_or(0, |meta| meta.count());
        Ok(NamespaceStats {
            name: ns.to_owned(),
            nodes,
            edges,
            node_labels: label_counts(&self.label_counts, ns)?,
            edge_labels: label_counts(&self.edge_label_counts, ns)?,
            texts,
            text_terms,
            vectors,
            vector_dimension: meta.filter(|_| vectors > 0).map(|meta| meta.dimension()),
        })
    }

    /// The indexed text of a node.
    pub fn text(&self, ns: &str, id: NodeId) -> Result<Option<String>> {
        if self.view.is_some() && !self.sees(ns, id)? {
            return Ok(None);
        }
        text::text(&self.texts, ns, id)
    }

    /// The stored (normalized) embedding of a node.
    pub fn embedding(&self, ns: &str, id: NodeId) -> Result<Option<Vec<f32>>> {
        if self.view.is_some() && !self.sees(ns, id)? {
            return Ok(None);
        }
        vector::embedding(&self.vectors, ns, id)
    }

    /// Full-text search ranked by BM25, best first.
    pub fn search_text(&self, ns: &str, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.visible_hits(ns, limit, |ask| {
            text::search(&self.postings, &self.text_stats, ns, query, ask)
        })
    }

    /// Approximate nearest neighbors by cosine similarity, best first.
    pub fn search_vector(&self, ns: &str, query: &[f32], limit: usize) -> Result<Vec<Hit>> {
        self.search_vector_with(ns, query, limit, limit.saturating_mul(4).max(64))
    }

    /// Like [`ReadTx::search_vector`], with the size `ef` of the candidate
    /// list: higher is slower and finds more of the true nearest neighbors.
    pub fn search_vector_with(
        &self,
        ns: &str,
        query: &[f32],
        limit: usize,
        ef: usize,
    ) -> Result<Vec<Hit>> {
        // Each round widens the walk with what it asks for.
        self.visible_hits(ns, limit, |ask| {
            let widened = if ask > limit {
                ef.max(ask.saturating_mul(4))
            } else {
                ef
            };
            self.raw_search_vector(ns, query, ask, widened)
        })
    }

    fn raw_search_vector(
        &self,
        ns: &str,
        query: &[f32],
        limit: usize,
        ef: usize,
    ) -> Result<Vec<Hit>> {
        vector::search(
            &self.vectors,
            &self.vectors_q8,
            &self.hnsw_links,
            &self.vector_meta,
            ns,
            query,
            limit,
            ef,
        )
    }
}

/// Who is writing: the principal recorded on what it creates, whether it
/// may change only what it recorded itself, and what it may see and change
/// (14.4).
#[derive(Clone, Debug)]
pub struct Actor {
    pub name: String,
    /// Refuse changes to nodes and edges recorded by anyone else.
    pub own_only: bool,
    /// Its rights record by record: each change is checked against them.
    pub view: Option<Arc<crate::View>>,
}

/// Where the writes of a transaction come from (14.5), for the audit log
/// and the change log: the account (none for the server's token), the
/// credential (`key:name`, `session:id`, `token`) and the address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Origin {
    pub account: Option<String>,
    pub via: Option<String>,
    pub address: Option<String>,
}

impl PartialEq for Actor {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.own_only == other.own_only
            && match (&self.view, &other.view) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

impl Eq for Actor {}

impl Actor {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            own_only: false,
            view: None,
        }
    }

    pub fn with_view(mut self, view: Arc<crate::View>) -> Self {
        self.view = Some(view);
        self
    }

    pub fn own_only(mut self) -> Self {
        self.own_only = true;
        self
    }
}

/// A write transaction. Nothing is visible to others until `commit`.
pub struct WriteTx {
    pub(crate) tx: WriteTransaction,
    /// The database, for read snapshots of what this transaction started
    /// from (vector inserts are planned on one at commit).
    pub(crate) database: Arc<Database>,
    /// Decoded vectors and links, shared by the vector writes of this transaction.
    pub(crate) vectors: vector::Cache,
    pub(crate) shared: Arc<Shared>,
    pub(crate) actor: Option<Actor>,
    /// The constraints of the namespace last written, read once.
    pub(crate) constraint_cache: Option<(String, Arc<Vec<crate::constraints::Constraint>>)>,
    /// What constraints did in this transaction.
    pub(crate) constraint_log: Vec<crate::constraints::ConstraintEvent>,
    /// Records whose property rules (16.5) are checked at
    /// [`WriteTx::check_deferred`] instead of at each write, when on.
    pub(crate) deferred_rules: Option<Vec<(String, bool, u128)>>,
    /// The event subscriptions of a namespace, read once (16.10).
    pub(crate) subscription_cache: Option<(String, Arc<Vec<crate::subscriptions::Subscription>>)>,
    /// Whether commits append to the change log.
    pub(crate) log_changes: bool,
    /// What this transaction changed, for the log.
    pub(crate) pending_changes: crate::changes::Pending,
    /// The primary records it touched, for the replication log (when on).
    pub(crate) capture: Option<crate::replication::Capture>,
    /// Applying a replication log entry: write what it says, derive the
    /// rest, decide nothing again.
    pub(crate) applying: bool,
    /// The commit's clock, when it is not now (an applied entry's).
    pub(crate) commit_at: Option<Timestamp>,
    /// Writes to namespaces fenced for a move go through (the move's own).
    pub(crate) past_fences: bool,
    /// Where its writes come from (14.5).
    pub(crate) origin: Option<Origin>,
}

impl WriteTx {
    /// Writes from now on are made by `actor` (until it is changed again):
    /// new nodes and edges record its name, and with `own_only` changes to
    /// records of others fail with [`Error::Forbidden`].
    pub fn act_as(&mut self, actor: Option<Actor>) {
        self.actor = actor;
    }

    pub fn actor(&self) -> Option<&Actor> {
        self.actor.as_ref()
    }

    /// Writes from now on come from `origin` (who, with what credential,
    /// from where), as the audit log and the change log record it.
    pub fn set_origin(&mut self, origin: Option<Origin>) {
        self.origin = origin;
    }

    pub fn origin(&self) -> Option<&Origin> {
        self.origin.as_ref()
    }

    /// Fails if the actor may change only its own records and this one is
    /// someone else's.
    fn guard(&self, recorded_by: Option<&str>, what: impl FnOnce() -> String) -> Result<()> {
        match &self.actor {
            Some(actor) if actor.own_only && recorded_by != Some(actor.name.as_str()) => {
                Err(Error::Forbidden(format!(
                    "{} may only change what it recorded, and {} was recorded by {}",
                    actor.name,
                    what(),
                    recorded_by.unwrap_or("someone else")
                )))
            }
            _ => Ok(()),
        }
    }

    /// The actor's view, when its writes are checked record by record (not
    /// while applying a replicated entry).
    fn view(&self) -> Option<Arc<crate::View>> {
        if self.applying {
            return None;
        }
        self.actor.as_ref().and_then(|actor| actor.view.clone())
    }

    /// Whether the actor sees the node `id` (true without a view).
    pub(crate) fn sees_node(&self, ns: &str, id: NodeId) -> Result<bool> {
        let Some(view) = self.view() else {
            return Ok(true);
        };
        if !view.per_record(ns) {
            return Ok(view.reads(ns));
        }
        Ok(self
            .node(ns, id)?
            .is_some_and(|node| view.node_visible(ns, &node)))
    }

    /// May the actor do `action` to `node` as it is now (to change or delete
    /// it)?
    pub(crate) fn guard_node_for(&self, action: crate::Act, ns: &str, node: &Node) -> Result<()> {
        self.guard(node.recorded_by.as_deref(), || {
            format!("node {:?}", node.display_name())
        })?;
        match self.view() {
            Some(view) => view.may_change_node(action, ns, Some(node), None, &[]),
            None => Ok(()),
        }
    }

    /// May the actor make `node` what it is (created or changed, with the
    /// properties `keys` written)?
    pub(crate) fn allow_node(
        &self,
        action: crate::Act,
        ns: &str,
        node: &Node,
        keys: &[&str],
    ) -> Result<()> {
        match self.view() {
            Some(view) => view.may_change_node(action, ns, None, Some(node), keys),
            None => Ok(()),
        }
    }

    pub(crate) fn guard_edge(&self, edge: &Edge) -> Result<()> {
        self.guard(edge.recorded_by.as_deref(), || {
            format!("the {} fact {}", edge.label, edge.id)
        })
    }

    pub(crate) fn guard_edge_for(&self, action: crate::Act, ns: &str, edge: &Edge) -> Result<()> {
        self.guard_edge(edge)?;
        match self.view() {
            Some(view) => view.may_change_edge(action, ns, Some(edge), None, &[], |id| {
                self.sees_node(ns, id)
            }),
            None => Ok(()),
        }
    }

    pub(crate) fn allow_edge(
        &self,
        action: crate::Act,
        ns: &str,
        edge: &Edge,
        keys: &[&str],
    ) -> Result<()> {
        match self.view() {
            Some(view) => view.may_change_edge(action, ns, None, Some(edge), keys, |id| {
                self.sees_node(ns, id)
            }),
            None => Ok(()),
        }
    }

    /// The node, if this actor may change it (update it, by default).
    fn node_to_change(&self, ns: &str, id: NodeId) -> Result<Node> {
        self.node_to_act(crate::Act::Update, ns, id)
    }

    fn node_to_act(&self, action: crate::Act, ns: &str, id: NodeId) -> Result<Node> {
        let node = self.node(ns, id)?.ok_or(Error::NodeNotFound(id))?;
        self.guard_node_for(action, ns, &node)?;
        Ok(node)
    }

    fn edge_to_change(&self, ns: &str, id: EdgeId) -> Result<Edge> {
        self.edge_to_act(crate::Act::Update, ns, id)
    }

    fn edge_to_act(&self, action: crate::Act, ns: &str, id: EdgeId) -> Result<Edge> {
        let edge = self.edge(ns, id)?.ok_or(Error::EdgeNotFound(id))?;
        self.guard_edge_for(action, ns, &edge)?;
        Ok(edge)
    }

    pub fn commit(mut self) -> Result<()> {
        // New vectors join their graphs now, all of them at once.
        vector::flush(&self.tx, &mut self.vectors, &self.database)?;
        self.fill_values()?;
        if let Some(at) = self.log_entry()? {
            self.commit_at = Some(at);
        }
        let touched = self.write_changes()?;
        let Self { tx, shared, .. } = self;
        shared.committing(|| tx.commit().map_err(storage))?;
        if !touched.is_empty() {
            shared.changes.notify(&touched);
        }
        Ok(())
    }

    pub fn abort(self) -> Result<()> {
        self.tx.abort().map_err(storage)
    }

    pub fn node(&self, ns: &str, id: NodeId) -> Result<Option<Node>> {
        let table = self.tx.open_table(NODES).map_err(storage)?;
        read_record(&table, ns, id.as_u128())
    }

    pub fn edge(&self, ns: &str, id: EdgeId) -> Result<Option<Edge>> {
        let table = self.tx.open_table(EDGES).map_err(storage)?;
        read_record(&table, ns, id.as_u128())
    }

    /// [`ReadTx::nodes_page`] as written so far in this transaction.
    pub fn nodes_page(
        &self,
        ns: &str,
        label: Option<&str>,
        after: Option<NodeId>,
        limit: usize,
    ) -> Result<Vec<Node>> {
        let after = after.map(NodeId::as_u128);
        let nodes = self.tx.open_table(NODES).map_err(storage)?;
        match label {
            None => scan_page(&nodes, ns, after, limit, |_: &Node| true),
            Some(label) => {
                let labels = self.tx.open_table(NODE_LABELS).map_err(storage)?;
                label_page(&labels, ns, label, after, limit)?
                    .into_iter()
                    .map(|id| read_record(&nodes, ns, id.as_u128())?.ok_or(Error::NodeNotFound(id)))
                    .collect()
            }
        }
    }

    /// The indexed text of a node, as written so far in this transaction.
    pub fn text(&self, ns: &str, id: NodeId) -> Result<Option<String>> {
        let table = self.tx.open_table(TEXTS).map_err(storage)?;
        text::text(&table, ns, id)
    }

    /// The stored (normalized) embedding of a node, as written so far in
    /// this transaction.
    pub fn embedding(&self, ns: &str, id: NodeId) -> Result<Option<Vec<f32>>> {
        let table = self.tx.open_table(VECTORS).map_err(storage)?;
        vector::embedding(&table, ns, id)
    }

    pub fn create_node(&mut self, ns: &str, new: NewNode) -> Result<NodeId> {
        let id = NodeId::new();
        self.upsert_node(ns, id, new)?;
        Ok(id)
    }

    /// Creates the node with the given id, or replaces it entirely if it
    /// exists: labels, properties, sources, text and embedding (a missing
    /// text or embedding is removed). Its edges and `created_at` are kept.
    /// Returns whether the node was created.
    ///
    /// This lets a client keep its own ids and sync idempotently.
    pub fn upsert_node(&mut self, ns: &str, id: NodeId, new: NewNode) -> Result<bool> {
        check_namespace(ns)?;
        let labels = normalize_labels(new.labels)?;
        let now = Timestamp::now();
        let existing = self.node(ns, id)?;
        if let Some(old) = &existing {
            self.guard_node_for(crate::Act::Update, ns, old)?;
        }
        let node = Node {
            id,
            labels,
            props: new.props,
            sources: new.sources,
            created_at: existing.as_ref().map_or(now, |old| old.created_at),
            updated_at: now,
            recorded_by: match &existing {
                Some(old) => old.recorded_by.clone(),
                None => self.actor.as_ref().map(|actor| actor.name.clone()),
            },
        };
        let written = changed_keys(existing.as_ref().map(|old| &old.props), &node.props);
        let written: Vec<&str> = written.iter().map(String::as_str).collect();
        self.allow_node(
            if existing.is_some() {
                crate::Act::Update
            } else {
                crate::Act::Create
            },
            ns,
            &node,
            &written,
        )?;
        self.put_node(ns, &node, existing.as_ref())?;
        self.replace_indexes(ns, id, new.text.as_deref(), new.embedding.as_deref())?;
        Ok(existing.is_none())
    }

    /// Writes a node exactly as given, timestamps included, with its text and
    /// embedding (replacing any previous ones). Used to import exports.
    pub fn restore_node(
        &mut self,
        ns: &str,
        mut node: Node,
        text: Option<&str>,
        embedding: Option<&[f32]>,
    ) -> Result<()> {
        check_namespace(ns)?;
        node.labels = normalize_labels(node.labels)?;
        let existing = self.node(ns, node.id)?;
        self.put_node(ns, &node, existing.as_ref())?;
        self.replace_indexes(ns, node.id, text, embedding)
    }

    fn replace_indexes(
        &mut self,
        ns: &str,
        id: NodeId,
        text: Option<&str>,
        embedding: Option<&[f32]>,
    ) -> Result<()> {
        self.note(Touch::Text(ns.to_owned(), id.as_u128()));
        self.note(Touch::Vector(ns.to_owned(), id.as_u128()));
        let changed_text = match text {
            Some(text) => {
                text::index(&self.tx, ns, id, text)?;
                true
            }
            None => text::remove(&self.tx, ns, id)?,
        };
        let changed_embedding = match embedding {
            Some(embedding) => {
                vector::insert(&self.tx, &mut self.vectors, ns, id, embedding)?;
                true
            }
            None => vector::remove(&self.tx, &mut self.vectors, ns, id)?,
        };
        if changed_text {
            self.log_node_part(ns, id, Part::Text)?;
        }
        if changed_embedding {
            self.log_node_part(ns, id, Part::Embedding)?;
        }
        Ok(())
    }

    /// Merges `props` into the node. A `null` value is stored as `null`
    /// (the key stays); [`WriteTx::remove_node_props`] removes keys.
    pub fn set_node_props(&mut self, ns: &str, id: NodeId, props: Props) -> Result<()> {
        let mut node = self.node_to_change(ns, id)?;
        let previous = node.clone();
        let keys: Vec<String> = props.keys().cloned().collect();
        merge_props(&mut node.props, props);
        node.updated_at = Timestamp::now();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.allow_node(crate::Act::Update, ns, &node, &keys)?;
        self.put_node(ns, &node, Some(&previous))
    }

    /// Removes these properties from the node (absent ones are ignored).
    pub fn remove_node_props(&mut self, ns: &str, id: NodeId, keys: &[&str]) -> Result<()> {
        let mut node = self.node_to_change(ns, id)?;
        if !keys.iter().any(|key| node.props.contains_key(*key)) {
            return Ok(());
        }
        let previous = node.clone();
        for key in keys {
            node.props.remove(*key);
        }
        node.updated_at = Timestamp::now();
        self.allow_node(crate::Act::Update, ns, &node, keys)?;
        self.put_node(ns, &node, Some(&previous))
    }

    pub fn add_label(&mut self, ns: &str, id: NodeId, label: &str) -> Result<()> {
        check_label(label)?;
        let mut node = self.node_to_change(ns, id)?;
        if let Err(at) = node
            .labels
            .binary_search_by(|item| item.as_str().cmp(label))
        {
            let previous = node.clone();
            node.labels.insert(at, label.to_owned());
            node.updated_at = Timestamp::now();
            self.allow_node(crate::Act::Update, ns, &node, &[])?;
            self.put_node(ns, &node, Some(&previous))?;
        }
        Ok(())
    }

    pub fn remove_label(&mut self, ns: &str, id: NodeId, label: &str) -> Result<()> {
        let mut node = self.node_to_change(ns, id)?;
        if let Ok(at) = node
            .labels
            .binary_search_by(|item| item.as_str().cmp(label))
        {
            if node.labels.len() == 1 {
                return Err(Error::Invalid("a node needs at least one label".into()));
            }
            let previous = node.clone();
            node.labels.remove(at);
            node.updated_at = Timestamp::now();
            self.allow_node(crate::Act::Update, ns, &node, &[])?;
            self.put_node(ns, &node, Some(&previous))?;
        }
        Ok(())
    }

    /// Indexes `text` for full-text search, replacing the previous text.
    pub fn set_text(&mut self, ns: &str, id: NodeId, text: &str) -> Result<()> {
        self.node_to_change(ns, id)?;
        self.note(Touch::Text(ns.to_owned(), id.as_u128()));
        text::index(&self.tx, ns, id, text)?;
        self.log_node_part(ns, id, Part::Text)
    }

    /// Indexes the texts of many nodes at once, replacing any they had: for
    /// loading, several times faster than one [`WriteTx::set_text`] each.
    pub fn set_texts(&mut self, ns: &str, texts: &[(NodeId, String)]) -> Result<()> {
        for (node, _) in texts {
            self.node_to_change(ns, *node)?;
            self.note(Touch::Text(ns.to_owned(), node.as_u128()));
        }
        text::index_many(&self.tx, ns, texts)?;
        for (node, _) in texts {
            self.log_node_part(ns, *node, Part::Text)?;
        }
        Ok(())
    }

    /// Removes the node's text from the index. Returns whether it had any.
    pub fn remove_text(&mut self, ns: &str, id: NodeId) -> Result<bool> {
        if let Some(node) = self.node(ns, id)? {
            self.guard_node_for(crate::Act::Update, ns, &node)?;
        }
        self.note(Touch::Text(ns.to_owned(), id.as_u128()));
        let removed = text::remove(&self.tx, ns, id)?;
        if removed {
            self.log_node_part(ns, id, Part::Text)?;
        }
        Ok(removed)
    }

    /// Indexes `embedding` for vector search, replacing the previous one. All
    /// embeddings of a namespace must have the same dimension.
    pub fn set_embedding(&mut self, ns: &str, id: NodeId, embedding: &[f32]) -> Result<()> {
        self.node_to_change(ns, id)?;
        self.note(Touch::Vector(ns.to_owned(), id.as_u128()));
        vector::insert(&self.tx, &mut self.vectors, ns, id, embedding)?;
        self.log_node_part(ns, id, Part::Embedding)
    }

    /// Removes the node's embedding from the index. Returns whether it had one.
    pub fn remove_embedding(&mut self, ns: &str, id: NodeId) -> Result<bool> {
        if let Some(node) = self.node(ns, id)? {
            self.guard_node_for(crate::Act::Update, ns, &node)?;
        }
        self.note(Touch::Vector(ns.to_owned(), id.as_u128()));
        let removed = vector::remove(&self.tx, &mut self.vectors, ns, id)?;
        if removed {
            self.log_node_part(ns, id, Part::Embedding)?;
        }
        Ok(removed)
    }

    /// Deletes the node, its edges and its indexed text and embedding, without
    /// keeping history.
    pub fn delete_node(&mut self, ns: &str, id: NodeId) -> Result<()> {
        // Its edges go too: with `own_only`, each must be the actor's, and
        // each must be one the actor may delete.
        let node = self.node_to_act(crate::Act::Delete, ns, id)?;
        let mut edges: Vec<EdgeId> = Vec::new();
        for definition in [ADJ_OUT, ADJ_IN] {
            let table = self.tx.open_table(definition).map_err(storage)?;
            edges.extend(
                adjacency(&table, ns, id, None)?
                    .into_iter()
                    .map(|entry| entry.edge),
            );
        }
        edges.sort();
        edges.dedup();
        // An edge the actor may not see would go with the node: refused,
        // without naming it (14.4).
        if let Some(view) = self.view() {
            for &id in &edges {
                let visible = match self.edge(ns, id)? {
                    Some(edge) => view.edge_visible(ns, &edge, |end| self.sees_node(ns, end))?,
                    None => true,
                };
                if !visible {
                    return Err(Error::Forbidden(format!(
                        "node {} has edges hidden from {}: it cannot be deleted by it",
                        node.id,
                        view.account.as_deref().unwrap_or("the caller")
                    )));
                }
            }
        }
        for edge in edges {
            self.delete_edge(ns, edge)?;
        }
        for touch in [Touch::Text, Touch::Vector, Touch::Memory] {
            self.note(touch(ns.to_owned(), id.as_u128()));
        }
        text::remove(&self.tx, ns, id)?;
        vector::remove(&self.tx, &mut self.vectors, ns, id)?;
        self.tx
            .open_table(MEMORY)
            .map_err(storage)?
            .remove((ns, id.as_u128()))
            .map_err(storage)?;
        self.remove_node_record(ns, &node)
    }

    /// Removes a node record and what derives from it (property index,
    /// names, labels, counters), leaving its edges, text, embedding and
    /// memory to the caller.
    pub(crate) fn remove_node_record(&mut self, ns: &str, node: &Node) -> Result<()> {
        let id = node.id;
        self.note(Touch::Node(ns.to_owned(), id.as_u128()));
        self.remember_before(
            ns,
            id.as_u128(),
            false,
            &node.labels,
            Some((node.labels.as_slice(), &node.props)),
        )?;
        self.index_node(ns, None, Some(node))?;
        let mut names = self.tx.open_table(NAMES).map_err(storage)?;
        for name in names_of(node) {
            names
                .remove((ns, name.as_str(), id.as_u128()))
                .map_err(storage)?;
        }
        drop(names);
        let mut labels = self.tx.open_table(NODE_LABELS).map_err(storage)?;
        for label in &node.labels {
            labels
                .remove((ns, label.as_str(), id.as_u128()))
                .map_err(storage)?;
        }
        drop(labels);
        for label in &node.labels {
            bump_node_label(&self.tx, ns, label, -1)?;
        }
        let mut nodes = self.tx.open_table(NODES).map_err(storage)?;
        nodes.remove((ns, id.as_u128())).map_err(storage)?;
        drop(nodes);
        self.log_node(ns, node, ChangeOp::Deleted, None);
        bump_namespace(&self.tx, ns, -1, 0)
    }

    pub fn create_edge(&mut self, ns: &str, new: NewEdge) -> Result<EdgeId> {
        let edge = Edge {
            id: EdgeId::new(),
            label: new.label,
            from: new.from,
            to: new.to,
            props: new.props,
            sources: new.sources,
            valid_from: new.valid_from,
            valid_to: new.valid_to,
            recorded_at: Timestamp::now(),
            expired_at: None,
            recorded_by: self.actor.as_ref().map(|actor| actor.name.clone()),
        };
        let mut edge = edge;
        let written: Vec<&str> = edge.props.keys().map(String::as_str).collect();
        self.allow_edge(crate::Act::Create, ns, &edge, &written)?;
        let pending = self.check_edge(ns, &mut edge)?;
        let id = edge.id;
        self.restore_edge(ns, edge.clone())?;
        self.settle_edge(ns, &edge, pending)?;
        Ok(id)
    }

    /// Writes an edge exactly as given, timestamps included, replacing any
    /// edge with its id. Both ends must exist. Used to import exports.
    pub fn restore_edge(&mut self, ns: &str, edge: Edge) -> Result<()> {
        check_namespace(ns)?;
        check_label(&edge.label)?;
        check_interval(edge.valid_from, edge.valid_to)?;
        for end in [edge.from, edge.to] {
            if self.node(ns, end)?.is_none() {
                return Err(Error::NodeNotFound(end));
            }
        }
        if self.edge(ns, edge.id)?.is_some() {
            self.delete_edge(ns, edge.id)?;
        }
        self.put_edge(ns, &edge)?;
        bump_namespace(&self.tx, ns, 0, 1)?;
        bump_edge_label(&self.tx, ns, &edge.label, 1)
    }

    /// Merges `props` into the edge. A `null` value is stored as `null`
    /// (the key stays); [`WriteTx::remove_edge_props`] removes keys.
    pub fn set_edge_props(&mut self, ns: &str, id: EdgeId, props: Props) -> Result<()> {
        let mut edge = self.edge_to_change(ns, id)?;
        let keys: Vec<String> = props.keys().cloned().collect();
        merge_props(&mut edge.props, props);
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.allow_edge(crate::Act::Update, ns, &edge, &keys)?;
        self.put_edge(ns, &edge)
    }

    /// Removes these properties from the edge (absent ones are ignored).
    pub fn remove_edge_props(&mut self, ns: &str, id: EdgeId, keys: &[&str]) -> Result<()> {
        let mut edge = self.edge_to_change(ns, id)?;
        if !keys.iter().any(|key| edge.props.contains_key(*key)) {
            return Ok(());
        }
        for key in keys {
            edge.props.remove(*key);
        }
        self.allow_edge(crate::Act::Update, ns, &edge, keys)?;
        self.put_edge(ns, &edge)
    }

    /// The fact stopped being true at `at`: the world changed, not what we know.
    pub fn invalidate_edge(&mut self, ns: &str, id: EdgeId, at: Timestamp) -> Result<()> {
        let mut edge = self.edge_to_act(crate::Act::Retract, ns, id)?;
        check_interval(edge.valid_from, Some(at))?;
        edge.valid_to = Some(at);
        self.put_edge(ns, &edge)
    }

    /// The fact was a mistake. It stays in history, visible with
    /// `EdgeFilter::known_at` at an earlier instant.
    pub fn retract_edge(&mut self, ns: &str, id: EdgeId) -> Result<()> {
        let mut edge = self.edge_to_act(crate::Act::Retract, ns, id)?;
        if edge.expired_at.is_none() {
            edge.expired_at = Some(Timestamp::now());
            self.put_edge(ns, &edge)?;
        }
        Ok(())
    }

    /// Deletes the outgoing edges of `from` (only those with `label`, if
    /// given), without keeping history. Returns how many were deleted.
    ///
    /// Useful to replace derived edges, such as a note's links, on each sync.
    pub fn delete_edges_from(
        &mut self,
        ns: &str,
        from: NodeId,
        label: Option<&str>,
    ) -> Result<usize> {
        let edges = {
            let table = self.tx.open_table(ADJ_OUT).map_err(storage)?;
            adjacency(&table, ns, from, label)?
        };
        for entry in &edges {
            self.delete_edge(ns, entry.edge)?;
        }
        Ok(edges.len())
    }

    /// Deletes the edge without keeping history.
    pub fn delete_edge(&mut self, ns: &str, id: EdgeId) -> Result<()> {
        let edge = self.edge_to_act(crate::Act::Delete, ns, id)?;
        self.note(Touch::Edge(ns.to_owned(), id.as_u128()));
        let label = std::slice::from_ref(&edge.label);
        self.remember_before(ns, id.as_u128(), true, label, Some((label, &edge.props)))?;
        self.index_edge(ns, None, Some(&edge))?;
        let (from, to, edge_id) = (edge.from.as_u128(), edge.to.as_u128(), id.as_u128());
        let label = edge.label.as_str();
        self.tx
            .open_table(ADJ_OUT)
            .map_err(storage)?
            .remove((ns, from, label, edge_id))
            .map_err(storage)?;
        self.tx
            .open_table(ADJ_IN)
            .map_err(storage)?
            .remove((ns, to, label, edge_id))
            .map_err(storage)?;
        self.tx
            .open_table(EDGES)
            .map_err(storage)?
            .remove((ns, edge_id))
            .map_err(storage)?;
        self.log_edge(ns, &edge, ChangeOp::Deleted);
        bump_namespace(&self.tx, ns, 0, -1)?;
        bump_edge_label(&self.tx, ns, label, -1)
    }

    /// Writes the node record and brings the label index, the names index and
    /// the counters from `previous` (`None`: a new node) to `node`.
    pub(crate) fn put_node(
        &mut self,
        ns: &str,
        node: &Node,
        previous: Option<&Node>,
    ) -> Result<()> {
        let bytes = encode(node)?;
        let id = node.id.as_u128();
        self.note(Touch::Node(ns.to_owned(), id));
        self.tx
            .open_table(NODES)
            .map_err(storage)?
            .insert((ns, id), bytes.as_slice())
            .map_err(storage)?;

        self.remember_before(
            ns,
            id,
            false,
            &node.labels,
            previous.map(|old| (old.labels.as_slice(), &old.props)),
        )?;
        let defs = self.index_defs(ns)?;
        self.index_node_with(&defs, ns, Some(node), previous)?;
        if !self.applying {
            self.record_history(ns, node, previous)?;
        }
        self.check_rules_later(ns, false, id, &defs, &node.labels, &node.props, true)?;
        let op = if previous.is_some() {
            ChangeOp::Updated
        } else {
            ChangeOp::Created
        };
        self.log_node(ns, node, op, Some(Part::Record));

        let old_names = previous.map(names_of).unwrap_or_default();
        let new_names = names_of(node);
        let mut names = self.tx.open_table(NAMES).map_err(storage)?;
        for name in old_names.difference(&new_names) {
            names.remove((ns, name.as_str(), id)).map_err(storage)?;
        }
        for name in new_names.difference(&old_names) {
            names.insert((ns, name.as_str(), id), ()).map_err(storage)?;
        }
        drop(names);

        let old_labels = previous.map(|old| old.labels.as_slice());
        let old = old_labels.unwrap_or_default();
        let removed: Vec<&String> = old.iter().filter(|l| !node.labels.contains(l)).collect();
        let added: Vec<&String> = node.labels.iter().filter(|l| !old.contains(l)).collect();
        let mut index = self.tx.open_table(NODE_LABELS).map_err(storage)?;
        for label in &removed {
            index.remove((ns, label.as_str(), id)).map_err(storage)?;
        }
        for label in &added {
            index
                .insert((ns, label.as_str(), id), ())
                .map_err(storage)?;
        }
        drop(index);
        for label in removed {
            bump_node_label(&self.tx, ns, label, -1)?;
        }
        for label in added {
            bump_node_label(&self.tx, ns, label, 1)?;
        }
        if old_labels.is_none() {
            bump_namespace(&self.tx, ns, 1, 0)?;
        }
        Ok(())
    }

    /// Writes the edge record and its adjacency entries (no counters).
    pub(crate) fn put_edge(&mut self, ns: &str, edge: &Edge) -> Result<()> {
        self.note(Touch::Edge(ns.to_owned(), edge.id.as_u128()));
        let bytes = encode(edge)?;
        let (from, to, id) = (edge.from.as_u128(), edge.to.as_u128(), edge.id.as_u128());
        let label = edge.label.as_str();
        let previous: Option<Edge> = self
            .tx
            .open_table(EDGES)
            .map_err(storage)?
            .insert((ns, id), bytes.as_slice())
            .map_err(storage)?
            .map(|old| decode(old.value()))
            .transpose()?;
        let existed = previous.is_some();
        self.remember_before(
            ns,
            id,
            true,
            std::slice::from_ref(&edge.label),
            previous
                .as_ref()
                .map(|old| (std::slice::from_ref(&old.label), &old.props)),
        )?;
        let defs = self.index_defs(ns)?;
        self.index_edge_with(&defs, ns, Some(edge), previous.as_ref())?;
        let labels = [edge.label.clone()];
        let live = !edge.is_retracted();
        self.check_rules_later(ns, true, id, &defs, &labels, &edge.props, live)?;
        self.log_edge(
            ns,
            edge,
            if existed {
                ChangeOp::Updated
            } else {
                ChangeOp::Created
            },
        );
        self.tx
            .open_table(ADJ_OUT)
            .map_err(storage)?
            .insert((ns, from, label, id), adjacency_value(edge, edge.to))
            .map_err(storage)?;
        self.tx
            .open_table(ADJ_IN)
            .map_err(storage)?
            .insert((ns, to, label, id), adjacency_value(edge, edge.from))
            .map_err(storage)?;
        Ok(())
    }
}

fn normalize_labels(labels: Vec<String>) -> Result<Vec<String>> {
    if labels.is_empty() {
        return Err(Error::Invalid("a node needs at least one label".into()));
    }
    labels.iter().try_for_each(|label| check_label(label))?;
    let mut labels = labels;
    labels.sort();
    labels.dedup();
    Ok(labels)
}

/// Merges `changes` into `target`: nulls are values like any other.
fn merge_props(target: &mut Props, changes: Props) {
    target.extend(changes);
}

pub(crate) fn check_interval(from: Option<Timestamp>, to: Option<Timestamp>) -> Result<()> {
    if let (Some(from), Some(to)) = (from, to)
        && to <= from
    {
        return Err(Error::Invalid("valid_to must be after valid_from".into()));
    }
    Ok(())
}

/// The properties that differ between `before` (none: a new record) and
/// `after`: written, changed or removed.
fn changed_keys(before: Option<&Props>, after: &Props) -> Vec<String> {
    let mut keys: Vec<String> = after
        .iter()
        .filter(|(key, value)| before.and_then(|b| b.get(*key)) != Some(*value))
        .map(|(key, _)| key.clone())
        .collect();
    if let Some(before) = before {
        keys.extend(
            before
                .keys()
                .filter(|key| !after.contains_key(*key))
                .cloned(),
        );
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use grafeno_core::NewNode;

    #[test]
    fn version_1_databases_get_counters() {
        let path = std::env::temp_dir().join(format!("grafeno-migration-{}.redb", NodeId::new()));
        {
            let db = Grafeno::open(&path).unwrap();
            db.write(|tx| {
                let a = tx.create_node("ns", NewNode::new("Person").label("Admin"))?;
                let b = tx.create_node("ns", NewNode::new("Person"))?;
                tx.create_edge("ns", NewEdge::new(a, "knows", b))?;
                tx.create_node("other", NewNode::new("Note"))?;
                Ok(())
            })
            .unwrap();
        }
        {
            // Turn it into a version 1 database: no `meta` and no counters.
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            tx.delete_table(META).unwrap();
            tx.delete_table(NS_COUNTS).unwrap();
            tx.delete_table(LABEL_COUNTS).unwrap();
            tx.delete_table(EDGE_LABEL_COUNTS).unwrap();
            tx.commit().unwrap();
        }
        {
            let db = Grafeno::open(&path).unwrap();
            let read = db.begin_read().unwrap();
            let names: Vec<(String, u64, u64)> = read
                .namespaces()
                .unwrap()
                .into_iter()
                .map(|info| (info.name, info.nodes, info.edges))
                .collect();
            assert_eq!(names, vec![("ns".into(), 2, 1), ("other".into(), 1, 0)]);
            let stats = read.stats("ns").unwrap();
            assert_eq!(stats.node_labels["Person"], 2);
            assert_eq!(stats.node_labels["Admin"], 1);
            assert_eq!(stats.edge_labels["knows"], 1);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn version_2_databases_get_new_adjacency_and_postings() {
        let path = std::env::temp_dir().join(format!("grafeno-v2-{}.redb", NodeId::new()));
        let (a, b) = {
            let db = Grafeno::open(&path).unwrap();
            db.write(|tx| {
                let a = tx.create_node("ns", NewNode::new("Note").text("alpha beta beta"))?;
                let b = tx.create_node("ns", NewNode::new("Note").text("gamma"))?;
                let edge = tx.create_edge("ns", NewEdge::new(a, "links_to", b))?;
                tx.create_edge("ns", NewEdge::new(b, "links_to", a))?;
                tx.retract_edge("ns", edge)?;
                Ok((a, b))
            })
            .unwrap()
        };
        {
            // Turn it into a version 2 database: the old tables, no new ones.
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            tx.delete_table(ADJ_OUT).unwrap();
            tx.delete_table(ADJ_IN).unwrap();
            tx.delete_table(POSTINGS).unwrap();
            tx.open_table(OLD_ADJ_OUT)
                .unwrap()
                .insert(("ns", 1, "x", 2), 3)
                .unwrap();
            tx.open_table(OLD_ADJ_IN).unwrap();
            tx.open_table(OLD_POSTINGS)
                .unwrap()
                .insert(("ns", "alpha", 1), 1)
                .unwrap();
            tx.open_table(META).unwrap().insert(SCHEMA_KEY, 2).unwrap();
            tx.commit().unwrap();
        }
        {
            let db = Grafeno::open(&path).unwrap();
            let read = db.begin_read().unwrap();
            let report = read.check("ns").unwrap();
            assert!(report.is_ok(), "{:?}", report.problems);
            let out = read
                .neighbors("ns", b, crate::Direction::Out, &crate::EdgeFilter::any())
                .unwrap();
            assert_eq!(out[0].node, a);
            let retracted = read
                .neighbors("ns", a, crate::Direction::Out, &crate::EdgeFilter::any())
                .unwrap();
            assert!(
                retracted.is_empty(),
                "the retraction survives the migration"
            );
            assert_eq!(read.search_text("ns", "beta", 5).unwrap()[0].node, a);
        }
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_read().unwrap();
            let names: Vec<String> = tx
                .list_tables()
                .unwrap()
                .map(|t| redb::TableHandle::name(&t).to_owned())
                .collect();
            for old in ["adj_out", "adj_in", "postings"] {
                assert!(!names.iter().any(|name| name == old), "{old} was dropped");
            }
            assert_eq!(
                tx.open_table(META)
                    .unwrap()
                    .get(SCHEMA_KEY)
                    .unwrap()
                    .unwrap()
                    .value(),
                SCHEMA_VERSION
            );
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn version_3_databases_get_a_names_index() {
        let path = std::env::temp_dir().join(format!("grafeno-v3-{}.redb", NodeId::new()));
        let ana = {
            let db = Grafeno::open(&path).unwrap();
            db.write(|tx| tx.create_node("ns", NewNode::new("Person").prop("name", "Ana García")))
                .unwrap()
        };
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            tx.delete_table(NAMES).unwrap();
            tx.delete_table(MEMORY).unwrap();
            tx.open_table(META).unwrap().insert(SCHEMA_KEY, 3).unwrap();
            tx.commit().unwrap();
        }
        {
            let db = Grafeno::open(&path).unwrap();
            let read = db.begin_read().unwrap();
            assert_eq!(read.find_by_name("ns", "ana garcia").unwrap(), vec![ana]);
            assert!(read.check("ns").unwrap().is_ok());
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn newer_layouts_are_refused() {
        let path = std::env::temp_dir().join(format!("grafeno-future-{}.redb", NodeId::new()));
        drop(Grafeno::open(&path).unwrap());
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            tx.open_table(META)
                .unwrap()
                .insert(SCHEMA_KEY, SCHEMA_VERSION + 1)
                .unwrap();
            tx.commit().unwrap();
        }
        assert!(matches!(Grafeno::open(&path), Err(Error::Invalid(_))));
        std::fs::remove_file(&path).unwrap();
    }
}
