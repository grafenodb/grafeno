//! Named vector indexes (16.7): several per namespace, each over one
//! property of the nodes with its labels (a list of numbers), with its own
//! dimension, similarity and HNSW parameters.
//!
//! An index is an [`IndexDef`] with `vector` set, so it is declared,
//! replicated, exported and dropped as any index. Its graph is a space of
//! [`crate::vector`] named `ns\0index`, kept in the same tables as the
//! node embeddings and in the same transaction as every write, on every
//! member (derived, as the other index entries are). An index created
//! with `building` set starts empty and is filled a chunk at a time by
//! [`WriteTx::build_index_step`] (logged as itself, so every member does
//! the same), while writes go on and keep the nodes they touch indexed.

use std::collections::HashMap;

use grafeno_core::{Error, NodeId, Props, Result};
use redb::WriteTransaction;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::index::IndexDef;
use crate::store::{
    HNSW_LINKS, NODE_LABELS, VECTOR_META, VECTORS, VECTORS_Q8, encode, nodes_with_label, storage,
};
use crate::vector::{self, DEFAULT_EF_CONSTRUCTION, DEFAULT_M, Similarity, VectorMeta};
use crate::{Hit, ReadTx, WriteTx};

/// Candidates on layer 0 of a search, by default.
pub const DEFAULT_EF_SEARCH: usize = 64;

/// A filter on labels that at most this many nodes pass is answered by
/// scoring those nodes exactly instead of walking the graph.
const EXACT_UP_TO: u64 = 4_096;

fn default_m() -> usize {
    DEFAULT_M
}

fn default_ef_construction() -> usize {
    DEFAULT_EF_CONSTRUCTION
}

fn default_ef_search() -> usize {
    DEFAULT_EF_SEARCH
}

/// What makes an index a vector one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VectorSpec {
    pub dimension: usize,
    #[serde(default)]
    pub similarity: Similarity,
    /// Links per node (twice as many on layer 0).
    #[serde(default = "default_m")]
    pub m: usize,
    /// Candidates while inserting.
    #[serde(default = "default_ef_construction")]
    pub ef_construction: usize,
    /// Candidates on layer 0 while searching, unless a search says.
    #[serde(default = "default_ef_search")]
    pub ef_search: usize,
    /// Being filled in the background: how far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub building: Option<Building>,
}

impl VectorSpec {
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension,
            similarity: Similarity::Cosine,
            m: DEFAULT_M,
            ef_construction: DEFAULT_EF_CONSTRUCTION,
            ef_search: DEFAULT_EF_SEARCH,
            building: None,
        }
    }

    fn params(&self) -> vector::Params {
        vector::Params {
            similarity: self.similarity,
            m: self.m,
            ef_construction: self.ef_construction,
        }
    }
}

/// How far a background build is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Building {
    /// The last node it indexed (nodes go in id order).
    #[serde(default)]
    pub after: Option<NodeId>,
    /// Nodes it went through.
    #[serde(default)]
    pub done: u64,
}

/// Where a background build is, for listings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BuildProgress {
    pub index: String,
    pub done: u64,
    /// Nodes with the index's labels now.
    pub total: u64,
    pub finished: bool,
}

/// The space of index `name` in `ns`.
pub(crate) fn space_of(ns: &str, name: &str) -> String {
    format!("{ns}\0{name}")
}

/// The vector a property holds: a list of numbers.
pub(crate) fn vector_of(value: &Json) -> Option<Vec<f32>> {
    let items = value.as_array()?;
    if items.is_empty() {
        return None;
    }
    items
        .iter()
        .map(|item| item.as_f64().map(|f| f as f32))
        .collect()
}

/// Removes every vector, link and the metadata of a space.
pub(crate) fn clear_space(tx: &WriteTransaction, space: &str) -> Result<()> {
    let keep = |key: &str| key != space;
    for table in [VECTORS, VECTORS_Q8] {
        tx.open_table(table)
            .map_err(storage)?
            .retain_in((space, 0u128)..=(space, u128::MAX), |key, _| keep(key.0))
            .map_err(storage)?;
    }
    tx.open_table(HNSW_LINKS)
        .map_err(storage)?
        .retain_in(
            (space, 0u128, 0u8)..=(space, u128::MAX, u8::MAX),
            |key, _| keep(key.0),
        )
        .map_err(storage)?;
    tx.open_table(VECTOR_META)
        .map_err(storage)?
        .remove(space)
        .map_err(storage)?;
    Ok(())
}

/// Makes the space of `def` empty, with its parameters.
pub(crate) fn reset_space(tx: &WriteTransaction, ns: &str, def: &IndexDef) -> Result<()> {
    let spec = def.vector.as_ref().expect("a vector index");
    let space = space_of(ns, &def.name);
    clear_space(tx, &space)?;
    tx.open_table(VECTOR_META)
        .map_err(storage)?
        .insert(
            space.as_str(),
            encode(&VectorMeta::new(spec.dimension, spec.params()))?.as_slice(),
        )
        .map_err(storage)?;
    Ok(())
}

/// Labels and properties a node must have to be kept by a search.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VectorFilter {
    /// Any of these labels (none: any label).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Each of these properties, with this value.
    #[serde(default)]
    pub props: Props,
}

impl VectorFilter {
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty() && self.props.is_empty()
    }
}

/// A vector search: which index (the node embeddings when `None`), how
/// many, how wide, and what to keep.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VectorSearch {
    pub index: Option<String>,
    pub limit: usize,
    /// Candidates on layer 0 (the index's `ef_search` when `None`).
    pub ef: Option<usize>,
    pub filter: Option<VectorFilter>,
    /// Never returned (the node a `similar` search starts from).
    pub exclude: Option<NodeId>,
}

impl WriteTx {
    /// Keeps the vector indexes of `defs` up to date for node `id`, from
    /// its `old` labels and properties to its `new` ones.
    pub(crate) fn index_vectors(
        &mut self,
        defs: &[IndexDef],
        ns: &str,
        id: u128,
        new: Option<(&[String], &Props)>,
        old: Option<(&[String], &Props)>,
    ) -> Result<()> {
        for def in defs.iter().filter(|def| !def.edges) {
            let Some(spec) = &def.vector else {
                continue;
            };
            let vector = |side: Option<(&[String], &Props)>| {
                side.filter(|(labels, _)| def.covers(labels))
                    .and_then(|(_, props)| props.get(&def.key))
                    .and_then(vector_of)
            };
            let (before, after) = (vector(old), vector(new));
            if before == after {
                continue;
            }
            let space = space_of(ns, &def.name);
            let node = NodeId::from_u128(id);
            match after {
                Some(after) => {
                    if after.len() != spec.dimension {
                        return Err(Error::Invalid(format!(
                            "{} holds vectors of dimension {}: node {node} has {} values in {}",
                            def.name,
                            spec.dimension,
                            after.len(),
                            def.key
                        )));
                    }
                    vector::insert(&self.tx, &mut self.vectors, &space, node, &after)?;
                }
                None => {
                    vector::remove(&self.tx, &mut self.vectors, &space, node)?;
                }
            }
        }
        Ok(())
    }

    /// Fills a new vector index from the nodes it covers (none while it is
    /// to be built in the background). Returns how many it holds.
    pub(crate) fn fill_vector_index(
        &mut self,
        ns: &str,
        def: &IndexDef,
        records: &[(u128, Props, bool)],
    ) -> Result<u64> {
        let spec = def.vector.as_ref().expect("a vector index");
        reset_space(&self.tx, ns, def)?;
        if spec.building.is_some() {
            return Ok(0);
        }
        let space = space_of(ns, &def.name);
        let mut count = 0;
        for (id, props, _) in records {
            let Some(found) = props.get(&def.key).and_then(vector_of) else {
                continue;
            };
            if found.len() != spec.dimension {
                return Err(Error::Invalid(format!(
                    "cannot index {}: node {} has {} values in {}, not {}",
                    def.describe(),
                    NodeId::from_u128(*id),
                    found.len(),
                    def.key,
                    spec.dimension
                )));
            }
            vector::insert(
                &self.tx,
                &mut self.vectors,
                &space,
                NodeId::from_u128(*id),
                &found,
            )?;
            count += 1;
        }
        Ok(count)
    }

    /// Indexes the next `chunk` nodes of vector index `name`, built in the
    /// background; `None` when it is not being built. Logged as itself:
    /// every member of a cluster does the same step.
    pub fn build_index_step(
        &mut self,
        ns: &str,
        name: &str,
        chunk: usize,
    ) -> Result<Option<BuildProgress>> {
        self.bulk(
            crate::replication::Bulk::BuildIndex {
                ns: ns.to_owned(),
                name: name.to_owned(),
                chunk,
            },
            |tx| tx.build_index_step_inner(ns, name, chunk),
        )
    }

    fn build_index_step_inner(
        &mut self,
        ns: &str,
        name: &str,
        chunk: usize,
    ) -> Result<Option<BuildProgress>> {
        let Some(mut def) = self
            .index_defs(ns)?
            .into_iter()
            .find(|def| def.name == name)
        else {
            return Ok(None);
        };
        let Some(spec) = def.vector.as_ref() else {
            return Ok(None);
        };
        let Some(mut building) = spec.building.clone() else {
            return Ok(None);
        };
        let dimension = spec.dimension;
        let mut ids = Vec::new();
        {
            let labels = self.tx.open_table(NODE_LABELS).map_err(storage)?;
            for label in def.labels() {
                ids.extend(nodes_with_label(&labels, ns, label)?);
            }
        }
        ids.sort();
        ids.dedup();
        let total = ids.len() as u64;
        let next: Vec<NodeId> = ids
            .into_iter()
            .filter(|id| building.after.is_none_or(|after| *id > after))
            .take(chunk.max(1))
            .collect();
        let space = space_of(ns, &def.name);
        for id in &next {
            let Some(node) = self.node(ns, *id)? else {
                continue;
            };
            // A vector of another dimension is left out, not an error: the
            // write that put it there was made before the index existed.
            if let Some(found) = node.props.get(&def.key).and_then(vector_of)
                && found.len() == dimension
            {
                vector::insert(&self.tx, &mut self.vectors, &space, *id, &found)?;
            }
        }
        building.done += next.len() as u64;
        building.after = next.last().copied().or(building.after);
        let finished = next.len() < chunk.max(1);
        if let Some(spec) = def.vector.as_mut() {
            spec.building = (!finished).then_some(building.clone());
        }
        self.tx
            .open_table(crate::index::INDEXES)
            .map_err(storage)?
            .insert((ns, def.name.as_str()), encode(&def)?.as_slice())
            .map_err(storage)?;
        Ok(Some(BuildProgress {
            index: name.to_owned(),
            done: building.done.min(total),
            total,
            finished,
        }))
    }
}

impl crate::Grafeno {
    /// Builds every vector index of `ns` still to be built, a chunk of
    /// `chunk` nodes per write transaction, so other writes go between
    /// them. Returns how many indexes it finished.
    pub fn build_pending_indexes(&self, ns: &str, chunk: usize) -> Result<usize> {
        let mut finished = 0;
        for name in self.begin_read()?.pending_indexes(ns)? {
            loop {
                let step = self.write(|tx| tx.build_index_step(ns, &name, chunk))?;
                match step {
                    Some(progress) if !progress.finished => continue,
                    Some(_) => {
                        finished += 1;
                        break;
                    }
                    None => break,
                }
            }
        }
        Ok(finished)
    }
}

impl ReadTx {
    /// The vector indexes of `ns` being built in the background.
    pub fn pending_indexes(&self, ns: &str) -> Result<Vec<String>> {
        Ok(self
            .index_defs(ns)?
            .into_iter()
            .filter(|def| {
                def.vector
                    .as_ref()
                    .is_some_and(|spec| spec.building.is_some())
            })
            .map(|def| def.name)
            .collect())
    }

    /// The vectors a vector index holds.
    pub(crate) fn vector_count(&self, ns: &str, name: &str) -> Result<u64> {
        Ok(vector::read_meta(&self.vector_meta, &space_of(ns, name))?
            .map_or(0, |meta| meta.count()))
    }

    /// The nodes closest to `query` in a vector index (`search.index`), or
    /// among the node embeddings, best first. With a filter (and with a
    /// view), the nodes that fail it are passed through during the walk
    /// and not kept, so a selective filter still finds `limit` nodes.
    pub fn vector_search(
        &self,
        ns: &str,
        query: &[f32],
        search: &VectorSearch,
    ) -> Result<Vec<Hit>> {
        let (space, default_ef) = match &search.index {
            Some(name) => {
                let def = self
                    .index_defs(ns)?
                    .into_iter()
                    .find(|def| def.name == *name)
                    .ok_or_else(|| Error::Invalid(format!("there is no index named {name:?}")))?;
                let spec = def.vector.ok_or_else(|| {
                    Error::Invalid(format!(
                        "{name} is not a vector index (CREATE VECTOR INDEX makes one)"
                    ))
                })?;
                (space_of(ns, name), spec.ef_search)
            }
            None => (ns.to_owned(), (search.limit * 4).max(DEFAULT_EF_SEARCH)),
        };
        let ef = search.ef.unwrap_or(default_ef).max(search.limit);
        let filter = search.filter.as_ref().filter(|filter| !filter.is_empty());
        // Labels alone are checked in the label index, without reading the
        // node (and its vector); with properties or a view, the node is read.
        let labels_only =
            filter.is_some_and(|filter| filter.props.is_empty()) && self.view().is_none();
        let mut seen: HashMap<u128, bool> = HashMap::new();
        let mut keep = |id: u128| -> Result<bool> {
            if search.exclude.is_some_and(|node| node.as_u128() == id) {
                return Ok(false);
            }
            if filter.is_none() && self.view().is_none() {
                return Ok(true);
            }
            if let Some(known) = seen.get(&id) {
                return Ok(*known);
            }
            let kept = if labels_only {
                let mut any = false;
                for label in &filter.expect("a filter").labels {
                    if self
                        .labels
                        .get((ns, label.as_str(), id))
                        .map_err(storage)?
                        .is_some()
                    {
                        any = true;
                        break;
                    }
                }
                any
            } else {
                // `node` leaves out what the view hides.
                match self.node(ns, NodeId::from_u128(id))? {
                    None => false,
                    Some(node) => filter.is_none_or(|filter| {
                        (filter.labels.is_empty()
                            || filter.labels.iter().any(|l| node.has_label(l)))
                            && filter
                                .props
                                .iter()
                                .all(|(key, value)| node.props.get(key) == Some(value))
                    }),
                }
            };
            seen.insert(id, kept);
            Ok(kept)
        };
        // Few nodes with the filter's labels: score them all, exactly.
        if let Some(filter) = filter
            && !filter.labels.is_empty()
        {
            let mut passing = 0u64;
            for label in &filter.labels {
                passing += self
                    .label_counts
                    .get((ns, label.as_str()))
                    .map_err(storage)?
                    .map_or(0, |guard| guard.value());
            }
            if passing <= EXACT_UP_TO {
                let mut ids = Vec::new();
                for label in &filter.labels {
                    ids.extend(nodes_with_label(&self.labels, ns, label)?);
                }
                ids.sort();
                ids.dedup();
                let mut kept = Vec::with_capacity(ids.len());
                for id in ids {
                    if keep(id.as_u128())? {
                        kept.push(id.as_u128());
                    }
                }
                return vector::rank_ids(
                    &self.vectors,
                    &self.vector_meta,
                    &space,
                    query,
                    &kept,
                    search.limit,
                );
            }
        }
        vector::search_with(
            &self.vectors,
            &self.vectors_q8,
            &self.hnsw_links,
            &self.vector_meta,
            &space,
            query,
            search.limit,
            ef,
            &mut keep,
        )
    }

    /// The nodes closest to `node`'s own vector (its property in the
    /// index, or its embedding), itself left out.
    pub fn similar(&self, ns: &str, node: NodeId, search: &VectorSearch) -> Result<Vec<Hit>> {
        let query = match &search.index {
            Some(name) => {
                let def = self
                    .index_defs(ns)?
                    .into_iter()
                    .find(|def| def.name == *name && def.vector.is_some())
                    .ok_or_else(|| {
                        Error::Invalid(format!("there is no vector index named {name:?}"))
                    })?;
                self.node(ns, node)?
                    .and_then(|found| found.props.get(&def.key).and_then(vector_of))
            }
            None => self.embedding(ns, node)?,
        };
        let Some(query) = query else {
            return Err(Error::Invalid(format!(
                "node {node} has no vector{}",
                search
                    .index
                    .as_ref()
                    .map(|name| format!(" in {name}"))
                    .unwrap_or_default()
            )));
        };
        self.vector_search(
            ns,
            &query,
            &VectorSearch {
                exclude: Some(node),
                ..search.clone()
            },
        )
    }

    /// The progress of the vector indexes being built in `ns`.
    pub fn index_builds(&self, ns: &str) -> Result<Vec<BuildProgress>> {
        let mut out = Vec::new();
        for def in self.index_defs(ns)? {
            let Some(building) = def.vector.as_ref().and_then(|spec| spec.building.clone()) else {
                continue;
            };
            let mut total = 0;
            for label in def.labels() {
                total += self
                    .label_counts
                    .get((ns, label))
                    .map_err(storage)?
                    .map_or(0, |guard| guard.value());
            }
            out.push(BuildProgress {
                index: def.name.clone(),
                done: building.done.min(total),
                total,
                finished: false,
            });
        }
        Ok(out)
    }
}
