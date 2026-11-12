//! redb tables and the reads shared by read and write transactions.

use std::ops::Bound;

use grafeno_core::{Edge, EdgeId, Error, NodeId, Result, Timestamp};
use redb::{ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub(crate) type RecordKey = (&'static str, u128);
pub(crate) type AdjKey = (&'static str, u128, &'static str, u128);
pub(crate) type LabelKey = (&'static str, &'static str, u128);
pub(crate) type PostingKey = (&'static str, &'static str, u128);
pub(crate) type LinkKey = (&'static str, u128, u8);

/// `(ns, node_id) -> Node`
pub(crate) const NODES: TableDefinition<RecordKey, &[u8]> = TableDefinition::new("nodes");
/// `(ns, edge_id) -> Edge`
pub(crate) const EDGES: TableDefinition<RecordKey, &[u8]> = TableDefinition::new("edges");
/// What a traversal needs to know about an edge, kept in the adjacency entries
/// so that filtering reads no edge record: `(other end, recorded_at,
/// expired_at, valid_from, valid_to)` in microseconds, with `i64::MIN` and
/// `i64::MAX` for open bounds.
pub(crate) type AdjValue = (u128, i64, i64, i64, i64);

/// How an [`AdjValue`] is stored (layout 13): a byte of flags (which of
/// `expired_at`, `valid_from` and `valid_to` are set), the other end (16
/// bytes), then `recorded_at` and each bound that is set as zigzag varints.
/// A plain fact takes 25 bytes instead of 48.
#[derive(Debug)]
pub(crate) struct Adj;

const EXPIRED: u8 = 1;
const VALID_FROM: u8 = 2;
const VALID_TO: u8 = 4;

fn put_varint(out: &mut Vec<u8>, value: i64) {
    let mut zigzag = ((value << 1) ^ (value >> 63)) as u64;
    loop {
        let byte = (zigzag & 0x7F) as u8;
        zigzag >>= 7;
        if zigzag == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn take_varint(data: &[u8], at: &mut usize) -> i64 {
    let mut zigzag = 0u64;
    let mut shift = 0;
    while let Some(&byte) = data.get(*at) {
        *at += 1;
        zigzag |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    ((zigzag >> 1) as i64) ^ -((zigzag & 1) as i64)
}

pub(crate) fn encode_adj(value: &AdjValue) -> Vec<u8> {
    let (other, recorded_at, expired_at, valid_from, valid_to) = *value;
    let flags = if expired_at != i64::MAX { EXPIRED } else { 0 }
        | if valid_from != i64::MIN {
            VALID_FROM
        } else {
            0
        }
        | if valid_to != i64::MAX { VALID_TO } else { 0 };
    let mut out = Vec::with_capacity(26);
    out.push(flags);
    out.extend_from_slice(&other.to_le_bytes());
    put_varint(&mut out, recorded_at);
    for (flag, value) in [
        (EXPIRED, expired_at),
        (VALID_FROM, valid_from),
        (VALID_TO, valid_to),
    ] {
        if flags & flag != 0 {
            put_varint(&mut out, value);
        }
    }
    out
}

pub(crate) fn decode_adj(data: &[u8]) -> AdjValue {
    let flags = data.first().copied().unwrap_or(0);
    let other = data
        .get(1..17)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u128::from_le_bytes);
    let mut at = 17;
    let recorded_at = take_varint(data, &mut at);
    let mut bound = |flag: u8, open: i64| {
        if flags & flag != 0 {
            take_varint(data, &mut at)
        } else {
            open
        }
    };
    let expired_at = bound(EXPIRED, i64::MAX);
    let valid_from = bound(VALID_FROM, i64::MIN);
    let valid_to = bound(VALID_TO, i64::MAX);
    (other, recorded_at, expired_at, valid_from, valid_to)
}

impl redb::Value for Adj {
    type SelfType<'a> = AdjValue;
    type AsBytes<'a> = Vec<u8>;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> AdjValue
    where
        Self: 'a,
    {
        decode_adj(data)
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a AdjValue) -> Vec<u8>
    where
        Self: 'b,
    {
        encode_adj(value)
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new("grafeno::Adjacency")
    }
}

/// `(ns, from, label, edge_id) -> AdjValue` with `to` as the other end.
pub(crate) const ADJ_OUT: TableDefinition<AdjKey, Adj> = TableDefinition::new("adjacency_out");
/// `(ns, to, label, edge_id) -> AdjValue` with `from` as the other end.
pub(crate) const ADJ_IN: TableDefinition<AdjKey, Adj> = TableDefinition::new("adjacency_in");
/// `(ns, label, node_id) -> ()`
pub(crate) const NODE_LABELS: TableDefinition<LabelKey, ()> = TableDefinition::new("node_labels");

/// `(ns, node_id) -> text`: the indexed text of a node.
pub(crate) const TEXTS: TableDefinition<RecordKey, &str> = TableDefinition::new("texts");
/// `(ns, node_id) -> number of terms` in the indexed text.
pub(crate) const TEXT_LENGTHS: TableDefinition<RecordKey, u32> =
    TableDefinition::new("text_lengths");
/// `(ns, term, node_id) -> (term frequency, terms in the text)`: the length is
/// repeated in every posting so that ranking needs no extra lookup.
pub(crate) const POSTINGS: TableDefinition<PostingKey, (u32, u32)> =
    TableDefinition::new("text_postings");
/// `ns -> (indexed documents, total terms)`
pub(crate) const TEXT_STATS: TableDefinition<&str, (u64, u64)> = TableDefinition::new("text_stats");

/// `(ns, node_id) -> normalized f32 vector` (little endian).
pub(crate) const VECTORS: TableDefinition<RecordKey, &[u8]> = TableDefinition::new("vectors");
/// `(ns, node_id) -> the same vector quantized`: its largest absolute value
/// (f32, little endian), then one signed byte per dimension, that value
/// scaled to 127. Searches walk the graph with these and rescore the best
/// candidates with the exact vectors.
pub(crate) const VECTORS_Q8: TableDefinition<RecordKey, &[u8]> = TableDefinition::new("vectors_q8");
/// `(ns, node_id, layer) -> neighbor ids` (u128, little endian) of the HNSW graph.
pub(crate) const HNSW_LINKS: TableDefinition<LinkKey, &[u8]> = TableDefinition::new("hnsw_links");
/// `ns -> VectorMeta` (JSON).
pub(crate) const VECTOR_META: TableDefinition<&str, &[u8]> = TableDefinition::new("vector_meta");

pub(crate) fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| Error::Serialization(e.to_string()))
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| Error::Serialization(e.to_string()))
}

pub(crate) fn check_namespace(ns: &str) -> Result<()> {
    if ns.is_empty() {
        return Err(Error::Invalid("namespace must not be empty".into()));
    }
    Ok(())
}

pub(crate) fn check_label(label: &str) -> Result<()> {
    if label.is_empty() || label.trim() != label {
        return Err(Error::Invalid(format!("invalid label {label:?}")));
    }
    Ok(())
}

pub(crate) fn read_record<R: DeserializeOwned>(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    id: u128,
) -> Result<Option<R>> {
    match table.get((ns, id)).map_err(storage)? {
        Some(guard) => decode(guard.value()).map(Some),
        None => Ok(None),
    }
}

/// Every record of a namespace, in id (creation) order.
pub(crate) fn scan_records<R: DeserializeOwned>(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
) -> Result<Vec<R>> {
    let mut out = Vec::new();
    for item in table.range((ns, 0u128)..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns {
            break;
        }
        out.push(decode(value.value())?);
    }
    Ok(out)
}

/// An edge seen from one of its ends, as stored in an adjacency table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Adjacent {
    pub(crate) edge: EdgeId,
    pub(crate) other: NodeId,
    pub(crate) recorded_at: i64,
    pub(crate) expired_at: i64,
    pub(crate) valid_from: i64,
    pub(crate) valid_to: i64,
}

/// The adjacency value of `edge` seen from the end opposite to `other`.
pub(crate) fn adjacency_value(edge: &Edge, other: NodeId) -> AdjValue {
    (
        other.as_u128(),
        edge.recorded_at.as_micros(),
        edge.expired_at.map_or(i64::MAX, Timestamp::as_micros),
        edge.valid_from.map_or(i64::MIN, Timestamp::as_micros),
        edge.valid_to.map_or(i64::MAX, Timestamp::as_micros),
    )
}

/// The edges of `node` in an adjacency table. With `label`, only edges with
/// that label.
pub(crate) fn adjacency(
    table: &impl ReadableTable<AdjKey, Adj>,
    ns: &str,
    node: NodeId,
    label: Option<&str>,
) -> Result<Vec<Adjacent>> {
    let node = node.as_u128();
    let mut out = Vec::new();
    for item in table
        .range((ns, node, label.unwrap_or(""), 0u128)..)
        .map_err(storage)?
    {
        let (key, value) = item.map_err(storage)?;
        let (key_ns, key_node, key_label, edge) = key.value();
        if key_ns != ns || key_node != node || label.is_some_and(|label| label != key_label) {
            break;
        }
        let (other, recorded_at, expired_at, valid_from, valid_to) = value.value();
        out.push(Adjacent {
            edge: EdgeId::from_u128(edge),
            other: NodeId::from_u128(other),
            recorded_at,
            expired_at,
            valid_from,
            valid_to,
        });
    }
    Ok(out)
}

/// Visits the edges of `node` in an adjacency table (with `label`, only
/// edges with that label) until `visit` returns false.
pub(crate) fn adjacency_each(
    table: &impl ReadableTable<AdjKey, Adj>,
    ns: &str,
    node: NodeId,
    label: Option<&str>,
    mut visit: impl FnMut(&Adjacent) -> bool,
) -> Result<()> {
    let node = node.as_u128();
    for item in table
        .range((ns, node, label.unwrap_or(""), 0u128)..)
        .map_err(storage)?
    {
        let (key, value) = item.map_err(storage)?;
        let (key_ns, key_node, key_label, edge) = key.value();
        if key_ns != ns || key_node != node || label.is_some_and(|label| label != key_label) {
            break;
        }
        let (other, recorded_at, expired_at, valid_from, valid_to) = value.value();
        let entry = Adjacent {
            edge: EdgeId::from_u128(edge),
            other: NodeId::from_u128(other),
            recorded_at,
            expired_at,
            valid_from,
            valid_to,
        };
        if !visit(&entry) {
            break;
        }
    }
    Ok(())
}

pub(crate) fn nodes_with_label(
    table: &impl ReadableTable<LabelKey, ()>,
    ns: &str,
    label: &str,
) -> Result<Vec<NodeId>> {
    let mut out = Vec::new();
    for item in table.range((ns, label, 0u128)..).map_err(storage)? {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, key_label, node) = key.value();
        if key_ns != ns || key_label != label {
            break;
        }
        out.push(NodeId::from_u128(node));
    }
    Ok(out)
}

/// `key -> value`: database metadata, such as the schema version.
pub(crate) const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
/// `ns -> (nodes, edges)`
pub(crate) const NS_COUNTS: TableDefinition<&str, (u64, u64)> =
    TableDefinition::new("namespace_counts");
/// `(ns, label) -> nodes with that label`
pub(crate) const LABEL_COUNTS: TableDefinition<CountKey, u64> =
    TableDefinition::new("label_counts");
/// `(ns, label) -> edges with that label`
pub(crate) const EDGE_LABEL_COUNTS: TableDefinition<CountKey, u64> =
    TableDefinition::new("edge_label_counts");

pub(crate) type CountKey = (&'static str, &'static str);

/// A key range with explicit bounds.
type Bounds<K> = (Bound<K>, Bound<K>);

/// Up to `limit` records of a namespace with an id greater than `after`, in
/// id order, that pass `keep`.
pub(crate) fn scan_page<R: DeserializeOwned>(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    after: Option<u128>,
    limit: usize,
    mut keep: impl FnMut(&R) -> bool,
) -> Result<Vec<R>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let lower = match after {
        Some(id) => Bound::Excluded((ns, id)),
        None => Bound::Included((ns, 0u128)),
    };
    let bounds: Bounds<(&str, u128)> = (lower, Bound::Unbounded);
    for item in table.range(bounds).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns {
            break;
        }
        let record = decode(value.value())?;
        if keep(&record) {
            out.push(record);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Up to `limit` ids of nodes with `label`, greater than `after`, in id order.
pub(crate) fn label_page(
    table: &impl ReadableTable<LabelKey, ()>,
    ns: &str,
    label: &str,
    after: Option<u128>,
    limit: usize,
) -> Result<Vec<NodeId>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let lower = match after {
        Some(id) => Bound::Excluded((ns, label, id)),
        None => Bound::Included((ns, label, 0u128)),
    };
    let bounds: Bounds<(&str, &str, u128)> = (lower, Bound::Unbounded);
    for item in table.range(bounds).map_err(storage)? {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, key_label, node) = key.value();
        if key_ns != ns || key_label != label {
            break;
        }
        out.push(NodeId::from_u128(node));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// `(ns, normalized name, node_id) -> ()`: the names and aliases of nodes,
/// for finding an entity by name (see `memory::normalize_name`).
pub(crate) const NAMES: TableDefinition<LabelKey, ()> = TableDefinition::new("names");
/// `(ns, node_id) -> (importance, accesses, last access in microseconds)`.
/// Nodes without a row have the default memory state.
pub(crate) const MEMORY: TableDefinition<RecordKey, (f32, u32, i64)> =
    TableDefinition::new("memory");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacency_values_round_trip() {
        let ends = [
            i64::MIN,
            i64::MIN + 1,
            -1,
            0,
            1,
            1_700_000_000_000_000,
            i64::MAX - 1,
            i64::MAX,
        ];
        for recorded in ends {
            for expired in ends {
                for from in ends {
                    for to in [i64::MAX, 0, -5, 1_640_995_200_000_000] {
                        let value = (u128::MAX - 7, recorded, expired, from, to);
                        let bytes = encode_adj(&value);
                        assert_eq!(decode_adj(&bytes), value);
                    }
                }
            }
        }
        // A plain fact: flags, the other end and a recent time.
        let plain = (42u128, 1_700_000_000_000_000, i64::MAX, i64::MIN, i64::MAX);
        assert_eq!(encode_adj(&plain).len(), 25);
    }
}
