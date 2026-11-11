//! Event subscriptions (16.10): the values that changed, for those who
//! ask, off by default.
//!
//! The change log (11.4) says which records changed. A subscription asks
//! for more on some of them: nodes, edges, properties (perhaps some keys),
//! texts or vectors, perhaps of some labels or with some property values.
//! When the database's events are on ([`crate::Grafeno::enable_events`])
//! and a namespace has subscriptions, each commit adds to the change log
//! entries they match the values before and after, which properties
//! changed, and the new text or vector (a vector's values only when a
//! subscription asks for them). Nothing else is recorded: with events off,
//! or no subscription, a write costs what it did.
//!
//! The values travel in the change log entry (`Change::values`), so they
//! keep its order, window, gaps and replication. [`ReadTx::changes`] leaves
//! them out; [`ReadTx::events`] gives the entries of one subscription with
//! them.

use std::collections::HashMap;

use grafeno_core::{Error, Props, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::changes::{Change, ChangeOp, ChangePage, Part};
use crate::db::{ReadTx, WriteTx};

/// The state kind of subscriptions, keyed by name.
pub(crate) const SUBSCRIPTION: &str = "event.subscription";

/// What a subscription follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTarget {
    /// Nodes created, changed or deleted.
    Nodes,
    /// Edges created, changed or deleted.
    Edges,
    /// Records whose properties changed (perhaps only some keys).
    Properties,
    /// Nodes whose text changed.
    Text,
    /// Nodes whose embedding changed.
    Vectors,
}

/// Where a subscription's events go.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Delivery {
    /// Read by clients (`GET /v1/{ns}/events/{name}`).
    Stream,
    /// Posted to a URL, in order, signed with the secret when there is one.
    Webhook {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret: Option<String>,
    },
}

/// A subscription to the values that change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subscription {
    pub name: String,
    pub on: Vec<EventTarget>,
    /// Only records with one of these labels (all when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// With `Properties`: only changes of these keys (any when empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Only records with these property values, before or after.
    #[serde(default, skip_serializing_if = "Props::is_empty")]
    pub equal: Props,
    /// With `Vectors`: the new vector's values, not only that it changed.
    #[serde(default)]
    pub vector_values: bool,
    pub delivery: Delivery,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

impl Subscription {
    /// What a subscription with this name and targets is by default.
    pub fn new(name: &str, on: Vec<EventTarget>) -> Self {
        Self {
            name: name.to_owned(),
            on,
            labels: Vec::new(),
            keys: Vec::new(),
            equal: Props::new(),
            vector_values: false,
            delivery: Delivery::Stream,
            created_at: Timestamp::now(),
            created_by: None,
        }
    }
}

/// The values a change log entry carries for subscriptions.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChangeValues {
    /// The properties before the commit (none for a record it created).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Props>,
    /// After it (none for a record it deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Props>,
    /// The keys whose values differ.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    /// The new text, when it changed and a subscription follows texts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The new vector, when it changed and a subscription asks for its
    /// values (`vector_changed` says it changed in any case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vector_changed: bool,
    /// The subscriptions it is for.
    pub subscriptions: Vec<String>,
}

/// The labels and properties of a record before a transaction changed it.
pub(crate) type Before = Option<(Vec<String>, Props)>;

/// Whether a subscription follows records with one of these labels.
fn follows(subscription: &Subscription, labels: &[String]) -> bool {
    subscription.labels.is_empty()
        || subscription
            .labels
            .iter()
            .any(|label| labels.contains(label))
}

fn changed_keys(before: Option<&Props>, after: Option<&Props>) -> Vec<String> {
    let empty = Props::new();
    let (before, after) = (before.unwrap_or(&empty), after.unwrap_or(&empty));
    let mut keys: Vec<String> = before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

impl WriteTx {
    /// The subscriptions of `ns` that commits must serve: none while
    /// events are off or the change log is.
    pub(crate) fn active_subscriptions(
        &mut self,
        ns: &str,
    ) -> Result<std::sync::Arc<Vec<Subscription>>> {
        if !self.log_changes || self.applying || !self.shared.changes.events() {
            return Ok(std::sync::Arc::default());
        }
        if let Some((cached, found)) = &self.subscription_cache
            && cached == ns
        {
            return Ok(found.clone());
        }
        let found = std::sync::Arc::new(read_subscriptions(self.state_entries(
            ns,
            SUBSCRIPTION,
            "",
            usize::MAX,
        )?)?);
        self.subscription_cache = Some((ns.to_owned(), found.clone()));
        Ok(found)
    }

    /// Keeps a record's state before its first change in this transaction,
    /// when a subscription may want it (`labels`: the record's now).
    pub(crate) fn remember_before(
        &mut self,
        ns: &str,
        id: u128,
        is_edge: bool,
        labels: &[String],
        before: Option<(&[String], &Props)>,
    ) -> Result<()> {
        let subscriptions = self.active_subscriptions(ns)?;
        let wanted = subscriptions.iter().any(|subscription| {
            follows(subscription, labels)
                || before.is_some_and(|(old, _)| follows(subscription, old))
        });
        if !wanted {
            return Ok(());
        }
        self.pending_changes.remember(
            ns,
            id,
            is_edge,
            before.map(|(labels, props)| (labels.to_vec(), props.clone())),
        );
        Ok(())
    }

    /// Adds to each pending change log entry the values its namespace's
    /// subscriptions ask for (once: at commit, or when a cluster builds the
    /// entry it replicates).
    pub(crate) fn fill_values(&mut self) -> Result<()> {
        if self.pending_changes.filled || self.applying {
            return Ok(());
        }
        self.pending_changes.filled = true;
        let count = self.pending_changes.entries().len();
        for at in 0..count {
            let (ns, change) = self.pending_changes.entries()[at].clone();
            let subscriptions = self.active_subscriptions(&ns)?;
            if subscriptions.is_empty() {
                continue;
            }
            let values = self.values_for(&ns, &change, &subscriptions)?;
            self.pending_changes.set_values(at, values);
        }
        Ok(())
    }

    fn values_for(
        &self,
        ns: &str,
        change: &Change,
        subscriptions: &[Subscription],
    ) -> Result<Option<ChangeValues>> {
        let is_edge = change.edge.is_some();
        let id = change
            .edge
            .map(|edge| edge.as_u128())
            .or(change.node.map(|node| node.as_u128()));
        let Some(id) = id else {
            return Ok(None);
        };
        let remembered = self.pending_changes.before(ns, id, is_edge);
        // Nothing to read for records no subscription follows.
        let old_labels = remembered
            .and_then(|before| before.as_ref())
            .map(|(labels, _)| labels.as_slice())
            .unwrap_or_default();
        if !subscriptions.iter().any(|subscription| {
            follows(subscription, &change.labels) || follows(subscription, old_labels)
        }) {
            return Ok(None);
        }
        let before: Before = remembered.cloned().flatten();
        let after: Option<(Vec<String>, Props)> = if change.op == ChangeOp::Deleted {
            None
        } else if is_edge {
            self.edge(ns, grafeno_core::EdgeId::from_u128(id))?
                .map(|edge| (vec![edge.label], edge.props))
        } else {
            self.node(ns, grafeno_core::NodeId::from_u128(id))?
                .map(|node| (node.labels, node.props))
        };
        let changed = changed_keys(
            before.as_ref().map(|(_, props)| props),
            after.as_ref().map(|(_, props)| props),
        );
        let record = change.parts.contains(&Part::Record) || change.op != ChangeOp::Updated;
        let text_changed = change.parts.contains(&Part::Text);
        let vector_changed = change.parts.contains(&Part::Embedding);
        let mut values = ChangeValues::default();
        let (mut with_record, mut with_text, mut with_vector) = (false, false, false);
        for subscription in subscriptions {
            let labelled = subscription.labels.is_empty()
                || subscription.labels.iter().any(|label| {
                    change.labels.contains(label)
                        || before
                            .as_ref()
                            .is_some_and(|(labels, _)| labels.contains(label))
                });
            let equal = subscription.equal.iter().all(|(key, value)| {
                after
                    .as_ref()
                    .is_some_and(|(_, props)| props.get(key) == Some(value))
                    || before
                        .as_ref()
                        .is_some_and(|(_, props)| props.get(key) == Some(value))
            });
            if !labelled || !equal {
                continue;
            }
            let mut matched = false;
            for target in &subscription.on {
                match target {
                    EventTarget::Nodes if !is_edge && record => {
                        matched = true;
                        with_record = true;
                    }
                    EventTarget::Edges if is_edge && record => {
                        matched = true;
                        with_record = true;
                    }
                    EventTarget::Properties
                        if record
                            && !changed.is_empty()
                            && (subscription.keys.is_empty()
                                || changed.iter().any(|key| subscription.keys.contains(key))) =>
                    {
                        matched = true;
                        with_record = true;
                    }
                    EventTarget::Text if !is_edge && text_changed => {
                        matched = true;
                        with_text = true;
                    }
                    EventTarget::Vectors if !is_edge && vector_changed => {
                        matched = true;
                        with_vector |= subscription.vector_values;
                        values.vector_changed = true;
                    }
                    _ => {}
                }
            }
            if matched {
                values.subscriptions.push(subscription.name.clone());
            }
        }
        if values.subscriptions.is_empty() {
            return Ok(None);
        }
        if with_record {
            values.before = before.map(|(_, props)| props);
            values.after = after.map(|(_, props)| props);
            values.changed = changed;
        }
        let node = grafeno_core::NodeId::from_u128(id);
        if with_text {
            values.text = self.text(ns, node)?;
        }
        if with_vector {
            values.vector = self.embedding(ns, node)?;
        }
        Ok(Some(values))
    }

    /// Declares a subscription (or replaces the one with its name). Fails
    /// while events are off.
    pub fn create_subscription(&mut self, ns: &str, subscription: Subscription) -> Result<()> {
        if !self.shared.changes.events() {
            return Err(Error::Invalid(
                "event subscriptions are off: set [events] enabled = true in grafeno.toml".into(),
            ));
        }
        if subscription.name.is_empty() || subscription.on.is_empty() {
            return Err(Error::Invalid(
                "a subscription needs a name and what it follows".into(),
            ));
        }
        crate::namespace::ensure(&self.tx, ns)?;
        let mut subscription = subscription;
        if subscription.created_by.is_none() {
            subscription.created_by = self.actor.as_ref().map(|actor| actor.name.clone());
        }
        let bytes =
            serde_json::to_vec(&subscription).map_err(|e| Error::Serialization(e.to_string()))?;
        self.put_state(ns, SUBSCRIPTION, &subscription.name, &bytes)?;
        self.subscription_cache = None;
        Ok(())
    }

    /// Removes a subscription; whether it existed.
    pub fn drop_subscription(&mut self, ns: &str, name: &str) -> Result<bool> {
        let existed = self.state(ns, SUBSCRIPTION, name)?.is_some();
        if existed {
            self.delete_state(ns, SUBSCRIPTION, name)?;
        }
        self.subscription_cache = None;
        Ok(existed)
    }

    pub fn subscriptions(&self, ns: &str) -> Result<Vec<Subscription>> {
        read_subscriptions(self.state_entries(ns, SUBSCRIPTION, "", usize::MAX)?)
    }
}

fn read_subscriptions(entries: Vec<(String, Vec<u8>)>) -> Result<Vec<Subscription>> {
    entries
        .into_iter()
        .map(|(_, bytes)| {
            serde_json::from_slice(&bytes).map_err(|e| Error::Serialization(e.to_string()))
        })
        .collect()
}

impl ReadTx {
    pub fn subscriptions(&self, ns: &str) -> Result<Vec<Subscription>> {
        read_subscriptions(self.state_entries(ns, SUBSCRIPTION, "", usize::MAX)?)
    }

    pub fn subscription(&self, ns: &str, name: &str) -> Result<Option<Subscription>> {
        Ok(self
            .subscriptions(ns)?
            .into_iter()
            .find(|subscription| subscription.name == name))
    }

    /// Up to `limit` change log entries after `since` that subscription
    /// `name` asked for, with their values (the page's positions cover the
    /// entries skipped). Values show every property, so a reader limited
    /// record by record (14.4) is refused.
    pub fn events(&self, ns: &str, name: &str, since: u64, limit: usize) -> Result<ChangePage> {
        if self.view().is_some_and(|view| view.per_record(ns)) {
            return Err(Error::Forbidden(
                "event values show whole records: they need read access to the whole namespace"
                    .into(),
            ));
        }
        if self.subscription(ns, name)?.is_none() {
            return Err(Error::Invalid(format!(
                "there is no event subscription named {name:?}"
            )));
        }
        let mut page = crate::changes::raw_page(&self.changes, ns, since, limit)?;
        let through = page.changes.last().map_or(since, |change| change.seq);
        page.changes.retain(|change| {
            change
                .values
                .as_ref()
                .is_some_and(|values| values.subscriptions.iter().any(|s| s == name))
        });
        page.through = through;
        Ok(page)
    }
}

impl crate::Grafeno {
    /// Turns event values on or off (off by default): with them on,
    /// commits add to the change log what the namespaces' subscriptions
    /// ask for.
    pub fn enable_events(&self, enabled: bool) {
        self.shared.changes.set_events(enabled);
    }

    pub fn events_enabled(&self) -> bool {
        self.shared.changes.events()
    }
}

/// Per pending entry, what the record was before (kept by
/// [`WriteTx::remember_before`]).
pub(crate) type Befores = HashMap<(String, u128, bool), Before>;
