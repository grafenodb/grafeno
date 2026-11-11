//! Property indexes: declared per namespace on a `(label, property)` pair,
//! kept in the same transaction as every node write.
//!
//! Entries are keyed by `(ns, index, value, node)`, where `value` is encoded
//! so that byte order is value order: equality, ranges and string prefixes
//! are key ranges. Only booleans, numbers and strings are indexed (lists,
//! maps and `null` are not). Numbers are compared as `f64`, so an index can
//! return a few extra nodes for integers beyond 2^53; callers check the
//! property itself, and the index only narrows the search.
//!
//! A fuzzy index (`fuzzy: true`) keeps, instead of the value, the
//! trigrams of the normalized text (of a string, or of each string of a
//! list): it finds the nodes whose text looks like a query
//! ([`ReadTx::fuzzy_search`]), typos and word order included, without
//! comparing with every node. Its entries share the table, tagged apart.
//!
//! Since 16.5 an index may also cover several properties (a composite
//! index, usable by a prefix of them), several labels (records with any of
//! them), or edges instead of nodes. A composite entry is its values one
//! after the other, each encoded so that it ends unambiguously (strings
//! escape their zero bytes and end with two), so byte order is still value
//! order, property by property. Single-property indexes keep their first
//! encoding, so no layout changed.

use std::collections::HashMap;
use std::ops::Bound;

use grafeno_core::{Edge, EdgeId, Error, Node, NodeId, Props, Result};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::store::{
    EDGES, NODE_LABELS, NODES, check_label, decode, encode, nodes_with_label, read_record, storage,
};
use crate::{ReadTx, WriteTx};

pub(crate) type IndexKey = (&'static str, &'static str, &'static [u8], u128);

/// `(ns, index name) -> IndexDef` (JSON).
pub(crate) const INDEXES: TableDefinition<(&str, &str), &[u8]> = TableDefinition::new("indexes");
/// `(ns, index name, encoded value, node) -> ()`
pub(crate) const INDEX_ENTRIES: TableDefinition<IndexKey, ()> =
    TableDefinition::new("index_entries");

/// A declared index: nodes with `label`, by their property `key`; or,
/// since 16.5, by several properties, over several labels, or on edges.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub label: String,
    pub key: String,
    /// No two nodes with the label may share a value.
    #[serde(default)]
    pub unique: bool,
    /// Trigrams of the text, for [`ReadTx::fuzzy_search`], instead of the
    /// value (not for lookups by value; never unique).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fuzzy: bool,
    /// The properties after `key`, in order: a composite index on `(key,
    /// more_keys…)`. A record is in it when it has every one of them with
    /// a value the index can hold; a lookup gives values for a prefix.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more_keys: Vec<String>,
    /// More labels: records with `label` or any of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more_labels: Vec<String>,
    /// On edges with the label(s) instead of nodes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edges: bool,
    /// Every record with the label(s) must have every property, with a
    /// value the index can hold (with `unique`: a key constraint).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    /// A full-text index (16.6) on the properties: their words, for
    /// [`ReadTx::fulltext_search`], instead of their values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulltext: Option<crate::fulltext::Fulltext>,
    /// A vector index (16.7) on the property: an HNSW graph of its vectors,
    /// for [`ReadTx::vector_search`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<crate::vindex::VectorSpec>,
}

impl IndexDef {
    /// The default name: `Person_age` (`Person_email_unique` for a
    /// constraint).
    pub fn default_name(label: &str, key: &str, unique: bool) -> String {
        if unique {
            format!("{label}_{key}_unique")
        } else {
            format!("{label}_{key}")
        }
    }

    /// `Person_name_fuzzy`.
    pub fn default_fuzzy_name(label: &str, key: &str) -> String {
        format!("{label}_{key}_fuzzy")
    }

    /// An index (not unique, not fuzzy) under its default name.
    pub fn new(label: &str, key: &str) -> Self {
        Self {
            name: Self::default_name(label, key, false),
            label: label.into(),
            key: key.into(),
            ..Self::default()
        }
    }

    /// An index on several properties of nodes with `label`, named
    /// `Person_last_first`.
    pub fn composite(label: &str, keys: &[&str]) -> Self {
        let mut def = Self {
            label: label.into(),
            key: keys.first().copied().unwrap_or_default().into(),
            more_keys: keys.iter().skip(1).map(|key| (*key).to_owned()).collect(),
            ..Self::default()
        };
        def.name = def.generated_name();
        def
    }

    /// An index on a property of edges with `label`, named `rated_stars`.
    pub fn on_edges(label: &str, key: &str) -> Self {
        Self {
            edges: true,
            ..Self::new(label, key)
        }
    }

    /// The name given when none is: labels, then properties, then
    /// `unique`, `key` or `fuzzy`, joined by `_`.
    pub fn generated_name(&self) -> String {
        let mut parts: Vec<&str> = self.labels();
        parts.extend(self.keys());
        if self.fuzzy {
            parts.push("fuzzy");
        } else if self.fulltext.is_some() {
            parts.push("fulltext");
        } else if self.vector.is_some() {
            parts.push("vector");
        } else if self.unique && self.required {
            parts.push("key");
        } else if self.unique {
            parts.push("unique");
        }
        parts.join("_")
    }

    /// Every property, in order.
    pub fn keys(&self) -> Vec<&str> {
        std::iter::once(self.key.as_str())
            .chain(self.more_keys.iter().map(String::as_str))
            .collect()
    }

    /// Every label.
    pub fn labels(&self) -> Vec<&str> {
        std::iter::once(self.label.as_str())
            .chain(self.more_labels.iter().map(String::as_str))
            .collect()
    }

    /// One property of nodes of one label: the index every version had.
    pub fn is_simple(&self) -> bool {
        self.more_keys.is_empty()
            && self.more_labels.is_empty()
            && !self.edges
            && self.fulltext.is_none()
            && self.vector.is_none()
    }

    /// Looked up by value (not fuzzy, not full-text): what plans use.
    pub fn by_value(&self) -> bool {
        !self.fuzzy && self.fulltext.is_none() && self.vector.is_none()
    }

    pub(crate) fn composite_keys(&self) -> bool {
        !self.more_keys.is_empty()
    }

    /// Whether a record with these labels belongs in the index.
    pub fn covers<S: AsRef<str>>(&self, labels: &[S]) -> bool {
        let mine = self.labels();
        labels.iter().any(|label| mine.contains(&label.as_ref()))
    }

    /// The same records and properties, whatever the name.
    pub fn same_shape(&self, other: &IndexDef) -> bool {
        let sorted = |def: &IndexDef| {
            let mut labels: Vec<String> = def.labels().into_iter().map(str::to_owned).collect();
            labels.sort_unstable();
            labels
        };
        sorted(self) == sorted(other)
            && self.keys() == other.keys()
            && self.edges == other.edges
            && self.unique == other.unique
            && self.fuzzy == other.fuzzy
            && self.fulltext == other.fulltext
            && self
                .vector
                .as_ref()
                .map(|spec| (spec.dimension, spec.similarity))
                == other
                    .vector
                    .as_ref()
                    .map(|spec| (spec.dimension, spec.similarity))
    }

    /// What it is on, for messages: `:Person(email)`, `[:rated](stars)`.
    pub fn describe(&self) -> String {
        let labels = self.labels().join("|");
        let keys = self.keys().join(", ");
        if self.edges {
            format!("[:{labels}]({keys})")
        } else {
            format!(":{labels}({keys})")
        }
    }

    /// A fuzzy index under its default name.
    pub fn fuzzy(label: &str, key: &str) -> Self {
        Self {
            name: Self::default_fuzzy_name(label, key),
            fuzzy: true,
            ..Self::new(label, key)
        }
    }
}

/// The tag of fuzzy-index entries: a trigram follows.
const GRAM: u8 = 4;

/// The trigrams a fuzzy index keeps for a value: of the normalized text of
/// a string, or of each string in a list.
pub(crate) fn grams_of(value: &Json) -> Vec<String> {
    let mut grams = std::collections::BTreeSet::new();
    let mut add = |text: &str| {
        grams.extend(crate::textsim::ngrams(&crate::normalize_name(text), 3));
    };
    match value {
        Json::String(s) => add(s),
        Json::Array(items) => items.iter().filter_map(Json::as_str).for_each(&mut add),
        _ => {}
    }
    grams.into_iter().collect()
}

fn gram_bytes(gram: &str) -> Vec<u8> {
    let mut out = vec![GRAM];
    out.extend_from_slice(gram.as_bytes());
    out
}

/// A node that looks like a fuzzy search's text.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct FuzzyHit {
    pub node: NodeId,
    /// From 0 to 1 (see [`ReadTx::fuzzy_search`]).
    pub score: f64,
}

/// A trigram shared by more nodes than this says little about a query
/// and costs a long read: its entries are left out of the candidate count.
const COMMON_GRAM: usize = 20_000;

/// An index and how many nodes it holds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IndexInfo {
    #[serde(flatten)]
    pub def: IndexDef,
    pub entries: u64,
}

/// Which values to look up.
#[derive(Clone, Debug, PartialEq)]
pub enum IndexRange {
    Equal(Json),
    /// Between two values of the same kind (numbers, strings or booleans);
    /// an unbounded end stays within that kind.
    Between {
        lower: Bound<Json>,
        upper: Bound<Json>,
    },
    /// Strings that start with this text.
    Prefix(String),
}

const BOOL: u8 = 1;
const NUMBER: u8 = 2;
const STRING: u8 = 3;

fn kind(value: &Json) -> Option<u8> {
    match value {
        Json::Bool(_) => Some(BOOL),
        Json::Number(_) => Some(NUMBER),
        Json::String(_) => Some(STRING),
        _ => None,
    }
}

/// The sortable bytes of a value, or `None` if it is not indexed.
pub(crate) fn encode_value(value: &Json) -> Option<Vec<u8>> {
    match value {
        Json::Bool(b) => Some(vec![BOOL, u8::from(*b)]),
        Json::Number(n) => {
            let f = n.as_f64()?;
            if !f.is_finite() {
                return None;
            }
            // Flip the bits so that byte order is numeric order (and -0 = 0).
            let f = if f == 0.0 { 0.0 } else { f };
            let bits = f.to_bits();
            let bits = if bits >> 63 == 1 {
                !bits
            } else {
                bits ^ (1 << 63)
            };
            let mut out = vec![NUMBER];
            out.extend_from_slice(&bits.to_be_bytes());
            Some(out)
        }
        Json::String(s) => {
            let mut out = vec![STRING];
            out.extend_from_slice(s.as_bytes());
            Some(out)
        }
        _ => None,
    }
}

/// One value of a composite entry: as [`encode_value`], but strings escape
/// their zero bytes (`00 FF`) and end with `00 00`, so a value ends where
/// the next one starts and byte order stays value order.
fn component(value: &Json) -> Option<Vec<u8>> {
    match value {
        Json::String(s) => {
            let mut out = vec![STRING];
            escape_into(&mut out, s);
            out.extend_from_slice(&[0, 0]);
            Some(out)
        }
        other => encode_value(other),
    }
}

fn escape_into(out: &mut Vec<u8>, text: &str) {
    for &byte in text.as_bytes() {
        out.push(byte);
        if byte == 0 {
            out.push(0xFF);
        }
    }
}

/// The entry of a record's properties in `def` (not a fuzzy one), or
/// `None` if a property is missing or holds what the index cannot.
pub(crate) fn entry_bytes(def: &IndexDef, props: &Props) -> Option<Vec<u8>> {
    if !def.composite_keys() {
        return encode_value(props.get(&def.key)?);
    }
    let mut out = Vec::new();
    for key in def.keys() {
        out.extend(component(props.get(key)?)?);
    }
    Some(out)
}

/// The byte range of a lookup in `def`: values equal to `prefix` for its
/// first properties, then `range` on the next one (anything when `None`).
/// `None`: nothing can match (a value the index cannot hold, a range that
/// mixes kinds, more values than properties).
pub(crate) fn seek_bytes(
    def: &IndexDef,
    prefix: &[Json],
    range: Option<&IndexRange>,
) -> Option<(Vec<u8>, Vec<u8>)> {
    if !def.composite_keys() {
        return match (prefix, range) {
            ([], Some(range)) => byte_range(range),
            ([value], None) => byte_range(&IndexRange::Equal(value.clone())),
            _ => None,
        };
    }
    if prefix.len() + usize::from(range.is_some()) > def.keys().len() {
        return None;
    }
    let mut start = Vec::new();
    for value in prefix {
        start.extend(component(value)?);
    }
    let with = |tail: &[u8]| {
        let mut out = start.clone();
        out.extend_from_slice(tail);
        out
    };
    // Whatever follows a value sorts before 0xFF, which no value's first
    // byte (its kind) reaches.
    Some(match range {
        None => (start.clone(), with(&[0xFF])),
        Some(IndexRange::Equal(value)) => {
            let value = component(value)?;
            let mut high = value.clone();
            high.push(0xFF);
            (with(&value), with(&high))
        }
        Some(IndexRange::Prefix(text)) => {
            let mut low = vec![STRING];
            escape_into(&mut low, text);
            let mut high = low.clone();
            high.push(0xFF);
            (with(&low), with(&high))
        }
        Some(IndexRange::Between { lower, upper }) => {
            let bound_kind = |bound: &Bound<Json>| match bound {
                Bound::Included(value) | Bound::Excluded(value) => kind(value),
                Bound::Unbounded => None,
            };
            let tag = bound_kind(lower).or_else(|| bound_kind(upper))?;
            if [lower, upper]
                .iter()
                .any(|bound| bound_kind(bound).is_some_and(|other| other != tag))
            {
                return None;
            }
            let low = match lower {
                Bound::Included(value) | Bound::Excluded(value) => component(value)?,
                Bound::Unbounded => vec![tag],
            };
            let high = match upper {
                Bound::Included(value) | Bound::Excluded(value) => {
                    let mut high = component(value)?;
                    high.push(0xFF);
                    high
                }
                Bound::Unbounded => vec![tag, 0xFF],
            };
            (with(&low), with(&high))
        }
    })
}

/// The value encoded by [`encode_value`] (numbers come back as `f64`
/// unless they are whole and fit an `i64`).
pub(crate) fn decode_value(bytes: &[u8]) -> Option<Json> {
    let (&tag, rest) = bytes.split_first()?;
    match tag {
        BOOL => Some(Json::Bool(*rest.first()? == 1)),
        NUMBER => {
            let bits = u64::from_be_bytes(rest.try_into().ok()?);
            let bits = if bits >> 63 == 1 {
                bits ^ (1 << 63)
            } else {
                !bits
            };
            let f = f64::from_bits(bits);
            if f.fract() == 0.0 && f.abs() < 9.0e15 {
                Some(Json::from(f as i64))
            } else {
                serde_json::Number::from_f64(f).map(Json::Number)
            }
        }
        STRING => std::str::from_utf8(rest)
            .ok()
            .map(|s| Json::String(s.to_owned())),
        _ => None,
    }
}

/// The byte range a lookup covers. Bounds are always inclusive: exclusion
/// is left to the caller's own check (numbers are compared as `f64` here).
pub(crate) fn byte_range(range: &IndexRange) -> Option<(Vec<u8>, Vec<u8>)> {
    match range {
        IndexRange::Equal(value) => {
            let bytes = encode_value(value)?;
            Some((bytes.clone(), bytes))
        }
        IndexRange::Prefix(prefix) => {
            let mut lower = vec![STRING];
            lower.extend_from_slice(prefix.as_bytes());
            let mut upper = lower.clone();
            // 0xFF never appears in UTF-8, so it sorts after every
            // continuation of the prefix.
            upper.push(0xFF);
            Some((lower, upper))
        }
        IndexRange::Between { lower, upper } => {
            let bound_kind = |bound: &Bound<Json>| match bound {
                Bound::Included(value) | Bound::Excluded(value) => kind(value),
                Bound::Unbounded => None,
            };
            let tag = bound_kind(lower).or_else(|| bound_kind(upper))?;
            if [lower, upper]
                .iter()
                .any(|bound| bound_kind(bound).is_some_and(|other| other != tag))
            {
                return None;
            }
            let low = match lower {
                Bound::Included(value) | Bound::Excluded(value) => encode_value(value)?,
                Bound::Unbounded => vec![tag],
            };
            let high = match upper {
                Bound::Included(value) | Bound::Excluded(value) => encode_value(value)?,
                Bound::Unbounded => vec![tag, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            };
            Some((low, high))
        }
    }
}

/// The index entries a node should have under these definitions.
pub(crate) fn entries_of<'a>(defs: &'a [IndexDef], node: &Node) -> Vec<(&'a IndexDef, Vec<u8>)> {
    record_entries(defs, false, &node.labels, &node.props)
}

/// The index entries of a node (`edges` false) or an edge with these
/// labels and properties.
pub(crate) fn record_entries<'a, S: AsRef<str>>(
    defs: &'a [IndexDef],
    edges: bool,
    labels: &[S],
    props: &Props,
) -> Vec<(&'a IndexDef, Vec<u8>)> {
    let mut out = Vec::new();
    for def in defs
        .iter()
        .filter(|def| def.edges == edges && def.covers(labels))
    {
        if let Some(fulltext) = &def.fulltext {
            if let Some(doc) = crate::fulltext::doc_of(def, fulltext, props) {
                out.extend(doc.entries.into_iter().map(|bytes| (def, bytes)));
            }
        } else if def.fuzzy {
            if let Some(value) = props.get(&def.key) {
                out.extend(grams_of(value).iter().map(|gram| (def, gram_bytes(gram))));
            }
        } else if let Some(bytes) = entry_bytes(def, props) {
            out.push((def, bytes));
        }
    }
    out
}

/// A record's values for the properties of `def`, for messages.
pub(crate) fn values_of(def: &IndexDef, props: &Props) -> String {
    let values: Vec<String> = def
        .keys()
        .iter()
        .map(|key| props.get(*key).map_or("null".into(), Json::to_string))
        .collect();
    if values.len() == 1 {
        values[0].clone()
    } else {
        format!("({})", values.join(", "))
    }
}

/// Whether two records hold the same values for every property of `def`
/// (equal bytes are equal values except for integers beyond 2^53).
pub(crate) fn same_values(def: &IndexDef, a: &Props, b: &Props) -> bool {
    def.keys().iter().all(|key| a.get(*key) == b.get(*key))
}

/// Nodes of fuzzy index `name` that share trigrams with `text`, best first:
/// candidates by trigrams in common, then scored with `measure` against
/// the node's value (the best of a list). Reads `node` for each candidate.
fn fuzzy(
    entries: &impl ReadableTable<IndexKey, ()>,
    node: impl Fn(NodeId) -> Result<Option<Node>>,
    def: &IndexDef,
    ns: &str,
    text: &str,
    measure: crate::textsim::Measure,
    limit: usize,
) -> Result<Vec<FuzzyHit>> {
    let query = crate::normalize_name(text);
    let grams = crate::textsim::ngrams(&query, 3);
    let mut shared: HashMap<u128, u32> = HashMap::new();
    for gram in &grams {
        let bytes = gram_bytes(gram);
        let mut seen = Vec::new();
        for item in entries
            .range(
                (ns, def.name.as_str(), bytes.as_slice(), 0u128)
                    ..=(ns, def.name.as_str(), bytes.as_slice(), u128::MAX),
            )
            .map_err(storage)?
        {
            seen.push(item.map_err(storage)?.0.value().3);
            if seen.len() > COMMON_GRAM {
                break;
            }
        }
        if seen.len() > COMMON_GRAM {
            continue;
        }
        for id in seen {
            *shared.entry(id).or_default() += 1;
        }
    }
    // The candidates sharing the most trigrams, then the measure on them.
    let mut candidates: Vec<(u128, u32)> = shared.into_iter().collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    candidates.truncate(limit.saturating_mul(10).max(50));
    let mut hits = Vec::new();
    for (id, _) in candidates {
        let id = NodeId::from_u128(id);
        let Some(found) = node(id)? else {
            continue;
        };
        let texts: Vec<&str> = match found.props.get(&def.key) {
            Some(Json::String(s)) => vec![s.as_str()],
            Some(Json::Array(items)) => items.iter().filter_map(Json::as_str).collect(),
            _ => Vec::new(),
        };
        let score = texts
            .iter()
            .map(|value| measure.compare(text, value))
            .fold(0.0, f64::max);
        hits.push(FuzzyHit { node: id, score });
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    hits.truncate(limit);
    Ok(hits)
}

fn fuzzy_def(defs: Vec<IndexDef>, label: &str, key: &str) -> Result<IndexDef> {
    defs.into_iter()
        .find(|def| def.fuzzy && def.is_simple() && def.label == label && def.key == key)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "no fuzzy index on :{label}({key}); create one with CREATE FUZZY INDEX FOR (n:{label}) ON (n.{key})"
            ))
        })
}

fn defs_in(
    table: &impl ReadableTable<(&'static str, &'static str), &'static [u8]>,
    ns: &str,
) -> Result<Vec<IndexDef>> {
    let mut out = Vec::new();
    for item in table.range((ns, "")..).map_err(storage)? {
        let (key, value) = item.map_err(storage)?;
        if key.value().0 != ns {
            break;
        }
        out.push(decode(value.value())?);
    }
    Ok(out)
}

fn lookup(
    table: &impl ReadableTable<IndexKey, ()>,
    ns: &str,
    name: &str,
    range: &IndexRange,
    limit: usize,
) -> Result<Vec<NodeId>> {
    let Some((low, high)) = byte_range(range) else {
        return Ok(Vec::new());
    };
    Ok(lookup_bytes(table, ns, name, &low, &high, limit)?
        .into_iter()
        .map(NodeId::from_u128)
        .collect())
}

/// The ids of the entries between two keys, both included, in key order.
fn lookup_bytes(
    table: &impl ReadableTable<IndexKey, ()>,
    ns: &str,
    name: &str,
    low: &[u8],
    high: &[u8],
    limit: usize,
) -> Result<Vec<u128>> {
    let mut out = Vec::new();
    for item in table
        .range((ns, name, low, 0u128)..=(ns, name, high, u128::MAX))
        .map_err(storage)?
    {
        if out.len() >= limit {
            break;
        }
        let (key, _) = item.map_err(storage)?;
        out.push(key.value().3);
    }
    Ok(out)
}

fn find_def(defs: Vec<IndexDef>, name: &str) -> Result<IndexDef> {
    defs.into_iter()
        .find(|def| def.name == name)
        .ok_or_else(|| Error::Invalid(format!("there is no index named {name:?}")))
}

fn seek_ids(
    table: &impl ReadableTable<IndexKey, ()>,
    ns: &str,
    def: &IndexDef,
    prefix: &[Json],
    range: Option<&IndexRange>,
    limit: usize,
) -> Result<Vec<u128>> {
    if !def.by_value() {
        let (kind, how) = if def.fuzzy {
            ("fuzzy", "fuzzy_search")
        } else if def.fulltext.is_some() {
            ("full-text", "fulltext_search")
        } else {
            ("vector", "vector_search")
        };
        return Err(Error::Invalid(format!(
            "{} is a {kind} index: search it with {how}",
            def.name
        )));
    }
    match seek_bytes(def, prefix, range) {
        Some((low, high)) => lookup_bytes(table, ns, &def.name, &low, &high, limit),
        None => Ok(Vec::new()),
    }
}

fn count(table: &impl ReadableTable<IndexKey, ()>, ns: &str, name: &str) -> Result<u64> {
    let empty: &[u8] = &[];
    let top: &[u8] = &[0xFF; 16];
    let mut entries = 0;
    for item in table
        .range((ns, name, empty, 0u128)..=(ns, name, top, u128::MAX))
        .map_err(storage)?
    {
        item.map_err(storage)?;
        entries += 1;
    }
    Ok(entries)
}

impl ReadTx {
    /// The indexes of a namespace, in name order.
    pub fn index_defs(&self, ns: &str) -> Result<Vec<IndexDef>> {
        defs_in(&self.indexes, ns)
    }

    /// The indexes with how many nodes each holds (counts every entry).
    pub fn indexes(&self, ns: &str) -> Result<Vec<IndexInfo>> {
        self.index_defs(ns)?
            .into_iter()
            .map(|def| {
                let entries = if def.vector.is_some() {
                    self.vector_count(ns, &def.name)?
                } else {
                    count(&self.index_entries, ns, &def.name)?
                };
                Ok(IndexInfo { def, entries })
            })
            .collect()
    }

    /// Up to `limit` nodes with `label` whose property `key` looks like
    /// `text`, through the fuzzy index on them, best first. Candidates are
    /// the nodes sharing the most trigrams with the normalized text; each
    /// is scored with `measure` (Dice over trigrams, Jaro-Winkler, …)
    /// against its value. Fails if there is no fuzzy index on `(label,
    /// key)`.
    pub fn fuzzy_search(
        &self,
        ns: &str,
        label: &str,
        key: &str,
        text: &str,
        measure: crate::textsim::Measure,
        limit: usize,
    ) -> Result<Vec<FuzzyHit>> {
        let def = fuzzy_def(self.index_defs(ns)?, label, key)?;
        fuzzy(
            &self.index_entries,
            |id| self.node(ns, id),
            &def,
            ns,
            text,
            measure,
            limit,
        )
    }

    /// Up to `limit` nodes in index `name` whose value is in `range`, in
    /// value order. An unknown index gives none.
    pub fn index_lookup(
        &self,
        ns: &str,
        name: &str,
        range: &IndexRange,
        limit: usize,
    ) -> Result<Vec<NodeId>> {
        let Some(view) = self.view() else {
            return lookup(&self.index_entries, ns, name, range, limit);
        };
        // An index on a property the view may not read finds nothing.
        if view.per_record(ns)
            && let Some(def) = self
                .index_defs(ns)?
                .into_iter()
                .find(|def| def.name == name)
            && view.property_hidden(ns, std::slice::from_ref(&def.label), &def.key)
        {
            return Ok(Vec::new());
        }
        let mut visible = Vec::new();
        for id in lookup(&self.index_entries, ns, name, range, usize::MAX)? {
            if self.sees(ns, id)? {
                visible.push(id);
                if visible.len() == limit {
                    break;
                }
            }
        }
        Ok(visible)
    }

    /// Up to `limit` nodes in node index `name` (of any kind: composite,
    /// over several labels) whose first properties equal `prefix` and whose
    /// next one is in `range` (anything when `None`), in value order.
    /// Numbers are compared as `f64` and range ends are included: check the
    /// properties of what it finds.
    pub fn index_seek_nodes(
        &self,
        ns: &str,
        name: &str,
        prefix: &[Json],
        range: Option<&IndexRange>,
        limit: usize,
    ) -> Result<Vec<NodeId>> {
        let def = find_def(self.index_defs(ns)?, name)?;
        if def.edges {
            return Err(Error::Invalid(format!("{name} is an index on edges")));
        }
        let Some(view) = self.view() else {
            return Ok(
                seek_ids(&self.index_entries, ns, &def, prefix, range, limit)?
                    .into_iter()
                    .map(NodeId::from_u128)
                    .collect(),
            );
        };
        if view.per_record(ns)
            && def.keys().iter().any(|key| {
                def.labels()
                    .iter()
                    .any(|label| view.property_hidden(ns, &[(*label).to_owned()], key))
            })
        {
            return Ok(Vec::new());
        }
        let mut visible = Vec::new();
        for id in seek_ids(&self.index_entries, ns, &def, prefix, range, usize::MAX)? {
            let id = NodeId::from_u128(id);
            if self.sees(ns, id)? {
                visible.push(id);
                if visible.len() == limit {
                    break;
                }
            }
        }
        Ok(visible)
    }

    /// [`ReadTx::index_seek_nodes`] for an index on edges: the edges found,
    /// retracted ones included (check them).
    pub fn index_seek_edges(
        &self,
        ns: &str,
        name: &str,
        prefix: &[Json],
        range: Option<&IndexRange>,
        limit: usize,
    ) -> Result<Vec<EdgeId>> {
        let def = find_def(self.index_defs(ns)?, name)?;
        if !def.edges {
            return Err(Error::Invalid(format!("{name} is an index on nodes")));
        }
        let viewed = self.view().is_some();
        let wanted = if viewed { usize::MAX } else { limit };
        let mut out = Vec::new();
        for id in seek_ids(&self.index_entries, ns, &def, prefix, range, wanted)? {
            let id = EdgeId::from_u128(id);
            // `edge` gives nothing the view hides.
            if !viewed || self.edge(ns, id)?.is_some() {
                out.push(id);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }
}

impl WriteTx {
    pub fn index_defs(&self, ns: &str) -> Result<Vec<IndexDef>> {
        defs_in(&self.tx.open_table(INDEXES).map_err(storage)?, ns)
    }

    /// [`ReadTx::index_seek_nodes`] as written so far in this transaction.
    pub fn index_seek_nodes(
        &self,
        ns: &str,
        name: &str,
        prefix: &[Json],
        range: Option<&IndexRange>,
        limit: usize,
    ) -> Result<Vec<NodeId>> {
        let def = find_def(self.index_defs(ns)?, name)?;
        if def.edges {
            return Err(Error::Invalid(format!("{name} is an index on edges")));
        }
        let table = self.tx.open_table(INDEX_ENTRIES).map_err(storage)?;
        Ok(seek_ids(&table, ns, &def, prefix, range, limit)?
            .into_iter()
            .map(NodeId::from_u128)
            .collect())
    }

    /// [`ReadTx::index_seek_edges`] as written so far in this transaction.
    pub fn index_seek_edges(
        &self,
        ns: &str,
        name: &str,
        prefix: &[Json],
        range: Option<&IndexRange>,
        limit: usize,
    ) -> Result<Vec<EdgeId>> {
        let def = find_def(self.index_defs(ns)?, name)?;
        if !def.edges {
            return Err(Error::Invalid(format!("{name} is an index on nodes")));
        }
        let table = self.tx.open_table(INDEX_ENTRIES).map_err(storage)?;
        Ok(seek_ids(&table, ns, &def, prefix, range, limit)?
            .into_iter()
            .map(EdgeId::from_u128)
            .collect())
    }

    /// [`ReadTx::fuzzy_search`] as written so far in this transaction.
    pub fn fuzzy_search(
        &self,
        ns: &str,
        label: &str,
        key: &str,
        text: &str,
        measure: crate::textsim::Measure,
        limit: usize,
    ) -> Result<Vec<FuzzyHit>> {
        let def = fuzzy_def(self.index_defs(ns)?, label, key)?;
        let table = self.tx.open_table(INDEX_ENTRIES).map_err(storage)?;
        fuzzy(
            &table,
            |id| self.node(ns, id),
            &def,
            ns,
            text,
            measure,
            limit,
        )
    }

    /// [`ReadTx::index_lookup`] as written so far in this transaction.
    pub fn index_lookup(
        &self,
        ns: &str,
        name: &str,
        range: &IndexRange,
        limit: usize,
    ) -> Result<Vec<NodeId>> {
        lookup(
            &self.tx.open_table(INDEX_ENTRIES).map_err(storage)?,
            ns,
            name,
            range,
            limit,
        )
    }

    /// Declares an index and fills it from the nodes that have the label.
    /// Returns how many nodes it holds. An identical definition is kept as
    /// it is; another index with the same name or on the same label and
    /// property is an error, as are duplicate values in a unique index.
    pub fn create_index(&mut self, ns: &str, def: IndexDef) -> Result<u64> {
        self.note(crate::replication::Touch::Index(
            ns.to_owned(),
            def.name.clone(),
        ));
        for label in def.labels() {
            check_label(label)?;
        }
        crate::namespace::ensure(&self.tx, ns)?;
        if def.keys().iter().any(|key| key.is_empty()) || def.name.is_empty() {
            return Err(Error::Invalid(
                "an index needs a name and a property".into(),
            ));
        }
        let mut keys = def.keys();
        keys.sort_unstable();
        keys.dedup();
        if keys.len() != def.keys().len() {
            return Err(Error::Invalid(format!(
                "{} names a property twice",
                def.describe()
            )));
        }
        let existing = self.index_defs(ns)?;
        if let Some(same) = existing.iter().find(|other| other.name == def.name) {
            if *same == def {
                let table = self.tx.open_table(INDEX_ENTRIES).map_err(storage)?;
                return count(&table, ns, &def.name);
            }
            return Err(Error::Invalid(format!(
                "an index named {:?} already exists",
                def.name
            )));
        }
        if def.fuzzy && (def.unique || !def.is_simple()) {
            return Err(Error::Invalid(
                "a fuzzy index is on one property of one node label, and not unique".into(),
            ));
        }
        if let Some(spec) = &def.vector {
            if def.unique
                || def.required
                || def.fuzzy
                || def.edges
                || def.fulltext.is_some()
                || !def.more_keys.is_empty()
            {
                return Err(Error::Invalid(
                    "a vector index is on one node property, and nothing else".into(),
                ));
            }
            if spec.dimension == 0
                || spec.dimension > 65_536
                || !(2..=128).contains(&spec.m)
                || spec.ef_construction == 0
                || spec.ef_search == 0
            {
                return Err(Error::Invalid(
                    "a vector index needs a dimension (1 to 65536), m from 2 to 128, and positive ef_construction and ef_search".into(),
                ));
            }
        }
        if let Some(fulltext) = &def.fulltext {
            if def.unique || def.required || def.fuzzy || def.edges {
                return Err(Error::Invalid(
                    "a full-text index is on node properties, and neither unique nor fuzzy".into(),
                ));
            }
            if fulltext.weights.len() > def.keys().len()
                || fulltext
                    .weights
                    .iter()
                    .any(|weight| !weight.is_finite() || *weight <= 0.0)
            {
                return Err(Error::Invalid(
                    "a full-text index has a positive weight per property, at most".into(),
                ));
            }
        }
        if def.required && !def.unique {
            return Err(Error::Invalid(
                "a required index is a key: it is unique too".into(),
            ));
        }
        if self
            .constraints(ns)?
            .iter()
            .any(|constraint| constraint.name() == def.name)
        {
            return Err(Error::Invalid(format!(
                "a constraint is named {:?}",
                def.name
            )));
        }
        if let Some(other) = existing
            .iter()
            .find(|other| other.same_shape(&def) && other.required == def.required)
        {
            return Err(Error::Invalid(format!(
                "index {:?} already covers {}",
                other.name,
                def.describe()
            )));
        }
        let records = self.records_of(ns, &def)?;
        if def.vector.is_some() {
            let count = self.fill_vector_index(ns, &def, &records)?;
            self.tx
                .open_table(INDEXES)
                .map_err(storage)?
                .insert((ns, def.name.as_str()), encode(&def)?.as_slice())
                .map_err(storage)?;
            return Ok(count);
        }
        let mut seen: HashMap<Vec<u8>, (u128, Props)> = HashMap::new();
        let mut entries = Vec::new();
        let mut docs = Vec::new();
        let what = if def.edges { "edges" } else { "nodes" };
        for (id, props, live) in &records {
            if def.required
                && *live
                && let Some(missing) = missing_key(&def, props)
            {
                return Err(Error::Invalid(format!(
                    "cannot make ({}) a key of {}: {} {} has no {missing} the index can hold",
                    def.keys().join(", "),
                    def.describe(),
                    &what[..what.len() - 1],
                    record_name(&def, *id),
                )));
            }
            if let Some(fulltext) = &def.fulltext {
                if let Some(doc) = crate::fulltext::doc_of(&def, fulltext, props) {
                    entries.extend(doc.entries.iter().map(|bytes| (bytes.clone(), *id)));
                    docs.push(doc);
                }
                continue;
            }
            if def.fuzzy {
                if let Some(value) = props.get(&def.key) {
                    entries.extend(grams_of(value).iter().map(|gram| (gram_bytes(gram), *id)));
                }
                continue;
            }
            let Some(bytes) = entry_bytes(&def, props) else {
                continue;
            };
            if def.unique && *live {
                if let Some((other, other_props)) = seen.get(&bytes)
                    && same_values(&def, other_props, props)
                {
                    return Err(Error::Invalid(format!(
                        "cannot make {} unique: {what} {} and {} both have {}",
                        def.describe(),
                        record_name(&def, *other),
                        record_name(&def, *id),
                        values_of(&def, props)
                    )));
                }
                seen.insert(bytes.clone(), (*id, props.clone()));
            }
            entries.push((bytes, *id));
        }
        let mut table = self.tx.open_table(INDEX_ENTRIES).map_err(storage)?;
        for (bytes, id) in &entries {
            table
                .insert((ns, def.name.as_str(), bytes.as_slice(), *id), ())
                .map_err(storage)?;
        }
        drop(table);
        if def.fulltext.is_some() {
            crate::fulltext::set_stats(&self.tx, ns, &def.name, &docs)?;
        }
        self.tx
            .open_table(INDEXES)
            .map_err(storage)?
            .insert((ns, def.name.as_str()), encode(&def)?.as_slice())
            .map_err(storage)?;
        Ok(entries.len() as u64)
    }

    /// Removes an index and its entries. Returns whether it existed.
    pub fn drop_index(&mut self, ns: &str, name: &str) -> Result<bool> {
        self.note(crate::replication::Touch::Index(
            ns.to_owned(),
            name.to_owned(),
        ));
        let existed = self
            .tx
            .open_table(INDEXES)
            .map_err(storage)?
            .remove((ns, name))
            .map_err(storage)?
            .is_some();
        let empty: &[u8] = &[];
        self.tx
            .open_table(INDEX_ENTRIES)
            .map_err(storage)?
            .retain_in((ns, name, empty, 0u128).., |key, _| {
                key.0 != ns || key.1 != name
            })
            .map_err(storage)?;
        crate::fulltext::drop_stats(&self.tx, ns, name)?;
        let space = crate::vindex::space_of(ns, name);
        crate::vindex::clear_space(&self.tx, &space)?;
        self.vectors.forget(&space);
        Ok(existed)
    }

    /// Brings the index entries of a node up to date: `node` is its new
    /// state (`None` when deleted), `previous` the old one (`None` when new).
    pub(crate) fn index_node(
        &mut self,
        ns: &str,
        node: Option<&Node>,
        previous: Option<&Node>,
    ) -> Result<()> {
        let defs = self.index_defs(ns)?;
        self.index_node_with(&defs, ns, node, previous)
    }

    /// [`WriteTx::index_node`] with the namespace's indexes read already.
    pub(crate) fn index_node_with(
        &mut self,
        defs: &[IndexDef],
        ns: &str,
        node: Option<&Node>,
        previous: Option<&Node>,
    ) -> Result<()> {
        let id = node
            .or(previous)
            .map(|node| node.id.as_u128())
            .expect("a node to index");
        self.index_record(
            defs,
            ns,
            false,
            id,
            node.map(|node| (node.labels.as_slice(), &node.props)),
            previous.map(|node| (node.labels.as_slice(), &node.props)),
            true,
        )
    }

    /// [`WriteTx::index_node`] for an edge. A retracted fact keeps its
    /// entries but no longer holds its unique values (the version that
    /// replaces an invalidated fact has the same ones).
    pub(crate) fn index_edge(
        &mut self,
        ns: &str,
        edge: Option<&Edge>,
        previous: Option<&Edge>,
    ) -> Result<()> {
        let defs = self.index_defs(ns)?;
        self.index_edge_with(&defs, ns, edge, previous)
    }

    /// [`WriteTx::index_edge`] with the namespace's indexes read already.
    pub(crate) fn index_edge_with(
        &mut self,
        defs: &[IndexDef],
        ns: &str,
        edge: Option<&Edge>,
        previous: Option<&Edge>,
    ) -> Result<()> {
        if !defs.iter().any(|def| def.edges) {
            return Ok(());
        }
        let id = edge
            .or(previous)
            .map(|edge| edge.id.as_u128())
            .expect("an edge to index");
        let new_labels = edge.map(|edge| vec![edge.label.clone()]);
        let old_labels = previous.map(|edge| vec![edge.label.clone()]);
        self.index_record(
            defs,
            ns,
            true,
            id,
            new_labels.as_deref().zip(edge.map(|edge| &edge.props)),
            old_labels.as_deref().zip(previous.map(|edge| &edge.props)),
            edge.is_some_and(|edge| !edge.is_retracted()),
        )
    }

    /// Moves a record's entries from `old` to `new` (labels, properties);
    /// with `check`, its new unique values must be its own.
    #[allow(clippy::too_many_arguments)]
    fn index_record(
        &mut self,
        defs: &[IndexDef],
        ns: &str,
        edges: bool,
        id: u128,
        new: Option<(&[String], &Props)>,
        old: Option<(&[String], &Props)>,
        check: bool,
    ) -> Result<()> {
        if !defs.iter().any(|def| def.edges == edges) {
            return Ok(());
        }
        if !edges && defs.iter().any(|def| def.vector.is_some()) {
            self.index_vectors(defs, ns, id, new, old)?;
        }
        let old_entries = old
            .map(|(labels, props)| record_entries(defs, edges, labels, props))
            .unwrap_or_default();
        let new_entries = new
            .map(|(labels, props)| record_entries(defs, edges, labels, props))
            .unwrap_or_default();
        // A full-text index holds the words of its fields, and BM25 their
        // counts: kept as the entries are.
        if !edges {
            for def in defs.iter().filter(|def| !def.edges) {
                let Some(fulltext) = &def.fulltext else {
                    continue;
                };
                let doc = |side: Option<(&[String], &Props)>| {
                    side.filter(|(labels, _)| def.covers(labels))
                        .and_then(|(_, props)| crate::fulltext::doc_of(def, fulltext, props))
                };
                crate::fulltext::update_stats(
                    &self.tx,
                    ns,
                    &def.name,
                    doc(old).as_ref(),
                    doc(new).as_ref(),
                )?;
            }
        }
        let old_set: std::collections::HashSet<(&str, &[u8])> = old_entries
            .iter()
            .map(|(def, bytes)| (def.name.as_str(), bytes.as_slice()))
            .collect();
        let new_set: std::collections::HashSet<(&str, &[u8])> = new_entries
            .iter()
            .map(|(def, bytes)| (def.name.as_str(), bytes.as_slice()))
            .collect();
        let mut table = self.tx.open_table(INDEX_ENTRIES).map_err(storage)?;
        for (def, bytes) in &old_entries {
            if !new_set.contains(&(def.name.as_str(), bytes.as_slice())) {
                table
                    .remove((ns, def.name.as_str(), bytes.as_slice(), id))
                    .map_err(storage)?;
            }
        }
        for (def, bytes) in &new_entries {
            if old_set.contains(&(def.name.as_str(), bytes.as_slice())) {
                continue;
            }
            if def.unique && check {
                let props = new.map(|(_, props)| props).expect("indexed");
                let mut others = Vec::new();
                for item in table
                    .range(
                        (ns, def.name.as_str(), bytes.as_slice(), 0u128)
                            ..=(ns, def.name.as_str(), bytes.as_slice(), u128::MAX),
                    )
                    .map_err(storage)?
                {
                    let (key, _) = item.map_err(storage)?;
                    let other = key.value().3;
                    if other != id {
                        others.push(other);
                    }
                }
                for other in others {
                    // Equal bytes are equal values except for huge
                    // integers: compare the values themselves.
                    if let Some(other_props) = self.live_props(ns, edges, other)?
                        && same_values(def, &other_props, props)
                    {
                        return Err(Error::Invalid(format!(
                            "{} must be unique among {}: {} {} already has {}",
                            def.keys().join(", "),
                            def.describe(),
                            if edges { "edge" } else { "node" },
                            record_name(def, other),
                            values_of(def, props)
                        )));
                    }
                }
            }
            table
                .insert((ns, def.name.as_str(), bytes.as_slice(), id), ())
                .map_err(storage)?;
        }
        Ok(())
    }

    /// The properties of a node, or of an edge that is not retracted.
    fn live_props(&self, ns: &str, edges: bool, id: u128) -> Result<Option<Props>> {
        if edges {
            let table = self.tx.open_table(EDGES).map_err(storage)?;
            Ok(read_record::<Edge>(&table, ns, id)?
                .filter(|edge| !edge.is_retracted())
                .map(|edge| edge.props))
        } else {
            let table = self.tx.open_table(NODES).map_err(storage)?;
            Ok(read_record::<Node>(&table, ns, id)?.map(|node| node.props))
        }
    }

    /// The records an index covers: id, properties, and whether it is
    /// current (a retracted edge is not).
    pub(crate) fn records_of(&self, ns: &str, def: &IndexDef) -> Result<Vec<(u128, Props, bool)>> {
        let mut out = Vec::new();
        if def.edges {
            let table = self.tx.open_table(EDGES).map_err(storage)?;
            for item in table.range((ns, 0u128)..).map_err(storage)? {
                let (key, value) = item.map_err(storage)?;
                if key.value().0 != ns {
                    break;
                }
                let edge: Edge = decode(value.value())?;
                if def.covers(std::slice::from_ref(&edge.label)) {
                    let live = !edge.is_retracted();
                    out.push((edge.id.as_u128(), edge.props, live));
                }
            }
            return Ok(out);
        }
        let labels = self.tx.open_table(NODE_LABELS).map_err(storage)?;
        let mut ids = Vec::new();
        for label in def.labels() {
            ids.extend(nodes_with_label(&labels, ns, label)?);
        }
        drop(labels);
        ids.sort();
        ids.dedup();
        let nodes = self.tx.open_table(NODES).map_err(storage)?;
        for id in ids {
            if let Some(node) = read_record::<Node>(&nodes, ns, id.as_u128())? {
                out.push((id.as_u128(), node.props, true));
            }
        }
        Ok(out)
    }
}

/// The first property of a key that a record lacks, holds as `null`, or
/// holds as a value the index cannot keep (a list, a map).
pub(crate) fn missing_key<'a>(def: &'a IndexDef, props: &Props) -> Option<&'a str> {
    def.keys()
        .into_iter()
        .find(|key| props.get(*key).and_then(component).is_none())
}

/// A node's or an edge's id as text.
pub(crate) fn record_name(def: &IndexDef, id: u128) -> String {
    if def.edges {
        EdgeId::from_u128(id).to_string()
    } else {
        NodeId::from_u128(id).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encoding_keeps_order() {
        let values = [
            json!(-1e9),
            json!(-2.5),
            json!(-1),
            json!(0),
            json!(0.5),
            json!(3),
            json!(1e12),
        ];
        let encoded: Vec<Vec<u8>> = values.iter().map(|v| encode_value(v).unwrap()).collect();
        let mut sorted = encoded.clone();
        sorted.sort();
        assert_eq!(encoded, sorted);
        assert_eq!(encode_value(&json!(-0.0)), encode_value(&json!(0)));
        assert!(encode_value(&json!("a")).unwrap() < encode_value(&json!("ab")).unwrap());
        assert!(encode_value(&json!(true)).unwrap() < encode_value(&json!(1)).unwrap());
        assert!(encode_value(&json!([1])).is_none() && encode_value(&json!(null)).is_none());
    }
}
