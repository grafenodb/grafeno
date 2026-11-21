//! Export and import: a JSON Lines dump of nodes (with their text and
//! embedding), edges and hyperedges, for backups, migrations and test data.
//!
//! The first line is a header; then, per namespace, the namespace itself,
//! its indexes, its nodes, its edges and then its hyperedges, so every
//! edge and hyperedge comes after the nodes it names:
//!
//! ```text
//! {"type":"header","format":"grafeno-export","version":2,"exported_at":1793664000000000}
//! {"type":"namespace","ns":"vault","created_at":1793664000000000,"description":"Notes"}
//! {"type":"node","ns":"vault","node":{"id":"…","labels":["Note"],…},"text":"…","embedding":[…]}
//! {"type":"edge","ns":"vault","edge":{"id":"…","label":"links_to","from":"…","to":"…",…}}
//! {"type":"index","ns":"vault","index":{"name":"Note_title","label":"Note","key":"title","unique":false}}
//! {"type":"hyperedge","ns":"vault","hyperedge":{"id":"…","label":"decision","participants":[…],…}}
//! ```
//!
//! Records keep their ids and timestamps. Embeddings are exported normalized.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};

use grafeno_core::{Edge, Error, HyperEdge, Node, NodeId, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::db::{Durability, Grafeno, ReadTx, WriteTx};
use crate::index::IndexDef;
use crate::memory::MemoryState;
use crate::store::{decode, storage};

pub const EXPORT_FORMAT: &str = "grafeno-export";
/// The newest export version this build writes and reads. Version 2 adds
/// namespace records, version 3 hyperedge records; earlier exports import
/// as before. An export is written with version 3 only when it holds a
/// hyperedge, so an older Grafeno still reads the others.
pub const EXPORT_VERSION: u32 = 3;

/// One line of an export.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Record {
    Header {
        format: String,
        version: u32,
        exported_at: Timestamp,
    },
    /// A namespace, so that empty ones survive an export.
    Namespace {
        ns: String,
        created_at: Timestamp,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    Node {
        ns: String,
        node: Node,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        embedding: Option<Vec<f32>>,
        /// Present when it differs from the default state.
        #[serde(skip_serializing_if = "Option::is_none")]
        memory: Option<MemoryState>,
    },
    Edge {
        ns: String,
        edge: Edge,
    },
    /// A declared property index, written before the namespace's nodes.
    Index {
        ns: String,
        index: IndexDef,
    },
    /// A hyperedge (version 3), after the namespace's nodes.
    #[serde(rename = "hyperedge")]
    HyperEdge {
        ns: String,
        hyperedge: HyperEdge,
    },
}

/// A line as read. Deserializing it directly (instead of through serde's
/// internally tagged enums, which buffer numbers as `f64`) keeps embeddings
/// bit for bit.
#[derive(Deserialize)]
struct RawRecord {
    #[serde(rename = "type")]
    kind: String,
    format: Option<String>,
    version: Option<u32>,
    exported_at: Option<Timestamp>,
    created_at: Option<Timestamp>,
    description: Option<String>,
    ns: Option<String>,
    node: Option<Node>,
    text: Option<String>,
    embedding: Option<Vec<f32>>,
    memory: Option<MemoryState>,
    edge: Option<Edge>,
    index: Option<IndexDef>,
    hyperedge: Option<HyperEdge>,
}

impl RawRecord {
    fn into_record(self) -> std::result::Result<Record, String> {
        let missing = |field: &str| format!("a {} record needs `{field}`", self.kind);
        Ok(match self.kind.as_str() {
            "header" => Record::Header {
                format: self.format.clone().ok_or_else(|| missing("format"))?,
                version: self.version.ok_or_else(|| missing("version"))?,
                exported_at: self.exported_at.unwrap_or(Timestamp::from_micros(0)),
            },
            "namespace" => Record::Namespace {
                ns: self.ns.clone().ok_or_else(|| missing("ns"))?,
                created_at: self.created_at.unwrap_or_else(Timestamp::now),
                description: self.description,
            },
            "node" => Record::Node {
                ns: self.ns.clone().ok_or_else(|| missing("ns"))?,
                node: self.node.clone().ok_or_else(|| missing("node"))?,
                text: self.text,
                embedding: self.embedding,
                memory: self.memory,
            },
            "edge" => Record::Edge {
                ns: self.ns.clone().ok_or_else(|| missing("ns"))?,
                edge: self.edge.clone().ok_or_else(|| missing("edge"))?,
            },
            "index" => Record::Index {
                ns: self.ns.clone().ok_or_else(|| missing("ns"))?,
                index: self.index.clone().ok_or_else(|| missing("index"))?,
            },
            "hyperedge" => Record::HyperEdge {
                ns: self.ns.clone().ok_or_else(|| missing("ns"))?,
                hyperedge: self.hyperedge.clone().ok_or_else(|| missing("hyperedge"))?,
            },
            other => return Err(format!("unknown record type {other:?}")),
        })
    }
}

/// How much an export or import moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DumpStats {
    pub namespaces: usize,
    pub nodes: u64,
    pub edges: u64,
    pub hyperedges: u64,
}

fn write_record(out: &mut impl Write, record: &Record) -> Result<()> {
    serde_json::to_writer(&mut *out, record).map_err(|e| Error::Serialization(e.to_string()))?;
    out.write_all(b"\n")
        .map_err(|e| Error::Storage(format!("cannot write the export: {e}")))
}

impl ReadTx {
    /// Writes `namespaces` (every namespace when empty) as JSON Lines. The
    /// export is consistent: it comes from this transaction's snapshot.
    pub fn export(&self, namespaces: &[String], out: &mut impl Write) -> Result<DumpStats> {
        let names: Vec<String> = if namespaces.is_empty() {
            self.namespaces()?
                .into_iter()
                .map(|info| info.name)
                .collect()
        } else {
            namespaces.to_vec()
        };
        // Version 3 only when a hyperedge is in it (as the reader sees).
        let mut hyperedges = false;
        for ns in &names {
            hyperedges |= !self.hyperedge_ids(ns, None, 1)?.is_empty();
        }
        write_record(
            out,
            &Record::Header {
                format: EXPORT_FORMAT.into(),
                version: if hyperedges { EXPORT_VERSION } else { 2 },
                exported_at: Timestamp::now(),
            },
        )?;
        let mut stats = DumpStats {
            namespaces: names.len(),
            ..DumpStats::default()
        };
        for ns in &names {
            let (created_at, description) = match self.namespace(ns)? {
                Some(info) => (info.created_at, info.description),
                None => (Timestamp::now(), None),
            };
            write_record(
                out,
                &Record::Namespace {
                    ns: ns.clone(),
                    created_at,
                    description,
                },
            )?;
            for index in self.index_defs(ns)? {
                write_record(
                    out,
                    &Record::Index {
                        ns: ns.clone(),
                        index,
                    },
                )?;
            }
            for item in self.nodes.range((ns.as_str(), 0u128)..).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                if key.value().0 != ns {
                    break;
                }
                let node: Node = decode(value.value())?;
                // What the reader may not see is not exported (14.4).
                let Some(node) = self.seen_node(ns, node) else {
                    continue;
                };
                let id: NodeId = node.id;
                let memory = self.memory(ns, &node)?;
                let record = Record::Node {
                    ns: ns.clone(),
                    text: self.text(ns, id)?,
                    embedding: self.embedding(ns, id)?,
                    memory: (memory != MemoryState::default_for(&node)).then_some(memory),
                    node,
                };
                write_record(out, &record)?;
                stats.nodes += 1;
            }
            for item in self.edges.range((ns.as_str(), 0u128)..).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                if key.value().0 != ns {
                    break;
                }
                let edge: Edge = decode(value.value())?;
                let Some(edge) = self.seen_edge(ns, edge)? else {
                    continue;
                };
                write_record(
                    out,
                    &Record::Edge {
                        ns: ns.clone(),
                        edge,
                    },
                )?;
                stats.edges += 1;
            }
            let mut after = None;
            loop {
                let page = self.hyperedges_page(ns, after, 1_000)?;
                let full = page.len() == 1_000;
                after = page.last().map(|edge| edge.id).or(after);
                for hyperedge in page {
                    write_record(
                        out,
                        &Record::HyperEdge {
                            ns: ns.clone(),
                            hyperedge,
                        },
                    )?;
                    stats.hyperedges += 1;
                }
                if !full {
                    break;
                }
            }
        }
        out.flush()
            .map_err(|e| Error::Storage(format!("cannot write the export: {e}")))?;
        Ok(stats)
    }
}

/// How an import writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportOptions {
    /// Records per transaction. `None` (the default): one transaction, all
    /// of the import or nothing. With chunks, an error leaves the chunks
    /// before it in place, and each chunk commits with eventual durability
    /// (one flush at the end): much faster and lighter on memory for large
    /// loads.
    pub chunk: Option<usize>,
}

impl ImportOptions {
    /// Large loads: 20,000 records per transaction.
    pub fn bulk() -> Self {
        Self {
            chunk: Some(20_000),
        }
    }
}

impl Grafeno {
    /// Reads an export and writes it in one transaction: all of it or nothing.
    /// Records whose ids already exist replace them.
    pub fn import(&self, input: impl BufRead) -> Result<DumpStats> {
        self.import_with(input, &ImportOptions::default())
    }

    /// [`Grafeno::import`] with options, such as bulk loading in chunks.
    pub fn import_with(&self, input: impl BufRead, options: &ImportOptions) -> Result<DumpStats> {
        let Some(chunk) = options.chunk else {
            return self.write(|tx| {
                let mut import = Import::default();
                for (index, line) in input.lines().enumerate() {
                    import.line(tx, index + 1, line)?;
                }
                import.finish()
            });
        };
        let durability = self.durability();
        self.set_durability(Durability::Eventual);
        let outcome = (|| {
            let mut import = Import {
                defer_texts: true,
                ..Import::default()
            };
            let mut tx = self.begin_write()?;
            let mut pending = 0;
            for (index, line) in input.lines().enumerate() {
                if let Err(error) = import.line(&mut tx, index + 1, line) {
                    tx.abort()?;
                    return Err(error);
                }
                pending += 1;
                if pending >= chunk.max(1) {
                    import.index_texts(&mut tx)?;
                    tx.commit()?;
                    tx = self.begin_write()?;
                    pending = 0;
                }
            }
            import.index_texts(&mut tx)?;
            tx.commit()?;
            import.finish()
        })();
        self.set_durability(durability);
        self.flush()?;
        outcome
    }
}

impl crate::Writer {
    /// [`Grafeno::import_with`] in chunks through the writer's bulk lane:
    /// each chunk of `chunk` records is one transaction, taken between the
    /// groups of interactive writes, so the import never holds the
    /// database for longer than one chunk. Chunks commit with eventual
    /// durability and the import flushes at the end. An error leaves the
    /// chunks before it in place.
    pub fn import(&self, input: impl BufRead, chunk: usize) -> Result<DumpStats> {
        let state = std::sync::Arc::new(std::sync::Mutex::new(Import {
            defer_texts: true,
            ..Import::default()
        }));
        let mut lines = input.lines();
        let mut number = 0usize;
        loop {
            let mut part: Vec<(usize, String)> = Vec::with_capacity(chunk.clamp(1, 100_000));
            for line in lines.by_ref() {
                number += 1;
                let line = line.map_err(|e| Error::Invalid(format!("line {number}: {e}")))?;
                part.push((number, line));
                if part.len() >= chunk.max(1) {
                    break;
                }
            }
            if part.is_empty() {
                break;
            }
            let last = part.len() < chunk.max(1);
            let shared = state.clone();
            let part = std::sync::Arc::new(part);
            self.write_bulk(move |tx| {
                // A failed attempt must leave the state as it was: work on a
                // copy and keep it only on success.
                let mut import = shared.lock().expect("not poisoned").clone();
                for (number, line) in part.iter() {
                    import.line(tx, *number, Ok(line.clone()))?;
                }
                import.index_texts(tx)?;
                *shared.lock().expect("not poisoned") = import;
                Ok(())
            })?;
            if last {
                break;
            }
        }
        self.flush()?;
        let import = std::mem::take(&mut *state.lock().expect("not poisoned"));
        import.finish()
    }
}

/// The state of an import across its lines.
#[derive(Clone, Default)]
struct Import {
    stats: DumpStats,
    namespaces: BTreeSet<String>,
    header: bool,
    /// Index texts in batches (bulk loading) instead of one by one.
    defer_texts: bool,
    texts: std::collections::BTreeMap<String, Vec<(NodeId, String)>>,
}

impl Import {
    fn line(
        &mut self,
        tx: &mut WriteTx,
        number: usize,
        line: std::io::Result<String>,
    ) -> Result<()> {
        let at = |message: String| Error::Invalid(format!("line {number}: {message}"));
        let line = line.map_err(|e| at(e.to_string()))?;
        if line.trim().is_empty() {
            return Ok(());
        }
        let raw: RawRecord = serde_json::from_str(&line).map_err(|e| at(e.to_string()))?;
        match raw.into_record().map_err(at)? {
            Record::Header {
                format, version, ..
            } => {
                if self.header {
                    return Err(at("a second header".into()));
                }
                if format != EXPORT_FORMAT {
                    return Err(at(format!("not a Grafeno export (format {format:?})")));
                }
                if version > EXPORT_VERSION {
                    return Err(at(format!(
                        "export version {version} is newer than this Grafeno reads \
                         ({EXPORT_VERSION})"
                    )));
                }
                self.header = true;
            }
            _ if !self.header => return Err(at("the export must start with a header".into())),
            Record::Node {
                ns,
                node,
                text,
                embedding,
                memory,
            } => {
                let id = node.id;
                let deferred = self.defer_texts && text.is_some();
                let now = if deferred { None } else { text.as_deref() };
                tx.restore_node(&ns, node, now, embedding.as_deref())
                    .map_err(|e| at(e.to_string()))?;
                if deferred && let Some(text) = text {
                    self.texts.entry(ns.clone()).or_default().push((id, text));
                }
                if let Some(memory) = memory {
                    tx.restore_memory(&ns, id, memory)
                        .map_err(|e| at(e.to_string()))?;
                }
                self.namespaces.insert(ns);
                self.stats.nodes += 1;
            }
            Record::Namespace {
                ns,
                created_at,
                description,
            } => {
                tx.restore_namespace(&ns, created_at, description)
                    .map_err(|e| at(e.to_string()))?;
                self.namespaces.insert(ns);
            }
            Record::Index { ns, index } => {
                tx.create_index(&ns, index).map_err(|e| at(e.to_string()))?;
            }
            Record::Edge { ns, edge } => {
                tx.restore_edge(&ns, edge).map_err(|e| at(e.to_string()))?;
                self.namespaces.insert(ns);
                self.stats.edges += 1;
            }
            Record::HyperEdge { ns, hyperedge } => {
                tx.restore_hyperedge(&ns, hyperedge)
                    .map_err(|e| at(e.to_string()))?;
                self.namespaces.insert(ns);
                self.stats.hyperedges += 1;
            }
        }
        Ok(())
    }

    /// Indexes the texts gathered so far.
    fn index_texts(&mut self, tx: &mut WriteTx) -> Result<()> {
        for (ns, texts) in std::mem::take(&mut self.texts) {
            tx.set_texts(&ns, &texts)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<DumpStats> {
        if !self.header {
            return Err(Error::Invalid("the export is empty".into()));
        }
        Ok(DumpStats {
            namespaces: self.namespaces.len(),
            ..self.stats
        })
    }
}
