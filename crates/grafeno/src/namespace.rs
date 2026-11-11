//! Namespaces as records of their own: created explicitly (or by the first
//! write to them), listed even when empty, and dropped with everything in
//! them.

use grafeno_core::{Error, Result, Timestamp};
use redb::{ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};

use crate::db::{ReadTx, WriteTx};
use crate::index::{INDEX_ENTRIES, INDEXES};
use crate::stats::NamespaceInfo;
use crate::store::{
    ADJ_IN, ADJ_OUT, EDGE_LABEL_COUNTS, EDGES, HNSW_LINKS, LABEL_COUNTS, MEMORY, NAMES,
    NODE_LABELS, NODES, NS_COUNTS, POSTINGS, TEXT_LENGTHS, TEXT_STATS, TEXTS, VECTOR_META, VECTORS,
    VECTORS_Q8, check_namespace, decode, encode, storage,
};

/// `ns -> NamespaceMeta` (JSON): every namespace, empty ones included.
pub(crate) const NAMESPACES: TableDefinition<&str, &[u8]> = TableDefinition::new("namespaces");

/// Namespace names that HTTP routes under `/v1` use.
pub const RESERVED: &[&str] = &[
    "namespaces",
    "auth",
    "accounts",
    "export",
    "import",
    "algorithms",
    "whoami",
    "keys",
    "backup",
    "metrics",
    "cluster",
    "roles",
    "privileges",
    "admin",
];

/// Names longer than this are refused when a namespace is created.
pub const MAX_NAMESPACE_LENGTH: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NamespaceMeta {
    pub(crate) created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
}

pub(crate) fn read_meta(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    ns: &str,
) -> Result<Option<NamespaceMeta>> {
    match table.get(ns).map_err(storage)? {
        Some(guard) => decode(guard.value()).map(Some),
        None => Ok(None),
    }
}

/// Registers `ns` if it is not registered yet (any write to a namespace
/// does this, so writes never need a `create_namespace` first).
pub(crate) fn ensure(tx: &WriteTransaction, ns: &str) -> Result<()> {
    let mut table = tx.open_table(NAMESPACES).map_err(storage)?;
    if table.get(ns).map_err(storage)?.is_none() {
        let meta = NamespaceMeta {
            created_at: Timestamp::now(),
            description: None,
        };
        table
            .insert(ns, encode(&meta)?.as_slice())
            .map_err(storage)?;
    }
    Ok(())
}

/// Checks a name for a new namespace: it must be usable in URLs, shells
/// and file names.
pub fn check_new_namespace(ns: &str) -> Result<()> {
    check_namespace(ns)?;
    if ns.chars().count() > MAX_NAMESPACE_LENGTH {
        return Err(Error::Invalid(format!(
            "a namespace name has at most {MAX_NAMESPACE_LENGTH} characters"
        )));
    }
    // Names the HTTP API uses for its own routes under /v1.
    if RESERVED.contains(&ns) {
        return Err(Error::Invalid(format!(
            "{ns:?} is reserved by the HTTP API; choose another name"
        )));
    }
    if ns.trim() != ns {
        return Err(Error::Invalid(
            "a namespace name cannot start or end with spaces".into(),
        ));
    }
    if let Some(bad) = ns
        .chars()
        .find(|c| c.is_control() || matches!(c, '/' | '\\' | '?' | '#' | '%'))
    {
        return Err(Error::Invalid(format!(
            "a namespace name cannot contain {bad:?}"
        )));
    }
    Ok(())
}

/// What dropping a namespace removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DropStats {
    pub nodes: u64,
    pub edges: u64,
}

/// The first key after every key whose first part is `ns`: `ns` followed by
/// a zero byte sorts after `ns` and before any longer name.
fn after(ns: &str) -> String {
    format!("{ns}\0")
}

impl ReadTx {
    /// One namespace, if it is registered.
    pub fn namespace(&self, ns: &str) -> Result<Option<NamespaceInfo>> {
        let Some(meta) = read_meta(&self.registry, ns)? else {
            return Ok(None);
        };
        let (nodes, edges) = self
            .ns_counts
            .get(ns)
            .map_err(storage)?
            .map(|guard| guard.value())
            .unwrap_or((0, 0));
        Ok(Some(NamespaceInfo {
            name: ns.to_owned(),
            nodes,
            edges,
            created_at: meta.created_at,
            description: meta.description,
        }))
    }
}

impl WriteTx {
    /// Creates an empty namespace. Fails with [`Error::Conflict`] if it
    /// exists.
    pub fn create_namespace(&mut self, ns: &str, description: Option<&str>) -> Result<()> {
        check_new_namespace(ns)?;
        self.note(crate::replication::Touch::Namespace(ns.to_owned()));
        let mut table = self.tx.open_table(NAMESPACES).map_err(storage)?;
        if table.get(ns).map_err(storage)?.is_some() {
            return Err(Error::Conflict(format!("namespace {ns:?} already exists")));
        }
        let meta = NamespaceMeta {
            created_at: Timestamp::now(),
            description: description
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_owned),
        };
        table
            .insert(ns, encode(&meta)?.as_slice())
            .map_err(storage)?;
        Ok(())
    }

    /// Registers a namespace with a known creation time (imports), or
    /// updates its description if it exists.
    pub(crate) fn restore_namespace(
        &mut self,
        ns: &str,
        created_at: Timestamp,
        description: Option<String>,
    ) -> Result<()> {
        check_namespace(ns)?;
        self.note(crate::replication::Touch::Namespace(ns.to_owned()));
        let mut table = self.tx.open_table(NAMESPACES).map_err(storage)?;
        let created_at = read_meta(&table, ns)?.map_or(created_at, |meta| meta.created_at);
        let meta = NamespaceMeta {
            created_at,
            description,
        };
        table
            .insert(ns, encode(&meta)?.as_slice())
            .map_err(storage)?;
        Ok(())
    }

    /// Sets (or, with `None`, removes) the description of a namespace.
    pub fn describe_namespace(&mut self, ns: &str, description: Option<&str>) -> Result<()> {
        self.note(crate::replication::Touch::Namespace(ns.to_owned()));
        let mut table = self.tx.open_table(NAMESPACES).map_err(storage)?;
        let mut meta =
            read_meta(&table, ns)?.ok_or_else(|| Error::NamespaceNotFound(ns.to_owned()))?;
        meta.description = description
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_owned);
        table
            .insert(ns, encode(&meta)?.as_slice())
            .map_err(storage)?;
        Ok(())
    }

    /// The namespace as written so far in this transaction.
    pub fn namespace(&self, ns: &str) -> Result<Option<NamespaceInfo>> {
        let table = self.tx.open_table(NAMESPACES).map_err(storage)?;
        let Some(meta) = read_meta(&table, ns)? else {
            return Ok(None);
        };
        let counts = self.tx.open_table(NS_COUNTS).map_err(storage)?;
        let (nodes, edges) = counts
            .get(ns)
            .map_err(storage)?
            .map(|guard| guard.value())
            .unwrap_or((0, 0));
        Ok(Some(NamespaceInfo {
            name: ns.to_owned(),
            nodes,
            edges,
            created_at: meta.created_at,
            description: meta.description,
        }))
    }

    /// Deletes a namespace and everything in it: nodes, edges, texts,
    /// vectors, indexes and counters. It cannot be undone (an export first
    /// keeps a copy).
    pub fn drop_namespace(&mut self, ns: &str) -> Result<DropStats> {
        self.bulk(
            crate::replication::Bulk::DropNamespace { ns: ns.to_owned() },
            |tx| tx.drop_namespace_inner(ns),
        )
    }

    fn drop_namespace_inner(&mut self, ns: &str) -> Result<DropStats> {
        let info = self
            .namespace(ns)?
            .ok_or_else(|| Error::NamespaceNotFound(ns.to_owned()))?;
        let end = after(ns);
        let tx = &self.tx;
        macro_rules! clear {
            ($table:expr, $low:expr, $high:expr) => {{
                let mut table = tx.open_table($table).map_err(storage)?;
                table
                    .retain_in($low..$high, |_, _| false)
                    .map_err(storage)?;
            }};
        }
        clear!(NODES, (ns, 0u128), (end.as_str(), 0u128));
        clear!(EDGES, (ns, 0u128), (end.as_str(), 0u128));
        clear!(TEXTS, (ns, 0u128), (end.as_str(), 0u128));
        clear!(TEXT_LENGTHS, (ns, 0u128), (end.as_str(), 0u128));
        clear!(VECTORS, (ns, 0u128), (end.as_str(), 0u128));
        clear!(VECTORS_Q8, (ns, 0u128), (end.as_str(), 0u128));
        clear!(crate::merge::MERGES, (ns, 0u128), (end.as_str(), 0u128));
        clear!(MEMORY, (ns, 0u128), (end.as_str(), 0u128));
        clear!(
            ADJ_OUT,
            (ns, 0u128, "", 0u128),
            (end.as_str(), 0u128, "", 0u128)
        );
        clear!(
            ADJ_IN,
            (ns, 0u128, "", 0u128),
            (end.as_str(), 0u128, "", 0u128)
        );
        clear!(NODE_LABELS, (ns, "", 0u128), (end.as_str(), "", 0u128));
        clear!(POSTINGS, (ns, "", 0u128), (end.as_str(), "", 0u128));
        clear!(NAMES, (ns, "", 0u128), (end.as_str(), "", 0u128));
        clear!(HNSW_LINKS, (ns, 0u128, 0u8), (end.as_str(), 0u128, 0u8));
        clear!(LABEL_COUNTS, (ns, ""), (end.as_str(), ""));
        clear!(EDGE_LABEL_COUNTS, (ns, ""), (end.as_str(), ""));
        clear!(INDEXES, (ns, ""), (end.as_str(), ""));
        clear!(
            crate::fulltext::FULLTEXT_STATS,
            (ns, ""),
            (end.as_str(), "")
        );
        clear!(crate::resolve::MATCH_RULES, (ns, ""), (end.as_str(), ""));
        clear!(
            crate::constraints::CONSTRAINTS,
            (ns, ""),
            (end.as_str(), "")
        );
        clear!(crate::state::STATE, (ns, "", ""), (end.as_str(), "", ""));
        clear!(crate::changes::CHANGES, (ns, 0u64), (end.as_str(), 0u64));
        let empty: &[u8] = &[];
        clear!(
            INDEX_ENTRIES,
            (ns, "", empty, 0u128),
            (end.as_str(), "", empty, 0u128)
        );
        tx.open_table(TEXT_STATS)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;
        tx.open_table(NS_COUNTS)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;
        tx.open_table(VECTOR_META)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;
        tx.open_table(NAMESPACES)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;
        self.vectors.forget(ns);
        Ok(DropStats {
            nodes: info.nodes,
            edges: info.edges,
        })
    }
}
