//! Vector index: HNSW (Malkov & Yashunin, 2016) stored in redb tables.
//!
//! Vectors are normalized on insert, so the distance is `1 - dot product`
//! (cosine). Each namespace has its own graph and a fixed dimension, set by
//! its first vector.
//!
//! A node's level is derived from its id (the random bits of a UUID v7), so it
//! never has to be stored and the entry point can always be recomputed.
//!
//! Each vector is also kept quantized to one signed byte per dimension
//! (`VECTORS_Q8`, a quarter of the bytes). Searches walk the graph with the
//! quantized vectors, computing distances straight from the stored bytes,
//! then rescore the `ef` best candidates with the exact vectors and keep the
//! best `limit`; inserts build the graph with the exact ones. Inserts and
//! removes in one write transaction share a [`Cache`] of decoded vectors and
//! link lists, updated by every write, so batch inserts do not read the same
//! entries from redb over and over.
//!
//! Since 16.7 a graph is a *space*: the namespace's node embeddings (the
//! space named as the namespace), or a named vector index (`ns\0index`),
//! each with its own similarity (cosine, dot product or Euclidean), `m` and
//! `ef_construction`, kept in its metadata. Only cosine spaces normalize
//! their vectors. A search can take a filter, applied while the graph is
//! walked: nodes that fail it are stepped through but not kept, so a
//! selective filter still finds `limit` results.
//!
//! A vector written in a transaction is stored at once and joins the graph
//! when the transaction commits ([`flush`]). With several new vectors in a
//! space, the expensive part of each insert, the search for its neighbors,
//! runs on a read snapshot of what the transaction started from, on several
//! threads, against the snapshot's graph and the other new vectors; the
//! writer then only writes the link lists, and works out again the few that
//! changed since the snapshot. The result depends on the vectors and their
//! order alone, so every member of a cluster that applies the same entry
//! builds the same graph.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use grafeno_core::{Error, NodeId, Result};
use redb::{Database, ReadableDatabase, ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};

use crate::Hit;
use crate::store::{
    HNSW_LINKS, LinkKey, RecordKey, VECTOR_META, VECTORS, VECTORS_Q8, decode, encode, storage,
};

/// Links per node on layers above 0, by default.
pub const DEFAULT_M: usize = 16;
/// Candidates while inserting, by default.
pub const DEFAULT_EF_CONSTRUCTION: usize = 100;
/// The search that finds new neighbors for the neighbors of a removed node.
const REPAIR_EF: usize = 16;
const MAX_LEVEL: u8 = 16;
/// The memory, by default, of the vectors and link lists a write
/// transaction keeps decoded (see [`crate::OpenOptions`]); past it the
/// cache starts over, which bounds very large write transactions.
pub const DEFAULT_VECTOR_CACHE: usize = 128 * 1024 * 1024;

/// How close two vectors are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Similarity {
    /// The angle between them: vectors are normalized; the score is the
    /// cosine, from -1 to 1.
    #[default]
    Cosine,
    /// Their dot product, unnormalized: the score is the product.
    Dot,
    /// Their straight-line distance `d`: the score is `1 / (1 + d)`.
    Euclidean,
}

impl Similarity {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "cosine" => Similarity::Cosine,
            "dot" | "dot_product" | "inner" => Similarity::Dot,
            "euclidean" | "l2" => Similarity::Euclidean,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Similarity::Cosine => "cosine",
            Similarity::Dot => "dot",
            Similarity::Euclidean => "euclidean",
        }
    }

    /// Smaller is closer.
    fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            Similarity::Cosine => 1.0 - dot(a, b),
            Similarity::Dot => -dot(a, b),
            Similarity::Euclidean => squared_l2(a, b),
        }
    }

    /// A distance as the score a search reports (larger is closer).
    fn score(self, distance: f32) -> f32 {
        match self {
            Similarity::Cosine => 1.0 - distance,
            Similarity::Dot => -distance,
            Similarity::Euclidean => 1.0 / (1.0 + distance.max(0.0).sqrt()),
        }
    }

    /// The vector as stored: normalized for cosine.
    fn prepare(self, vector: &[f32]) -> Result<Vec<f32>> {
        match self {
            Similarity::Cosine => normalize(vector),
            _ if vector.iter().any(|value| !value.is_finite()) => {
                Err(Error::Invalid("vector values must be finite".into()))
            }
            _ => Ok(vector.to_vec()),
        }
    }
}

/// The parameters of a space, fixed when it is created.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Params {
    pub similarity: Similarity,
    pub m: usize,
    pub ef_construction: usize,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            similarity: Similarity::Cosine,
            m: DEFAULT_M,
            ef_construction: DEFAULT_EF_CONSTRUCTION,
        }
    }
}

fn default_m() -> usize {
    DEFAULT_M
}

fn default_ef_construction() -> usize {
    DEFAULT_EF_CONSTRUCTION
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct VectorMeta {
    dimension: usize,
    count: u64,
    entry: Option<NodeId>,
    #[serde(default)]
    similarity: Similarity,
    #[serde(default = "default_m")]
    m: usize,
    #[serde(default = "default_ef_construction")]
    ef_construction: usize,
    /// Vectors removed or replaced since the graph was built (or last
    /// compacted): each leaves its neighbors repaired, not rebuilt.
    #[serde(default)]
    removed: u64,
    /// Link lists those removals rewrote.
    #[serde(default)]
    repaired: u64,
}

impl VectorMeta {
    pub(crate) fn new(dimension: usize, params: Params) -> Self {
        Self {
            dimension,
            count: 0,
            entry: None,
            similarity: params.similarity,
            m: params.m,
            ef_construction: params.ef_construction,
            removed: 0,
            repaired: 0,
        }
    }

    pub(crate) fn params(&self) -> Params {
        Params {
            similarity: self.similarity,
            m: self.m,
            ef_construction: self.ef_construction,
        }
    }

    pub(crate) fn removed(&self) -> u64 {
        self.removed
    }

    pub(crate) fn repaired(&self) -> u64 {
        self.repaired
    }

    pub(crate) fn dimension(&self) -> usize {
        self.dimension
    }

    pub(crate) fn count(&self) -> u64 {
        self.count
    }
}

fn level_of(node: NodeId, m: usize) -> u8 {
    let bits = (node.as_u128() as u64) & ((1 << 52) - 1);
    let uniform = (bits as f64 + 1.0) / (1u64 << 52) as f64;
    let level = (-uniform.ln() / (m.max(2) as f64).ln()).floor();
    (level as u8).min(MAX_LEVEL)
}

fn max_links(layer: u8, m: usize) -> usize {
    if layer == 0 { 2 * m } else { m }
}

fn normalize(vector: &[f32]) -> Result<Vec<f32>> {
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(Error::Invalid("vector values must be finite".into()));
    }
    let norm = dot(vector, vector).sqrt();
    if norm == 0.0 {
        return Err(Error::Invalid("vector must not be zero".into()));
    }
    // Already normalized (such as an exported vector): keep its exact bits.
    if (norm - 1.0).abs() <= 1e-6 {
        return Ok(vector.to_vec());
    }
    Ok(vector.iter().map(|value| value / norm).collect())
}

/// Dot product with eight independent sums, so that the compiler can use
/// SIMD instructions. Both slices have the same length.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let (a_chunks, a_rest) = a.as_chunks::<8>();
    let (b_chunks, b_rest) = b.as_chunks::<8>();
    let mut sums = [0.0f32; 8];
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for lane in 0..8 {
            sums[lane] += x[lane] * y[lane];
        }
    }
    let rest: f32 = a_rest.iter().zip(b_rest).map(|(x, y)| x * y).sum();
    sums.iter().sum::<f32>() + rest
}

/// The squared straight-line distance, with eight independent sums.
fn squared_l2(a: &[f32], b: &[f32]) -> f32 {
    let (a_chunks, a_rest) = a.as_chunks::<8>();
    let (b_chunks, b_rest) = b.as_chunks::<8>();
    let mut sums = [0.0f32; 8];
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for lane in 0..8 {
            let d = x[lane] - y[lane];
            sums[lane] += d * d;
        }
    }
    let rest: f32 = a_rest
        .iter()
        .zip(b_rest)
        .map(|(x, y)| (x - y) * (x - y))
        .sum();
    sums.iter().sum::<f32>() + rest
}

/// [`squared_l2`] against a vector stored as little-endian bytes.
fn squared_l2_bytes(query: &[f32], bytes: &[u8]) -> f32 {
    let (values, _) = bytes.as_chunks::<4>();
    query
        .iter()
        .zip(values)
        .map(|(x, value)| {
            let d = x - f32::from_le_bytes(*value);
            d * d
        })
        .sum()
}

/// [`squared_l2`] against a quantized vector.
fn squared_l2_q8(query: &[f32], bytes: &[u8]) -> f32 {
    let Some((scale, values)) = bytes.split_first_chunk::<4>() else {
        return f32::INFINITY;
    };
    let scale = f32::from_le_bytes(*scale) / 127.0;
    query
        .iter()
        .zip(values)
        .map(|(x, q)| {
            let d = x - f32::from(*q as i8) * scale;
            d * d
        })
        .sum()
}

/// The distance from `query` to a stored vector, from its bytes.
fn distance_bytes(similarity: Similarity, query: &[f32], bytes: &[u8], quantized: bool) -> f32 {
    match (similarity, quantized) {
        (Similarity::Cosine, false) => 1.0 - dot_bytes(query, bytes),
        (Similarity::Cosine, true) => 1.0 - dot_q8(query, bytes),
        (Similarity::Dot, false) => -dot_bytes(query, bytes),
        (Similarity::Dot, true) => -dot_q8(query, bytes),
        (Similarity::Euclidean, false) => squared_l2_bytes(query, bytes),
        (Similarity::Euclidean, true) => squared_l2_q8(query, bytes),
    }
}

/// [`dot`] against a vector stored as little-endian bytes, without decoding
/// it first. The sums run in the same order, so the result is identical.
fn dot_bytes(query: &[f32], bytes: &[u8]) -> f32 {
    let (query_chunks, query_rest) = query.as_chunks::<8>();
    let (byte_chunks, _) = bytes.as_chunks::<32>();
    let mut sums = [0.0f32; 8];
    for (x, chunk) in query_chunks.iter().zip(byte_chunks) {
        let (values, _) = chunk.as_chunks::<4>();
        for lane in 0..8 {
            sums[lane] += x[lane] * f32::from_le_bytes(values[lane]);
        }
    }
    let (rest_bytes, _) = bytes[query_chunks.len() * 32..].as_chunks::<4>();
    let rest: f32 = query_rest
        .iter()
        .zip(rest_bytes)
        .map(|(x, value)| x * f32::from_le_bytes(*value))
        .sum();
    sums.iter().sum::<f32>() + rest
}

/// A vector quantized: its largest absolute value, then each
/// value scaled so that one becomes 127, rounded to a signed byte.
pub(crate) fn quantize(vector: &[f32]) -> Vec<u8> {
    let scale = vector
        .iter()
        .fold(0.0f32, |most, value| most.max(value.abs()));
    let mut out = Vec::with_capacity(4 + vector.len());
    out.extend_from_slice(&scale.to_le_bytes());
    let factor = if scale > 0.0 { 127.0 / scale } else { 0.0 };
    out.extend(
        vector
            .iter()
            .map(|value| (value * factor).round().clamp(-127.0, 127.0) as i8 as u8),
    );
    out
}

/// [`dot`] of `query` with a quantized vector, from its bytes.
fn dot_q8(query: &[f32], bytes: &[u8]) -> f32 {
    let Some((scale, values)) = bytes.split_first_chunk::<4>() else {
        return 0.0;
    };
    let scale = f32::from_le_bytes(*scale) / 127.0;
    let (query_chunks, query_rest) = query.as_chunks::<8>();
    let (value_chunks, value_rest) = values.as_chunks::<8>();
    let mut sums = [0.0f32; 8];
    for (x, q) in query_chunks.iter().zip(value_chunks) {
        for lane in 0..8 {
            sums[lane] += x[lane] * f32::from(q[lane] as i8);
        }
    }
    let rest: f32 = query_rest
        .iter()
        .zip(value_rest)
        .map(|(x, q)| x * f32::from(*q as i8))
        .sum();
    (sums.iter().sum::<f32>() + rest) * scale
}

fn vector_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn bytes_vector(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

fn links_bytes(links: &[u128]) -> Vec<u8> {
    links.iter().flat_map(|id| id.to_le_bytes()).collect()
}

fn bytes_links(bytes: &[u8]) -> Vec<u128> {
    let (chunks, _) = bytes.as_chunks::<16>();
    chunks
        .iter()
        .map(|chunk| u128::from_le_bytes(*chunk))
        .collect()
}

pub(crate) fn read_meta(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    ns: &str,
) -> Result<Option<VectorMeta>> {
    match table.get(ns).map_err(storage)? {
        Some(guard) => decode(guard.value()).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn embedding(
    vectors: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    node: NodeId,
) -> Result<Option<Vec<f32>>> {
    Ok(vectors
        .get((ns, node.as_u128()))
        .map_err(storage)?
        .map(|guard| bytes_vector(guard.value())))
}

/// Decoded vectors (`None`: the node has none) and link lists of one
/// namespace's graph.
struct NamespaceCache {
    vectors: HashMap<u128, Option<Arc<[f32]>>>,
    links: HashMap<(u128, u8), Arc<[u128]>>,
    /// The bytes it may take, roughly.
    budget: usize,
    /// The bytes of one cached vector, entry included (from the dimension).
    vector_bytes: usize,
}

impl NamespaceCache {
    fn new(budget: usize) -> Self {
        Self {
            vectors: HashMap::new(),
            links: HashMap::new(),
            budget,
            vector_bytes: 0,
        }
    }

    /// The bytes it takes, estimated: a map entry, a vector of the
    /// namespace's dimension, and link lists of up to `M0` neighbors.
    fn bytes(&self) -> usize {
        const ENTRY: usize = 64;
        self.vectors.len() * (ENTRY + self.vector_bytes)
            + self.links.len() * (ENTRY + 2 * DEFAULT_M * 16)
    }

    fn trim(&mut self, dimension: usize) {
        self.vector_bytes = dimension * 4;
        if self.bytes() > self.budget {
            self.vectors.clear();
            self.links.clear();
        }
    }
}

/// The vector-graph cache of a write transaction, per namespace. It lives
/// as long as the transaction, so it never outlives the data it copies.
pub(crate) struct Cache {
    namespaces: HashMap<String, NamespaceCache>,
    /// The bytes each namespace's cache may take.
    budget: usize,
    /// Vectors stored in this transaction and not yet in their graph, in
    /// the order they were written: `(space, id, vector as stored)`.
    pending: Vec<(String, u128, Arc<[f32]>)>,
    /// Per space, ids whose vector this transaction removed or replaced: a
    /// snapshot still has the old one.
    removed: HashMap<String, HashSet<u128>>,
    /// Spaces whose tables this transaction changed directly (cleared,
    /// rebuilt): a snapshot's graph says nothing about them.
    stale: HashSet<String>,
    /// Spaces already flushed once in this transaction: a snapshot lacks
    /// what that flush linked.
    flushed: HashSet<String>,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new(DEFAULT_VECTOR_CACHE)
    }
}

impl Cache {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            namespaces: HashMap::new(),
            budget,
            pending: Vec::new(),
            removed: HashMap::new(),
            stale: HashSet::new(),
            flushed: HashSet::new(),
        }
    }

    fn namespace(&mut self, ns: &str) -> &mut NamespaceCache {
        if !self.namespaces.contains_key(ns) {
            self.namespaces
                .insert(ns.to_owned(), NamespaceCache::new(self.budget));
        }
        self.namespaces.get_mut(ns).expect("inserted above")
    }

    /// Drops what is cached for `ns`, after its tables were changed
    /// directly: the vectors waiting to join its graph went with them.
    pub(crate) fn forget(&mut self, ns: &str) {
        self.namespaces.remove(ns);
        self.pending.retain(|(space, _, _)| space != ns);
        self.stale.insert(ns.to_owned());
    }
}

/// A candidate ordered by distance (smaller is closer).
#[derive(Clone, Copy, Debug)]
struct Scored(f32, u128);

impl PartialEq for Scored {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Scored {}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

/// Read access to one space's HNSW graph.
struct Space<'a, V, L> {
    ns: &'a str,
    similarity: Similarity,
    m: usize,
    vectors: &'a V,
    links: &'a L,
    /// `None` for searches: they visit each vector once, so decoding and
    /// keeping vectors would only cost.
    cache: Option<&'a mut NamespaceCache>,
    /// `vectors` holds quantized vectors (searches).
    quantized: bool,
}

impl<V, L> Space<'_, V, L>
where
    V: ReadableTable<RecordKey, &'static [u8]>,
    L: ReadableTable<LinkKey, &'static [u8]>,
{
    fn vector(&mut self, id: u128) -> Result<Option<Arc<[f32]>>> {
        if let Some(cached) = self.cache.as_ref().and_then(|cache| cache.vectors.get(&id)) {
            return Ok(cached.clone());
        }
        let vector: Option<Arc<[f32]>> = self
            .vectors
            .get((self.ns, id))
            .map_err(storage)?
            .map(|guard| bytes_vector(guard.value()).into());
        if let Some(cache) = self.cache.as_mut() {
            cache.vectors.insert(id, vector.clone());
        }
        Ok(vector)
    }

    /// `None` when the node has no vector: links may point to removed nodes.
    fn distance(&mut self, query: &[f32], id: u128) -> Result<Option<f32>> {
        let similarity = self.similarity;
        if let Some(cached) = self.cache.as_ref().and_then(|cache| cache.vectors.get(&id)) {
            return Ok(cached
                .as_ref()
                .map(|vector| similarity.distance(query, vector)));
        }
        Ok(self
            .vectors
            .get((self.ns, id))
            .map_err(storage)?
            .map(|guard| distance_bytes(similarity, query, guard.value(), self.quantized)))
    }

    fn links(&mut self, id: u128, layer: u8) -> Result<Arc<[u128]>> {
        if let Some(cached) = self
            .cache
            .as_ref()
            .and_then(|cache| cache.links.get(&(id, layer)))
        {
            return Ok(cached.clone());
        }
        let links: Arc<[u128]> = self
            .links
            .get((self.ns, id, layer))
            .map_err(storage)?
            .map(|guard| bytes_links(guard.value()).into())
            .unwrap_or_else(|| Arc::from([]));
        if let Some(cache) = self.cache.as_mut() {
            cache.links.insert((id, layer), links.clone());
        }
        Ok(links)
    }

    /// The `ef` nodes closest to `query` on `layer`, closest first.
    fn search_layer(
        &mut self,
        query: &[f32],
        entries: &[Scored],
        ef: usize,
        layer: u8,
    ) -> Result<Vec<Scored>> {
        self.search_layer_with(query, entries, ef, layer, &mut |_| Ok(true))
    }

    /// [`Space::search_layer`] keeping only the nodes `keep` accepts. The
    /// others are still walked through, so the walk reaches past them: it
    /// goes on while fewer than `ef` are kept or a candidate is closer than
    /// the farthest kept one.
    fn search_layer_with(
        &mut self,
        query: &[f32],
        entries: &[Scored],
        ef: usize,
        layer: u8,
        keep: &mut dyn FnMut(u128) -> Result<bool>,
    ) -> Result<Vec<Scored>> {
        let mut visited: HashSet<u128> = entries.iter().map(|entry| entry.1).collect();
        let mut candidates: BinaryHeap<Reverse<Scored>> =
            entries.iter().copied().map(Reverse).collect();
        let mut results: BinaryHeap<Scored> = BinaryHeap::new();
        for entry in entries {
            if keep(entry.1)? {
                results.push(*entry);
            }
        }
        while let Some(Reverse(candidate)) = candidates.pop() {
            if results.len() >= ef && results.peek().is_some_and(|worst| candidate.0 > worst.0) {
                break;
            }
            for &neighbor in self.links(candidate.1, layer)?.iter() {
                if !visited.insert(neighbor) {
                    continue;
                }
                let Some(distance) = self.distance(query, neighbor)? else {
                    continue;
                };
                if results.len() < ef || results.peek().is_some_and(|worst| distance < worst.0) {
                    candidates.push(Reverse(Scored(distance, neighbor)));
                    if keep(neighbor)? {
                        results.push(Scored(distance, neighbor));
                        if results.len() > ef {
                            results.pop();
                        }
                    }
                }
            }
        }
        Ok(results.into_sorted_vec())
    }

    /// Chooses up to `m` neighbors from `candidates` (closest first) with the
    /// paper's heuristic: skip a candidate that is closer to an already chosen
    /// neighbor than to the base, so links spread in different directions.
    /// Skipped candidates fill the remaining slots.
    fn select(&mut self, candidates: &[Scored], m: usize) -> Result<Vec<u128>> {
        let mut chosen: Vec<(u128, Arc<[f32]>)> = Vec::new();
        let mut skipped = Vec::new();
        for &Scored(distance, id) in candidates {
            if chosen.len() >= m {
                break;
            }
            let Some(vector) = self.vector(id)? else {
                continue;
            };
            let similarity = self.similarity;
            if chosen
                .iter()
                .all(|(_, other)| similarity.distance(&vector, other) > distance)
            {
                chosen.push((id, vector));
            } else {
                skipped.push(id);
            }
        }
        let mut out: Vec<u128> = chosen.into_iter().map(|(id, _)| id).collect();
        out.extend(skipped.into_iter().take(m.saturating_sub(out.len())));
        Ok(out)
    }

    /// Reduces `links` of `base` to `m`, keeping the best by the heuristic.
    fn prune(&mut self, base: u128, links: &[u128], m: usize) -> Result<Vec<u128>> {
        let Some(base_vector) = self.vector(base)? else {
            return Ok(Vec::new());
        };
        let mut scored = Vec::new();
        for &id in links {
            if id == base {
                continue;
            }
            if let Some(distance) = self.distance(&base_vector, id)? {
                scored.push(Scored(distance, id));
            }
        }
        scored.sort();
        scored.dedup_by_key(|item| item.1);
        self.select(&scored, m)
    }

    /// Greedy descent from the entry point down to `layer + 1`.
    fn descend(&mut self, query: &[f32], entry: NodeId, layer: u8) -> Result<Vec<Scored>> {
        let entry = entry.as_u128();
        let Some(distance) = self.distance(query, entry)? else {
            return Ok(Vec::new());
        };
        let mut entries = vec![Scored(distance, entry)];
        for current in (layer + 1..=level_of(NodeId::from_u128(entry), self.m)).rev() {
            entries = self.search_layer(query, &entries, 1, current)?;
        }
        Ok(entries)
    }
}

/// Stores `vector` for `node`, replacing any previous vector; it joins the
/// graph when the transaction commits ([`flush`]).
pub(crate) fn insert(
    tx: &WriteTransaction,
    cache: &mut Cache,
    ns: &str,
    node: NodeId,
    vector: &[f32],
) -> Result<()> {
    let params = {
        let table = tx.open_table(VECTOR_META).map_err(storage)?;
        read_meta(&table, ns)?.map_or(Params::default(), |meta| Params {
            similarity: meta.similarity,
            m: meta.m,
            ef_construction: meta.ef_construction,
        })
    };
    let query = params.similarity.prepare(vector)?;
    remove(tx, cache, ns, node)?;

    let mut meta_table = tx.open_table(VECTOR_META).map_err(storage)?;
    let mut meta =
        read_meta(&meta_table, ns)?.unwrap_or_else(|| VectorMeta::new(query.len(), params));
    if meta.count > 0 && meta.dimension != query.len() {
        return Err(Error::Invalid(format!(
            "{} stores vectors of dimension {}, got {}",
            describe_space(ns),
            meta.dimension,
            query.len()
        )));
    }
    meta.dimension = query.len();
    meta.count += 1;
    meta_table
        .insert(ns, encode(&meta)?.as_slice())
        .map_err(storage)?;

    let id = node.as_u128();
    tx.open_table(VECTORS)
        .map_err(storage)?
        .insert((ns, id), vector_bytes(&query).as_slice())
        .map_err(storage)?;
    tx.open_table(VECTORS_Q8)
        .map_err(storage)?
        .insert((ns, id), quantize(&query).as_slice())
        .map_err(storage)?;
    let stored: Arc<[f32]> = Arc::from(query.as_slice());
    let space = cache.namespace(ns);
    space.trim(query.len());
    space.vectors.insert(id, Some(stored.clone()));
    cache.pending.push((ns.to_owned(), id, stored));
    Ok(())
}

/// Links a stored vector into its graph, the way one insert does: searches
/// the graph as it is in `tx` and writes the lists.
fn link(tx: &WriteTransaction, cache: &mut Cache, ns: &str, id: u128, query: &[f32]) -> Result<()> {
    let mut meta_table = tx.open_table(VECTOR_META).map_err(storage)?;
    let mut meta = read_meta(&meta_table, ns)?
        .ok_or_else(|| Error::Storage(format!("vector metadata missing for {ns:?}")))?;
    let (similarity, m) = (meta.similarity, meta.m);
    let node = NodeId::from_u128(id);
    let vectors = tx.open_table(VECTORS).map_err(storage)?;
    let mut links = tx.open_table(HNSW_LINKS).map_err(storage)?;
    let cache = cache.namespace(ns);
    let level = level_of(node, m);

    if let Some(entry) = meta.entry {
        let entry_level = level_of(entry, m);
        let mut entries = Space {
            ns,
            similarity,
            m,
            vectors: &vectors,
            links: &links,
            cache: Some(&mut *cache),
            quantized: false,
        }
        .descend(query, entry, level.min(entry_level))?;

        for layer in (0..=level.min(entry_level)).rev() {
            // Read phase: compute every link list of this layer…
            let mut space = Space {
                ns,
                similarity,
                m,
                vectors: &vectors,
                links: &links,
                cache: Some(&mut *cache),
                quantized: false,
            };
            let mut found = space.search_layer(query, &entries, meta.ef_construction, layer)?;
            // Links left pointing at this id (from before it lost its vector)
            // lead the search back to the node itself: never its own neighbor.
            found.retain(|Scored(_, other)| *other != id);
            let own = space.select(&found, m)?;
            let mut updates = vec![(id, own.clone())];
            for &neighbor in &own {
                let mut list = space.links(neighbor, layer)?.to_vec();
                list.push(id);
                if list.len() > max_links(layer, m) {
                    list = space.prune(neighbor, &list, max_links(layer, m))?;
                }
                updates.push((neighbor, list));
            }
            entries = found;
            // …then write them, to the table and the cache.
            for (node, list) in updates {
                links
                    .insert((ns, node, layer), links_bytes(&list).as_slice())
                    .map_err(storage)?;
                cache.links.insert((node, layer), list.into());
            }
        }
        if level > entry_level {
            meta.entry = Some(node);
        }
    } else {
        meta.entry = Some(node);
    }
    meta_table
        .insert(ns, encode(&meta)?.as_slice())
        .map_err(storage)?;
    Ok(())
}

/// Below this many new vectors in a space, a flush links them one by one:
/// planning would not pay for its threads.
const PLAN_MIN: usize = 2;
/// Above this many, too: the other new vectors are candidate neighbors of
/// each, compared one by one.
const PLAN_MAX: usize = 4_096;

/// Links the vectors written in this transaction into their graphs. Each
/// space's new vectors are planned on a read snapshot of `database` (what
/// this transaction started from) when there are several, else linked one
/// by one.
pub(crate) fn flush(tx: &WriteTransaction, cache: &mut Cache, database: &Database) -> Result<()> {
    if cache.pending.is_empty() {
        return Ok(());
    }
    let pending = std::mem::take(&mut cache.pending);
    let mut spaces: Vec<(String, Vec<NewVector>)> = Vec::new();
    for (space, id, vector) in pending {
        match spaces.iter_mut().find(|(name, _)| *name == space) {
            Some((_, items)) => items.push((id, vector)),
            None => spaces.push((space, vec![(id, vector)])),
        }
    }
    for (space, items) in spaces {
        // Only vectors still stored: a later write may have removed them.
        let items: Vec<(u128, Arc<[f32]>)> = {
            let vectors = tx.open_table(VECTORS).map_err(storage)?;
            let mut kept = Vec::with_capacity(items.len());
            for (id, vector) in items {
                if vectors
                    .get((space.as_str(), id))
                    .map_err(storage)?
                    .is_some()
                {
                    kept.push((id, vector));
                }
            }
            kept
        };
        let planned = (PLAN_MIN..=PLAN_MAX).contains(&items.len())
            && !cache.stale.contains(&space)
            && !cache.flushed.contains(&space);
        if planned {
            plan_and_apply(tx, cache, database, &space, &items)?;
        } else {
            for (id, vector) in &items {
                link(tx, cache, &space, *id, vector)?;
            }
        }
        cache.flushed.insert(space);
    }
    Ok(())
}

/// A vector waiting to join its graph: its node, and the vector as stored.
type NewVector = (u128, Arc<[f32]>);

/// What one new vector's insert will write, worked out on a snapshot.
struct Plan {
    id: u128,
    level: u8,
    /// Per layer, its own neighbors.
    own: Vec<Vec<u128>>,
    /// Per layer, each neighbor that gains it: the neighbor's list in the
    /// snapshot (`None` for another new vector) and its list with this one.
    gains: Vec<Vec<Gain>>,
}

type Gain = (u128, Option<Arc<[u128]>>, Vec<u128>);

/// The heuristic of [`Space::select`], over vectors from `vector`.
fn select_from(
    candidates: &[Scored],
    m: usize,
    similarity: Similarity,
    vector: &mut dyn FnMut(u128) -> Result<Option<Arc<[f32]>>>,
) -> Result<Vec<u128>> {
    let mut chosen: Vec<(u128, Arc<[f32]>)> = Vec::new();
    let mut skipped = Vec::new();
    for &Scored(distance, id) in candidates {
        if chosen.len() >= m {
            break;
        }
        let Some(found) = vector(id)? else {
            continue;
        };
        if chosen
            .iter()
            .all(|(_, other)| similarity.distance(&found, other) > distance)
        {
            chosen.push((id, found));
        } else {
            skipped.push(id);
        }
    }
    let mut out: Vec<u128> = chosen.into_iter().map(|(id, _)| id).collect();
    out.extend(skipped.into_iter().take(m.saturating_sub(out.len())));
    Ok(out)
}

/// [`Space::prune`] over vectors from `vector`.
fn prune_from(
    base: u128,
    links: &[u128],
    m: usize,
    similarity: Similarity,
    vector: &mut dyn FnMut(u128) -> Result<Option<Arc<[f32]>>>,
) -> Result<Vec<u128>> {
    let Some(base_vector) = vector(base)? else {
        return Ok(Vec::new());
    };
    let mut scored = Vec::new();
    for &id in links {
        if id == base {
            continue;
        }
        if let Some(other) = vector(id)? {
            scored.push(Scored(similarity.distance(&base_vector, &other), id));
        }
    }
    scored.sort();
    scored.dedup_by_key(|item| item.1);
    select_from(&scored, m, similarity, vector)
}

/// Plans each new vector of `space` on a snapshot, on several threads, then
/// writes the plans in order.
fn plan_and_apply(
    tx: &WriteTransaction,
    cache: &mut Cache,
    database: &Database,
    space: &str,
    items: &[(u128, Arc<[f32]>)],
) -> Result<()> {
    let meta = {
        let table = tx.open_table(VECTOR_META).map_err(storage)?;
        read_meta(&table, space)?
            .ok_or_else(|| Error::Storage(format!("vector metadata missing for {space:?}")))?
    };
    let (similarity, m, ef) = (meta.similarity, meta.m, meta.ef_construction);
    let removed: HashSet<u128> = cache.removed.get(space).cloned().unwrap_or_default();
    let new: HashMap<u128, Arc<[f32]>> = items.iter().cloned().collect();
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(items.len())
        .max(1);
    let budget = cache.budget / threads;
    let context = Planning {
        items,
        space,
        similarity,
        m,
        ef,
        removed: &removed,
        new: &new,
    };

    // Each thread plans every `threads`-th vector on its own snapshot: all
    // see the same commit, as this transaction holds the only writer.
    let planned: Vec<Result<Vec<(usize, Plan)>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|first| {
                let context = &context;
                scope.spawn(move || -> Result<Vec<(usize, Plan)>> {
                    let read = database.begin_read().map_err(storage)?;
                    let vectors = read.open_table(VECTORS).map_err(storage)?;
                    let links = read.open_table(HNSW_LINKS).map_err(storage)?;
                    let entry = {
                        let table = read.open_table(VECTOR_META).map_err(storage)?;
                        read_meta(&table, context.space)?.and_then(|meta| meta.entry)
                    };
                    let mut local = NamespaceCache::new(budget);
                    let mut out = Vec::new();
                    for at in (first..context.items.len()).step_by(threads) {
                        out.push((at, context.plan(at, entry, &vectors, &links, &mut local)?));
                    }
                    Ok(out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(Error::Storage("planning a vector panicked".into())))
            })
            .collect()
    });
    let mut plans: Vec<Option<Plan>> = (0..items.len()).map(|_| None).collect();
    for thread in planned {
        for (at, plan) in thread? {
            plans[at] = Some(plan);
        }
    }

    // The writer's part: the lists, checked against what changed since.
    let vectors = tx.open_table(VECTORS).map_err(storage)?;
    let mut links = tx.open_table(HNSW_LINKS).map_err(storage)?;
    let mut meta_table = tx.open_table(VECTOR_META).map_err(storage)?;
    let ns_cache = cache.namespace(space);
    for plan in plans.into_iter().flatten() {
        let id = plan.id;
        for layer in 0..=plan.level {
            let max = max_links(layer, m);
            let mut writes: Vec<(u128, Vec<u128>)> = Vec::new();
            {
                let mut live = Space {
                    ns: space,
                    similarity,
                    m,
                    vectors: &vectors,
                    links: &links,
                    cache: Some(&mut *ns_cache),
                    quantized: false,
                };
                // Its own list, with what earlier new vectors already linked
                // to it.
                let current = live.links(id, layer)?;
                let mut own: Vec<u128> = Vec::new();
                for &other in &plan.own[layer as usize] {
                    if live.vector(other)?.is_some() {
                        own.push(other);
                    }
                }
                if !current.is_empty() {
                    for &other in current.iter() {
                        if !own.contains(&other) {
                            own.push(other);
                        }
                    }
                    if own.len() > max {
                        own = live.prune(id, &own, max)?;
                    }
                }
                writes.push((id, own));
                // Each neighbor that gains it: the planned list when the
                // neighbor's list is still the snapshot's, else worked out
                // again from its list as it is now.
                for (neighbor, snapshot, planned) in &plan.gains[layer as usize] {
                    if live.vector(*neighbor)?.is_none() {
                        continue;
                    }
                    let current = live.links(*neighbor, layer)?;
                    let list = match snapshot {
                        Some(snapshot) if *snapshot == current => planned.clone(),
                        _ => {
                            let mut list = current.to_vec();
                            if !list.contains(&id) {
                                list.push(id);
                            }
                            if list.len() > max {
                                list = live.prune(*neighbor, &list, max)?;
                            }
                            list
                        }
                    };
                    writes.push((*neighbor, list));
                }
            }
            for (node, list) in writes {
                links
                    .insert((space, node, layer), links_bytes(&list).as_slice())
                    .map_err(storage)?;
                ns_cache.links.insert((node, layer), list.into());
            }
        }
        let mut meta = read_meta(&meta_table, space)?
            .ok_or_else(|| Error::Storage(format!("vector metadata missing for {space:?}")))?;
        if meta
            .entry
            .is_none_or(|entry| plan.level > level_of(entry, m))
        {
            meta.entry = Some(NodeId::from_u128(id));
            meta_table
                .insert(space, encode(&meta)?.as_slice())
                .map_err(storage)?;
        }
    }
    Ok(())
}

/// What planning the new vectors of one space shares.
struct Planning<'a> {
    items: &'a [(u128, Arc<[f32]>)],
    space: &'a str,
    similarity: Similarity,
    m: usize,
    ef: usize,
    /// Ids whose vector this transaction removed or replaced.
    removed: &'a HashSet<u128>,
    /// The new vectors, by id.
    new: &'a HashMap<u128, Arc<[f32]>>,
}

impl Planning<'_> {
    /// The plan of `items[at]`: its neighbors among the snapshot's graph
    /// (the vectors this transaction removed or replaced left out) and the
    /// other new vectors, compared one by one, chosen as an insert chooses
    /// them; and the lists of the neighbors that gain it.
    fn plan<V, L>(
        &self,
        at: usize,
        entry: Option<NodeId>,
        vectors: &V,
        links: &L,
        local: &mut NamespaceCache,
    ) -> Result<Plan>
    where
        V: ReadableTable<RecordKey, &'static [u8]>,
        L: ReadableTable<LinkKey, &'static [u8]>,
    {
        let (id, query) = (self.items[at].0, self.items[at].1.clone());
        let (similarity, m) = (self.similarity, self.m);
        let level = level_of(NodeId::from_u128(id), m);
        let mut snapshot = Space {
            ns: self.space,
            similarity,
            m,
            vectors,
            links,
            cache: Some(local),
            quantized: false,
        };
        let mut found: Vec<Vec<Scored>> = vec![Vec::new(); level as usize + 1];
        if let Some(entry) = entry {
            let top = level.min(level_of(entry, m));
            let mut entries = snapshot.descend(&query, entry, top)?;
            for layer in (0..=top).rev() {
                let layer_found = snapshot.search_layer(&query, &entries, self.ef, layer)?;
                found[layer as usize] = layer_found
                    .iter()
                    .copied()
                    .filter(|Scored(_, other)| {
                        *other != id
                            && !self.removed.contains(other)
                            && !self.new.contains_key(other)
                    })
                    .collect();
                entries = layer_found;
            }
        }
        let mut own = Vec::with_capacity(level as usize + 1);
        let mut gains = Vec::with_capacity(level as usize + 1);
        for layer in 0..=level {
            let mut candidates = std::mem::take(&mut found[layer as usize]);
            for (other, other_vector) in self.items {
                if *other != id && level_of(NodeId::from_u128(*other), m) >= layer {
                    candidates.push(Scored(similarity.distance(&query, other_vector), *other));
                }
            }
            candidates.sort();
            let mut seen = HashSet::new();
            candidates.retain(|Scored(_, other)| seen.insert(*other));
            candidates.truncate(self.ef);
            let chosen = select_from(&candidates, m, similarity, &mut |other| {
                self.vector(&mut snapshot, other)
            })?;
            let max = max_links(layer, m);
            let mut layer_gains = Vec::with_capacity(chosen.len());
            for &neighbor in &chosen {
                if self.new.contains_key(&neighbor) {
                    layer_gains.push((neighbor, None, Vec::new()));
                    continue;
                }
                let before = snapshot.links(neighbor, layer)?;
                let mut list = before.to_vec();
                list.push(id);
                if list.len() > max {
                    list = prune_from(neighbor, &list, max, similarity, &mut |other| {
                        self.vector(&mut snapshot, other)
                    })?;
                }
                layer_gains.push((neighbor, Some(before), list));
            }
            own.push(chosen);
            gains.push(layer_gains);
        }
        Ok(Plan {
            id,
            level,
            own,
            gains,
        })
    }

    /// A vector as the transaction has it: a new one, none for one it
    /// removed, else the snapshot's.
    fn vector<V, L>(&self, snapshot: &mut Space<'_, V, L>, id: u128) -> Result<Option<Arc<[f32]>>>
    where
        V: ReadableTable<RecordKey, &'static [u8]>,
        L: ReadableTable<LinkKey, &'static [u8]>,
    {
        if let Some(fresh) = self.new.get(&id) {
            return Ok(Some(fresh.clone()));
        }
        if self.removed.contains(&id) {
            return Ok(None);
        }
        snapshot.vector(id)
    }
}

/// Removes the vector of `node` and reconnects its neighbors. Returns whether
/// there was one.
pub(crate) fn remove(
    tx: &WriteTransaction,
    cache: &mut Cache,
    ns: &str,
    node: NodeId,
) -> Result<bool> {
    let id = node.as_u128();
    let mut vectors = tx.open_table(VECTORS).map_err(storage)?;
    tx.open_table(VECTORS_Q8)
        .map_err(storage)?
        .remove((ns, id))
        .map_err(storage)?;
    if vectors.remove((ns, id)).map_err(storage)?.is_none() {
        return Ok(false);
    }
    // Not linked yet, it leaves the queue; a snapshot keeps the old vector.
    cache
        .pending
        .retain(|(space, other, _)| !(*other == id && space == ns));
    cache.removed.entry(ns.to_owned()).or_default().insert(id);
    let waiting: HashSet<u128> = cache
        .pending
        .iter()
        .filter(|(space, _, _)| space == ns)
        .map(|(_, other, _)| *other)
        .collect();
    let (similarity, m) = {
        let table = tx.open_table(VECTOR_META).map_err(storage)?;
        read_meta(&table, ns)?.map_or((Similarity::Cosine, DEFAULT_M), |meta| {
            (meta.similarity, meta.m)
        })
    };
    let cache = cache.namespace(ns);
    cache.vectors.insert(id, None);
    let mut links = tx.open_table(HNSW_LINKS).map_err(storage)?;
    let mut repaired = 0u64;
    for layer in 0..=level_of(node, m) {
        let own = links
            .remove((ns, id, layer))
            .map_err(storage)?
            .map(|guard| bytes_links(guard.value()))
            .unwrap_or_default();
        cache.links.remove(&(id, layer));
        let mut space = Space {
            ns,
            similarity,
            m,
            vectors: &vectors,
            links: &links,
            cache: Some(&mut *cache),
            quantized: false,
        };
        // Who to repair: the removed node's neighbors, and the nodes two hops
        // away that link to it (links are not symmetric, and a link left
        // pointing at a removed node is a lost slot). Nodes removed earlier
        // (dangling ids) are skipped: they have no vector, and writing a
        // list for them would leave links without a vector.
        let mut repair: Vec<u128> = Vec::new();
        let mut seen: HashSet<u128> = HashSet::from([id]);
        for &neighbor in &own {
            if seen.insert(neighbor) && space.vector(neighbor)?.is_some() {
                repair.push(neighbor);
            }
        }
        let direct = repair.len();
        for at in 0..direct {
            let neighbor = repair[at];
            for &other in space.links(neighbor, layer)?.iter() {
                if seen.insert(other)
                    && space.links(other, layer)?.contains(&id)
                    && space.vector(other)?.is_some()
                {
                    repair.push(other);
                }
            }
        }
        let live_own: Vec<u128> = own
            .iter()
            .copied()
            .filter(|n| repair[..direct].contains(n))
            .collect();
        let mut updates = Vec::new();
        for (at, &neighbor) in repair.iter().enumerate() {
            // Candidates: its links, the removed node's links, and, for the
            // removed node's own neighbors, a search from where they are, so
            // that repairs reach past the neighborhood and do not close it
            // into an island.
            let mut candidates: Vec<u128> = space
                .links(neighbor, layer)?
                .iter()
                .copied()
                .filter(|other| *other != id)
                .collect();
            candidates.extend(live_own.iter().copied().filter(|other| *other != neighbor));
            if at < direct
                && let Some(vector) = space.vector(neighbor)?
            {
                let mut entries = Vec::new();
                for &start in candidates.iter().take(m) {
                    if let Some(distance) = space.distance(&vector, start)? {
                        entries.push(Scored(distance, start));
                    }
                }
                if !entries.is_empty() {
                    let found = space.search_layer(&vector, &entries, REPAIR_EF, layer)?;
                    candidates.extend(found.into_iter().map(|Scored(_, other)| other));
                }
            }
            updates.push((
                neighbor,
                space.prune(neighbor, &candidates, max_links(layer, m))?,
            ));
        }
        // A neighbor that only the removed node linked to would be lost to
        // every search: if neither a repaired list nor its own neighbors
        // link back to it, its closest neighbor does.
        for &orphan in &live_own {
            let repaired = |updates: &[(u128, Vec<u128>)]| {
                updates.iter().any(|(_, list)| list.contains(&orphan))
            };
            let Some(own_list) = updates
                .iter()
                .find(|(node, _)| *node == orphan)
                .map(|(_, list)| list.clone())
            else {
                continue;
            };
            if repaired(&updates) {
                continue;
            }
            let mut linked = false;
            for &other in &own_list {
                if space.links(other, layer)?.contains(&orphan) {
                    linked = true;
                    break;
                }
            }
            if linked {
                continue;
            }
            let Some(&closest) = own_list.first() else {
                continue;
            };
            let mut list: Vec<u128> = match updates.iter().find(|(node, _)| *node == closest) {
                Some((_, list)) => list.clone(),
                None => space.links(closest, layer)?.to_vec(),
            };
            if list.len() >= max_links(layer, m) {
                // Make room by dropping its farthest link.
                list = space.prune(closest, &list, max_links(layer, m) - 1)?;
            }
            list.push(orphan);
            updates.retain(|(node, _)| *node != closest);
            updates.push((closest, list));
        }
        repaired += updates.len() as u64;
        for (neighbor, list) in updates {
            links
                .insert((ns, neighbor, layer), links_bytes(&list).as_slice())
                .map_err(storage)?;
            cache.links.insert((neighbor, layer), list.into());
        }
    }

    let mut meta_table = tx.open_table(VECTOR_META).map_err(storage)?;
    let mut meta = read_meta(&meta_table, ns)?
        .ok_or_else(|| Error::Storage(format!("vector metadata missing for {ns:?}")))?;
    meta.count = meta.count.saturating_sub(1);
    meta.removed += 1;
    meta.repaired += repaired;
    if meta.entry == Some(node) {
        // Vectors waiting to be linked cannot lead a search anywhere yet.
        meta.entry = highest_node(&vectors, ns, m, &waiting)?;
    }
    meta_table
        .insert(ns, encode(&meta)?.as_slice())
        .map_err(storage)?;
    Ok(true)
}

/// The node with the highest level in the namespace (the new entry point).
fn highest_node(
    vectors: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    m: usize,
    waiting: &HashSet<u128>,
) -> Result<Option<NodeId>> {
    let mut best: Option<(u8, NodeId)> = None;
    for item in vectors.range((ns, 0u128)..).map_err(storage)? {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, id) = key.value();
        if key_ns != ns {
            break;
        }
        if waiting.contains(&id) {
            continue;
        }
        let node = NodeId::from_u128(id);
        let level = level_of(node, m);
        if best.is_none_or(|(top, _)| level > top) {
            best = Some((level, node));
        }
    }
    Ok(best.map(|(_, node)| node))
}

/// What names a space in messages: a namespace, or an index of one.
fn describe_space(space: &str) -> String {
    match space.split_once('\0') {
        Some((ns, index)) => format!("vector index {index:?} of {ns:?}"),
        None => format!("namespace {space:?}"),
    }
}

/// The `limit` nodes most similar to `query`, best first. `ef` is the size of
/// the candidate list on layer 0: higher is slower and more accurate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search(
    vectors: &impl ReadableTable<RecordKey, &'static [u8]>,
    quantized: &impl ReadableTable<RecordKey, &'static [u8]>,
    links: &impl ReadableTable<LinkKey, &'static [u8]>,
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    ns: &str,
    query: &[f32],
    limit: usize,
    ef: usize,
) -> Result<Vec<Hit>> {
    search_with(
        vectors,
        quantized,
        links,
        meta,
        ns,
        query,
        limit,
        ef,
        &mut |_| Ok(true),
    )
}

/// [`search`] keeping only the nodes `keep` accepts, decided during the
/// walk.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_with(
    vectors: &impl ReadableTable<RecordKey, &'static [u8]>,
    quantized: &impl ReadableTable<RecordKey, &'static [u8]>,
    links: &impl ReadableTable<LinkKey, &'static [u8]>,
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    ns: &str,
    query: &[f32],
    limit: usize,
    ef: usize,
    keep: &mut dyn FnMut(u128) -> Result<bool>,
) -> Result<Vec<Hit>> {
    let Some(meta) = read_meta(meta, ns)? else {
        return Ok(Vec::new());
    };
    let Some(entry) = meta.entry else {
        return Ok(Vec::new());
    };
    if query.len() != meta.dimension {
        return Err(Error::Invalid(format!(
            "{} stores vectors of dimension {}, got {}",
            describe_space(ns),
            meta.dimension,
            query.len()
        )));
    }
    let similarity = meta.similarity;
    let query = similarity.prepare(query)?;
    // Walk the graph with the quantized vectors…
    let mut space = Space {
        ns,
        similarity,
        m: meta.m,
        vectors: quantized,
        links,
        cache: None,
        quantized: true,
    };
    let entries = space.descend(&query, entry, 0)?;
    let found = space.search_layer_with(&query, &entries, ef.max(limit), 0, keep)?;
    // …then rescore the candidates with the exact ones.
    let mut exact = Vec::with_capacity(found.len());
    for Scored(_, id) in found {
        if let Some(guard) = vectors.get((ns, id)).map_err(storage)? {
            exact.push(Scored(
                distance_bytes(similarity, &query, guard.value(), false),
                id,
            ));
        }
    }
    exact.sort();
    Ok(exact
        .into_iter()
        .take(limit)
        .map(|Scored(distance, id)| Hit {
            node: NodeId::from_u128(id),
            score: similarity.score(distance),
        })
        .collect())
}

/// The `limit` of `ids` closest to `query`, scored exactly from their
/// stored vectors (ids without one are left out): what a filter that few
/// nodes pass asks for, cheaper than a walk.
pub(crate) fn rank_ids(
    vectors: &impl ReadableTable<RecordKey, &'static [u8]>,
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    ns: &str,
    query: &[f32],
    ids: &[u128],
    limit: usize,
) -> Result<Vec<Hit>> {
    let Some(meta) = read_meta(meta, ns)? else {
        return Ok(Vec::new());
    };
    if query.len() != meta.dimension {
        return Err(Error::Invalid(format!(
            "{} stores vectors of dimension {}, got {}",
            describe_space(ns),
            meta.dimension,
            query.len()
        )));
    }
    let similarity = meta.similarity;
    let query = similarity.prepare(query)?;
    let mut best: BinaryHeap<Scored> = BinaryHeap::new();
    for &id in ids {
        let Some(guard) = vectors.get((ns, id)).map_err(storage)? else {
            continue;
        };
        best.push(Scored(
            distance_bytes(similarity, &query, guard.value(), false),
            id,
        ));
        if best.len() > limit {
            best.pop();
        }
    }
    Ok(best
        .into_sorted_vec()
        .into_iter()
        .map(|Scored(distance, id)| Hit {
            node: NodeId::from_u128(id),
            score: similarity.score(distance),
        })
        .collect())
}

/// Checks the vector index of a namespace: vectors, metadata and links.
/// Returns the number of vectors, of dangling links, and of vectors that
/// cannot be reached from the entry point on layer 0 (searches can never
/// return them; a few appear as the graph changes, and compacting the
/// namespace reconnects them).
pub(crate) fn check(
    vectors: &impl ReadableTable<RecordKey, &'static [u8]>,
    links: &impl ReadableTable<LinkKey, &'static [u8]>,
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    ns: &str,
    nodes: &HashSet<u128>,
    problems: &mut Vec<String>,
) -> Result<(u64, u64, u64)> {
    let meta = read_meta(meta, ns)?;
    let mut ids = HashSet::new();
    for item in vectors.range((ns, 0u128)..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let (key_ns, id) = key.value();
        if key_ns != ns {
            break;
        }
        ids.insert(id);
        let node = NodeId::from_u128(id);
        if !nodes.contains(&id) {
            problems.push(format!("vector of missing node {node}"));
        }
        let vector = bytes_vector(value.value());
        if let Some(meta) = &meta
            && vector.len() != meta.dimension
        {
            problems.push(format!(
                "vector of node {node} has dimension {}, expected {}",
                vector.len(),
                meta.dimension
            ));
        }
        let norm = dot(&vector, &vector).sqrt();
        let normalized = meta
            .as_ref()
            .is_none_or(|meta| meta.similarity == Similarity::Cosine);
        if normalized && (norm - 1.0).abs() > 1e-3 {
            problems.push(format!(
                "vector of node {node} is not normalized (norm {norm})"
            ));
        }
    }
    let count = ids.len() as u64;
    match &meta {
        None if count > 0 => problems.push(format!("{count} vectors but no vector metadata")),
        None => {}
        Some(meta) => {
            if meta.count != count {
                problems.push(format!(
                    "vector metadata counts {}, found {count}",
                    meta.count
                ));
            }
            match meta.entry {
                None if count > 0 => {
                    problems.push("vectors exist but there is no entry point".into())
                }
                Some(entry) if !ids.contains(&entry.as_u128()) => {
                    problems.push(format!("entry point {entry} has no vector"))
                }
                _ => {}
            }
        }
    }

    let mut dangling = 0u64;
    for item in links.range((ns, 0u128, 0u8)..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        let (key_ns, id, layer) = key.value();
        if key_ns != ns {
            break;
        }
        let node = NodeId::from_u128(id);
        if !ids.contains(&id) {
            problems.push(format!(
                "links of node {node} without a vector, layer {layer}"
            ));
        }
        if layer > level_of(node, meta.as_ref().map_or(DEFAULT_M, |meta| meta.m)) {
            problems.push(format!(
                "links of node {node} on layer {layer}, above its level"
            ));
        }
        dangling += bytes_links(value.value())
            .iter()
            .filter(|neighbor| !ids.contains(neighbor))
            .count() as u64;
    }

    let mut unreachable = 0;
    if let Some(entry) = meta.as_ref().and_then(|meta| meta.entry)
        && ids.contains(&entry.as_u128())
    {
        let mut seen = HashSet::from([entry.as_u128()]);
        let mut stack = vec![entry.as_u128()];
        while let Some(id) = stack.pop() {
            let neighbors = links
                .get((ns, id, 0u8))
                .map_err(storage)?
                .map(|guard| bytes_links(guard.value()))
                .unwrap_or_default();
            for neighbor in neighbors {
                if ids.contains(&neighbor) && seen.insert(neighbor) {
                    stack.push(neighbor);
                }
            }
        }
        unreachable = count.saturating_sub(seen.len() as u64);
    }
    Ok((count, dangling, unreachable))
}

/// Estimates recall@10 of searches with `ef`: the vectors of `sample` nodes
/// spread over the namespace are the queries, and exact answers come from
/// one pass over every vector. `None` when there are no vectors.
pub(crate) fn estimate_recall(
    vectors: &impl ReadableTable<RecordKey, &'static [u8]>,
    quantized: &impl ReadableTable<RecordKey, &'static [u8]>,
    links: &impl ReadableTable<LinkKey, &'static [u8]>,
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    ns: &str,
    sample: usize,
    ef: usize,
) -> Result<Option<f64>> {
    let Some(info) = read_meta(meta, ns)? else {
        return Ok(None);
    };
    if info.count == 0 || sample == 0 {
        return Ok(None);
    }
    let step = (info.count as usize / sample).max(1);
    let mut queries: Vec<(u128, Vec<f32>)> = Vec::new();
    for (at, item) in vectors.range((ns, 0u128)..).map_err(storage)?.enumerate() {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns || queries.len() == sample {
            break;
        }
        if at % step == 0 {
            queries.push((key.value().1, bytes_vector(value.value())));
        }
    }
    // The exact 10 nearest of each query, in one pass (the query itself
    // excluded, as searches return it first).
    let mut best: Vec<BinaryHeap<Reverse<Scored>>> = vec![BinaryHeap::new(); queries.len()];
    for item in vectors.range((ns, 0u128)..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns {
            break;
        }
        let id = key.value().1;
        for ((query_id, query), heap) in queries.iter().zip(best.iter_mut()) {
            if *query_id == id {
                continue;
            }
            let similarity = dot_bytes(query, value.value());
            heap.push(Reverse(Scored(similarity, id)));
            if heap.len() > 10 {
                heap.pop();
            }
        }
    }
    let mut found = 0usize;
    let mut total = 0usize;
    for ((query_id, query), heap) in queries.iter().zip(best) {
        let truth: HashSet<u128> = heap.into_iter().map(|Reverse(Scored(_, id))| id).collect();
        let hits = search(vectors, quantized, links, meta, ns, query, 11, ef)?;
        found += hits
            .iter()
            .filter(|hit| hit.node.as_u128() != *query_id && truth.contains(&hit.node.as_u128()))
            .take(10)
            .count();
        total += truth.len();
    }
    Ok((total > 0).then(|| found as f64 / total as f64))
}

#[cfg(test)]
mod quantization_tests {
    use super::*;

    #[test]
    fn quantized_dot_products_are_close() {
        let mut state = 7u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 2_000) as f32 / 1_000.0 - 1.0
        };
        for dimension in [3, 8, 13, 128, 384] {
            let a = normalize(&(0..dimension).map(|_| next()).collect::<Vec<_>>()).unwrap();
            let b = normalize(&(0..dimension).map(|_| next()).collect::<Vec<_>>()).unwrap();
            let q = quantize(&b);
            assert_eq!(q.len(), 4 + dimension);
            let error = (dot(&a, &b) - dot_q8(&a, &q)).abs();
            // A step of 1/127 of the largest value per dimension, averaging out.
            assert!(error < 0.02, "dimension {dimension}: error {error}");
        }
        // The largest value is exact.
        let q = quantize(&[0.6, -0.8]);
        assert_eq!(q[4] as i8, 95);
        assert_eq!(q[5] as i8, -127);
    }
}
