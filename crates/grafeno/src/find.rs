//! Looking up nodes by their properties, as rows of a table: conditions on
//! properties, one property to sort by, and pages that go on from a cursor
//! (keyset paging), with the property indexes doing the work.
//!
//! The order of a sort is the order of the property indexes: booleans,
//! then numbers, then strings (each in its own order; `7` and `7.0` are
//! one value), and the nodes without a value an index can hold (missing,
//! `null`, a list or a map) after all of them, whichever the direction.
//! Equal values go in id order (reversed when descending), so that every
//! node has one place and a cursor says where a page ended.

use redb::ReadableTable;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::ops::Bound;

use grafeno_core::{Error, Node, NodeId, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::db::ReadTx;
use crate::index::{IndexDef, IndexKey, IndexRange, RecordKind, component, encode_value};
use crate::store::storage;

/// A condition on one property.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cmp {
    /// Equal (`7` equals `7.0`); `null` equals nothing.
    Eq(Json),
    /// Has a value that is not equal (a missing property is not).
    Ne(Json),
    Lt(Json),
    Lte(Json),
    Gt(Json),
    Gte(Json),
    /// Equal to one of these.
    In(Vec<Json>),
    /// A string that starts with this text.
    Prefix(String),
    /// Has the property (`true`) or has not (`false`, or `null`).
    Exists(bool),
}

/// A property and what its value must be.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    pub key: String,
    pub cmp: Cmp,
}

/// The property a lookup sorts by.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrderBy {
    pub key: String,
    #[serde(default)]
    pub descending: bool,
}

impl OrderBy {
    pub fn asc(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            descending: false,
        }
    }

    pub fn desc(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            descending: true,
        }
    }
}

/// A lookup of nodes: [`ReadTx::find_nodes`].
#[derive(Clone, Debug, PartialEq)]
pub struct Find {
    /// Only nodes with this label (an index serves only a lookup with one).
    pub label: Option<String>,
    /// Every one must hold.
    pub conditions: Vec<Filter>,
    /// By id without one.
    pub order: Option<OrderBy>,
    pub limit: usize,
    /// Where the previous page ended ([`FindPage::next`]).
    pub after: Option<String>,
}

/// The most a page may hold.
pub const MAX_FIND_LIMIT: usize = 10_000;

impl Find {
    pub fn label(label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
            conditions: Vec::new(),
            order: None,
            limit: 100,
            after: None,
        }
    }

    /// Every node of the namespace.
    pub fn any() -> Self {
        Self {
            label: None,
            ..Self::label("")
        }
    }

    pub fn filter(mut self, key: impl Into<String>, cmp: Cmp) -> Self {
        self.conditions.push(Filter {
            key: key.into(),
            cmp,
        });
        self
    }

    pub fn equals(self, key: impl Into<String>, value: Json) -> Self {
        self.filter(key, Cmp::Eq(value))
    }

    pub fn order_by(mut self, order: OrderBy) -> Self {
        self.order = Some(order);
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn after(mut self, cursor: impl Into<String>) -> Self {
        self.after = Some(cursor.into());
        self
    }
}

/// One page of a lookup.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FindPage {
    pub nodes: Vec<Node>,
    /// Where the next page starts; `None` on the last one.
    pub next: Option<String>,
    /// How the page was found: the index it went through, or the scan.
    pub plan: String,
}

/// Where a node sorts: whether it has a value an index can hold, the
/// value's sortable bytes, its id.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Place {
    /// `None`: no value an index can hold (sorts after every value).
    value: Option<Vec<u8>>,
    id: u128,
}

/// What a cursor holds: the value sorted by (when there is one) and the
/// node's id.
#[derive(Serialize, Deserialize)]
struct CursorData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v: Option<Json>,
    id: String,
}

fn place_of(node: &Node, order: Option<&OrderBy>) -> Place {
    Place {
        value: order.and_then(|order| node.props.get(&order.key).and_then(encode_value)),
        id: node.id.as_u128(),
    }
}

/// The order of two places for a sort in `descending` (or not), values
/// before nodes without one.
fn compare(a: &Place, b: &Place, descending: bool) -> Ordering {
    match (&a.value, &b.value) {
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => {
            let ordering = a.id.cmp(&b.id);
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        }
        (Some(x), Some(y)) => {
            let ordering = x.cmp(y).then(a.id.cmp(&b.id));
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        }
    }
}

fn encode_cursor(node: &Node, order: Option<&OrderBy>) -> String {
    let value = order
        .and_then(|order| node.props.get(&order.key))
        .filter(|value| encode_value(value).is_some())
        .cloned();
    let data = CursorData {
        v: value,
        id: format!("{:032x}", node.id.as_u128()),
    };
    let json = serde_json::to_vec(&data).expect("a cursor serializes");
    json.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_cursor(text: &str) -> Result<(Option<Json>, Place)> {
    let bad = || Error::Invalid(format!("{text:?} is not a cursor of this lookup"));
    if text.len() % 2 != 0 || !text.is_ascii() {
        return Err(bad());
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16))
        .collect::<std::result::Result<Vec<u8>, _>>()
        .map_err(|_| bad())?;
    let data: CursorData = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    let id = u128::from_str_radix(&data.id, 16).map_err(|_| bad())?;
    let value = data
        .v
        .as_ref()
        .map(|v| encode_value(v).ok_or_else(bad))
        .transpose()?;
    Ok((data.v, Place { value, id }))
}

/// Whether `value` holds `cmp`.
fn holds(value: Option<&Json>, cmp: &Cmp) -> bool {
    let value = value.filter(|value| !value.is_null());
    let same_kind = |a: &[u8], b: &[u8]| a.first() == b.first();
    let ordered = |bound: &Json, test: fn(Ordering) -> bool| {
        let (Some(have), Some(want)) = (value.and_then(encode_value), encode_value(bound)) else {
            return false;
        };
        same_kind(&have, &want) && test(have.cmp(&want))
    };
    match cmp {
        Cmp::Eq(want) => equal(value, want),
        Cmp::Ne(want) => value.is_some() && !want.is_null() && !equal(value, want),
        Cmp::Lt(bound) => ordered(bound, Ordering::is_lt),
        Cmp::Lte(bound) => ordered(bound, Ordering::is_le),
        Cmp::Gt(bound) => ordered(bound, Ordering::is_gt),
        Cmp::Gte(bound) => ordered(bound, Ordering::is_ge),
        Cmp::In(values) => values.iter().any(|want| equal(value, want)),
        Cmp::Prefix(prefix) => {
            matches!(value, Some(Json::String(text)) if text.starts_with(prefix.as_str()))
        }
        Cmp::Exists(wanted) => value.is_some() == *wanted,
    }
}

fn equal(value: Option<&Json>, want: &Json) -> bool {
    match (value, want) {
        (None, _) | (_, Json::Null) => false,
        (Some(value), want) => match (encode_value(value), encode_value(want)) {
            (Some(a), Some(b)) => a == b,
            // Lists and maps: equal as JSON.
            _ => value == want,
        },
    }
}

fn fits(node: &Node, find: &Find) -> bool {
    find.label
        .as_ref()
        .is_none_or(|label| node.labels.iter().any(|have| have == label))
        && find
            .conditions
            .iter()
            .all(|condition| holds(node.props.get(&condition.key), &condition.cmp))
}

/// How an index can find the candidates: equal values for its first
/// properties, then one range (or several values) on the next.
struct Seek {
    def: IndexDef,
    prefix: Vec<Json>,
    /// Ranges on the property after the prefix; empty: none.
    ranges: Vec<IndexRange>,
}

/// The values a condition on an index's next property can seek.
fn ranges_of(conditions: &[&Cmp]) -> Vec<IndexRange> {
    let mut lower = Bound::Unbounded;
    let mut upper = Bound::Unbounded;
    for cmp in conditions {
        match cmp {
            Cmp::Eq(value) if encode_value(value).is_some() => {
                return vec![IndexRange::Equal(value.clone())];
            }
            // Lists or maps among the values: not through an index.
            Cmp::In(values) if values.iter().all(|value| encode_value(value).is_some()) => {
                return values
                    .iter()
                    .map(|value| IndexRange::Equal(value.clone()))
                    .collect();
            }
            Cmp::Prefix(text) => return vec![IndexRange::Prefix(text.clone())],
            Cmp::Gt(value) => lower = Bound::Excluded(value.clone()),
            Cmp::Gte(value) => lower = Bound::Included(value.clone()),
            Cmp::Lt(value) => upper = Bound::Excluded(value.clone()),
            Cmp::Lte(value) => upper = Bound::Included(value.clone()),
            _ => {}
        }
    }
    if matches!((&lower, &upper), (Bound::Unbounded, Bound::Unbounded)) {
        return Vec::new();
    }
    vec![IndexRange::Between { lower, upper }]
}

impl ReadTx {
    /// One page of the nodes `find` describes, in its order. Indexes on the
    /// label do the work: one on the properties with equal values then the
    /// property sorted by reads the page in order and stops; otherwise the
    /// best index for the conditions finds the candidates, which are
    /// sorted; otherwise the label (or the namespace) is scanned.
    pub fn find_nodes(&self, ns: &str, find: &Find) -> Result<FindPage> {
        if find.limit == 0 || find.limit > MAX_FIND_LIMIT {
            return Err(Error::Invalid(format!(
                "a lookup's limit must be between 1 and {MAX_FIND_LIMIT}"
            )));
        }
        let after = find.after.as_deref().map(decode_cursor).transpose()?;
        if let (Some((value, _)), None) = (&after, &find.order) {
            if value.is_some() {
                return Err(Error::Invalid(
                    "the cursor is of a lookup with another order".into(),
                ));
            }
        }
        let after_place = after.as_ref().map(|(_, place)| place.clone());
        let after_value = after.and_then(|(value, _)| value);
        // Indexes see every record; a reader who sees only some of them
        // scans, and sees what the view lets through.
        let indexed = find.label.is_some() && !self.view().is_some_and(|view| view.per_record(ns));
        let defs: Vec<IndexDef> = if indexed {
            self.index_defs(ns)?
                .into_iter()
                .filter(|def| {
                    def.by_value()
                        && def.kind() == RecordKind::Node
                        && find
                            .label
                            .as_deref()
                            .is_some_and(|label| def.covers(&[label]))
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut nodes = Vec::with_capacity(find.limit + 1);
        let plan;
        if let Some(order) = &find.order {
            let ordering = match self.ordering_index(&defs, find, order) {
                // Conditions beyond the equal values read in order filter
                // the entries: worth it unless they keep few of them, when
                // sorting the candidates another index finds reads less.
                Some((def, prefix))
                    if find.conditions.iter().any(|condition| {
                        !matches!(condition.cmp, Cmp::Eq(_))
                            || !def.keys()[..prefix.len()].contains(&condition.key.as_str())
                    }) =>
                {
                    match self.candidates(ns, find, &defs)? {
                        (Some(ids), _) => {
                            let found = ids.len().max(1) as f64;
                            let entries = self.index_entry_count(ns, &def.name)?.max(1) as f64;
                            let reads = (find.limit + 1) as f64 * entries / found.min(entries);
                            (reads <= found).then_some((def, prefix))
                        }
                        (None, _) => Some((def, prefix)),
                    }
                }
                other => other,
            };
            if let Some((def, prefix)) = ordering {
                plan = format!("index {} in order", def.name);
                self.ordered_page(
                    ns,
                    find,
                    order,
                    &def,
                    &prefix,
                    after_place.as_ref(),
                    after_value.as_ref(),
                    &mut nodes,
                )?;
            } else {
                let (ids, how) = self.candidates(ns, find, &defs)?;
                plan = format!("{how}, sorted");
                let mut best: BinaryHeap<Ranked> = BinaryHeap::new();
                let mut push = |node: Node| {
                    let place = place_of(&node, Some(order));
                    if after_place
                        .as_ref()
                        .is_some_and(|after| compare(&place, after, order.descending).is_le())
                    {
                        return;
                    }
                    best.push(Ranked {
                        place,
                        descending: order.descending,
                        node,
                    });
                    if best.len() > find.limit + 1 {
                        best.pop();
                    }
                };
                self.each_candidate(ns, find, ids, &mut push)?;
                let mut ranked = best.into_vec();
                ranked.sort_by(|a, b| compare(&a.place, &b.place, order.descending));
                nodes.extend(ranked.into_iter().map(|ranked| ranked.node));
            }
        } else {
            let (ids, how) = self.candidates(ns, find, &defs)?;
            plan = format!("{how}, by id");
            let after = after_place.map(|place| place.id);
            match ids {
                Some(mut ids) => {
                    ids.sort_unstable();
                    ids.dedup();
                    let from = after.map_or(0, |after| ids.partition_point(|id| *id <= after));
                    for id in &ids[from..] {
                        if nodes.len() > find.limit {
                            break;
                        }
                        if let Some(node) = self.node(ns, NodeId::from_u128(*id))? {
                            if fits(&node, find) {
                                nodes.push(node);
                            }
                        }
                    }
                }
                None => {
                    let mut cursor = after.map(NodeId::from_u128);
                    'pages: loop {
                        let page = self.nodes_page(ns, find.label.as_deref(), cursor, 512)?;
                        let full = page.len() == 512;
                        cursor = page.last().map(|node| node.id).or(cursor);
                        for node in page {
                            if fits(&node, find) {
                                nodes.push(node);
                                if nodes.len() > find.limit {
                                    break 'pages;
                                }
                            }
                        }
                        if !full {
                            break;
                        }
                    }
                }
            }
        }
        let next = if nodes.len() > find.limit {
            nodes.truncate(find.limit);
            nodes
                .last()
                .map(|node| encode_cursor(node, find.order.as_ref()))
        } else {
            None
        };
        Ok(FindPage { nodes, next, plan })
    }

    /// An index that holds the candidates in the order asked: equal values
    /// for its first properties, and the sorted property last.
    fn ordering_index(
        &self,
        defs: &[IndexDef],
        find: &Find,
        order: &OrderBy,
    ) -> Option<(IndexDef, Vec<Json>)> {
        let mut best: Option<(IndexDef, Vec<Json>)> = None;
        for def in defs {
            let keys = def.keys();
            let (last, first) = keys.split_last().expect("an index has a property");
            if *last != order.key {
                continue;
            }
            let mut prefix = Vec::new();
            for key in first {
                let value = find
                    .conditions
                    .iter()
                    .find_map(|condition| match &condition.cmp {
                        Cmp::Eq(value)
                            if condition.key == *key && encode_value(value).is_some() =>
                        {
                            Some(value.clone())
                        }
                        _ => None,
                    });
                match value {
                    Some(value) => prefix.push(value),
                    None => break,
                }
            }
            if prefix.len() != first.len() {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(_, have)| prefix.len() > have.len())
            {
                best = Some((def.clone(), prefix));
            }
        }
        best
    }

    /// The candidates an index finds for the conditions (`None`: scan),
    /// and how they were found.
    fn candidates(
        &self,
        ns: &str,
        find: &Find,
        defs: &[IndexDef],
    ) -> Result<(Option<Vec<u128>>, String)> {
        // `IN []` matches nothing.
        if find
            .conditions
            .iter()
            .any(|condition| matches!(&condition.cmp, Cmp::In(values) if values.is_empty()))
        {
            return Ok((Some(Vec::new()), "nothing (IN [])".into()));
        }
        let mut best: Option<(usize, Seek)> = None;
        for def in defs {
            let mut prefix = Vec::new();
            let mut ranges = Vec::new();
            for key in def.keys() {
                let on_key: Vec<&Cmp> = find
                    .conditions
                    .iter()
                    .filter(|condition| condition.key == key)
                    .map(|condition| &condition.cmp)
                    .collect();
                let found = ranges_of(&on_key);
                match found.as_slice() {
                    [IndexRange::Equal(value)] => prefix.push(value.clone()),
                    [] => break,
                    _ => {
                        ranges = found;
                        break;
                    }
                }
            }
            // A composite index holds only the nodes with every one of its
            // properties: it may find the candidates only when the
            // conditions ask for a value on each.
            if def.composite_keys()
                && !def.keys().iter().all(|key| {
                    let on_key: Vec<&Cmp> = find
                        .conditions
                        .iter()
                        .filter(|condition| condition.key == *key)
                        .map(|condition| &condition.cmp)
                        .collect();
                    !ranges_of(&on_key).is_empty()
                })
            {
                continue;
            }
            // A simple index takes its one value as a range.
            if !def.composite_keys() && prefix.len() == 1 {
                ranges = vec![IndexRange::Equal(prefix.remove(0))];
            }
            let score = prefix.len() * 2 + usize::from(!ranges.is_empty());
            if score > 0 && best.as_ref().is_none_or(|(have, _)| score > *have) {
                best = Some((
                    score,
                    Seek {
                        def: def.clone(),
                        prefix,
                        ranges,
                    },
                ));
            }
        }
        let Some((_, seek)) = best else {
            return Ok((
                None,
                match &find.label {
                    Some(label) => format!("scan of :{label}"),
                    None => "scan of every node".into(),
                },
            ));
        };
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        let ranges: Vec<Option<&IndexRange>> = if seek.ranges.is_empty() {
            vec![None]
        } else {
            seek.ranges.iter().map(Some).collect()
        };
        for range in ranges {
            for id in self.index_seek_nodes(ns, &seek.def.name, &seek.prefix, range, usize::MAX)? {
                if seen.insert(id) {
                    ids.push(id.as_u128());
                }
            }
        }
        Ok((Some(ids), format!("index {}", seek.def.name)))
    }

    fn each_candidate(
        &self,
        ns: &str,
        find: &Find,
        ids: Option<Vec<u128>>,
        take: &mut dyn FnMut(Node),
    ) -> Result<()> {
        match ids {
            Some(ids) => {
                for id in ids {
                    if let Some(node) = self.node(ns, NodeId::from_u128(id))? {
                        if fits(&node, find) {
                            take(node);
                        }
                    }
                }
            }
            None => {
                let mut cursor = None;
                loop {
                    let page = self.nodes_page(ns, find.label.as_deref(), cursor, 512)?;
                    let full = page.len() == 512;
                    cursor = page.last().map(|node| node.id).or(cursor);
                    for node in page {
                        if fits(&node, find) {
                            take(node);
                        }
                    }
                    if !full {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// A page read from an index in order: the entries after the cursor's,
    /// then (once they run out, if the index does not hold every node of
    /// the label) the nodes without a value, by id.
    #[allow(clippy::too_many_arguments)]
    fn ordered_page(
        &self,
        ns: &str,
        find: &Find,
        order: &OrderBy,
        def: &IndexDef,
        prefix: &[Json],
        after: Option<&Place>,
        after_value: Option<&Json>,
        nodes: &mut Vec<Node>,
    ) -> Result<()> {
        let wanted = find.limit + 1;
        // A cursor past the values goes straight to the nodes without one.
        if after.is_none_or(|after| after.value.is_some()) {
            let start = match (after, after_value) {
                (Some(after), Some(value)) => Some((value.clone(), after.id)),
                _ => None,
            };
            let mut position = start
                .map(|(value, id)| -> Result<(Vec<u8>, u128)> {
                    let mut bytes = Vec::new();
                    if def.composite_keys() {
                        for value in prefix {
                            bytes.extend(component(value).ok_or_else(|| {
                                Error::Invalid("a value an index cannot hold".into())
                            })?);
                        }
                        bytes.extend(component(&value).ok_or_else(|| {
                            Error::Invalid("a value an index cannot hold".into())
                        })?);
                    } else {
                        bytes = encode_value(&value)
                            .ok_or_else(|| Error::Invalid("a value an index cannot hold".into()))?;
                    }
                    Ok((bytes, id))
                })
                .transpose()?;
            loop {
                let entries = self.index_entries_in_order(
                    ns,
                    def,
                    prefix,
                    order.descending,
                    position.as_ref().map(|(bytes, id)| (bytes.as_slice(), *id)),
                    256,
                )?;
                let done = entries.len() < 256;
                for (bytes, id) in &entries {
                    if let Some(node) = self.node(ns, NodeId::from_u128(*id))? {
                        if fits(&node, find) {
                            nodes.push(node);
                            if nodes.len() >= wanted {
                                return Ok(());
                            }
                        }
                    }
                    position = Some((bytes.clone(), *id));
                }
                if done {
                    break;
                }
            }
        }
        if self.index_holds_label(ns, def, find.label.as_deref())? {
            return Ok(());
        }
        // The nodes the index does not hold, by id.
        let mut rest = Vec::new();
        self.each_candidate(ns, find, None, &mut |node| {
            if place_of(&node, Some(order)).value.is_none() {
                rest.push(node);
            }
        })?;
        rest.sort_by(|a, b| {
            let ordering = a.id.cmp(&b.id);
            if order.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
        for node in rest {
            let place = place_of(&node, Some(order));
            if after.is_some_and(|after| compare(&place, after, order.descending).is_le()) {
                continue;
            }
            nodes.push(node);
            if nodes.len() >= wanted {
                break;
            }
        }
        Ok(())
    }

    /// Up to `limit` entries of `def` with `prefix`, in order (reversed
    /// when `descending`), after `after` (an entry's bytes and id).
    fn index_entries_in_order(
        &self,
        ns: &str,
        def: &IndexDef,
        prefix: &[Json],
        descending: bool,
        after: Option<(&[u8], u128)>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, u128)>> {
        self.entries_in_order(ns, def, prefix, None, descending, after, limit)
    }

    /// [`ReadTx::index_entries_in_order`], only values of `kind` when
    /// given.
    #[allow(clippy::too_many_arguments)]
    fn entries_in_order(
        &self,
        ns: &str,
        def: &IndexDef,
        prefix: &[Json],
        kind: Option<ValueKind>,
        descending: bool,
        after: Option<(&[u8], u128)>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, u128)>> {
        let mut low = Vec::new();
        if def.composite_keys() {
            for value in prefix {
                match component(value) {
                    Some(bytes) => low.extend(bytes),
                    None => return Ok(Vec::new()),
                }
            }
        } else if !prefix.is_empty() {
            return Err(Error::Invalid(format!(
                "{} is on one property: it takes no values before the sorted one",
                def.name
            )));
        }
        // Every entry with the prefix (of the kind) is at or above `low`
        // and below `high`: a kind's byte, or 0xFF, which no kind reaches.
        let mut high = low.clone();
        match kind {
            Some(kind) => {
                low.push(kind.tag());
                high.push(kind.tag() + 1);
            }
            None => high.push(0xFF),
        }
        let name = def.name.as_str();
        let lower: Bound<(&str, &str, &[u8], u128)> = match (after, descending) {
            (Some((bytes, id)), false) if bytes >= low.as_slice() => {
                Bound::Excluded((ns, name, bytes, id))
            }
            _ => Bound::Included((ns, name, low.as_slice(), 0u128)),
        };
        let upper: Bound<(&str, &str, &[u8], u128)> = match (after, descending) {
            (Some((bytes, id)), true) if bytes < high.as_slice() => {
                Bound::Excluded((ns, name, bytes, id))
            }
            _ => Bound::Excluded((ns, name, high.as_slice(), 0u128)),
        };
        if let (Some((bytes, _)), false) = (after, descending) {
            if bytes >= high.as_slice() {
                return Ok(Vec::new());
            }
        }
        if let (Some((bytes, _)), true) = (after, descending) {
            if bytes < low.as_slice() {
                return Ok(Vec::new());
            }
        }
        let range = self.index_entries.range((lower, upper)).map_err(storage)?;
        let mut out = Vec::with_capacity(limit);
        let mut push = |item: std::result::Result<
            (redb::AccessGuard<'_, IndexKey>, redb::AccessGuard<'_, ()>),
            redb::StorageError,
        >|
         -> Result<bool> {
            let (key, _) = item.map_err(storage)?;
            let (_, _, bytes, id) = key.value();
            out.push((bytes.to_vec(), id));
            Ok(out.len() < limit)
        };
        if descending {
            for item in range.rev() {
                if !push(item)? {
                    break;
                }
            }
        } else {
            for item in range {
                if !push(item)? {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Whether `def` holds every node with `label` (it may then stand for
    /// the label in a sort): a key constraint does, and an index whose
    /// entries are as many as the nodes of its labels. The count is kept
    /// per commit.
    pub(crate) fn index_holds_label(
        &self,
        ns: &str,
        def: &IndexDef,
        label: Option<&str>,
    ) -> Result<bool> {
        if label.is_none() {
            return Ok(false);
        }
        if def.required {
            return Ok(true);
        }
        let counts = crate::stats::label_counts(&self.label_counts, ns)?;
        let nodes: u64 = def
            .labels()
            .iter()
            .map(|label| counts.get(*label).copied().unwrap_or(0))
            .sum();
        let entries = self.index_entry_count(ns, &def.name)?;
        Ok(entries == nodes)
    }

    /// How many entries index `name` of `ns` holds, kept per commit.
    pub(crate) fn index_entry_count(&self, ns: &str, name: &str) -> Result<u64> {
        if let Some(generation) = self.generation {
            if let Some(count) = self.shared.index_count(generation, ns, name) {
                return Ok(count);
            }
        }
        let count = crate::index::count_entries(&self.index_entries, ns, name)?;
        if let Some(generation) = self.generation {
            self.shared.keep_index_count(generation, ns, name, count);
        }
        Ok(count)
    }
}

/// The kinds of value a property index holds, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Bool,
    Number,
    String,
}

impl ValueKind {
    fn tag(self) -> u8 {
        match self {
            ValueKind::Bool => 1,
            ValueKind::Number => 2,
            ValueKind::String => 3,
        }
    }
}

/// An entry of a property index: where a node is in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexPosition {
    bytes: Vec<u8>,
    pub id: NodeId,
}

impl IndexPosition {
    /// Whether both entries hold the same values (`7` and `7.0` are one).
    pub fn same_values(&self, other: &IndexPosition) -> bool {
        self.bytes == other.bytes
    }
}

impl ReadTx {
    /// Up to `limit` entries of the node index `name` of `ns`, in its order
    /// (reversed when `descending`), after `after`: those with equal values
    /// `prefix` for its first properties (a composite index), and, with
    /// `kind`, a value of that kind for the next one. Equal values go in id
    /// order. It reads the index as stored: a reader with a view that
    /// checks records one by one gets nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn index_scan_nodes(
        &self,
        ns: &str,
        name: &str,
        prefix: &[Json],
        kind: Option<ValueKind>,
        descending: bool,
        after: Option<&IndexPosition>,
        limit: usize,
    ) -> Result<Vec<IndexPosition>> {
        self.shared.index_uses.note(ns, name);
        if self
            .view()
            .is_some_and(|view| view.per_record(ns) || !view.reads(ns))
        {
            return Ok(Vec::new());
        }
        let def = self
            .index_defs(ns)?
            .into_iter()
            .find(|def| def.name == name)
            .ok_or_else(|| Error::Invalid(format!("there is no index named {name:?}")))?;
        if def.kind() != RecordKind::Node || !def.by_value() {
            return Err(Error::Invalid(format!(
                "{name} is not an index of nodes by value"
            )));
        }
        Ok(self
            .entries_in_order(
                ns,
                &def,
                prefix,
                kind,
                descending,
                after.map(|after| (after.bytes.as_slice(), after.id.as_u128())),
                limit,
            )?
            .into_iter()
            .map(|(bytes, id)| IndexPosition {
                bytes,
                id: NodeId::from_u128(id),
            })
            .collect())
    }

    /// Whether the index `name` of `ns` holds every node of its labels:
    /// then a sort by its last property may read it in place of the label.
    /// A key constraint does by its rule; otherwise its entries are counted
    /// (once per commit) against the nodes of its labels.
    pub fn index_holds_every_node(&self, ns: &str, name: &str) -> Result<bool> {
        let Some(def) = self
            .index_defs(ns)?
            .into_iter()
            .find(|def| def.name == name)
        else {
            return Ok(false);
        };
        let label = def.label.clone();
        self.index_holds_label(ns, &def, Some(&label))
    }
}

/// A node in the heap of the best ones: the worst on top.
struct Ranked {
    place: Place,
    descending: bool,
    node: Node,
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.place == other.place
    }
}

impl Eq for Ranked {}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        compare(&self.place, &other.place, self.descending)
    }
}
