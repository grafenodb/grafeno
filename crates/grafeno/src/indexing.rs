//! Indexing policies per namespace (docs/roadmap/
//! STORAGE_AND_INDEXING_OPTIMIZATION.md, 6.6 and phase 6).
//!
//! A namespace's policy says which of its node properties get indexes
//! without a `CREATE INDEX`:
//!
//! - **automatic**: when a write gives a label a property it has not had,
//!   the value decides (a string of up to 256 characters, a number or a
//!   boolean: a property index, which serves equality, ranges and
//!   prefixes; a longer string: a full-text index); lists, maps and nulls
//!   get none.
//! - **manual**: none; indexes come from `CREATE INDEX` or from the
//!   `indexes` of a write.
//! - **hybrid**: automatic, with rules and exclusions. Precedence, first
//!   that applies: a rule for the label and property, an exclusion (for
//!   every label or for this one), a rule for the property in any label,
//!   the automatic decision.
//! - **default**: what the database is told for namespaces without a mode
//!   of their own ([`crate::OpenOptions::indexing`]: manual for the
//!   library, automatic for a server). A namespace created by an earlier
//!   release has no policy: manual, until a migration gives it one.
//!
//! Each pair of label and property is decided once, at its first value,
//! and the decision kept with the namespace (as a state entry, replicated
//! like any), so that a dropped automatic index is not made again and a
//! write looks up a set, not a policy. Automatic indexes are ordinary
//! [`crate::IndexDef`]s with `origin: auto`: listed, replicated, exported,
//! checked and dropped like the others. A pair is decided before the record
//! that brings it is written, so the new index is filled from the records
//! that already have the property, which in a namespace that has always
//! been automatic are none: a write does not wait for a build. Turning a
//! namespace with data automatic records the pairs it already holds as
//! decided, so nothing is built behind a write; the migration's
//! `--indexing` plans indexes for existing data.
//!
//! Bulk loads (phase 7) hold their decisions back: the pairs a load brings
//! are gathered as it writes ([`DeferredIndexes`]) and decided once, after
//! it, when each new index is filled in one pass over the records, the
//! indexes of a label sharing one read of them. A load that fails before
//! the end leaves its pairs undecided; the next write that brings one
//! decides it and fills its index from the records already there.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use grafeno_core::{Error, Node, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::db::WriteTx;
use crate::index::IndexDef;

/// State entries of a namespace's decisions.
pub(crate) const DECISIONS: &str = "indexing";
/// A string longer than this gets a full-text index, not a property one.
pub const MAX_EXACT_CHARS: usize = 256;
/// Automatic indexes a label may have at most, by default.
pub const MAX_AUTO_PER_LABEL: usize = 32;

/// How a namespace decides its indexes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexingMode {
    /// The database's setting ([`crate::OpenOptions::indexing`]).
    #[default]
    Default,
    Automatic,
    Manual,
    Hybrid,
}

/// What a property may be indexed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexKind {
    /// Equality and prefixes: a property index.
    Exact,
    /// Ranges: the same property index (values are kept in order).
    Range,
    /// Words: a full-text index.
    Fulltext,
    /// Nothing (an exclusion, as a rule).
    None,
}

/// The rules of one label.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelRule {
    /// What each property is indexed for.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Vec<IndexKind>>,
    /// Properties of this label never indexed automatically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

/// A namespace's indexing policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexingPolicy {
    #[serde(default)]
    pub mode: IndexingMode,
    /// Properties never indexed automatically, in any label.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Rules per label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, LabelRule>,
    /// Rules for a property in any label.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, Vec<IndexKind>>,
    /// Automatic indexes per label at most ([`MAX_AUTO_PER_LABEL`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_label: Option<usize>,
}

impl IndexingPolicy {
    pub fn with_mode(mode: IndexingMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Rejects what makes no sense: rules in a manual policy, a kind and
    /// `none` together.
    pub fn validate(&self) -> Result<()> {
        let rules =
            !self.labels.is_empty() || !self.properties.is_empty() || !self.exclude.is_empty();
        if self.mode == IndexingMode::Manual && rules {
            return Err(Error::Invalid(
                "a manual indexing policy has no rules: use hybrid".into(),
            ));
        }
        let kinds = self.properties.values().chain(
            self.labels
                .values()
                .flat_map(|rule| rule.properties.values()),
        );
        for kinds in kinds {
            if kinds.contains(&IndexKind::None) && kinds.len() > 1 {
                return Err(Error::Invalid(
                    "a rule is `none` or kinds of index, not both".into(),
                ));
            }
        }
        if self.max_per_label == Some(0) {
            return Err(Error::Invalid("max_per_label must be at least 1".into()));
        }
        Ok(())
    }
}

/// Where an index came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexOrigin {
    /// `CREATE INDEX`, or the `indexes` of a write.
    #[default]
    Manual,
    /// The namespace's policy.
    Auto,
}

impl IndexOrigin {
    pub fn is_manual(&self) -> bool {
        *self == IndexOrigin::Manual
    }
}

/// Whether a property is the engine's own, never indexed automatically.
fn internal(property: &str) -> bool {
    property.starts_with('@') || property.starts_with('_')
}

/// What the automatic policy does with `value`.
pub fn automatic(value: &Json) -> Vec<IndexKind> {
    match value {
        Json::String(text) if text.chars().count() <= MAX_EXACT_CHARS => vec![IndexKind::Exact],
        Json::String(_) => vec![IndexKind::Fulltext],
        Json::Number(_) => vec![IndexKind::Exact, IndexKind::Range],
        Json::Bool(_) => vec![IndexKind::Exact],
        // Lists need entries per item (phase 7); maps, nested properties;
        // nulls, nothing.
        Json::Array(_) | Json::Object(_) | Json::Null => Vec::new(),
    }
}

/// What `policy`, in effective `mode`, indexes `property` of `label` for,
/// its first value being `value` (6.6's precedence). Empty: nothing.
pub fn decide(
    policy: &IndexingPolicy,
    mode: IndexingMode,
    label: &str,
    property: &str,
    value: &Json,
) -> Vec<IndexKind> {
    if matches!(mode, IndexingMode::Manual | IndexingMode::Default) || internal(property) {
        return Vec::new();
    }
    let rule = policy.labels.get(label);
    let kinds = if let Some(kinds) = rule.and_then(|rule| rule.properties.get(property)) {
        kinds.clone()
    } else if policy.exclude.iter().any(|p| p == property)
        || rule.is_some_and(|rule| rule.exclude.iter().any(|p| p == property))
    {
        Vec::new()
    } else if let Some(kinds) = policy.properties.get(property) {
        kinds.clone()
    } else {
        automatic(value)
    };
    let mut kinds: Vec<IndexKind> = kinds
        .into_iter()
        .filter(|k| *k != IndexKind::None)
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

/// The pairs of label and property a bulk load brought that their
/// namespaces had not decided, with the first value of each: decided
/// together, after the load ([`WriteTx::apply_deferred_indexes`]).
#[derive(Clone, Debug, Default)]
pub struct DeferredIndexes {
    /// Per namespace, `label \u{1f} property` to (label, property, value).
    pairs: BTreeMap<String, BTreeMap<String, (String, String, Json)>>,
}

impl DeferredIndexes {
    pub fn is_empty(&self) -> bool {
        self.pairs.values().all(BTreeMap::is_empty)
    }

    /// How many pairs wait.
    pub fn len(&self) -> usize {
        self.pairs.values().map(BTreeMap::len).sum()
    }
}

/// A namespace's policy and decisions, as a write uses them.
pub(crate) struct NsIndexing {
    pub(crate) mode: IndexingMode,
    pub(crate) policy: IndexingPolicy,
    /// `label \u{1f} property`.
    pub(crate) decided: HashSet<String>,
}

/// The policies and decisions writes have read, per namespace: forgotten
/// by every commit that changes them.
#[derive(Default)]
pub(crate) struct Cache {
    namespaces: Mutex<HashMap<String, Arc<NsIndexing>>>,
    /// The mode of namespaces without one of their own.
    default_mode: Mutex<IndexingMode>,
}

impl Cache {
    pub(crate) fn set_default(&self, mode: IndexingMode) {
        *self.default_mode.lock().unwrap_or_else(|e| e.into_inner()) = match mode {
            IndexingMode::Default => IndexingMode::Manual,
            other => other,
        };
        self.clear();
    }

    pub(crate) fn default_mode(&self) -> IndexingMode {
        match *self.default_mode.lock().unwrap_or_else(|e| e.into_inner()) {
            IndexingMode::Default => IndexingMode::Manual,
            other => other,
        }
    }

    pub(crate) fn forget(&self, ns: &str) {
        self.namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(ns);
    }

    pub(crate) fn clear(&self) {
        self.namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn get(&self, ns: &str) -> Option<Arc<NsIndexing>> {
        self.namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(ns)
            .cloned()
    }

    fn put(&self, ns: &str, state: Arc<NsIndexing>) {
        self.namespaces
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ns.to_owned(), state);
    }
}

/// How many reads used each index since the database was opened (6.5).
#[derive(Default)]
pub(crate) struct Uses {
    counts: Mutex<HashMap<String, u64>>,
}

impl Uses {
    pub(crate) fn note(&self, ns: &str, name: &str) {
        *self
            .counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(format!("{ns}\u{1f}{name}"))
            .or_default() += 1;
    }

    pub(crate) fn get(&self, ns: &str, name: &str) -> u64 {
        self.counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&format!("{ns}\u{1f}{name}"))
            .copied()
            .unwrap_or(0)
    }
}

fn pair(label: &str, property: &str) -> String {
    format!("{label}\u{1f}{property}")
}

/// A decision as kept.
#[derive(Serialize, Deserialize)]
struct Decision {
    kinds: Vec<IndexKind>,
    /// The pair was in the namespace before its policy indexed anything:
    /// left to the migration.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    existing: bool,
}

impl WriteTx {
    /// The namespace's effective mode and decisions, from the cache when
    /// nothing in this transaction changed them.
    pub(crate) fn ns_indexing(&self, ns: &str) -> Result<Arc<NsIndexing>> {
        let clean = !self.indexing_dirty.contains(ns);
        if clean && let Some(found) = self.shared.indexing.get(ns) {
            return Ok(found);
        }
        let policy = crate::namespace::policy_of(&self.tx, ns)?;
        let (mode, policy) = match policy {
            // An earlier release's namespace: manual until migrated (Q3).
            None => (
                IndexingMode::Manual,
                IndexingPolicy::with_mode(IndexingMode::Manual),
            ),
            Some(policy) => (
                match policy.mode {
                    IndexingMode::Default => self.shared.indexing.default_mode(),
                    other => other,
                },
                policy,
            ),
        };
        let decided = self
            .state_entries(ns, DECISIONS, "", usize::MAX)?
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let state = Arc::new(NsIndexing {
            mode,
            policy,
            decided,
        });
        if clean {
            self.shared.indexing.put(ns, state.clone());
        }
        Ok(state)
    }

    /// Before `node` is written: decides the pairs of label and property
    /// it brings that its namespace has not decided, and creates the
    /// indexes the policy wants for them.
    pub(crate) fn decide_indexes(&mut self, ns: &str, node: &Node) -> Result<()> {
        if self.applying || !self.deciding_indexes || node.props.is_empty() {
            return Ok(());
        }
        let state = self.ns_indexing(ns)?;
        if state.mode == IndexingMode::Manual {
            return Ok(());
        }
        let mut new: Vec<(&str, &str, &Json)> = Vec::new();
        for label in &node.labels {
            for (property, value) in &node.props {
                let key = pair(label, property);
                if !state.decided.contains(&key)
                    && !self.indexing_decided.contains(&key)
                    && !value.is_null()
                {
                    new.push((label.as_str(), property.as_str(), value));
                }
            }
        }
        if new.is_empty() {
            return Ok(());
        }
        // A bulk load: gathered, decided after it.
        if let Some(deferred) = self.deferred_indexes.as_mut() {
            let pairs = deferred.pairs.entry(ns.to_owned()).or_default();
            for (label, property, value) in new {
                pairs
                    .entry(pair(label, property))
                    .or_insert_with(|| (label.to_owned(), property.to_owned(), value.clone()));
            }
            return Ok(());
        }
        for (label, property, value) in new {
            let kinds = decide(&state.policy, state.mode, label, property, value);
            self.declare_indexes(
                ns,
                label,
                property,
                &kinds,
                crate::IndexOrigin::Auto,
                &state.policy,
            )?;
            self.record_decision(ns, label, property, &kinds, false)?;
        }
        Ok(())
    }

    fn record_decision(
        &mut self,
        ns: &str,
        label: &str,
        property: &str,
        kinds: &[IndexKind],
        existing: bool,
    ) -> Result<()> {
        let key = pair(label, property);
        let value = serde_json::to_vec(&Decision {
            kinds: kinds.to_vec(),
            existing,
        })
        .map_err(|e| Error::Serialization(e.to_string()))?;
        self.indexing_decided.insert(key.clone());
        self.put_state(ns, DECISIONS, &key, &value)?;
        self.indexing_dirty.insert(ns.to_owned());
        Ok(())
    }

    /// Creates, for `property` of `label`, the indexes `kinds` call for
    /// that no index (of any origin) gives already. Automatic ones stop at
    /// the label's limit.
    pub(crate) fn declare_indexes(
        &mut self,
        ns: &str,
        label: &str,
        property: &str,
        kinds: &[IndexKind],
        origin: crate::IndexOrigin,
        policy: &IndexingPolicy,
    ) -> Result<Vec<String>> {
        let mut made = Vec::new();
        if kinds.is_empty() {
            return Ok(made);
        }
        let defs = self.index_defs(ns)?;
        let on_label =
            |def: &&IndexDef| !def.edges && !def.hyperedges && def.labels().contains(&label);
        let exact = defs.iter().filter(on_label).any(|def| {
            def.key == property && def.fulltext.is_none() && def.vector.is_none() && !def.fuzzy
        });
        let words = defs
            .iter()
            .filter(on_label)
            .any(|def| def.fulltext.is_some() && def.keys().contains(&property));
        let mut automatic = defs
            .iter()
            .filter(on_label)
            .filter(|def| def.origin == crate::IndexOrigin::Auto)
            .count();
        let limit = policy.max_per_label.unwrap_or(MAX_AUTO_PER_LABEL);
        let wanted = [
            (
                !exact
                    && kinds
                        .iter()
                        .any(|k| matches!(k, IndexKind::Exact | IndexKind::Range)),
                false,
            ),
            (!words && kinds.contains(&IndexKind::Fulltext), true),
        ];
        let prefix = if origin == crate::IndexOrigin::Auto {
            "auto_"
        } else {
            ""
        };
        for (want, fulltext) in wanted {
            if !want {
                continue;
            }
            if origin == crate::IndexOrigin::Auto && automatic >= limit {
                break;
            }
            let mut def = IndexDef::new(label, property);
            def.name = format!(
                "{prefix}{label}_{property}{}",
                if fulltext { "_text" } else { "" }
            );
            if defs.iter().any(|other| other.name == def.name) {
                continue;
            }
            if fulltext {
                def.fulltext = Some(crate::fulltext::Fulltext::default());
            }
            def.origin = origin;
            made.push(def.name.clone());
            self.create_index(ns, def)?;
            automatic += usize::from(origin == crate::IndexOrigin::Auto);
        }
        Ok(made)
    }

    /// Sets a namespace's indexing policy. Indexes already there stay, and
    /// none is built: with data in the namespace, the pairs of label and
    /// property it holds are recorded as decided (`existing`), and only
    /// pairs it has not seen get automatic indexes from now on; the
    /// migration's `--indexing` plans indexes for what is there.
    pub fn set_namespace_indexing(&mut self, ns: &str, policy: IndexingPolicy) -> Result<()> {
        policy.validate()?;
        crate::namespace::set_policy(self, ns, Some(policy.clone()))?;
        self.indexing_dirty.insert(ns.to_owned());
        let mode = match policy.mode {
            IndexingMode::Default => self.shared.indexing.default_mode(),
            other => other,
        };
        if mode == IndexingMode::Manual {
            return Ok(());
        }
        for (label, property) in self.pairs_of(ns)? {
            let key = pair(&label, &property);
            if self.state(ns, DECISIONS, &key)?.is_none() {
                self.record_decision(ns, &label, &property, &[], true)?;
            }
        }
        Ok(())
    }

    /// Every pair of label and property the nodes of `ns` hold, with the
    /// first value seen (by node id).
    pub(crate) fn pairs_with_values(&self, ns: &str) -> Result<BTreeMap<(String, String), Json>> {
        let mut out: BTreeMap<(String, String), Json> = BTreeMap::new();
        let mut after = None;
        loop {
            let page = self.nodes_page(ns, None, after, 1024)?;
            let done = page.len() < 1024;
            after = page.last().map(|node| node.id);
            for node in page {
                for label in &node.labels {
                    for (property, value) in &node.props {
                        if !value.is_null() {
                            out.entry((label.clone(), property.clone()))
                                .or_insert_with(|| value.clone());
                        }
                    }
                }
            }
            if done {
                break;
            }
        }
        Ok(out)
    }

    fn pairs_of(&self, ns: &str) -> Result<Vec<(String, String)>> {
        Ok(self.pairs_with_values(ns)?.into_keys().collect())
    }

    /// What the namespace's policy wants for the data it holds: the
    /// indexes it would create, as (label, property, kinds), for pairs
    /// without an index that gives them. For the migration.
    pub(crate) fn planned_indexes(
        &mut self,
        ns: &str,
    ) -> Result<Vec<(String, String, Vec<IndexKind>)>> {
        self.indexing_dirty.insert(ns.to_owned());
        let state = self.ns_indexing(ns)?;
        let mut out = Vec::new();
        for ((label, property), value) in self.pairs_with_values(ns)? {
            let kinds = decide(&state.policy, state.mode, &label, &property, &value);
            if !kinds.is_empty() {
                out.push((label, property, kinds));
            }
        }
        Ok(out)
    }

    /// The migration's indexing step for one pair: its indexes, and the
    /// decision (no longer `existing`).
    pub(crate) fn apply_planned(
        &mut self,
        ns: &str,
        label: &str,
        property: &str,
        kinds: &[IndexKind],
    ) -> Result<Vec<String>> {
        let state = self.ns_indexing(ns)?;
        let made = self.declare_indexes(
            ns,
            label,
            property,
            kinds,
            crate::IndexOrigin::Auto,
            &state.policy,
        )?;
        self.record_decision(ns, label, property, kinds, false)?;
        Ok(made)
    }

    /// Holds this transaction's automatic decisions back, gathering them in
    /// `deferred` (what earlier transactions of the same load gathered)
    /// until [`WriteTx::take_deferred_indexes`].
    pub fn defer_index_decisions(&mut self, deferred: DeferredIndexes) {
        self.deferred_indexes = Some(deferred);
    }

    /// The decisions held back so far; writes decide again from now on.
    pub fn take_deferred_indexes(&mut self) -> DeferredIndexes {
        self.deferred_indexes.take().unwrap_or_default()
    }

    /// Decides the pairs a bulk load held back, as its writes would have,
    /// and creates the indexes their policies want, filled from the
    /// records: the indexes of one label share a single read of them.
    /// Pairs decided meanwhile (by another write) are left as they are.
    /// Returns the indexes created.
    pub fn apply_deferred_indexes(&mut self, deferred: DeferredIndexes) -> Result<Vec<String>> {
        if deferred.is_empty() {
            return Ok(Vec::new());
        }
        let outer = self.deferred_indexes.take();
        let made = self.with_build_records(|tx| {
            let mut made = Vec::new();
            for (ns, pairs) in deferred.pairs {
                tx.indexing_dirty.insert(ns.clone());
                let state = tx.ns_indexing(&ns)?;
                if state.mode == IndexingMode::Manual {
                    continue;
                }
                for (key, (label, property, value)) in pairs {
                    if state.decided.contains(&key) || tx.indexing_decided.contains(&key) {
                        continue;
                    }
                    let kinds = decide(&state.policy, state.mode, &label, &property, &value);
                    made.extend(tx.declare_indexes(
                        &ns,
                        &label,
                        &property,
                        &kinds,
                        crate::IndexOrigin::Auto,
                        &state.policy,
                    )?);
                    tx.record_decision(&ns, &label, &property, &kinds, false)?;
                }
            }
            Ok(made)
        });
        self.deferred_indexes = outer;
        made
    }

    /// Runs `write` with its automatic decisions held back, then applies
    /// them (in the same transaction). Inside another such write, the outer
    /// one applies them.
    pub fn with_deferred_indexes<T>(
        &mut self,
        write: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if self.deferred_indexes.is_some() {
            return write(self);
        }
        self.deferred_indexes = Some(DeferredIndexes::default());
        let out = write(self);
        let deferred = self.take_deferred_indexes();
        let out = out?;
        self.apply_deferred_indexes(deferred)?;
        Ok(out)
    }

    /// The `indexes` of a write: declared as `CREATE INDEX` would, for
    /// each label of the node, unless an index gives them already.
    pub fn declare_property_indexes(
        &mut self,
        ns: &str,
        labels: &[String],
        indexes: &BTreeMap<String, Vec<IndexKind>>,
    ) -> Result<()> {
        for (property, kinds) in indexes {
            if internal(property) {
                return Err(Error::Invalid(format!(
                    "{property:?} is the engine's own: it is not indexed"
                )));
            }
            for label in labels {
                self.declare_indexes(
                    ns,
                    label,
                    property,
                    kinds,
                    crate::IndexOrigin::Manual,
                    &IndexingPolicy::default(),
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use IndexKind::*;
    use serde_json::json;

    fn hybrid() -> IndexingPolicy {
        serde_json::from_value(json!({
            "mode": "hybrid",
            "exclude": ["rawContent"],
            "labels": {
                "Document": {
                    "properties": {"title": ["exact", "fulltext"], "rawContent": ["fulltext"]},
                    "exclude": ["secret"]
                }
            },
            "properties": {"secret": ["exact"], "summary": ["fulltext"]}
        }))
        .unwrap()
    }

    #[test]
    fn precedence_as_a_table() {
        let policy = hybrid();
        let long = json!("x".repeat(300));
        let cases: Vec<(&str, &str, Json, Vec<IndexKind>)> = vec![
            // A rule for the label and property wins over everything.
            ("Document", "title", json!("short"), vec![Exact, Fulltext]),
            ("Document", "rawContent", json!("text"), vec![Fulltext]),
            // An exclusion for every label, without a label rule.
            ("Note", "rawContent", json!("text"), vec![]),
            // An exclusion for the label wins over a rule for any label.
            ("Document", "secret", json!("s"), vec![]),
            // A rule for the property in any label.
            ("Note", "secret", json!("s"), vec![Exact]),
            ("Note", "summary", json!("short"), vec![Fulltext]),
            // The automatic decision.
            ("Note", "name", json!("Ana"), vec![Exact]),
            ("Note", "body", long, vec![Fulltext]),
            ("Note", "size", json!(3.5), vec![Exact, Range]),
            ("Note", "done", json!(true), vec![Exact]),
            ("Note", "tags", json!(["a"]), vec![]),
            ("Note", "meta", json!({"a": 1}), vec![]),
            ("Note", "gone", Json::Null, vec![]),
            // The engine's own properties, never.
            ("Note", "@embedding", json!([0.1]), vec![]),
            ("Note", "_internal", json!("x"), vec![]),
        ];
        for (label, property, value, expected) in cases {
            assert_eq!(
                decide(&policy, IndexingMode::Hybrid, label, property, &value),
                expected,
                "{label}.{property}"
            );
        }
        // Manual: nothing, whatever the value.
        assert!(decide(&policy, IndexingMode::Manual, "Note", "name", &json!("Ana")).is_empty());
    }

    #[test]
    fn policies_are_checked() {
        assert!(hybrid().validate().is_ok());
        let mut manual = hybrid();
        manual.mode = IndexingMode::Manual;
        assert!(manual.validate().is_err());
        let both: IndexingPolicy = serde_json::from_value(
            json!({"mode": "hybrid", "properties": {"a": ["none", "exact"]}}),
        )
        .unwrap();
        assert!(both.validate().is_err());
        assert!(
            serde_json::from_value::<IndexingPolicy>(json!({"mode": "automatic", "nope": 1}))
                .is_err()
        );
    }
}
