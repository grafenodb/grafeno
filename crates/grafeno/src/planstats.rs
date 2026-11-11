//! Statistics for the query planner: how values spread in each property
//! index, and how many edges of each label leave and reach a node.
//!
//! They are built from a snapshot on first use and kept in memory, shared
//! by every transaction of the database; they are not stored. They are
//! rebuilt when the namespace drifts from them (nodes or edges added or
//! removed beyond a tenth, an index created or dropped), or on demand with
//! [`ReadTx::analyze`]. While one reader rebuilds them, the others plan
//! with the previous ones.
//!
//! For each value index: entries, distinct values, the most common values
//! with their counts, and equal-depth buckets (each holds the same number
//! of entries), so an equality or a range can be estimated without reading
//! the index. For each edge label: edges, and the distinct nodes they leave
//! and reach, which give the average degree in each direction.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use grafeno_core::Result;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::ReadTx;
use crate::index::{IndexDef, IndexKey, IndexRange, decode_value, encode_value, seek_bytes};
use crate::store::{Adj, AdjKey, storage};

/// The most common values kept per index.
pub const COMMON_VALUES: usize = 64;
/// Equal-depth buckets per index.
pub const BUCKETS: usize = 64;

/// How values spread in one index.
#[derive(Clone, Debug, Serialize)]
pub struct IndexStats {
    pub name: String,
    pub label: String,
    /// Its properties, joined by `,` (one for most indexes).
    pub key: String,
    pub unique: bool,
    pub entries: u64,
    pub distinct: u64,
    /// Records with the label(s) when the statistics were built.
    pub label_nodes: u64,
    /// The most common values and how many records hold each, most first
    /// (of a single-property index).
    pub common: Vec<(Json, u64)>,
    #[serde(skip)]
    common_bytes: Vec<(Vec<u8>, u64)>,
    /// `BUCKETS + 1` encoded values: bucket `i` holds the entries between
    /// `bounds[i]` and `bounds[i + 1]`, a `BUCKETS`th of them.
    #[serde(skip)]
    bounds: Vec<Vec<u8>>,
}

impl IndexStats {
    /// Nodes estimated to hold a value in `range`.
    pub fn estimate(&self, range: &IndexRange) -> f64 {
        if self.entries == 0 {
            return 0.0;
        }
        if let IndexRange::Equal(value) = range {
            if self.unique {
                return 1.0;
            }
            let Some(bytes) = encode_value(value) else {
                return 0.0;
            };
            if let Some((_, count)) = self.common_bytes.iter().find(|(b, _)| *b == bytes) {
                return *count as f64;
            }
            let listed: u64 = self.common_bytes.iter().map(|(_, count)| count).sum();
            let rest = self.distinct.saturating_sub(self.common_bytes.len() as u64);
            // Every distinct value is listed, and this one is not among them.
            if rest == 0 {
                return 0.0;
            }
            return (self.entries.saturating_sub(listed) as f64 / rest as f64).max(1.0);
        }
        let Some((low, high)) = crate::index::byte_range(range) else {
            return 0.0;
        };
        self.estimate_bytes(&low, &high)
    }

    /// Records estimated in a lookup of a composite index (`def`, whose
    /// statistics these are): equal values for its first properties, then
    /// `range` on the next (anything when `None`).
    pub fn estimate_seek(
        &self,
        def: &IndexDef,
        prefix: &[Json],
        range: Option<&IndexRange>,
    ) -> f64 {
        if self.entries == 0 {
            return 0.0;
        }
        let complete = prefix.len() + usize::from(matches!(range, Some(IndexRange::Equal(_))))
            == def.keys().len();
        if complete && self.unique {
            return 1.0;
        }
        let Some((low, high)) = seek_bytes(def, prefix, range) else {
            return 0.0;
        };
        if complete {
            // `low` is then the whole entry.
            if let Some((_, count)) = self.common_bytes.iter().find(|(b, _)| *b == low) {
                return *count as f64;
            }
            return self.per_value().max(1.0);
        }
        self.estimate_bytes(&low, &high)
    }

    fn estimate_bytes(&self, low: &[u8], high: &[u8]) -> f64 {
        let entries = self.entries as f64;
        if self.bounds.len() < 2 {
            return entries;
        }
        let buckets = (self.bounds.len() - 1) as f64;
        let mut share = 0.0;
        for pair in self.bounds.windows(2) {
            let (start, end) = (pair[0].as_slice(), pair[1].as_slice());
            if start > high || end < low {
                continue;
            }
            // A bucket inside the range counts whole, one it crosses half.
            share += if low <= start && end <= high {
                1.0
            } else {
                0.5
            };
        }
        // A bucket boundary repeated by a common value: equalities and
        // narrow ranges fall in at most a bucket or two.
        (entries * share / buckets).clamp(0.0, entries)
    }

    /// Nodes per value, on average: what an equality with an unknown
    /// value finds.
    pub fn per_value(&self) -> f64 {
        if self.unique {
            1.0
        } else if self.distinct == 0 {
            0.0
        } else {
            self.entries as f64 / self.distinct as f64
        }
    }
}

/// Edges of one label and the nodes they join.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeLabelStats {
    pub label: String,
    pub edges: u64,
    /// Distinct nodes the edges leave.
    pub sources: u64,
    /// Distinct nodes the edges reach.
    pub targets: u64,
}

impl EdgeLabelStats {
    /// Edges leaving a node that has at least one.
    pub fn out_degree(&self) -> f64 {
        self.edges as f64 / self.sources.max(1) as f64
    }

    /// Edges reaching a node that has at least one.
    pub fn in_degree(&self) -> f64 {
        self.edges as f64 / self.targets.max(1) as f64
    }
}

/// What the planner knows about a namespace beyond its counts.
#[derive(Clone, Debug, Serialize)]
pub struct PlannerStats {
    pub ns: String,
    /// Nodes and edges when the statistics were built.
    pub nodes: u64,
    pub edges: u64,
    pub indexes: Vec<IndexStats>,
    pub edge_labels: Vec<EdgeLabelStats>,
    /// How long building them took.
    pub build_ms: f64,
}

/// Every field of [`IndexStats`], the encoded ones too.
#[derive(Serialize, Deserialize)]
struct WireIndex {
    name: String,
    label: String,
    key: String,
    unique: bool,
    entries: u64,
    distinct: u64,
    label_nodes: u64,
    common: Vec<(Json, u64)>,
    common_bytes: Vec<(Vec<u8>, u64)>,
    bounds: Vec<Vec<u8>>,
}

/// Every field of [`PlannerStats`].
#[derive(Serialize, Deserialize)]
struct WireStats {
    ns: String,
    nodes: u64,
    edges: u64,
    indexes: Vec<WireIndex>,
    edge_labels: Vec<EdgeLabelStats>,
    build_ms: f64,
}

impl PlannerStats {
    /// The statistics whole, for another process to plan exactly as this
    /// one would (a query split across cluster members).
    pub fn to_wire(&self) -> Json {
        let wire = WireStats {
            ns: self.ns.clone(),
            nodes: self.nodes,
            edges: self.edges,
            indexes: self
                .indexes
                .iter()
                .map(|index| WireIndex {
                    name: index.name.clone(),
                    label: index.label.clone(),
                    key: index.key.clone(),
                    unique: index.unique,
                    entries: index.entries,
                    distinct: index.distinct,
                    label_nodes: index.label_nodes,
                    common: index.common.clone(),
                    common_bytes: index.common_bytes.clone(),
                    bounds: index.bounds.clone(),
                })
                .collect(),
            edge_labels: self.edge_labels.clone(),
            build_ms: self.build_ms,
        };
        serde_json::to_value(wire).unwrap_or(Json::Null)
    }

    /// Statistics from [`PlannerStats::to_wire`].
    pub fn from_wire(json: &Json) -> Option<Self> {
        let wire: WireStats = serde_json::from_value(json.clone()).ok()?;
        Some(Self {
            ns: wire.ns,
            nodes: wire.nodes,
            edges: wire.edges,
            indexes: wire
                .indexes
                .into_iter()
                .map(|index| IndexStats {
                    name: index.name,
                    label: index.label,
                    key: index.key,
                    unique: index.unique,
                    entries: index.entries,
                    distinct: index.distinct,
                    label_nodes: index.label_nodes,
                    common: index.common,
                    common_bytes: index.common_bytes,
                    bounds: index.bounds,
                })
                .collect(),
            edge_labels: wire.edge_labels,
            build_ms: wire.build_ms,
        })
    }

    pub fn index(&self, name: &str) -> Option<&IndexStats> {
        self.indexes.iter().find(|index| index.name == name)
    }

    pub fn edge_label(&self, label: &str) -> Option<&EdgeLabelStats> {
        self.edge_labels.iter().find(|edge| edge.label == label)
    }

    /// Whether the namespace moved too far from these statistics: an
    /// index created or dropped, or a tenth more or fewer nodes or edges
    /// (and at least a hundred).
    fn stale(&self, nodes: u64, edges: u64, defs: &[IndexDef]) -> bool {
        let drift = |then: u64, now: u64| then.abs_diff(now) > (then / 10).max(100);
        drift(self.nodes, nodes)
            || drift(self.edges, edges)
            || self.indexes.len() != defs.iter().filter(|def| def.by_value()).count()
            || defs.iter().filter(|def| def.by_value()).any(|def| {
                self.index(&def.name)
                    .is_none_or(|index| index.key != def.keys().join(","))
            })
    }
}

/// The statistics of every namespace of a database, shared by its
/// transactions.
#[derive(Default)]
pub(crate) struct Cache {
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Default)]
struct Entry {
    stats: Option<Arc<PlannerStats>>,
    building: bool,
}

impl ReadTx {
    /// The planner's statistics for `ns`: the ones in memory while they
    /// still describe the namespace, rebuilt from this snapshot otherwise.
    /// While another reader rebuilds them, the previous ones (or `None`
    /// before the first build).
    pub fn planner_stats(&self, ns: &str) -> Result<Option<Arc<PlannerStats>>> {
        let (nodes, edges) = self
            .ns_counts
            .get(ns)
            .map_err(storage)?
            .map(|guard| guard.value())
            .unwrap_or((0, 0));
        let defs = self.index_defs(ns)?;
        let cache = &self.shared.planner;
        {
            let mut entries = cache.entries.lock().unwrap_or_else(|e| e.into_inner());
            let entry = entries.entry(ns.to_owned()).or_default();
            match &entry.stats {
                Some(stats) if !stats.stale(nodes, edges, &defs) => {
                    return Ok(Some(stats.clone()));
                }
                _ if entry.building => return Ok(entry.stats.clone()),
                _ => entry.building = true,
            }
        }
        let built = build(self, ns, nodes, edges, &defs);
        let mut entries = cache.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(ns.to_owned()).or_default();
        entry.building = false;
        let stats = Arc::new(built?);
        entry.stats = Some(stats.clone());
        Ok(Some(stats))
    }

    /// The planner's statistics for `ns` held in memory, however old,
    /// without building any: for statements that hold the write lock,
    /// which should not wait for a build.
    pub fn cached_planner_stats(&self, ns: &str) -> Option<Arc<PlannerStats>> {
        let entries = self
            .shared
            .planner
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        entries.get(ns).and_then(|entry| entry.stats.clone())
    }

    /// Rebuilds the planner's statistics for `ns` from this snapshot now,
    /// and keeps them for the planner.
    pub fn analyze(&self, ns: &str) -> Result<Arc<PlannerStats>> {
        let (nodes, edges) = self
            .ns_counts
            .get(ns)
            .map_err(storage)?
            .map(|guard| guard.value())
            .unwrap_or((0, 0));
        let defs = self.index_defs(ns)?;
        let stats = Arc::new(build(self, ns, nodes, edges, &defs)?);
        let mut entries = self
            .shared
            .planner
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        entries.entry(ns.to_owned()).or_default().stats = Some(stats.clone());
        Ok(stats)
    }
}

fn build(
    read: &ReadTx,
    ns: &str,
    nodes: u64,
    edges: u64,
    defs: &[IndexDef],
) -> Result<PlannerStats> {
    let started = Instant::now();
    let mut indexes = Vec::new();
    for def in defs.iter().filter(|def| def.by_value()) {
        let counts = if def.edges {
            &read.edge_label_counts
        } else {
            &read.label_counts
        };
        // Several labels may share nodes: their sum is an upper bound.
        let mut label_nodes = 0;
        for label in def.labels() {
            label_nodes += counts
                .get((ns, label))
                .map_err(storage)?
                .map_or(0, |guard| guard.value());
        }
        indexes.push(index_stats(&read.index_entries, ns, def, label_nodes)?);
    }
    let mut by_label: HashMap<String, EdgeLabelStats> = HashMap::new();
    for (table, outgoing) in [(&read.out, true), (&read.inn, false)] {
        for (label, count, nodes) in adjacency_stats(table, ns)? {
            let entry = by_label
                .entry(label.clone())
                .or_insert_with(|| EdgeLabelStats {
                    label,
                    edges: 0,
                    sources: 0,
                    targets: 0,
                });
            if outgoing {
                entry.edges = count;
                entry.sources = nodes;
            } else {
                entry.targets = nodes;
            }
        }
    }
    let mut edge_labels: Vec<EdgeLabelStats> = by_label.into_values().collect();
    edge_labels.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(PlannerStats {
        ns: ns.to_owned(),
        nodes,
        edges,
        indexes,
        edge_labels,
        build_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

fn index_range<'t>(
    table: &'t impl ReadableTable<IndexKey, ()>,
    ns: &str,
    name: &str,
) -> Result<redb::Range<'t, IndexKey, ()>> {
    let empty: &[u8] = &[];
    let top: &[u8] = &[0xFF; 16];
    table
        .range((ns, name, empty, 0u128)..=(ns, name, top, u128::MAX))
        .map_err(storage)
}

/// Two passes over the index: values and their counts, then the bucket
/// bounds (every `entries / BUCKETS`th entry).
fn index_stats(
    table: &impl ReadableTable<IndexKey, ()>,
    ns: &str,
    def: &IndexDef,
    label_nodes: u64,
) -> Result<IndexStats> {
    let mut entries = 0u64;
    let mut distinct = 0u64;
    // The most common values, least common on top to drop it first.
    let mut common: BinaryHeap<Reverse<(u64, Vec<u8>)>> = BinaryHeap::new();
    let mut keep = |value: Vec<u8>, count: u64| {
        if common.len() < COMMON_VALUES {
            common.push(Reverse((count, value)));
        } else if common
            .peek()
            .is_some_and(|Reverse((least, _))| count > *least)
        {
            common.pop();
            common.push(Reverse((count, value)));
        }
    };
    let mut run: Option<(Vec<u8>, u64)> = None;
    for item in index_range(table, ns, &def.name)? {
        let (key, _) = item.map_err(storage)?;
        let value = key.value().2;
        entries += 1;
        match &mut run {
            Some((current, count)) if current.as_slice() == value => *count += 1,
            _ => {
                distinct += 1;
                if let Some((value, count)) = run.take() {
                    keep(value, count);
                }
                run = Some((value.to_vec(), 1));
            }
        }
    }
    if let Some((value, count)) = run {
        keep(value, count);
    }
    let mut common: Vec<(Vec<u8>, u64)> = common
        .into_iter()
        .map(|Reverse((count, value))| (value, count))
        .collect();
    common.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut bounds = Vec::new();
    if entries > 0 {
        let buckets = (BUCKETS as u64).min(entries);
        let mut next = 0u64;
        let mut last = Vec::new();
        for (at, item) in (0u64..).zip(index_range(table, ns, &def.name)?) {
            let (key, _) = item.map_err(storage)?;
            let value = key.value().2;
            if at == next {
                bounds.push(value.to_vec());
                next = (bounds.len() as u64 * entries / buckets).min(entries - 1);
                if next == at {
                    next += 1;
                }
            }
            last.clear();
            last.extend_from_slice(value);
        }
        if bounds.last() != Some(&last) || bounds.len() == 1 {
            bounds.push(last);
        }
    }
    Ok(IndexStats {
        name: def.name.clone(),
        label: def.label.clone(),
        key: def.keys().join(","),
        unique: def.unique,
        entries,
        distinct,
        label_nodes,
        common: if def.more_keys.is_empty() {
            common
                .iter()
                .filter_map(|(bytes, count)| decode_value(bytes).map(|value| (value, *count)))
                .collect()
        } else {
            Vec::new()
        },
        common_bytes: common,
        bounds,
    })
}

/// Per edge label: edges, and distinct nodes on this side. Entries are
/// sorted by node, then label, so each (node, label) pair is one run.
fn adjacency_stats(
    table: &impl ReadableTable<AdjKey, Adj>,
    ns: &str,
) -> Result<Vec<(String, u64, u64)>> {
    let mut counts: HashMap<String, (u64, u64)> = HashMap::new();
    let mut previous: Option<(u128, String)> = None;
    for item in table.range((ns, 0u128, "", 0u128)..).map_err(storage)? {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, node, label, _) = key.value();
        if key_ns != ns {
            break;
        }
        let entry = counts.entry(label.to_owned()).or_default();
        entry.0 += 1;
        let new_pair = previous
            .as_ref()
            .is_none_or(|(prev_node, prev_label)| *prev_node != node || prev_label != label);
        if new_pair {
            entry.1 += 1;
            previous = Some((node, label.to_owned()));
        }
    }
    Ok(counts
        .into_iter()
        .map(|(label, (edges, nodes))| (label, edges, nodes))
        .collect())
}
