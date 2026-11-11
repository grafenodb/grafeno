//! Integrity check of a namespace, and rebuilding its indexes from the records.
//!
//! Node, edge, text and vector records are the source of truth. Everything
//! else (label index, adjacency, postings, HNSW links, counters) is derived
//! and can be rebuilt from them.

use std::collections::{BTreeMap, HashMap, HashSet};

use grafeno_core::{Edge, Node, NodeId, Result};
use redb::ReadableTable;
use serde::Serialize;

use crate::db::{ReadTx, WriteTx};
use crate::index::{INDEX_ENTRIES, entries_of, entry_bytes, record_entries, values_of};
use crate::memory::names_of;
use crate::stats::{bump_edge_label, bump_namespace, label_counts};
use crate::store::{
    ADJ_IN, ADJ_OUT, EDGE_LABEL_COUNTS, EDGES, HNSW_LINKS, LABEL_COUNTS, MEMORY, NAMES,
    NODE_LABELS, NODES, NS_COUNTS, POSTINGS, RecordKey, TEXT_LENGTHS, TEXT_STATS, TEXTS,
    VECTOR_META, VECTORS, VECTORS_Q8, adjacency_value, decode, storage,
};
use crate::{text, vector};

const MAX_PROBLEMS: usize = 100;

/// The result of [`ReadTx::check`].
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CheckReport {
    pub namespace: String,
    pub nodes: u64,
    pub edges: u64,
    pub texts: u64,
    pub vectors: u64,
    /// Vector-graph links that point to removed nodes. They are expected
    /// after deletes, and searches skip them.
    pub dangling_links: u64,
    /// Vectors that cannot be reached from the vector graph's entry point:
    /// searches never return them. A few appear as vectors change;
    /// [`WriteTx::compact_vectors`] reconnects them.
    pub unreachable_vectors: u64,
    /// Inconsistencies found, at most 100 (the last line counts the rest).
    pub problems: Vec<String>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// The result of [`WriteTx::rebuild_indexes`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RebuildReport {
    pub namespace: String,
    pub nodes: u64,
    pub edges: u64,
    pub texts: u64,
    pub vectors: u64,
    /// Records dropped because their node no longer exists.
    pub dropped_edges: u64,
    pub dropped_texts: u64,
    pub dropped_vectors: u64,
}

impl ReadTx {
    /// Checks that every index and counter of the namespace agrees with its
    /// records, and that every vector can be reached by searches.
    pub fn check(&self, ns: &str) -> Result<CheckReport> {
        // Every derived index is compared with its records through a
        // fingerprint (a sum of hashes, so order does not matter): records
        // and index are each read once, in order. Only an index whose
        // fingerprint differs is looked at entry by entry, to say what is
        // wrong. Memory grows with the node ids (to check edge ends) and the
        // values of unique indexes, not with the records.
        let mut problems = Vec::new();
        let defs = self.index_defs(ns)?;
        let mut ids: HashSet<u128> = HashSet::new();
        let mut node_count = 0u64;

        // Nodes: what they call for in the label, name and property indexes.
        let mut node_labels: BTreeMap<String, u64> = BTreeMap::new();
        let (mut labels_expected, mut names_expected, mut index_expected) =
            (Print::default(), Print::default(), Print::default());
        let mut index_counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut unique_seen: Vec<HashMap<String, String>> = vec![HashMap::new(); defs.len()];
        for item in self.nodes.range((ns, 0u128)..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            if key.value().0 != ns {
                break;
            }
            let node: Node = decode(value.value())?;
            node_count += 1;
            let id = node.id;
            ids.insert(id.as_u128());
            if node.labels.is_empty() {
                problems.push(format!("node {id} has no labels"));
            }
            if node.labels.windows(2).any(|pair| pair[0] >= pair[1]) {
                problems.push(format!("labels of node {id} are not sorted and unique"));
            }
            for label in &node.labels {
                *node_labels.entry(label.clone()).or_default() += 1;
                labels_expected.add(&(label.as_str(), id.as_u128()));
            }
            for name in names_of(&node) {
                names_expected.add(&(name.as_str(), id.as_u128()));
            }
            for (def, bytes) in entries_of(&defs, &node) {
                *index_counts.entry(def.name.clone()).or_default() += 1;
                index_expected.add(&(def.name.as_str(), bytes.as_slice(), id.as_u128()));
            }
            unique_clash(
                &defs,
                &mut unique_seen,
                false,
                &node.labels,
                &node.props,
                &id.to_string(),
                &mut problems,
            );
        }

        // Edges: their ends exist, and what they call for in adjacency.
        let mut edge_labels: BTreeMap<String, u64> = BTreeMap::new();
        let mut edge_count = 0u64;
        let (mut out_expected, mut in_expected) = (Print::default(), Print::default());
        for item in self.edges.range((ns, 0u128)..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            if key.value().0 != ns {
                break;
            }
            let edge: Edge = decode(value.value())?;
            edge_count += 1;
            *edge_labels.entry(edge.label.clone()).or_default() += 1;
            for end in [edge.from, edge.to] {
                if !ids.contains(&end.as_u128()) && problems.len() < MAX_PROBLEMS {
                    problems.push(format!("edge {} points to missing node {end}", edge.id));
                }
            }
            let (label, edge_id) = (edge.label.as_str(), edge.id.as_u128());
            let labels = [edge.label.clone()];
            for (def, bytes) in record_entries(&defs, true, &labels, &edge.props) {
                *index_counts.entry(def.name.clone()).or_default() += 1;
                index_expected.add(&(def.name.as_str(), bytes.as_slice(), edge_id));
            }
            if !edge.is_retracted() {
                unique_clash(
                    &defs,
                    &mut unique_seen,
                    true,
                    &labels,
                    &edge.props,
                    &edge.id.to_string(),
                    &mut problems,
                );
            }
            out_expected.add(&(
                edge.from.as_u128(),
                label,
                edge_id,
                adjacency_value(&edge, edge.to),
            ));
            in_expected.add(&(
                edge.to.as_u128(),
                label,
                edge_id,
                adjacency_value(&edge, edge.from),
            ));
        }

        drop(unique_seen);

        // The indexes, each read once in order.
        let mut labels_stored = Print::default();
        let mut indexed_labels: BTreeMap<String, u64> = BTreeMap::new();
        for item in self.labels.range((ns, "", 0u128)..).map_err(storage)? {
            let (key, _) = item.map_err(storage)?;
            let (key_ns, label, id) = key.value();
            if key_ns != ns {
                break;
            }
            labels_stored.add(&(label, id));
            *indexed_labels.entry(label.to_owned()).or_default() += 1;
        }
        let mut names_stored = Print::default();
        for item in self.names.range((ns, "", 0u128)..).map_err(storage)? {
            let (key, _) = item.map_err(storage)?;
            let (key_ns, name, id) = key.value();
            if key_ns != ns {
                break;
            }
            names_stored.add(&(name, id));
        }
        let empty: &[u8] = &[];
        let mut index_stored = Print::default();
        let mut stored_counts: BTreeMap<String, u64> = BTreeMap::new();
        for item in self
            .index_entries
            .range((ns, "", empty, 0u128)..)
            .map_err(storage)?
        {
            let (key, _) = item.map_err(storage)?;
            let (key_ns, name, bytes, id) = key.value();
            if key_ns != ns {
                break;
            }
            index_stored.add(&(name, bytes, id));
            *stored_counts.entry(name.to_owned()).or_default() += 1;
        }
        let mut adjacency_stored = [Print::default(), Print::default()];
        for (table, print) in [&self.out, &self.inn]
            .into_iter()
            .zip(adjacency_stored.iter_mut())
        {
            for item in table.range((ns, 0u128, "", 0u128)..).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                let (key_ns, node, label, edge) = key.value();
                if key_ns != ns {
                    break;
                }
                print.add(&(node, label, edge, value.value()));
            }
        }

        // Where a fingerprint differs, find what is wrong.
        if labels_stored != labels_expected {
            let before = problems.len();
            self.each_node(ns, &mut problems, |read, node, problems| {
                for label in &node.labels {
                    if read
                        .labels
                        .get((ns, label.as_str(), node.id.as_u128()))
                        .map_err(storage)?
                        .is_none()
                    {
                        problems.push(format!(
                            "label index misses node {} under {label:?}",
                            node.id
                        ));
                    }
                }
                Ok(())
            })?;
            for (label, count) in &indexed_labels {
                let expected = node_labels.get(label).copied().unwrap_or(0);
                if *count != expected {
                    problems.push(format!(
                        "label index lists {count} nodes under {label:?}, but {expected} nodes have it"
                    ));
                }
            }
            if problems.len() == before {
                problems.push("the label index does not match the nodes".into());
            }
        }
        if names_stored != names_expected {
            let before = problems.len();
            self.each_node(ns, &mut problems, |read, node, problems| {
                for name in names_of(node) {
                    if read
                        .names
                        .get((ns, name.as_str(), node.id.as_u128()))
                        .map_err(storage)?
                        .is_none()
                    {
                        problems.push(format!("names index misses {name:?} of node {}", node.id));
                    }
                }
                Ok(())
            })?;
            if names_stored.count != names_expected.count {
                problems.push(format!(
                    "{} entries in the names index, expected {}",
                    names_stored.count, names_expected.count
                ));
            }
            if problems.len() == before {
                problems.push("the names index does not match the nodes".into());
            }
        }
        if index_stored != index_expected {
            let before = problems.len();
            self.each_node(ns, &mut problems, |read, node, problems| {
                for (def, bytes) in entries_of(&defs, node) {
                    if read
                        .index_entries
                        .get((ns, def.name.as_str(), bytes.as_slice(), node.id.as_u128()))
                        .map_err(storage)?
                        .is_none()
                    {
                        problems.push(format!("index {:?} misses node {}", def.name, node.id));
                    }
                }
                Ok(())
            })?;
            for item in self.edges.range((ns, 0u128)..).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                if key.value().0 != ns || problems.len() >= MAX_PROBLEMS {
                    break;
                }
                let edge: Edge = decode(value.value())?;
                let labels = [edge.label.clone()];
                for (def, bytes) in record_entries(&defs, true, &labels, &edge.props) {
                    if self
                        .index_entries
                        .get((ns, def.name.as_str(), bytes.as_slice(), edge.id.as_u128()))
                        .map_err(storage)?
                        .is_none()
                    {
                        problems.push(format!("index {:?} misses edge {}", def.name, edge.id));
                    }
                }
            }
            for (name, count) in &stored_counts {
                let expected = index_counts.get(name).copied().unwrap_or(0);
                if *count > expected {
                    problems.push(format!(
                        "index {name:?} has {} stale entries ({count} for {expected} records)",
                        count - expected
                    ));
                }
            }
            if problems.len() == before {
                problems.push("a property index does not match the records".into());
            }
        }
        for ((stored, expected), name) in adjacency_stored
            .iter()
            .zip([&out_expected, &in_expected])
            .zip(["outgoing", "incoming"])
        {
            if stored == expected {
                continue;
            }
            let before = problems.len();
            let table = if name == "outgoing" {
                &self.out
            } else {
                &self.inn
            };
            for item in self.edges.range((ns, 0u128)..).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                if key.value().0 != ns || problems.len() >= MAX_PROBLEMS {
                    break;
                }
                let edge: Edge = decode(value.value())?;
                let (node, other) = if name == "outgoing" {
                    (edge.from, edge.to)
                } else {
                    (edge.to, edge.from)
                };
                let found = table
                    .get((ns, node.as_u128(), edge.label.as_str(), edge.id.as_u128()))
                    .map_err(storage)?
                    .map(|g| g.value());
                if found != Some(adjacency_value(&edge, other)) {
                    problems.push(format!("{name} adjacency of edge {} is wrong", edge.id));
                }
            }
            if stored.count != edge_count {
                problems.push(format!(
                    "{} {name} adjacency entries for {edge_count} edges",
                    stored.count
                ));
            }
            if problems.len() == before {
                problems.push(format!("the {name} adjacency does not match the edges"));
            }
        }

        // Counters.
        let counted = self
            .ns_counts
            .get(ns)
            .map_err(storage)?
            .map(|guard| guard.value())
            .unwrap_or((0, 0));
        let actual = (node_count, edge_count);
        if counted != actual {
            problems.push(format!(
                "namespace counters say {counted:?} (nodes, edges), records say {actual:?}"
            ));
        }
        if label_counts(&self.label_counts, ns)? != node_labels {
            problems.push("node label counters do not match the nodes".into());
        }
        if label_counts(&self.edge_label_counts, ns)? != edge_labels {
            problems.push("edge label counters do not match the edges".into());
        }

        // Memory states.
        for item in self.memory.range((ns, 0u128)..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            let (key_ns, id) = key.value();
            if key_ns != ns {
                break;
            }
            if !ids.contains(&id) {
                problems.push(format!(
                    "memory state of missing node {}",
                    NodeId::from_u128(id)
                ));
            }
            let (importance, _, _) = value.value();
            if !(0.0..=1.0).contains(&importance) {
                problems.push(format!(
                    "importance {importance} of node {} is out of range",
                    NodeId::from_u128(id)
                ));
            }
        }

        let texts = text::check(
            &self.texts,
            &self.text_lengths,
            &self.postings,
            &self.text_stats,
            ns,
            &ids,
            &mut problems,
        )?;
        let (vectors, dangling_links, unreachable_vectors) = vector::check(
            &self.vectors,
            &self.hnsw_links,
            &self.vector_meta,
            ns,
            &ids,
            &mut problems,
        )?;

        if problems.len() > MAX_PROBLEMS {
            let rest = problems.len() - MAX_PROBLEMS;
            problems.truncate(MAX_PROBLEMS);
            problems.push(format!("… and {rest} more"));
        }
        Ok(CheckReport {
            namespace: ns.to_owned(),
            nodes: node_count,
            edges: edge_count,
            texts,
            vectors,
            dangling_links,
            unreachable_vectors,
            problems,
        })
    }
}

impl ReadTx {
    /// Runs `visit` on every node of `ns`, in order, until it has found
    /// [`MAX_PROBLEMS`] problems.
    fn each_node(
        &self,
        ns: &str,
        problems: &mut Vec<String>,
        mut visit: impl FnMut(&Self, &Node, &mut Vec<String>) -> Result<()>,
    ) -> Result<()> {
        for item in self.nodes.range((ns, 0u128)..).map_err(storage)? {
            let (key, value) = item.map_err(storage)?;
            if key.value().0 != ns || problems.len() >= MAX_PROBLEMS {
                break;
            }
            let node: Node = decode(value.value())?;
            visit(self, &node, problems)?;
        }
        Ok(())
    }
}

/// A fingerprint of a set of entries: how many, and the sum of a 128-bit
/// hash of each. Two sets with equal fingerprints are, but for a chance of
/// about 2^-64, equal; the order the entries come in does not matter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Print {
    count: u64,
    sum: u128,
}

impl Print {
    fn add(&mut self, entry: &impl std::hash::Hash) {
        use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
        let hash = |seed: u8| {
            let mut hasher = BuildHasherDefault::<DefaultHasher>::default().build_hasher();
            std::hash::Hash::hash(&seed, &mut hasher);
            entry.hash(&mut hasher);
            std::hash::Hasher::finish(&hasher)
        };
        self.count += 1;
        self.sum = self
            .sum
            .wrapping_add(u128::from(hash(1)) << 64 | u128::from(hash(2)));
    }
}

impl WriteTx {
    /// Rebuilds every index and counter of the namespace from its node, edge,
    /// text and vector records. Edges, texts and vectors of missing nodes are
    /// dropped. The vector graph is built anew, which also removes dangling
    /// links.
    pub fn rebuild_indexes(&mut self, ns: &str) -> Result<RebuildReport> {
        self.bulk(
            crate::replication::Bulk::RebuildIndexes { ns: ns.to_owned() },
            |tx| tx.rebuild_indexes_inner(ns),
        )
    }

    fn rebuild_indexes_inner(&mut self, ns: &str) -> Result<RebuildReport> {
        let tx = &self.tx;
        // Node and edge records stay where they are and are read back a page
        // at a time below; only the node ids are kept meanwhile.
        let mut ids: HashSet<u128> = HashSet::new();
        for item in tx
            .open_table(NODES)
            .map_err(storage)?
            .range((ns, 0u128)..)
            .map_err(storage)?
        {
            let (key, _) = item.map_err(storage)?;
            if key.value().0 != ns {
                break;
            }
            ids.insert(key.value().1);
        }
        // Texts and vectors are removed and indexed again, so they are held.
        let mut texts = Vec::new();
        for item in tx
            .open_table(TEXTS)
            .map_err(storage)?
            .range((ns, 0u128)..)
            .map_err(storage)?
        {
            let (key, value) = item.map_err(storage)?;
            if key.value().0 != ns {
                break;
            }
            texts.push((key.value().1, value.value().to_owned()));
        }
        let mut vectors = Vec::new();
        {
            let table = tx.open_table(VECTORS).map_err(storage)?;
            for item in table.range((ns, 0u128)..).map_err(storage)? {
                let (key, _) = item.map_err(storage)?;
                if key.value().0 != ns {
                    break;
                }
                let id = NodeId::from_u128(key.value().1);
                if let Some(vector) = vector::embedding(&table, ns, id)? {
                    vectors.push((id, vector));
                }
            }
        }

        // Clear everything derived, plus the texts and vectors that will be
        // indexed again. Each range starts at the namespace and keeps the
        // entries of the namespaces after it.
        let other = |key_ns: &str| key_ns != ns;
        let tx = &self.tx;
        for definition in [NODE_LABELS, NAMES] {
            tx.open_table(definition)
                .map_err(storage)?
                .retain_in((ns, "", 0u128).., |key, _| other(key.0))
                .map_err(storage)?;
        }
        // Index definitions stay; their entries are rebuilt with the nodes.
        let empty: &[u8] = &[];
        tx.open_table(INDEX_ENTRIES)
            .map_err(storage)?
            .retain_in((ns, "", empty, 0u128).., |key, _| other(key.0))
            .map_err(storage)?;
        tx.open_table(crate::fulltext::FULLTEXT_STATS)
            .map_err(storage)?
            .retain_in((ns, "").., |key, _| other(key.0))
            .map_err(storage)?;
        // Memory states are records too: only those of missing nodes go.
        tx.open_table(MEMORY)
            .map_err(storage)?
            .retain_in((ns, 0u128).., |key, _| other(key.0) || ids.contains(&key.1))
            .map_err(storage)?;
        for definition in [ADJ_OUT, ADJ_IN] {
            tx.open_table(definition)
                .map_err(storage)?
                .retain_in((ns, 0u128, "", 0u128).., |key, _| other(key.0))
                .map_err(storage)?;
        }
        tx.open_table(POSTINGS)
            .map_err(storage)?
            .retain_in((ns, "", 0u128).., |key, _| other(key.0))
            .map_err(storage)?;
        tx.open_table(TEXT_LENGTHS)
            .map_err(storage)?
            .retain_in((ns, 0u128).., |key, _| other(key.0))
            .map_err(storage)?;
        tx.open_table(TEXTS)
            .map_err(storage)?
            .retain_in((ns, 0u128).., |key, _| other(key.0))
            .map_err(storage)?;
        for definition in [VECTORS, VECTORS_Q8] {
            tx.open_table(definition)
                .map_err(storage)?
                .retain_in((ns, 0u128).., |key, _| other(key.0))
                .map_err(storage)?;
        }
        tx.open_table(HNSW_LINKS)
            .map_err(storage)?
            .retain_in((ns, 0u128, 0u8).., |key, _| other(key.0))
            .map_err(storage)?;
        for definition in [LABEL_COUNTS, EDGE_LABEL_COUNTS] {
            tx.open_table(definition)
                .map_err(storage)?
                .retain_in((ns, "").., |key, _| other(key.0))
                .map_err(storage)?;
        }
        tx.open_table(TEXT_STATS)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;
        tx.open_table(VECTOR_META)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;
        tx.open_table(NS_COUNTS)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;

        // The tables were changed underneath the vector cache.
        self.vectors.forget(ns);

        let mut report = RebuildReport {
            namespace: ns.to_owned(),
            ..RebuildReport::default()
        };
        let mut after = None;
        while let Some(page) = page::<Node>(&self.tx, NODES, ns, &mut after)? {
            for node in &page {
                self.put_node(ns, node, None)?;
                report.nodes += 1;
            }
        }
        let mut after = None;
        while let Some(page) = page::<Edge>(&self.tx, EDGES, ns, &mut after)? {
            for edge in &page {
                self.rebuild_edge(ns, edge, &ids, &mut report)?;
            }
        }
        for (id, content) in &texts {
            if ids.contains(id) {
                text::index(&self.tx, ns, NodeId::from_u128(*id), content)?;
                report.texts += 1;
            } else {
                report.dropped_texts += 1;
            }
        }
        for (id, embedding) in &vectors {
            if ids.contains(&id.as_u128()) {
                vector::insert(&self.tx, &mut self.vectors, ns, *id, embedding)?;
                report.vectors += 1;
            } else {
                report.dropped_vectors += 1;
            }
        }
        Ok(report)
    }

    fn rebuild_edge(
        &mut self,
        ns: &str,
        edge: &Edge,
        ids: &HashSet<u128>,
        report: &mut RebuildReport,
    ) -> Result<()> {
        if ids.contains(&edge.from.as_u128()) && ids.contains(&edge.to.as_u128()) {
            self.put_edge(ns, edge)?;
            // The record stayed while its index entries went: put them back.
            self.index_edge(ns, Some(edge), None)?;
            bump_namespace(&self.tx, ns, 0, 1)?;
            bump_edge_label(&self.tx, ns, &edge.label, 1)?;
            report.edges += 1;
        } else {
            self.tx
                .open_table(EDGES)
                .map_err(storage)?
                .remove((ns, edge.id.as_u128()))
                .map_err(storage)?;
            report.dropped_edges += 1;
        }
        Ok(())
    }
}

/// Records two current records of a unique index holding the same values.
fn unique_clash<S: AsRef<str>>(
    defs: &[crate::IndexDef],
    seen: &mut [HashMap<String, String>],
    edges: bool,
    labels: &[S],
    props: &grafeno_core::Props,
    id: &str,
    problems: &mut Vec<String>,
) {
    for (def, seen) in defs.iter().zip(seen.iter_mut()) {
        // Only values the index holds can clash: not a null.
        if def.unique
            && def.edges == edges
            && def.covers(labels)
            && entry_bytes(def, props).is_some()
            && let Some(other) = seen.insert(values_of(def, props), id.to_owned())
        {
            problems.push(format!(
                "{} {other} and {id} share {} = {} in unique index {:?}",
                if edges { "edges" } else { "nodes" },
                def.keys().join(", "),
                values_of(def, props),
                def.name
            ));
        }
    }
}

/// The next records of `ns` after `after` (moved on past them), up to
/// 10 000; `None` when there are no more.
fn page<R: serde::de::DeserializeOwned>(
    tx: &redb::WriteTransaction,
    definition: redb::TableDefinition<RecordKey, &'static [u8]>,
    ns: &str,
    after: &mut Option<u128>,
) -> Result<Option<Vec<R>>> {
    use std::ops::Bound;
    let table = tx.open_table(definition).map_err(storage)?;
    let start = match *after {
        Some(id) => Bound::Excluded((ns, id)),
        None => Bound::Included((ns, 0u128)),
    };
    let mut out = Vec::new();
    for item in table
        .range::<(&str, u128)>((start, Bound::Unbounded))
        .map_err(storage)?
    {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns || out.len() == 10_000 {
            break;
        }
        *after = Some(key.value().1);
        out.push(decode(value.value())?);
    }
    Ok((!out.is_empty()).then_some(out))
}

#[cfg(test)]
mod tests {
    use grafeno_core::{NewEdge, NewNode};

    use crate::Grafeno;
    use crate::store::{
        ADJ_IN, HNSW_LINKS, LABEL_COUNTS, NODE_LABELS, NS_COUNTS, POSTINGS, TEXT_LENGTHS,
    };

    #[test]
    fn check_finds_corruption_and_rebuild_repairs_it() {
        let db = Grafeno::in_memory().unwrap();
        let (a, b) = db
            .write(|tx| {
                let a = tx.create_node(
                    "ns",
                    NewNode::new("Person")
                        .text("alpha beta")
                        .embedding(vec![1.0, 0.0]),
                )?;
                let b = tx.create_node(
                    "ns",
                    NewNode::new("Person")
                        .text("beta gamma")
                        .embedding(vec![0.0, 1.0]),
                )?;
                tx.create_edge("ns", NewEdge::new(a, "knows", b))?;
                tx.create_node("other", NewNode::new("Note").text("untouched"))?;
                Ok((a, b))
            })
            .unwrap();
        assert!(db.begin_read().unwrap().check("ns").unwrap().is_ok());

        // Damage every derived structure.
        db.write(|tx| {
            let t = &tx.tx;
            t.open_table(NODE_LABELS)
                .unwrap()
                .remove(("ns", "Person", a.as_u128()))
                .unwrap();
            t.open_table(ADJ_IN)
                .unwrap()
                .retain(|key, _| key.0 != "ns")
                .unwrap();
            t.open_table(POSTINGS)
                .unwrap()
                .insert(("ns", "ghost", b.as_u128()), (7, 1))
                .unwrap();
            t.open_table(TEXT_LENGTHS)
                .unwrap()
                .insert(("ns", a.as_u128()), 99)
                .unwrap();
            t.open_table(NS_COUNTS)
                .unwrap()
                .insert("ns", (5, 5))
                .unwrap();
            t.open_table(LABEL_COUNTS)
                .unwrap()
                .insert(("ns", "Person"), 1)
                .unwrap();
            t.open_table(HNSW_LINKS)
                .unwrap()
                .retain(|key, _| key.0 != "ns")
                .unwrap();
            Ok(())
        })
        .unwrap();

        let report = db.begin_read().unwrap().check("ns").unwrap();
        let all = report.problems.join("\n");
        for expected in [
            "label index misses node",
            "incoming adjacency of edge",
            "postings in the text index",
            "text length of node",
            "namespace counters say",
            "node label counters",
        ] {
            assert!(all.contains(expected), "missing {expected:?} in:\n{all}");
        }
        assert!(
            report.unreachable_vectors > 0,
            "the cut links strand vectors"
        );

        db.write(|tx| tx.rebuild_indexes("ns")).unwrap();
        let read = db.begin_read().unwrap();
        let report = read.check("ns").unwrap();
        assert!(report.is_ok(), "{:?}", report.problems);
        assert!(read.check("other").unwrap().is_ok());
        assert_eq!(read.search_text("ns", "gamma", 5).unwrap()[0].node, b);
        assert!(read.search_text("ns", "ghost", 5).unwrap().is_empty());
        assert_eq!(read.search_vector("ns", &[1.0, 0.1], 1).unwrap()[0].node, a);
    }
}

/// What compacting a namespace's vector graph did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CompactReport {
    pub vectors: u64,
    /// Before and after: dangling links and unreachable vectors.
    pub dangling_before: u64,
    pub unreachable_before: u64,
    pub unreachable_after: u64,
}

impl ReadTx {
    /// Estimates the recall@10 of vector searches with `ef` (see
    /// [`ReadTx::search_vector_with`]) from `sample` of the namespace's own
    /// vectors used as queries against exact answers. Reads every vector
    /// once. `None` when the namespace has no vectors.
    pub fn vector_recall(&self, ns: &str, sample: usize, ef: usize) -> Result<Option<f64>> {
        vector::estimate_recall(
            &self.vectors,
            &self.vectors_q8,
            &self.hnsw_links,
            &self.vector_meta,
            ns,
            sample,
            ef,
        )
    }
}

impl WriteTx {
    /// Builds the namespace's vector graph again from its vectors: every
    /// link is chosen afresh, dangling links go and every vector is
    /// reachable. Searches see the old graph until the commit. Takes about
    /// as long as inserting every vector.
    pub fn compact_vectors(&mut self, ns: &str) -> Result<CompactReport> {
        self.bulk(
            crate::replication::Bulk::CompactVectors { ns: ns.to_owned() },
            |tx| tx.compact_vectors_inner(ns),
        )
    }

    fn compact_vectors_inner(&mut self, ns: &str) -> Result<CompactReport> {
        let (dangling_before, unreachable_before) = {
            let vectors = self.tx.open_table(VECTORS).map_err(storage)?;
            let links = self.tx.open_table(HNSW_LINKS).map_err(storage)?;
            let meta = self.tx.open_table(VECTOR_META).map_err(storage)?;
            let ids: HashSet<u128> = vectors
                .range((ns, 0u128)..)
                .map_err(storage)?
                .map_while(|item| {
                    item.ok()
                        .filter(|(key, _)| key.value().0 == ns)
                        .map(|(key, _)| key.value().1)
                })
                .collect();
            let mut ignored = Vec::new();
            let (_, dangling, unreachable) =
                vector::check(&vectors, &links, &meta, ns, &ids, &mut ignored)?;
            (dangling, unreachable)
        };
        let mut all = Vec::new();
        {
            let table = self.tx.open_table(VECTORS).map_err(storage)?;
            for item in table.range((ns, 0u128)..).map_err(storage)? {
                let (key, _) = item.map_err(storage)?;
                if key.value().0 != ns {
                    break;
                }
                let id = NodeId::from_u128(key.value().1);
                if let Some(vector) = vector::embedding(&table, ns, id)? {
                    all.push((id, vector));
                }
            }
        }
        let other = |key_ns: &str| key_ns != ns;
        for definition in [VECTORS, VECTORS_Q8] {
            self.tx
                .open_table(definition)
                .map_err(storage)?
                .retain_in((ns, 0u128).., |key, _| other(key.0))
                .map_err(storage)?;
        }
        self.tx
            .open_table(HNSW_LINKS)
            .map_err(storage)?
            .retain_in((ns, 0u128, 0u8).., |key, _| other(key.0))
            .map_err(storage)?;
        self.tx
            .open_table(VECTOR_META)
            .map_err(storage)?
            .remove(ns)
            .map_err(storage)?;
        self.vectors.forget(ns);
        for (id, vector) in &all {
            vector::insert(&self.tx, &mut self.vectors, ns, *id, vector)?;
        }
        let unreachable_after = {
            let vectors = self.tx.open_table(VECTORS).map_err(storage)?;
            let links = self.tx.open_table(HNSW_LINKS).map_err(storage)?;
            let meta = self.tx.open_table(VECTOR_META).map_err(storage)?;
            let ids: HashSet<u128> = all.iter().map(|(id, _)| id.as_u128()).collect();
            let mut ignored = Vec::new();
            vector::check(&vectors, &links, &meta, ns, &ids, &mut ignored)?.2
        };
        Ok(CompactReport {
            vectors: all.len() as u64,
            dangling_before,
            unreachable_before,
            unreachable_after,
        })
    }
}
