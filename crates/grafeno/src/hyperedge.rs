//! Hyperedges: facts that relate two or more nodes at once, each in a role.
//!
//! A hyperedge is a record of its own (`hyperedges`), not a node joined to
//! its participants by binary edges: nothing about nodes and edges changes,
//! and no edge is ever turned into a hyperedge or back.
//!
//! Three tables hold them, all written in the transaction that changes the
//! hyperedge:
//!
//! * `hyperedges`: `(ns, id) -> HyperEdge` (JSON), the record.
//! * `hyperedge_incidence`: `(ns, node, label, id) -> ()`, once per distinct
//!   node that takes part (a node in two roles has one entry). The hyperedges
//!   of a node, or of a node with one label, are a range read.
//! * `hyperedge_labels`: `(ns, label, id) -> ()`, the hyperedges of a label
//!   in id order, for pages.
//!
//! The label of a hyperedge never changes (as an edge's), so its entries
//! move only when participants do. Both indexes are derived from the
//! records: [`crate::ReadTx::check`] compares them and
//! [`crate::WriteTx::rebuild_indexes`] writes them again.
//!
//! A node that takes part in a hyperedge cannot be deleted by
//! [`WriteTx::delete_node`] ([`Error::Conflict`]): [`WriteTx::delete_node_with`]
//! says what to do with its hyperedges ([`OnHyperEdges`]). No hyperedge ever
//! names a node that does not exist.
//!
//! The replication log carries them (`Op::HyperEdge`, entry format 8):
//! members write the record and derive its index entries. Exports, the
//! change log and event subscriptions do not carry them yet (see
//! `docs/hyperedges.md`).

use std::collections::BTreeSet;
use std::ops::Bound;
use std::sync::atomic::Ordering;

use grafeno_core::{
    Error, HyperEdge, HyperEdgeId, NewHyperEdge, NodeId, Participant, Props, Result, Timestamp,
};
use redb::{ReadableTable, TableDefinition};

use crate::db::{ReadTx, WriteTx, check_interval, merge_props};
use crate::store::{
    AdjKey, LabelKey, RecordKey, check_label, check_namespace, decode, encode, storage,
};

/// `(ns, hyperedge_id) -> HyperEdge` (JSON).
pub(crate) const HYPEREDGES: TableDefinition<RecordKey, &[u8]> = TableDefinition::new("hyperedges");
/// `(ns, node_id, label, hyperedge_id) -> ()`: once per distinct node that
/// takes part.
pub(crate) const HYPEREDGE_INCIDENCE: TableDefinition<AdjKey, ()> =
    TableDefinition::new("hyperedge_incidence");
/// `(ns, label, hyperedge_id) -> ()`.
pub(crate) const HYPEREDGE_LABELS: TableDefinition<LabelKey, ()> =
    TableDefinition::new("hyperedge_labels");

type LabelEntry<'a> = (&'a str, &'a str, u128);
type RecordEntry<'a> = (&'a str, u128);

/// How many participants a hyperedge may have unless
/// [`crate::Grafeno::set_max_participants`] says otherwise.
pub const DEFAULT_MAX_PARTICIPANTS: usize = 256;

/// What [`WriteTx::delete_node_with`] does with the hyperedges a deleted
/// node takes part in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnHyperEdges {
    /// Refuse to delete the node ([`Error::Conflict`]): what
    /// [`WriteTx::delete_node`] does.
    #[default]
    Restrict,
    /// Delete the hyperedges with the node.
    Cascade,
    /// Take the node out of each hyperedge (every role it plays there);
    /// a hyperedge left with fewer than two distinct nodes is deleted.
    Detach,
}

/// How [`ReadTx::hyperedges_by_participants`] compares the nodes asked for
/// with the nodes of a hyperedge (roles are not compared).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticipantMatch {
    /// Every node asked for takes part; others may too.
    Contains,
    /// The nodes that take part are exactly those asked for.
    Exact,
}

/// Which hyperedges a read returns: by default those not retracted, of any
/// label, whatever their valid time (as [`crate::EdgeFilter`] does for edges).
#[derive(Clone, Debug, Default)]
pub struct HyperEdgeFilter {
    labels: Vec<String>,
    valid_at: Option<Timestamp>,
    known_at: Option<Timestamp>,
}

impl HyperEdgeFilter {
    pub fn any() -> Self {
        Self::default()
    }

    /// Only hyperedges with this label (can be repeated).
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }

    /// Only facts that are true at `at`.
    pub fn valid_at(mut self, at: Timestamp) -> Self {
        self.valid_at = Some(at);
        self
    }

    /// What was known at `at`, including hyperedges retracted later.
    pub fn known_at(mut self, at: Timestamp) -> Self {
        self.known_at = Some(at);
        self
    }

    pub fn accepts(&self, edge: &HyperEdge) -> bool {
        let known = match self.known_at {
            Some(at) => edge.known_at(at),
            None => !edge.is_retracted(),
        };
        known
            && self.valid_at.is_none_or(|at| edge.holds_at(at))
            && (self.labels.is_empty() || self.labels.contains(&edge.label))
    }

    /// The labels to read the incidence index by (none: all of them).
    fn labels(&self) -> Vec<Option<&str>> {
        if self.labels.is_empty() {
            return vec![None];
        }
        let unique: BTreeSet<&str> = self.labels.iter().map(String::as_str).collect();
        unique.into_iter().map(Some).collect()
    }
}

/// Sorts the participants and checks them: no more than `max`, roles that
/// are valid labels, no (node, role) pair twice, two distinct nodes or more.
pub(crate) fn normalize_participants(
    mut participants: Vec<Participant>,
    max: usize,
) -> Result<Vec<Participant>> {
    if participants.len() > max {
        return Err(Error::Invalid(format!(
            "a hyperedge takes at most {max} participants, and {} were given",
            participants.len()
        )));
    }
    for participant in &participants {
        if let Some(role) = &participant.role
            && (role.is_empty() || role.trim() != role)
        {
            return Err(Error::Invalid(format!("invalid role {role:?}")));
        }
    }
    participants.sort();
    if let Some(pair) = participants.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(Error::Invalid(match &pair[0].role {
            Some(role) => format!("node {} takes part twice as {role}", pair[0].node),
            None => format!("node {} takes part twice without a role", pair[0].node),
        }));
    }
    let mut distinct = participants.iter().map(|p| p.node).collect::<Vec<_>>();
    distinct.dedup();
    if distinct.len() < 2 {
        return Err(Error::Invalid(
            "a hyperedge needs at least two distinct nodes".into(),
        ));
    }
    Ok(participants)
}

/// The ids of the hyperedges `node` takes part in, in label then id order
/// (with `label`, only that label's).
pub(crate) fn incident(
    table: &impl ReadableTable<AdjKey, ()>,
    ns: &str,
    node: NodeId,
    label: Option<&str>,
) -> Result<Vec<HyperEdgeId>> {
    let node = node.as_u128();
    let mut out = Vec::new();
    for item in table
        .range((ns, node, label.unwrap_or(""), 0u128)..)
        .map_err(storage)?
    {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, key_node, key_label, id) = key.value();
        if key_ns != ns || key_node != node || label.is_some_and(|label| label != key_label) {
            break;
        }
        out.push(HyperEdgeId::from_u128(id));
    }
    Ok(out)
}

fn read_hyperedge(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    id: HyperEdgeId,
) -> Result<Option<HyperEdge>> {
    match table.get((ns, id.as_u128())).map_err(storage)? {
        Some(guard) => decode(guard.value()).map(Some),
        None => Ok(None),
    }
}

/// Up to `limit` ids of the hyperedges with `label` after `after`, in id order.
fn label_ids(
    table: &impl ReadableTable<LabelKey, ()>,
    ns: &str,
    label: &str,
    after: Option<HyperEdgeId>,
    limit: usize,
) -> Result<Vec<HyperEdgeId>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let lower = match after {
        Some(id) => Bound::Excluded((ns, label, id.as_u128())),
        None => Bound::Included((ns, label, 0u128)),
    };
    let bounds: (Bound<LabelEntry>, Bound<LabelEntry>) = (lower, Bound::Unbounded);
    for item in table.range(bounds).map_err(storage)? {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, key_label, id) = key.value();
        if key_ns != ns || key_label != label {
            break;
        }
        out.push(HyperEdgeId::from_u128(id));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Up to `limit` ids of the hyperedges of `ns` after `after`, in id order.
fn page_ids(
    table: &impl ReadableTable<RecordKey, &'static [u8]>,
    ns: &str,
    after: Option<HyperEdgeId>,
    limit: usize,
) -> Result<Vec<HyperEdgeId>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let lower = match after {
        Some(id) => Bound::Excluded((ns, id.as_u128())),
        None => Bound::Included((ns, 0u128)),
    };
    let bounds: (Bound<RecordEntry>, Bound<RecordEntry>) = (lower, Bound::Unbounded);
    for item in table.range(bounds).map_err(storage)? {
        let (key, _) = item.map_err(storage)?;
        let (key_ns, id) = key.value();
        if key_ns != ns {
            break;
        }
        out.push(HyperEdgeId::from_u128(id));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl crate::Grafeno {
    /// Sets how many participants a hyperedge may have (at least 2), for
    /// every transaction begun from now on.
    pub fn set_max_participants(&self, max: usize) -> Result<()> {
        if max < 2 {
            return Err(Error::Invalid(
                "a hyperedge needs room for two participants at least".into(),
            ));
        }
        self.shared.max_participants.store(max, Ordering::Relaxed);
        Ok(())
    }

    /// How many participants a hyperedge may have.
    pub fn max_participants(&self) -> usize {
        self.shared.max_participants.load(Ordering::Relaxed)
    }
}

impl WriteTx {
    /// The ids of the hyperedges `node` takes part in (with `label`, only
    /// that label's), in label then id order, as this transaction sees
    /// them; no view applies.
    pub fn hyperedge_ids_of(
        &self,
        ns: &str,
        node: NodeId,
        label: Option<&str>,
    ) -> Result<Vec<HyperEdgeId>> {
        let table = self.tx.open_table(HYPEREDGE_INCIDENCE).map_err(storage)?;
        incident(&table, ns, node, label)
    }

    /// Up to `limit` ids of the hyperedges with `label` after `after`, in
    /// id order; no view applies.
    pub fn hyperedge_ids_with_label(
        &self,
        ns: &str,
        label: &str,
        after: Option<HyperEdgeId>,
        limit: usize,
    ) -> Result<Vec<HyperEdgeId>> {
        let table = self.tx.open_table(HYPEREDGE_LABELS).map_err(storage)?;
        label_ids(&table, ns, label, after, limit)
    }

    /// Up to `limit` ids of the hyperedges of `ns` after `after`, in id
    /// order; no view applies.
    pub fn hyperedge_ids(
        &self,
        ns: &str,
        after: Option<HyperEdgeId>,
        limit: usize,
    ) -> Result<Vec<HyperEdgeId>> {
        let table = self.tx.open_table(HYPEREDGES).map_err(storage)?;
        page_ids(&table, ns, after, limit)
    }
    /// Checks the participants of a hyperedge to write: sorted, valid, and
    /// each node existing (and visible to the actor) in `ns`.
    fn checked_participants(
        &self,
        ns: &str,
        participants: Vec<Participant>,
    ) -> Result<Vec<Participant>> {
        let max = self.shared.max_participants.load(Ordering::Relaxed);
        let participants = normalize_participants(participants, max)?;
        let mut last = None;
        for participant in &participants {
            if last == Some(participant.node) {
                continue;
            }
            last = Some(participant.node);
            if self.node(ns, participant.node)?.is_none()
                || !self.sees_node(ns, participant.node)?
            {
                return Err(Error::NodeNotFound(participant.node));
            }
        }
        Ok(participants)
    }

    /// May the actor do `action` to the hyperedge as it is now (`before`)
    /// or as it will be (`after`), writing the properties `keys`?
    fn allow_hyperedge(
        &self,
        action: crate::Act,
        ns: &str,
        before: Option<&HyperEdge>,
        after: Option<&HyperEdge>,
        keys: &[&str],
    ) -> Result<()> {
        if let Some(edge) = before {
            self.guard(edge.recorded_by.as_deref(), || {
                format!("the {} hyperedge {}", edge.label, edge.id)
            })?;
        }
        match self.view() {
            Some(view) => view
                .may_change_hyperedge(action, ns, before, after, keys, |id| self.sees_node(ns, id)),
            None => Ok(()),
        }
    }

    /// The hyperedge, as stored (no view applies).
    pub fn hyperedge(&self, ns: &str, id: HyperEdgeId) -> Result<Option<HyperEdge>> {
        let table = self.tx.open_table(HYPEREDGES).map_err(storage)?;
        read_hyperedge(&table, ns, id)
    }

    fn hyperedge_to_act(&self, action: crate::Act, ns: &str, id: HyperEdgeId) -> Result<HyperEdge> {
        let edge = self
            .hyperedge(ns, id)?
            .ok_or(Error::HyperEdgeNotFound(id))?;
        self.allow_hyperedge(action, ns, Some(&edge), None, &[])?;
        Ok(edge)
    }

    /// Creates a hyperedge. Its participants must be nodes of `ns`: at least
    /// two distinct ones, each (node, role) pair once.
    pub fn create_hyperedge(&mut self, ns: &str, new: NewHyperEdge) -> Result<HyperEdgeId> {
        check_namespace(ns)?;
        check_label(&new.label)?;
        check_interval(new.valid_from, new.valid_to)?;
        let participants = self.checked_participants(ns, new.participants)?;
        let edge = HyperEdge {
            id: HyperEdgeId::new(),
            label: new.label,
            participants,
            props: new.props,
            sources: new.sources,
            valid_from: new.valid_from,
            valid_to: new.valid_to,
            recorded_at: Timestamp::now(),
            expired_at: None,
            recorded_by: self.actor().map(|actor| actor.name.clone()),
        };
        let written: Vec<&str> = edge.props.keys().map(String::as_str).collect();
        self.allow_hyperedge(crate::Act::Create, ns, None, Some(&edge), &written)?;
        self.put_hyperedge(ns, &edge, None)?;
        Ok(edge.id)
    }

    /// Writes a hyperedge exactly as given, id and timestamps included,
    /// replacing any with its id: what an import does. Its participants are
    /// checked as on creation (and sorted).
    pub fn restore_hyperedge(&mut self, ns: &str, mut edge: HyperEdge) -> Result<()> {
        check_namespace(ns)?;
        check_label(&edge.label)?;
        check_interval(edge.valid_from, edge.valid_to)?;
        edge.participants = self.checked_participants(ns, edge.participants)?;
        let written: Vec<&str> = edge.props.keys().map(String::as_str).collect();
        let previous = self.hyperedge(ns, edge.id)?;
        if let Some(old) = &previous {
            self.allow_hyperedge(crate::Act::Update, ns, Some(old), Some(&edge), &written)?;
        } else {
            self.allow_hyperedge(crate::Act::Create, ns, None, Some(&edge), &written)?;
        }
        match previous {
            // Another label: its entries go and come back under the new one.
            Some(old) if old.label != edge.label => {
                self.remove_hyperedge(ns, &old)?;
                self.put_hyperedge(ns, &edge, None)
            }
            previous => self.put_hyperedge(ns, &edge, previous.as_ref()),
        }
    }

    /// May the actor change `before` into `after` (`None`: delete it) as
    /// part of a merge?
    pub(crate) fn allow_merge_hyperedge(
        &self,
        action: crate::Act,
        ns: &str,
        before: &HyperEdge,
        after: Option<&HyperEdge>,
    ) -> Result<()> {
        self.allow_hyperedge(action, ns, Some(before), None, &[])?;
        if let Some(after) = after {
            self.allow_hyperedge(action, ns, None, Some(after), &[])?;
        }
        Ok(())
    }

    /// Writes a hyperedge a merge changed (participants already checked).
    pub(crate) fn put_merged_hyperedge(
        &mut self,
        ns: &str,
        edge: &HyperEdge,
        previous: &HyperEdge,
    ) -> Result<()> {
        self.put_hyperedge(ns, edge, Some(previous))
    }

    /// Deletes a hyperedge a merge left with one node.
    pub(crate) fn remove_merged_hyperedge(&mut self, ns: &str, edge: &HyperEdge) -> Result<()> {
        self.remove_hyperedge(ns, edge)
    }

    /// Merges `props` into the hyperedge (a `null` value is stored as `null`).
    pub fn set_hyperedge_props(&mut self, ns: &str, id: HyperEdgeId, props: Props) -> Result<()> {
        let before = self.hyperedge_to_act(crate::Act::Update, ns, id)?;
        let keys: Vec<String> = props.keys().cloned().collect();
        let mut edge = before.clone();
        merge_props(&mut edge.props, props);
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.allow_hyperedge(crate::Act::Update, ns, None, Some(&edge), &keys)?;
        self.put_hyperedge(ns, &edge, Some(&before))
    }

    /// Removes these properties from the hyperedge (absent ones are ignored).
    pub fn remove_hyperedge_props(
        &mut self,
        ns: &str,
        id: HyperEdgeId,
        keys: &[&str],
    ) -> Result<()> {
        let before = self.hyperedge_to_act(crate::Act::Update, ns, id)?;
        if !keys.iter().any(|key| before.props.contains_key(*key)) {
            return Ok(());
        }
        let mut edge = before.clone();
        for key in keys {
            edge.props.remove(*key);
        }
        self.allow_hyperedge(crate::Act::Update, ns, None, Some(&edge), keys)?;
        self.put_hyperedge(ns, &edge, Some(&before))
    }

    /// Replaces the participants of the hyperedge (checked as on creation).
    pub fn set_hyperedge_participants(
        &mut self,
        ns: &str,
        id: HyperEdgeId,
        participants: Vec<Participant>,
    ) -> Result<()> {
        let before = self.hyperedge_to_act(crate::Act::Update, ns, id)?;
        let participants = self.checked_participants(ns, participants)?;
        let mut edge = before.clone();
        edge.participants = participants;
        self.allow_hyperedge(crate::Act::Update, ns, None, Some(&edge), &[])?;
        self.put_hyperedge(ns, &edge, Some(&before))
    }

    /// The fact stopped being true at `at`: the world changed, not what we know.
    pub fn invalidate_hyperedge(&mut self, ns: &str, id: HyperEdgeId, at: Timestamp) -> Result<()> {
        let before = self.hyperedge_to_act(crate::Act::Retract, ns, id)?;
        check_interval(before.valid_from, Some(at))?;
        let mut edge = before.clone();
        edge.valid_to = Some(at);
        self.put_hyperedge(ns, &edge, Some(&before))
    }

    /// The fact was a mistake. It stays in history, visible with
    /// [`HyperEdgeFilter::known_at`] at an earlier instant.
    pub fn retract_hyperedge(&mut self, ns: &str, id: HyperEdgeId) -> Result<()> {
        let before = self.hyperedge_to_act(crate::Act::Retract, ns, id)?;
        if before.expired_at.is_none() {
            let mut edge = before.clone();
            edge.expired_at = Some(Timestamp::now());
            self.put_hyperedge(ns, &edge, Some(&before))?;
        }
        Ok(())
    }

    /// Deletes the hyperedge and its index entries, without keeping history.
    pub fn delete_hyperedge(&mut self, ns: &str, id: HyperEdgeId) -> Result<()> {
        let edge = self.hyperedge_to_act(crate::Act::Delete, ns, id)?;
        self.remove_hyperedge(ns, &edge)
    }

    /// The hyperedges `node` takes part in (their ids), seen or not.
    pub(crate) fn incident_hyperedges(&self, ns: &str, node: NodeId) -> Result<Vec<HyperEdgeId>> {
        let table = self.tx.open_table(HYPEREDGE_INCIDENCE).map_err(storage)?;
        incident(&table, ns, node, None)
    }

    /// Before `node` is deleted: refuses, or deletes or detaches its
    /// hyperedges, as `policy` says.
    pub(crate) fn settle_hyperedges_of(
        &mut self,
        ns: &str,
        node: NodeId,
        policy: OnHyperEdges,
    ) -> Result<()> {
        let ids = self.incident_hyperedges(ns, node)?;
        if ids.is_empty() {
            return Ok(());
        }
        // A hyperedge the actor may not see would be changed: refused,
        // without naming it (as edges, 14.4).
        if policy != OnHyperEdges::Restrict
            && let Some(view) = self.view()
        {
            for &id in &ids {
                let visible = match self.hyperedge(ns, id)? {
                    Some(edge) => view.hyperedge_visible(ns, &edge, |n| self.sees_node(ns, n))?,
                    None => true,
                };
                if !visible {
                    return Err(Error::Forbidden(format!(
                        "node {node} takes part in hyperedges hidden from {}: it cannot be \
                         deleted by it",
                        view.account.as_deref().unwrap_or("the caller")
                    )));
                }
            }
        }
        match policy {
            OnHyperEdges::Restrict => Err(Error::Conflict(format!(
                "node {node} takes part in hyperedges: delete them first, or delete the node \
                 with delete_node_with and Cascade or Detach"
            ))),
            OnHyperEdges::Cascade => {
                for id in ids {
                    self.delete_hyperedge(ns, id)?;
                }
                Ok(())
            }
            OnHyperEdges::Detach => {
                for id in ids {
                    let before = self.hyperedge_to_act(crate::Act::Update, ns, id)?;
                    let mut edge = before.clone();
                    edge.participants.retain(|p| p.node != node);
                    if edge.nodes().len() < 2 {
                        self.allow_hyperedge(crate::Act::Delete, ns, Some(&before), None, &[])?;
                        self.remove_hyperedge(ns, &before)?;
                    } else {
                        self.allow_hyperedge(crate::Act::Update, ns, None, Some(&edge), &[])?;
                        self.put_hyperedge(ns, &edge, Some(&before))?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Deletes the node as [`WriteTx::delete_node`] does, doing with the
    /// hyperedges it takes part in what `policy` says.
    pub fn delete_node_with(&mut self, ns: &str, id: NodeId, policy: OnHyperEdges) -> Result<()> {
        // The node must be one the actor may delete before anything goes.
        self.node_to_act(crate::Act::Delete, ns, id)?;
        self.settle_hyperedges_of(ns, id, policy)?;
        self.delete_node(ns, id)
    }

    /// Writes the hyperedge record and brings its index entries from
    /// `previous` (`None`: a new hyperedge) to `edge`.
    pub(crate) fn put_hyperedge(
        &mut self,
        ns: &str,
        edge: &HyperEdge,
        previous: Option<&HyperEdge>,
    ) -> Result<()> {
        if let Some(old) = previous
            && old.label != edge.label
        {
            return Err(Error::Invalid(
                "the label of a hyperedge cannot change".into(),
            ));
        }
        let id = edge.id.as_u128();
        self.note(crate::replication::Touch::HyperEdge(ns.to_owned(), id));
        let label = std::slice::from_ref(&edge.label);
        self.remember_before(
            ns,
            id,
            true,
            label,
            previous.map(|old| (std::slice::from_ref(&old.label), &old.props)),
        )?;
        let op = if previous.is_some() {
            crate::changes::ChangeOp::Updated
        } else {
            crate::changes::ChangeOp::Created
        };
        self.log_hyperedge(ns, edge, op);
        let label = edge.label.as_str();
        let bytes = encode(edge)?;
        self.tx
            .open_table(HYPEREDGES)
            .map_err(storage)?
            .insert((ns, id), bytes.as_slice())
            .map_err(storage)?;
        let old: BTreeSet<NodeId> = previous
            .map(|old| old.nodes())
            .unwrap_or_default()
            .into_iter()
            .collect();
        let new: BTreeSet<NodeId> = edge.nodes().into_iter().collect();
        let mut incidence = self.tx.open_table(HYPEREDGE_INCIDENCE).map_err(storage)?;
        for node in old.difference(&new) {
            incidence
                .remove((ns, node.as_u128(), label, id))
                .map_err(storage)?;
        }
        for node in new.difference(&old) {
            incidence
                .insert((ns, node.as_u128(), label, id), ())
                .map_err(storage)?;
        }
        drop(incidence);
        if previous.is_none() {
            self.tx
                .open_table(HYPEREDGE_LABELS)
                .map_err(storage)?
                .insert((ns, label, id), ())
                .map_err(storage)?;
        }
        self.index_hyperedge(ns, Some(edge), previous)?;
        if let Some(old) = previous
            && !self.applying
        {
            self.record_hyperedge_history(ns, edge, old)?;
        }
        // Not null, types and keys: now, or at the end of a statement.
        let defs = self.index_defs(ns)?;
        self.check_rules_later(
            ns,
            crate::index::RecordKind::HyperEdge,
            id,
            &defs,
            std::slice::from_ref(&edge.label),
            &edge.props,
            !edge.is_retracted(),
        )
    }

    /// Removes the hyperedge record and its index entries.
    pub(crate) fn remove_hyperedge(&mut self, ns: &str, edge: &HyperEdge) -> Result<()> {
        let id = edge.id.as_u128();
        self.note(crate::replication::Touch::HyperEdge(ns.to_owned(), id));
        let label = std::slice::from_ref(&edge.label);
        self.remember_before(ns, id, true, label, Some((label, &edge.props)))?;
        self.log_hyperedge(ns, edge, crate::changes::ChangeOp::Deleted);
        let label = edge.label.as_str();
        let mut incidence = self.tx.open_table(HYPEREDGE_INCIDENCE).map_err(storage)?;
        for node in edge.nodes() {
            incidence
                .remove((ns, node.as_u128(), label, id))
                .map_err(storage)?;
        }
        drop(incidence);
        self.tx
            .open_table(HYPEREDGE_LABELS)
            .map_err(storage)?
            .remove((ns, label, id))
            .map_err(storage)?;
        self.index_hyperedge(ns, None, Some(edge))?;
        self.tx
            .open_table(HYPEREDGES)
            .map_err(storage)?
            .remove((ns, id))
            .map_err(storage)?;
        Ok(())
    }

    /// Writes the incidence and label entries of every hyperedge of `ns`
    /// again from the records, dropping hyperedges left with fewer than two
    /// existing nodes. Returns (hyperedges kept, hyperedges dropped).
    pub(crate) fn rebuild_hyperedges(&mut self, ns: &str) -> Result<(u64, u64)> {
        let end = format!("{ns}\0");
        {
            let mut incidence = self.tx.open_table(HYPEREDGE_INCIDENCE).map_err(storage)?;
            incidence
                .retain_in(
                    (ns, 0u128, "", 0u128)..(end.as_str(), 0u128, "", 0u128),
                    |_, _| false,
                )
                .map_err(storage)?;
            let mut labels = self.tx.open_table(HYPEREDGE_LABELS).map_err(storage)?;
            labels
                .retain_in((ns, "", 0u128)..(end.as_str(), "", 0u128), |_, _| false)
                .map_err(storage)?;
        }
        let records: Vec<HyperEdge> = {
            let table = self.tx.open_table(HYPEREDGES).map_err(storage)?;
            crate::store::scan_records(&table, ns)?
        };
        let (mut kept, mut dropped) = (0, 0);
        for mut edge in records {
            let mut gone = Vec::new();
            for node in edge.nodes() {
                if self.node(ns, node)?.is_none() {
                    gone.push(node);
                }
            }
            edge.participants.retain(|p| !gone.contains(&p.node));
            if edge.nodes().len() < 2 {
                self.tx
                    .open_table(HYPEREDGES)
                    .map_err(storage)?
                    .remove((ns, edge.id.as_u128()))
                    .map_err(storage)?;
                dropped += 1;
            } else {
                self.put_hyperedge(ns, &edge, None)?;
                kept += 1;
            }
        }
        Ok((kept, dropped))
    }
}

impl ReadTx {
    /// Whether no hyperedge with this id exists, seen or not.
    pub(crate) fn hyperedge_gone(&self, ns: &str, id: HyperEdgeId) -> Result<bool> {
        let records = &self.hyperedges;
        Ok(records.get((ns, id.as_u128())).map_err(storage)?.is_none())
    }

    /// The ids that `self.hyperedge` would return a hyperedge for: with a
    /// view, only those it sees.
    fn visible_ids(&self, ns: &str, ids: Vec<HyperEdgeId>) -> Result<Vec<HyperEdgeId>> {
        if self.view().is_none() {
            return Ok(ids);
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if self.hyperedge(ns, id)?.is_some() {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// The ids of the hyperedges `node` takes part in (with `label`, only
    /// that label's), in label then id order: [`ReadTx::node_hyperedges`]
    /// without reading the records when the snapshot has no view.
    pub fn hyperedge_ids_of(
        &self,
        ns: &str,
        node: NodeId,
        label: Option<&str>,
    ) -> Result<Vec<HyperEdgeId>> {
        if !self.sees(ns, node)? {
            return Ok(Vec::new());
        }
        let incidence = &self.hyperedge_incidence;
        let ids = incident(incidence, ns, node, label)?;
        self.visible_ids(ns, ids)
    }

    /// Up to `limit` ids of the hyperedges with `label` after `after`, in id
    /// order (with a view, those it sees: fewer than `limit` may come back
    /// before the end).
    pub fn hyperedge_ids_with_label(
        &self,
        ns: &str,
        label: &str,
        after: Option<HyperEdgeId>,
        limit: usize,
    ) -> Result<Vec<HyperEdgeId>> {
        let table = &self.hyperedge_labels;
        let ids = label_ids(table, ns, label, after, limit)?;
        self.visible_ids(ns, ids)
    }

    /// Up to `limit` ids of the hyperedges of `ns` after `after`, in id
    /// order (with a view, those it sees: fewer than `limit` may come back
    /// before the end).
    pub fn hyperedge_ids(
        &self,
        ns: &str,
        after: Option<HyperEdgeId>,
        limit: usize,
    ) -> Result<Vec<HyperEdgeId>> {
        let records = &self.hyperedges;
        let ids = page_ids(records, ns, after, limit)?;
        self.visible_ids(ns, ids)
    }


    /// The hyperedge as this snapshot's view sees it: hidden when its label
    /// or any participant is, without hidden properties.
    fn seen_hyperedge(&self, ns: &str, edge: HyperEdge) -> Result<Option<HyperEdge>> {
        match self.view() {
            None => Ok(Some(edge)),
            Some(view) => {
                if !view.reads(ns) {
                    return Ok(None);
                }
                view.hyperedge(ns, edge, |id| self.sees(ns, id))
            }
        }
    }

    /// The hyperedge with this id.
    pub fn hyperedge(&self, ns: &str, id: HyperEdgeId) -> Result<Option<HyperEdge>> {
        let records = &self.hyperedges;
        match read_hyperedge(records, ns, id)? {
            Some(edge) => self.seen_hyperedge(ns, edge),
            None => Ok(None),
        }
    }

    /// The hyperedges `node` takes part in that pass `filter`, in label then
    /// id order. Empty when the node does not exist or is hidden.
    pub fn node_hyperedges(
        &self,
        ns: &str,
        node: NodeId,
        filter: &HyperEdgeFilter,
    ) -> Result<Vec<HyperEdge>> {
        if !self.sees(ns, node)? {
            return Ok(Vec::new());
        }
        let (records, incidence) = (&self.hyperedges, &self.hyperedge_incidence);
        let mut out = Vec::new();
        for label in filter.labels() {
            for id in incident(incidence, ns, node, label)? {
                let Some(edge) = read_hyperedge(records, ns, id)? else {
                    return Err(Error::Storage(format!(
                        "the incidence index names hyperedge {id}, which does not exist"
                    )));
                };
                if filter.accepts(&edge)
                    && let Some(edge) = self.seen_hyperedge(ns, edge)?
                {
                    out.push(edge);
                }
            }
        }
        Ok(out)
    }

    /// Up to `limit` hyperedges with `label` that pass `filter`, with an id
    /// greater than `after`, in id order: pass the last id of a page to get
    /// the next.
    pub fn hyperedges_with_label(
        &self,
        ns: &str,
        label: &str,
        filter: &HyperEdgeFilter,
        after: Option<HyperEdgeId>,
        limit: usize,
    ) -> Result<Vec<HyperEdge>> {
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        let records = &self.hyperedges;
        let labels = &self.hyperedge_labels;
        let lower = match after {
            Some(id) => Bound::Excluded((ns, label, id.as_u128())),
            None => Bound::Included((ns, label, 0u128)),
        };
        let bounds: (Bound<LabelEntry>, Bound<LabelEntry>) = (lower, Bound::Unbounded);
        for item in labels.range(bounds).map_err(storage)? {
            let (key, _) = item.map_err(storage)?;
            let (key_ns, key_label, id) = key.value();
            if key_ns != ns || key_label != label {
                break;
            }
            let id = HyperEdgeId::from_u128(id);
            let Some(edge) = read_hyperedge(records, ns, id)? else {
                return Err(Error::Storage(format!(
                    "the label index names hyperedge {id}, which does not exist"
                )));
            };
            if filter.accepts(&edge)
                && let Some(edge) = self.seen_hyperedge(ns, edge)?
            {
                out.push(edge);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The hyperedges whose nodes include all of `nodes` ([`ParticipantMatch::Contains`])
    /// or are exactly `nodes` ([`ParticipantMatch::Exact`]), that pass
    /// `filter`, in id order. Roles are not compared: read them from the
    /// participants of what is returned.
    pub fn hyperedges_by_participants(
        &self,
        ns: &str,
        nodes: &[NodeId],
        mode: ParticipantMatch,
        filter: &HyperEdgeFilter,
    ) -> Result<Vec<HyperEdge>> {
        let wanted: BTreeSet<NodeId> = nodes.iter().copied().collect();
        if wanted.is_empty() {
            return Err(Error::Invalid("name at least one node".into()));
        }
        for &node in &wanted {
            if !self.sees(ns, node)? {
                return Ok(Vec::new());
            }
        }
        let (records, incidence) = (&self.hyperedges, &self.hyperedge_incidence);
        // The ids every node asked for takes part in: the intersection of
        // their incidence lists, read once each.
        let mut common: Option<BTreeSet<HyperEdgeId>> = None;
        for &node in &wanted {
            let mut ids = BTreeSet::new();
            for label in filter.labels() {
                ids.extend(incident(incidence, ns, node, label)?);
            }
            common = Some(match common {
                None => ids,
                Some(before) => before.intersection(&ids).copied().collect(),
            });
            if common.as_ref().is_some_and(BTreeSet::is_empty) {
                break;
            }
        }
        let mut out = Vec::new();
        for id in common.unwrap_or_default() {
            let Some(edge) = read_hyperedge(records, ns, id)? else {
                return Err(Error::Storage(format!(
                    "the incidence index names hyperedge {id}, which does not exist"
                )));
            };
            let matches = match mode {
                ParticipantMatch::Contains => true,
                ParticipantMatch::Exact => edge.nodes().len() == wanted.len(),
            };
            if matches
                && filter.accepts(&edge)
                && let Some(edge) = self.seen_hyperedge(ns, edge)?
            {
                out.push(edge);
            }
        }
        Ok(out)
    }

    /// Up to `limit` hyperedges of `ns` with an id greater than `after`, in
    /// id order, whatever their label or time.
    pub fn hyperedges_page(
        &self,
        ns: &str,
        after: Option<HyperEdgeId>,
        limit: usize,
    ) -> Result<Vec<HyperEdge>> {
        let records = &self.hyperedges;
        let page: Vec<HyperEdge> =
            crate::store::scan_page(records, ns, after.map(HyperEdgeId::as_u128), limit, |_| {
                true
            })?;
        let mut out = Vec::with_capacity(page.len());
        for edge in page {
            if let Some(edge) = self.seen_hyperedge(ns, edge)? {
                out.push(edge);
            }
        }
        Ok(out)
    }

    /// Checks the hyperedges of `ns`: participants sorted, unique and
    /// existing, two distinct nodes or more, and the incidence and label
    /// indexes equal to what the records call for. Problems are appended.
    pub(crate) fn check_hyperedges(&self, ns: &str, problems: &mut Vec<String>) -> Result<u64> {
        let (records, incidence) = (&self.hyperedges, &self.hyperedge_incidence);
        let labels = &self.hyperedge_labels;
        let mut expected_incidence: BTreeSet<(u128, String, u128)> = BTreeSet::new();
        let mut expected_labels: BTreeSet<(String, u128)> = BTreeSet::new();
        let mut count = 0u64;
        for edge in crate::store::scan_records::<HyperEdge>(records, ns)? {
            count += 1;
            let id = edge.id;
            if edge.participants.windows(2).any(|pair| pair[0] >= pair[1]) {
                problems.push(format!(
                    "participants of hyperedge {id} are not sorted and unique"
                ));
            }
            let nodes = edge.nodes();
            if nodes.len() < 2 {
                problems.push(format!("hyperedge {id} has fewer than two distinct nodes"));
            }
            for node in &nodes {
                if !self.raw_node_exists(ns, *node)? {
                    problems.push(format!(
                        "hyperedge {id} names node {node}, which does not exist"
                    ));
                }
                expected_incidence.insert((node.as_u128(), edge.label.clone(), id.as_u128()));
            }
            expected_labels.insert((edge.label.clone(), id.as_u128()));
        }
        let end = format!("{ns}\0");
        let mut found_incidence = BTreeSet::new();
        for item in incidence
            .range((ns, 0u128, "", 0u128)..(end.as_str(), 0u128, "", 0u128))
            .map_err(storage)?
        {
            let (key, _) = item.map_err(storage)?;
            let (_, node, label, id) = key.value();
            found_incidence.insert((node, label.to_owned(), id));
        }
        for (node, label, id) in expected_incidence.difference(&found_incidence) {
            problems.push(format!(
                "the incidence index lacks node {} in the {label} hyperedge {}",
                NodeId::from_u128(*node),
                HyperEdgeId::from_u128(*id)
            ));
        }
        for (node, label, id) in found_incidence.difference(&expected_incidence) {
            problems.push(format!(
                "the incidence index has node {} in the {label} hyperedge {}, which it is not in",
                NodeId::from_u128(*node),
                HyperEdgeId::from_u128(*id)
            ));
        }
        let mut found_labels = BTreeSet::new();
        for item in labels
            .range((ns, "", 0u128)..(end.as_str(), "", 0u128))
            .map_err(storage)?
        {
            let (key, _) = item.map_err(storage)?;
            let (_, label, id) = key.value();
            found_labels.insert((label.to_owned(), id));
        }
        for (label, id) in expected_labels.symmetric_difference(&found_labels) {
            problems.push(format!(
                "the hyperedge label index disagrees about the {label} hyperedge {}",
                HyperEdgeId::from_u128(*id)
            ));
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u128) -> NodeId {
        NodeId::from_u128(n)
    }

    #[test]
    fn participants_are_sorted_and_checked() {
        let sorted = normalize_participants(
            vec![
                Participant::with_role(node(2), "reviewer"),
                Participant::with_role(node(1), "proposer"),
                Participant::with_role(node(1), "reviewer"),
            ],
            10,
        )
        .unwrap();
        assert_eq!(sorted[0], Participant::with_role(node(1), "proposer"));
        assert_eq!(sorted[2].node, node(2));

        let one_node = vec![
            Participant::with_role(node(1), "a"),
            Participant::with_role(node(1), "b"),
        ];
        assert!(matches!(
            normalize_participants(one_node, 10),
            Err(Error::Invalid(_))
        ));
        let twice = vec![
            Participant::with_role(node(1), "a"),
            Participant::with_role(node(1), "a"),
            Participant::new(node(2)),
        ];
        assert!(matches!(
            normalize_participants(twice, 10),
            Err(Error::Invalid(_))
        ));
        let bad_role = vec![
            Participant::with_role(node(1), " a"),
            Participant::new(node(2)),
        ];
        assert!(matches!(
            normalize_participants(bad_role, 10),
            Err(Error::Invalid(_))
        ));
        let many = (0..11).map(|n| Participant::new(node(n))).collect();
        assert!(matches!(
            normalize_participants(many, 10),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            normalize_participants(Vec::new(), 10),
            Err(Error::Invalid(_))
        ));
    }
}
