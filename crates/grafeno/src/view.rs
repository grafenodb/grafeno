//! What one caller may see and change, record by record (phase 14.4).
//!
//! A [`View`] wraps an account's [`Rights`]: a node is visible when every
//! label of it may be read (at the namespace or the label) and none is
//! denied, an edge when its label may be read and both of its ends are
//! visible, and a property is hidden when it is denied on a label of its
//! record. Record policies narrow further: a record must match every
//! policy that applies to it. Hidden records are absent, as if they did
//! not exist; hidden properties are absent from the records returned.
//!
//! Without label or property rules or policies in a namespace, its records
//! are all visible or all hidden, and nothing is checked per record.
//!
//! Writes check each change ([`View::may_change_node`],
//! [`View::may_change_edge`]): the record must be visible, its labels and
//! properties writable for the operation, and write policies must hold
//! before and after.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use grafeno_core::{Edge, Error, HyperEdge, Node, NodeId, Props, Result};
use serde_json::Value;

use crate::access::Account;
use crate::rights::{Act, Applies, Condition, Policy, PolicyValue, Rights};

/// What a caller may see and change.
#[derive(Debug)]
pub struct View {
    pub rights: Arc<Rights>,
    /// The caller's account (`None`: an unnamed caller).
    pub account: Option<String>,
    /// The caller's attributes, for policies (`$account.tenant`).
    pub attributes: serde_json::Map<String, Value>,
    /// Per namespace: (it may be read at all, it is decided per record).
    namespaces: RwLock<HashMap<String, (bool, bool)>>,
}

impl View {
    pub fn new(rights: Arc<Rights>, account: Option<&Account>) -> Self {
        Self {
            rights,
            account: account.map(|a| a.name.clone()),
            attributes: account.map(|a| a.attributes.clone()).unwrap_or_default(),
            namespaces: RwLock::default(),
        }
    }

    /// Whether `ns` may be read at all, and whether that is decided per
    /// record.
    fn namespace(&self, ns: &str) -> (bool, bool) {
        if let Some(found) = self
            .namespaces
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(ns)
        {
            return *found;
        }
        let per_record = self.rights.per_record(ns);
        let readable = per_record || self.rights.on_namespace(Act::Read, ns);
        self.namespaces
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ns.to_owned(), (readable, per_record));
        (readable, per_record)
    }

    /// Whether records of `ns` are checked one by one (otherwise
    /// [`View::reads`] decides for all of them).
    pub fn per_record(&self, ns: &str) -> bool {
        self.namespace(ns).1
    }

    /// Whether anything in `ns` may be read.
    pub fn reads(&self, ns: &str) -> bool {
        self.namespace(ns).0
    }

    fn holds(&self, condition: &Condition, props: &Props, recorded_by: Option<&str>) -> bool {
        match condition {
            Condition::RecordedByCaller => {
                recorded_by.is_some() && recorded_by == self.account.as_deref()
            }
            Condition::Property { key, equals } => {
                let wanted = match equals {
                    PolicyValue::Caller => match &self.account {
                        Some(name) => Value::String(name.clone()),
                        None => return false,
                    },
                    PolicyValue::Attribute { name } => match self.attributes.get(name) {
                        Some(value) => value.clone(),
                        None => return false,
                    },
                    PolicyValue::Value { value } => value.clone(),
                };
                if key == "@recorded_by" {
                    return recorded_by.map(|r| Value::String(r.to_owned())) == Some(wanted);
                }
                props.get(key) == Some(&wanted)
            }
        }
    }

    /// Whether every policy for `applies` that covers a record holds.
    fn policies_hold(
        &self,
        ns: &str,
        labels: Option<&[String]>,
        props: &Props,
        recorded_by: Option<&str>,
        write: bool,
    ) -> bool {
        if self.rights.superuser {
            return true;
        }
        self.rights.policies.iter().all(|policy: &Policy| {
            let kind = match policy.applies {
                Applies::All => true,
                Applies::Read => !write,
                Applies::Write => write,
            };
            let namespace = match policy.namespace.strip_suffix('*') {
                Some(prefix) => ns.starts_with(prefix),
                None => policy.namespace == ns,
            };
            let label = match (&policy.label, labels) {
                (None, _) => true,
                (Some(label), Some(labels)) => labels.iter().any(|l| l == label),
                (Some(_), None) => false,
            };
            !(kind && namespace && label) || self.holds(&policy.condition, props, recorded_by)
        })
    }

    /// Whether the caller may see `node`.
    pub fn node_visible(&self, ns: &str, node: &Node) -> bool {
        let (readable, per_record) = self.namespace(ns);
        if !per_record {
            return readable;
        }
        self.rights.on_labels(Act::Read, ns, &node.labels, false)
            && self.policies_hold(
                ns,
                Some(&node.labels),
                &node.props,
                node.recorded_by.as_deref(),
                false,
            )
    }

    /// `node` as the caller sees it: `None` when hidden, else without its
    /// hidden properties.
    pub fn node(&self, ns: &str, mut node: Node) -> Option<Node> {
        if !self.node_visible(ns, &node) {
            return None;
        }
        if self.per_record(ns) {
            let labels = node.labels.clone();
            node.props
                .retain(|key, _| !self.rights.property_denied(Act::Read, ns, &labels, key));
        }
        Some(node)
    }

    /// Whether the caller may see `edge`; `visible` says whether a node is
    /// (its ends must be).
    pub fn edge_visible(
        &self,
        ns: &str,
        edge: &Edge,
        visible: impl Fn(NodeId) -> Result<bool>,
    ) -> Result<bool> {
        let (readable, per_record) = self.namespace(ns);
        if !per_record {
            return Ok(readable);
        }
        Ok(self
            .rights
            .on_labels(Act::Read, ns, std::slice::from_ref(&edge.label), true)
            && self.policies_hold(ns, None, &edge.props, edge.recorded_by.as_deref(), false)
            && visible(edge.from)?
            && visible(edge.to)?)
    }

    /// `edge` as the caller sees it, given whether its ends are visible.
    pub fn edge(
        &self,
        ns: &str,
        mut edge: Edge,
        visible: impl Fn(NodeId) -> Result<bool>,
    ) -> Result<Option<Edge>> {
        if !self.edge_visible(ns, &edge, visible)? {
            return Ok(None);
        }
        if self.per_record(ns) {
            let labels = [edge.label.clone()];
            edge.props
                .retain(|key, _| !self.rights.property_denied(Act::Read, ns, &labels, key));
        }
        Ok(Some(edge))
    }

    /// Whether a record policy covers anything in `ns`.
    pub fn has_policies(&self, ns: &str) -> bool {
        !self.rights.superuser
            && self
                .rights
                .policies
                .iter()
                .any(|policy| match policy.namespace.strip_suffix('*') {
                    Some(prefix) => ns.starts_with(prefix),
                    None => policy.namespace == ns,
                })
    }

    /// Whether a property of a record with these labels is hidden.
    pub fn property_hidden(&self, ns: &str, labels: &[String], key: &str) -> bool {
        self.per_record(ns) && self.rights.property_denied(Act::Read, ns, labels, key)
    }

    fn refuse(&self, privilege: &str, what: String) -> Error {
        Error::Forbidden(format!(
            "{} has no `{privilege}` privilege on {what}",
            match &self.account {
                Some(name) => format!("the account {name:?}"),
                None => "the caller".to_owned(),
            }
        ))
    }

    /// May the caller do `action` (create, update or delete) on `node` in
    /// `ns`, changing the properties `keys`? `before` is the node as it was
    /// (to update or delete it), `after` as it will be (created or updated).
    pub fn may_change_node(
        &self,
        action: Act,
        ns: &str,
        before: Option<&Node>,
        after: Option<&Node>,
        keys: &[&str],
    ) -> Result<()> {
        if self.rights.superuser {
            return Ok(());
        }
        if let Some(node) = before
            && !self.node_visible(ns, node)
        {
            // What it may not see does not exist for it.
            return Err(Error::NodeNotFound(node.id));
        }
        for node in [before, after].into_iter().flatten() {
            if !self.rights.on_labels(action, ns, &node.labels, false) {
                return Err(self.refuse(
                    &action.name(),
                    format!("nodes {} in {ns:?}", node.labels.join(":")),
                ));
            }
            for key in keys {
                if self.rights.property_denied(action, ns, &node.labels, key) {
                    return Err(self.refuse(
                        "write",
                        format!("the property {key} of {} in {ns:?}", node.labels.join(":")),
                    ));
                }
            }
            if !self.policies_hold(
                ns,
                Some(&node.labels),
                &node.props,
                node.recorded_by.as_deref(),
                true,
            ) {
                return Err(Error::Forbidden(format!(
                    "a record policy of {} does not let it change this node in {ns:?}",
                    self.account.as_deref().unwrap_or("the caller")
                )));
            }
        }
        Ok(())
    }

    /// [`View::may_change_node`] for an edge; `visible` says whether its
    /// ends are visible.
    pub fn may_change_edge(
        &self,
        action: Act,
        ns: &str,
        before: Option<&Edge>,
        after: Option<&Edge>,
        keys: &[&str],
        visible: impl Fn(NodeId) -> Result<bool>,
    ) -> Result<()> {
        if self.rights.superuser {
            return Ok(());
        }
        if let Some(edge) = before
            && !self.edge_visible(ns, edge, &visible)?
        {
            return Err(Error::EdgeNotFound(edge.id));
        }
        for edge in [before, after].into_iter().flatten() {
            let labels = std::slice::from_ref(&edge.label);
            if !self.rights.on_labels(action, ns, labels, true) {
                return Err(self.refuse(&action.name(), format!("edges {} in {ns:?}", edge.label)));
            }
            for key in keys {
                if self.rights.property_denied(action, ns, labels, key) {
                    return Err(self.refuse(
                        "write",
                        format!("the property {key} of {} edges in {ns:?}", edge.label),
                    ));
                }
            }
            if !self.policies_hold(ns, None, &edge.props, edge.recorded_by.as_deref(), true) {
                return Err(Error::Forbidden(format!(
                    "a record policy of {} does not let it change this edge in {ns:?}",
                    self.account.as_deref().unwrap_or("the caller")
                )));
            }
        }
        // Its ends must be visible to connect them.
        if let Some(edge) = after
            && (!visible(edge.from)? || !visible(edge.to)?)
        {
            return Err(Error::NodeNotFound(if visible(edge.from)? {
                edge.to
            } else {
                edge.from
            }));
        }
        Ok(())
    }

    /// Whether the caller may see `edge`: its label is read as an edge
    /// label is, and every node in it must be visible.
    pub fn hyperedge_visible(
        &self,
        ns: &str,
        edge: &HyperEdge,
        visible: impl Fn(NodeId) -> Result<bool>,
    ) -> Result<bool> {
        let (readable, per_record) = self.namespace(ns);
        if !per_record {
            return Ok(readable);
        }
        if !self
            .rights
            .on_labels(Act::Read, ns, std::slice::from_ref(&edge.label), true)
            || !self.policies_hold(ns, None, &edge.props, edge.recorded_by.as_deref(), false)
        {
            return Ok(false);
        }
        for node in edge.nodes() {
            if !visible(node)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `edge` as the caller sees it, given whether its nodes are visible.
    pub fn hyperedge(
        &self,
        ns: &str,
        mut edge: HyperEdge,
        visible: impl Fn(NodeId) -> Result<bool>,
    ) -> Result<Option<HyperEdge>> {
        if !self.hyperedge_visible(ns, &edge, visible)? {
            return Ok(None);
        }
        if self.per_record(ns) {
            let labels = [edge.label.clone()];
            edge.props
                .retain(|key, _| !self.rights.property_denied(Act::Read, ns, &labels, key));
        }
        Ok(Some(edge))
    }

    /// [`View::may_change_edge`] for a hyperedge: its label and properties
    /// are checked as an edge's, and every node in it must be visible.
    pub fn may_change_hyperedge(
        &self,
        action: Act,
        ns: &str,
        before: Option<&HyperEdge>,
        after: Option<&HyperEdge>,
        keys: &[&str],
        visible: impl Fn(NodeId) -> Result<bool>,
    ) -> Result<()> {
        if self.rights.superuser {
            return Ok(());
        }
        if let Some(edge) = before
            && !self.hyperedge_visible(ns, edge, &visible)?
        {
            return Err(Error::HyperEdgeNotFound(edge.id));
        }
        for edge in [before, after].into_iter().flatten() {
            let labels = std::slice::from_ref(&edge.label);
            if !self.rights.on_labels(action, ns, labels, true) {
                return Err(self.refuse(
                    &action.name(),
                    format!("hyperedges {} in {ns:?}", edge.label),
                ));
            }
            for key in keys {
                if self.rights.property_denied(action, ns, labels, key) {
                    return Err(self.refuse(
                        "write",
                        format!("the property {key} of {} hyperedges in {ns:?}", edge.label),
                    ));
                }
            }
            if !self.policies_hold(ns, None, &edge.props, edge.recorded_by.as_deref(), true) {
                return Err(Error::Forbidden(format!(
                    "a record policy of {} does not let it change this hyperedge in {ns:?}",
                    self.account.as_deref().unwrap_or("the caller")
                )));
            }
        }
        if let Some(edge) = after {
            for node in edge.nodes() {
                if !visible(node)? {
                    return Err(Error::NodeNotFound(node));
                }
            }
        }
        Ok(())
    }
}
