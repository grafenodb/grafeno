use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{EdgeId, HyperEdgeId, NodeId, Timestamp};

/// Properties of a node, edge or hyperedge.
pub type Props = serde_json::Map<String, Value>;

/// A node: an entity, document, chunk, episode… whatever its labels say.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    /// Sorted, without duplicates.
    pub labels: Vec<String>,
    #[serde(default)]
    pub props: Props,
    /// Nodes this one was derived from (provenance).
    #[serde(default)]
    pub sources: Vec<NodeId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// The principal (API key, agent) that created it, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
}

impl Node {
    pub fn has_label(&self, label: &str) -> bool {
        self.labels
            .binary_search_by(|item| item.as_str().cmp(label))
            .is_ok()
    }

    pub fn prop(&self, key: &str) -> Option<&Value> {
        self.props.get(key)
    }

    /// The `name`, `title` or `label` property, if the node has one.
    pub fn explicit_name(&self) -> Option<&str> {
        ["name", "title", "label"]
            .iter()
            .find_map(|key| self.prop(key).and_then(Value::as_str))
    }

    /// A short human name: [`Node::explicit_name`], or the first label and
    /// the start of the id.
    pub fn display_name(&self) -> String {
        self.explicit_name().map(str::to_owned).unwrap_or_else(|| {
            let label = self.labels.first().map(String::as_str).unwrap_or("Node");
            let id = self.id.to_string();
            format!("{label} {}", &id[id.len() - 8..])
        })
    }
}

/// A directed edge: a fact relating two nodes.
///
/// It has two time axes. *Valid time* (`valid_from`, `valid_to`) says when the
/// fact is true in the world; `None` is open-ended. *Record time*
/// (`recorded_at`, `expired_at`) says when the engine learned it and when it
/// was retracted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub label: String,
    pub from: NodeId,
    pub to: NodeId,
    #[serde(default)]
    pub props: Props,
    #[serde(default)]
    pub sources: Vec<NodeId>,
    pub valid_from: Option<Timestamp>,
    pub valid_to: Option<Timestamp>,
    pub recorded_at: Timestamp,
    pub expired_at: Option<Timestamp>,
    /// The principal (API key, agent) that recorded it, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
}

impl Edge {
    /// The fact is true at `at` (`valid_from <= at < valid_to`).
    pub fn holds_at(&self, at: Timestamp) -> bool {
        self.valid_from.is_none_or(|from| from <= at) && self.valid_to.is_none_or(|to| at < to)
    }

    /// The engine knew the fact at `at` (`recorded_at <= at < expired_at`).
    pub fn known_at(&self, at: Timestamp) -> bool {
        self.recorded_at <= at && self.expired_at.is_none_or(|expired| at < expired)
    }

    pub fn is_retracted(&self) -> bool {
        self.expired_at.is_some()
    }

    /// The other end, seen from `node`.
    pub fn other(&self, node: NodeId) -> NodeId {
        if self.from == node {
            self.to
        } else {
            self.from
        }
    }
}

/// Data to create a node.
#[derive(Clone, Debug, Default)]
pub struct NewNode {
    pub labels: Vec<String>,
    pub props: Props,
    pub sources: Vec<NodeId>,
    /// Text indexed for full-text search (BM25).
    pub text: Option<String>,
    /// Embedding indexed for vector search.
    pub embedding: Option<Vec<f32>>,
}

impl NewNode {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            labels: vec![label.into()],
            ..Self::default()
        }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }

    pub fn prop(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.props.insert(key.into(), value.into());
        self
    }

    pub fn source(mut self, source: NodeId) -> Self {
        self.sources.push(source);
        self
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    pub fn embedding(mut self, embedding: impl Into<Vec<f32>>) -> Self {
        self.embedding = Some(embedding.into());
        self
    }
}

/// Data to create an edge.
#[derive(Clone, Debug)]
pub struct NewEdge {
    pub from: NodeId,
    pub label: String,
    pub to: NodeId,
    pub props: Props,
    pub sources: Vec<NodeId>,
    pub valid_from: Option<Timestamp>,
    pub valid_to: Option<Timestamp>,
}

impl NewEdge {
    pub fn new(from: NodeId, label: impl Into<String>, to: NodeId) -> Self {
        Self {
            from,
            label: label.into(),
            to,
            props: Props::new(),
            sources: Vec::new(),
            valid_from: None,
            valid_to: None,
        }
    }

    pub fn prop(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.props.insert(key.into(), value.into());
        self
    }

    pub fn source(mut self, source: NodeId) -> Self {
        self.sources.push(source);
        self
    }

    pub fn valid_from(mut self, at: Timestamp) -> Self {
        self.valid_from = Some(at);
        self
    }

    pub fn valid_to(mut self, at: Timestamp) -> Self {
        self.valid_to = Some(at);
        self
    }
}

/// A node taking part in a hyperedge, with the role it plays there.
///
/// Participants sort by node, then role (no role first): that is the order a
/// hyperedge stores them in. The order carries no meaning (no direction).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Participant {
    pub node: NodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl Participant {
    /// A participant without a role.
    pub fn new(node: NodeId) -> Self {
        Self { node, role: None }
    }

    /// A participant playing `role`.
    pub fn with_role(node: NodeId, role: impl Into<String>) -> Self {
        Self {
            node,
            role: Some(role.into()),
        }
    }
}

/// A hyperedge: one fact relating two or more nodes at once (a decision,
/// a collaboration, an event), each in a role of its own.
///
/// Unlike an [`Edge`] it has no direction and any number of ends. It is a
/// record of its own: its id, label, properties, provenance and both time
/// axes work as an edge's do (valid time, record time).
///
/// Invariants, kept by the engine: `participants` is sorted and has no
/// repeated (node, role) pair; at least two distinct nodes take part; a
/// node may take part more than once, each time in a different role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HyperEdge {
    pub id: HyperEdgeId,
    /// What kind of relation it is (`decision`, `collaboration`…).
    pub label: String,
    pub participants: Vec<Participant>,
    #[serde(default)]
    pub props: Props,
    #[serde(default)]
    pub sources: Vec<NodeId>,
    pub valid_from: Option<Timestamp>,
    pub valid_to: Option<Timestamp>,
    pub recorded_at: Timestamp,
    pub expired_at: Option<Timestamp>,
    /// The principal (API key, agent) that recorded it, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_by: Option<String>,
}

impl HyperEdge {
    /// The fact is true at `at` (`valid_from <= at < valid_to`).
    pub fn holds_at(&self, at: Timestamp) -> bool {
        self.valid_from.is_none_or(|from| from <= at) && self.valid_to.is_none_or(|to| at < to)
    }

    /// The engine knew the fact at `at` (`recorded_at <= at < expired_at`).
    pub fn known_at(&self, at: Timestamp) -> bool {
        self.recorded_at <= at && self.expired_at.is_none_or(|expired| at < expired)
    }

    pub fn is_retracted(&self) -> bool {
        self.expired_at.is_some()
    }

    /// The distinct nodes that take part, in order.
    pub fn nodes(&self) -> Vec<NodeId> {
        let mut nodes: Vec<NodeId> = self.participants.iter().map(|p| p.node).collect();
        nodes.dedup();
        nodes
    }

    /// Whether `node` takes part (in any role).
    pub fn has_node(&self, node: NodeId) -> bool {
        self.participants.iter().any(|p| p.node == node)
    }

    /// The roles `node` plays (`None` for a participant without a role).
    pub fn roles_of(&self, node: NodeId) -> Vec<Option<&str>> {
        self.participants
            .iter()
            .filter(|p| p.node == node)
            .map(|p| p.role.as_deref())
            .collect()
    }

    /// The nodes that play `role`.
    pub fn with_role(&self, role: &str) -> Vec<NodeId> {
        self.participants
            .iter()
            .filter(|p| p.role.as_deref() == Some(role))
            .map(|p| p.node)
            .collect()
    }
}

/// Data to create a hyperedge.
#[derive(Clone, Debug)]
pub struct NewHyperEdge {
    pub label: String,
    pub participants: Vec<Participant>,
    pub props: Props,
    pub sources: Vec<NodeId>,
    pub valid_from: Option<Timestamp>,
    pub valid_to: Option<Timestamp>,
}

impl NewHyperEdge {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            participants: Vec::new(),
            props: Props::new(),
            sources: Vec::new(),
            valid_from: None,
            valid_to: None,
        }
    }

    /// Adds a participant without a role.
    pub fn node(mut self, node: NodeId) -> Self {
        self.participants.push(Participant::new(node));
        self
    }

    /// Adds a participant playing `role`.
    pub fn member(mut self, node: NodeId, role: impl Into<String>) -> Self {
        self.participants.push(Participant::with_role(node, role));
        self
    }

    pub fn prop(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.props.insert(key.into(), value.into());
        self
    }

    pub fn source(mut self, source: NodeId) -> Self {
        self.sources.push(source);
        self
    }

    pub fn valid_from(mut self, at: Timestamp) -> Self {
        self.valid_from = Some(at);
        self
    }

    pub fn valid_to(mut self, at: Timestamp) -> Self {
        self.valid_to = Some(at);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn participants_sort_by_node_then_role() {
        let (a, b) = (NodeId::from_u128(1), NodeId::from_u128(2));
        let mut list = vec![
            Participant::with_role(b, "x"),
            Participant::with_role(a, "z"),
            Participant::new(a),
            Participant::with_role(a, "b"),
        ];
        list.sort();
        assert_eq!(
            list,
            vec![
                Participant::new(a),
                Participant::with_role(a, "b"),
                Participant::with_role(a, "z"),
                Participant::with_role(b, "x"),
            ]
        );
    }

    #[test]
    fn hyperedges_round_trip_through_json_without_empty_roles() {
        let edge = HyperEdge {
            id: HyperEdgeId::from_u128(9),
            label: "decision".into(),
            participants: vec![
                Participant::new(NodeId::from_u128(1)),
                Participant::with_role(NodeId::from_u128(2), "reviewer"),
            ],
            props: Props::new(),
            sources: Vec::new(),
            valid_from: None,
            valid_to: None,
            recorded_at: Timestamp::from_micros(5),
            expired_at: None,
            recorded_by: None,
        };
        let json = serde_json::to_string(&edge).unwrap();
        assert_eq!(json.matches("\"role\"").count(), 1);
        let back: HyperEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(back, edge);
        assert_eq!(
            back.nodes(),
            vec![NodeId::from_u128(1), NodeId::from_u128(2)]
        );
        assert_eq!(back.with_role("reviewer"), vec![NodeId::from_u128(2)]);
    }
}
