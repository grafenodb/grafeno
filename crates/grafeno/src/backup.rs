//! Online backups: a consistent copy of the whole database, written from a
//! read snapshot while writes go on.

use std::path::Path;

use tempfile::NamedTempFile;

use grafeno_core::{Error, Result};
use redb::{
    Database, ReadTransaction, ReadableDatabase, ReadableTable, TableDefinition, TableHandle,
    WriteTransaction,
};
use serde::Serialize;

use crate::access::{ACCOUNTS, API_KEYS, SESSIONS};
use crate::audit::AUDIT;
use crate::changes::CHANGES;
use crate::constraints::CONSTRAINTS;
use crate::db::{FileIntegrity, Grafeno, ReadTx};
use crate::index::{INDEX_ENTRIES, INDEXES};
use crate::merge::MERGES;
use crate::namespace::NAMESPACES;
use crate::resolve::MATCH_RULES;
use crate::rights::ROLES;
use crate::state::STATE;
use crate::store::{
    ADJ_IN, ADJ_OUT, EDGE_LABEL_COUNTS, EDGES, HNSW_LINKS, LABEL_COUNTS, MEMORY, META, NAMES,
    NODE_LABELS, NODES, NS_COUNTS, POSTINGS, TEXT_LENGTHS, TEXT_STATS, TEXTS, VECTOR_META, VECTORS,
    VECTORS_Q8, storage,
};

/// What a backup copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BackupStats {
    pub tables: usize,
    pub entries: u64,
    pub bytes: u64,
}

// Each operation owns its temporary file; it never removes others' files nor
// publishes an incomplete target. persist_noclobber also guards against races.
fn output(path: &Path) -> Result<NamedTempFile> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(Error::Invalid(format!(
                "{} exists already; choose a new file",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(storage(error)),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    NamedTempFile::with_prefix_in(".grafeno-backup-", parent).map_err(storage)
}

fn publish(file: NamedTempFile, path: &Path) -> Result<u64> {
    file.as_file().sync_all().map_err(storage)?;
    let bytes = file.as_file().metadata().map_err(storage)?.len();
    file.persist_noclobber(path).map_err(|error| {
        Error::Storage(format!(
            "cannot publish {} without replacing an existing file: {}",
            path.display(),
            error.error
        ))
    })?;
    Ok(bytes)
}

impl Grafeno {
    /// Recovers a closed backup into a new database file. The source is read
    /// only; checksum verification, layout upgrades and namespace checks run
    /// on a private copy. Reported repairs and namespace inconsistencies are
    /// rejected. Existing destinations (including symbolic links) are never replaced.
    ///
    /// Use a completed backup, not a live database that another process writes.
    /// No session is opened or replaced by this operation.
    pub fn recover_backup(source: &Path, path: &Path) -> Result<u64> {
        let mut file = output(path)?;
        let mut input = std::fs::File::open(source).map_err(storage)?;
        if !input.metadata().map_err(storage)?.is_file() {
            return Err(Error::Invalid("the backup must be a regular file".into()));
        }
        std::io::copy(&mut input, file.as_file_mut()).map_err(storage)?;
        if Self::verify_file(file.path())? != FileIntegrity::Clean {
            return Err(Error::Invalid(
                "the backup required repairs; recovery was not published".into(),
            ));
        }
        {
            // Do not turn an unrelated redb database into an empty Grafeno one.
            let db = Database::open(file.path()).map_err(storage)?;
            let read = db.begin_read().map_err(storage)?;
            let mut grafeno = false;
            for table in read.list_tables().map_err(storage)? {
                grafeno |= table.name() == NODES.name();
            }
            if !grafeno {
                return Err(Error::Invalid(
                    "the backup is not a Grafeno database".into(),
                ));
            }
        }
        {
            let db = Self::open(file.path())?;
            let read = db.begin_read()?;
            for namespace in read.namespaces()? {
                let report = read.check(&namespace.name)?;
                if !report.is_ok() {
                    return Err(Error::Invalid(format!(
                        "inconsistent backup in {}: {}",
                        namespace.name,
                        report.problems.join("; ")
                    )));
                }
            }
        }
        publish(file, path)
    }
}

fn copy<K: redb::Key + 'static, V: redb::Value + 'static>(
    from: &ReadTransaction,
    to: &WriteTransaction,
    definition: TableDefinition<K, V>,
) -> Result<u64> {
    let source = from.open_table(definition).map_err(storage)?;
    let mut target = to.open_table(definition).map_err(storage)?;
    let mut entries = 0;
    for item in source.iter().map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        target.insert(key.value(), value.value()).map_err(storage)?;
        entries += 1;
    }
    Ok(entries)
}

/// Replaces every table of `to` with the same table of `from` (a backup of
/// this layout), in one transaction.
pub(crate) fn restore_into(from: &ReadTransaction, to: &WriteTransaction) -> Result<u64> {
    let mut entries = 0;
    macro_rules! replace {
        ($($table:expr),* $(,)?) => {$(
            to.delete_table($table).map_err(storage)?;
            entries += copy(from, to, $table)?;
        )*};
    }
    replace!(
        META,
        NODES,
        EDGES,
        ADJ_OUT,
        ADJ_IN,
        NODE_LABELS,
        TEXTS,
        TEXT_LENGTHS,
        POSTINGS,
        TEXT_STATS,
        VECTORS,
        VECTORS_Q8,
        HNSW_LINKS,
        VECTOR_META,
        NS_COUNTS,
        LABEL_COUNTS,
        EDGE_LABEL_COUNTS,
        NAMES,
        MEMORY,
        INDEXES,
        INDEX_ENTRIES,
        NAMESPACES,
        ACCOUNTS,
        API_KEYS,
        SESSIONS,
        ROLES,
        AUDIT,
        MERGES,
        MATCH_RULES,
        STATE,
        CONSTRAINTS,
        CHANGES,
        crate::replication::REPLICATION_LOG,
    );
    Ok(entries)
}

impl ReadTx {
    /// Writes a copy of the whole database, as this snapshot sees it, to a
    /// new file at `path` (which must not exist). Writers are not blocked;
    /// the copy is a valid database, opened like any other. It is written
    /// to a unique temporary file next to `path` and published without
    /// replacement when complete. Errors clean up this operation's temporary file.
    pub fn backup(&self, path: &Path) -> Result<BackupStats> {
        let partial = output(path)?;
        let outcome = (|| {
            let db = Database::create(partial.path()).map_err(storage)?;
            let tx = db.begin_write().map_err(storage)?;
            let from = &self.tx;
            // A table added later and forgotten here would be lost: refuse.
            let known: Vec<&str> = vec![
                META.name(),
                NODES.name(),
                EDGES.name(),
                ADJ_OUT.name(),
                ADJ_IN.name(),
                NODE_LABELS.name(),
                TEXTS.name(),
                TEXT_LENGTHS.name(),
                POSTINGS.name(),
                TEXT_STATS.name(),
                VECTORS.name(),
                VECTORS_Q8.name(),
                HNSW_LINKS.name(),
                VECTOR_META.name(),
                NS_COUNTS.name(),
                LABEL_COUNTS.name(),
                EDGE_LABEL_COUNTS.name(),
                NAMES.name(),
                MEMORY.name(),
                INDEXES.name(),
                INDEX_ENTRIES.name(),
                crate::fulltext::FULLTEXT_STATS.name(),
                NAMESPACES.name(),
                ACCOUNTS.name(),
                API_KEYS.name(),
                SESSIONS.name(),
                ROLES.name(),
                AUDIT.name(),
                MERGES.name(),
                MATCH_RULES.name(),
                STATE.name(),
                CONSTRAINTS.name(),
                CHANGES.name(),
                crate::replication::REPLICATION_LOG.name(),
            ];
            for table in from.list_tables().map_err(storage)? {
                if !known.contains(&table.name()) {
                    return Err(Error::Storage(format!(
                        "the backup does not know the table {:?}",
                        table.name()
                    )));
                }
            }
            let mut entries = 0;
            entries += copy(from, &tx, META)?;
            entries += copy(from, &tx, NODES)?;
            entries += copy(from, &tx, EDGES)?;
            entries += copy(from, &tx, ADJ_OUT)?;
            entries += copy(from, &tx, ADJ_IN)?;
            entries += copy(from, &tx, NODE_LABELS)?;
            entries += copy(from, &tx, TEXTS)?;
            entries += copy(from, &tx, TEXT_LENGTHS)?;
            entries += copy(from, &tx, POSTINGS)?;
            entries += copy(from, &tx, TEXT_STATS)?;
            entries += copy(from, &tx, VECTORS)?;
            entries += copy(from, &tx, VECTORS_Q8)?;
            entries += copy(from, &tx, HNSW_LINKS)?;
            entries += copy(from, &tx, VECTOR_META)?;
            entries += copy(from, &tx, NS_COUNTS)?;
            entries += copy(from, &tx, LABEL_COUNTS)?;
            entries += copy(from, &tx, EDGE_LABEL_COUNTS)?;
            entries += copy(from, &tx, NAMES)?;
            entries += copy(from, &tx, MEMORY)?;
            entries += copy(from, &tx, INDEXES)?;
            entries += copy(from, &tx, INDEX_ENTRIES)?;
            entries += copy(from, &tx, crate::fulltext::FULLTEXT_STATS)?;
            entries += copy(from, &tx, NAMESPACES)?;
            entries += copy(from, &tx, ACCOUNTS)?;
            entries += copy(from, &tx, API_KEYS)?;
            entries += copy(from, &tx, SESSIONS)?;
            entries += copy(from, &tx, ROLES)?;
            entries += copy(from, &tx, AUDIT)?;
            entries += copy(from, &tx, MERGES)?;
            entries += copy(from, &tx, MATCH_RULES)?;
            entries += copy(from, &tx, STATE)?;
            entries += copy(from, &tx, CONSTRAINTS)?;
            entries += copy(from, &tx, CHANGES)?;
            entries += copy(from, &tx, crate::replication::REPLICATION_LOG)?;
            tx.commit().map_err(storage)?;
            drop(db);
            Ok(entries)
        })();
        let entries = outcome?;
        let bytes = publish(partial, path)?;
        Ok(BackupStats {
            tables: 27,
            entries,
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_preserves_a_destination_created_after_staging() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("copia.redb");
        let staged = output(&target).unwrap();
        let temporary = staged.path().to_path_buf();
        std::fs::write(&target, b"aparecio durante la copia").unwrap();
        assert!(publish(staged, &target).is_err());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"aparecio durante la copia"
        );
        assert!(!temporary.exists());
    }

    #[test]
    fn recovery_rejects_inconsistent_counters_without_changing_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("inconsistente.redb");
        let target = dir.path().join("recuperada.redb");
        {
            let db = Grafeno::open(&source).unwrap();
            db.write(|tx| tx.create_node("lab", crate::NewNode::new("Person")))
                .unwrap();
        }
        {
            let db = Database::open(&source).unwrap();
            let tx = db.begin_write().unwrap();
            tx.open_table(NS_COUNTS)
                .unwrap()
                .insert("lab", (99, 0))
                .unwrap();
            tx.commit().unwrap();
        }
        let original = std::fs::read(&source).unwrap();
        let error = Grafeno::recover_backup(&source, &target).unwrap_err();
        assert!(error.to_string().contains("inconsistent backup"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_backup_cleans_its_own_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("futura.redb");
        let target = dir.path().join("copia.redb");
        drop(Grafeno::open(&source).unwrap());
        {
            let db = Database::open(&source).unwrap();
            let tx = db.begin_write().unwrap();
            tx.open_table(TableDefinition::<u64, u64>::new("future_table"))
                .unwrap();
            tx.commit().unwrap();
        }
        let db = Grafeno::open(&source).unwrap();
        let error = db.begin_read().unwrap().backup(&target).unwrap_err();
        assert!(
            error.to_string().contains("does not know the table"),
            "{error}"
        );
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
