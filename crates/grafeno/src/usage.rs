//! What a database takes on disk: per table (entries, the bytes its
//! leaves store, B-tree metadata, space lost inside pages), for the whole
//! file (pages allocated, the file's size, free pages a compaction could
//! give back), and per namespace (its records and the bytes they hold).
//!
//! The table and file figures are redb's own and exact; a namespace's are
//! counted from its records (exact for what is counted, without the keys
//! and the pages they share with other namespaces).

use std::path::PathBuf;

use grafeno_core::Result;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableHandle};
use serde::Serialize;

use crate::db::{Grafeno, ReadTx};
use crate::store::storage;

/// One table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TableUsage {
    pub name: String,
    pub entries: u64,
    /// Keys and values in the leaves.
    pub stored_bytes: u64,
    /// Branch pages and the leaves' own bookkeeping.
    pub metadata_bytes: u64,
    /// Space inside the table's pages that holds nothing.
    pub fragmented_bytes: u64,
    pub leaf_pages: u64,
    pub branch_pages: u64,
    pub tree_height: u32,
}

/// The whole database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StorageUsage {
    /// The layout version of the file.
    pub layout: u64,
    pub page_size: u64,
    /// Every table, largest stored first.
    pub tables: Vec<TableUsage>,
    pub stored_bytes: u64,
    pub metadata_bytes: u64,
    pub fragmented_bytes: u64,
    /// Pages in use, as bytes (`None` in memory).
    pub allocated_bytes: Option<u64>,
    /// The file's size (`None` in memory).
    pub file_bytes: Option<u64>,
    /// Pages of the file in use by nothing: what a compaction can give
    /// back, at most (`None` in memory).
    pub free_bytes: Option<u64>,
}

/// One namespace, from its records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NamespaceUsage {
    pub name: String,
    pub nodes: u64,
    pub edges: u64,
    pub hyperedges: u64,
    /// Bytes of the node, edge and hyperedge records (values only).
    pub node_bytes: u64,
    pub edge_bytes: u64,
    pub hyperedge_bytes: u64,
    /// Node texts: how many and their bytes.
    pub texts: u64,
    pub text_bytes: u64,
    /// Vectors of the namespace's own vector graph and their bytes.
    pub vectors: u64,
    pub vector_bytes: u64,
    /// Index entries of every index of the namespace (property, full-text,
    /// fuzzy) and the bytes of their keys.
    pub index_entries: u64,
    pub index_key_bytes: u64,
    /// Change log entries kept and their bytes.
    pub changes: u64,
    pub change_bytes: u64,
}

impl Grafeno {
    /// What each table and the file take. It reads a snapshot, and asks
    /// redb for the pages in use with a write transaction it does not
    /// commit: it waits for a write in progress.
    pub fn storage_usage(&self) -> Result<StorageUsage> {
        let (allocated, page_size) = {
            let tx = self.raw().begin_write().map_err(storage)?;
            let stats = tx.stats().map_err(storage)?;
            let out = (stats.allocated_pages(), stats.page_size() as u64);
            tx.abort().map_err(storage)?;
            out
        };
        let read = self.raw().begin_read().map_err(storage)?;
        let mut tables = Vec::new();
        for handle in read.list_tables().map_err(storage)? {
            let name = handle.name().to_owned();
            let table = read.open_untyped_table(handle).map_err(storage)?;
            let stats = table.stats().map_err(storage)?;
            tables.push(TableUsage {
                name,
                entries: table.len().map_err(storage)?,
                stored_bytes: stats.stored_bytes(),
                metadata_bytes: stats.metadata_bytes(),
                fragmented_bytes: stats.fragmented_bytes(),
                leaf_pages: stats.leaf_pages(),
                branch_pages: stats.branch_pages(),
                tree_height: stats.tree_height(),
            });
        }
        tables.sort_by(|a, b| {
            b.stored_bytes
                .cmp(&a.stored_bytes)
                .then(a.name.cmp(&b.name))
        });
        let sum = |f: fn(&TableUsage) -> u64| tables.iter().map(f).sum::<u64>();
        let file_bytes = self
            .path()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|meta| meta.len());
        let allocated_bytes = file_bytes.map(|_| allocated * page_size);
        Ok(StorageUsage {
            layout: self.layout()?,
            page_size,
            stored_bytes: sum(|t| t.stored_bytes),
            metadata_bytes: sum(|t| t.metadata_bytes),
            fragmented_bytes: sum(|t| t.fragmented_bytes),
            tables,
            allocated_bytes,
            free_bytes: file_bytes
                .zip(allocated_bytes)
                .map(|(file, used)| file.saturating_sub(used)),
            file_bytes,
        })
    }

    /// The file this database was opened from (`None` in memory).
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }
}

impl ReadTx {
    /// What the records of `ns` take, counted from them: it reads every
    /// record of the namespace (a scan, for administration).
    pub fn namespace_usage(&self, ns: &str) -> Result<NamespaceUsage> {
        let mut usage = NamespaceUsage {
            name: ns.to_owned(),
            ..NamespaceUsage::default()
        };
        let end = format!("{ns}\0");
        for item in self
            .nodes
            .range((ns, 0u128)..(end.as_str(), 0u128))
            .map_err(storage)?
        {
            let (_, value) = item.map_err(storage)?;
            usage.nodes += 1;
            usage.node_bytes += value.value().len() as u64;
        }
        for item in self
            .edges
            .range((ns, 0u128)..(end.as_str(), 0u128))
            .map_err(storage)?
        {
            let (_, value) = item.map_err(storage)?;
            usage.edges += 1;
            usage.edge_bytes += value.value().len() as u64;
        }
        for item in self
            .hyperedges
            .range((ns, 0u128)..(end.as_str(), 0u128))
            .map_err(storage)?
        {
            let (_, value) = item.map_err(storage)?;
            usage.hyperedges += 1;
            usage.hyperedge_bytes += value.value().len() as u64;
        }
        for item in self
            .texts
            .range((ns, 0u128)..(end.as_str(), 0u128))
            .map_err(storage)?
        {
            let (_, value) = item.map_err(storage)?;
            usage.texts += 1;
            usage.text_bytes += value.value().len() as u64;
        }
        for item in self
            .vectors
            .range((ns, 0u128)..(end.as_str(), 0u128))
            .map_err(storage)?
        {
            let (_, value) = item.map_err(storage)?;
            usage.vectors += 1;
            usage.vector_bytes += value.value().len() as u64;
        }
        let empty: &[u8] = &[];
        for item in self
            .index_entries
            .range((ns, "", empty, 0u128)..(end.as_str(), "", empty, 0u128))
            .map_err(storage)?
        {
            let (key, _) = item.map_err(storage)?;
            let (_, name, bytes, _) = key.value();
            usage.index_entries += 1;
            usage.index_key_bytes += (name.len() + bytes.len() + 16) as u64;
        }
        for item in self
            .changes
            .range((ns, 0u64)..(end.as_str(), 0u64))
            .map_err(storage)?
        {
            let (_, value) = item.map_err(storage)?;
            usage.changes += 1;
            usage.change_bytes += value.value().len() as u64;
        }
        Ok(usage)
    }
}
