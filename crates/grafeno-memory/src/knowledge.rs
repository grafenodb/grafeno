//! How a memory or a fact came to be known, and how sure it is.
//!
//! Two properties with a fixed meaning, on nodes and on edges:
//!
//! - [`CERTAINTY`]: a number from 0 to 1.
//! - [`ORIGIN`]: [`Origin::Stated`] (someone said it), [`Origin::Observed`]
//!   (the agent saw it: a file, an output), [`Origin::Inferred`] (the agent
//!   concluded it) or [`Origin::Verified`] (checked against a source).
//!
//! A record with neither was written before they existed, or by someone
//! who did not say: it reads as not stated, never as verified. Nothing in
//! the engine decides either value; an agent states them, and
//! [`verify`] records a check with the source that supports it.
//!
//! The kinds of memory an agent stores are labels: [`KINDS`] are the ones
//! with a meaning here, beside any other.

use grafeno::{Edge, EdgeId, Error, Node, NodeId, Props, Result, Timestamp, Value, WriteTx};
use serde::Serialize;
use serde_json::json;

/// How sure a memory or a fact is, from 0 to 1.
pub const CERTAINTY: &str = "certainty";
/// How a memory or a fact came to be known.
pub const ORIGIN: &str = "origin";
/// The node that supports a verified memory or fact.
pub const VERIFIED_BY: &str = "verified_by";
/// When it was verified.
pub const VERIFIED_AT: &str = "verified_at";
/// What it was before it was verified: `{ "origin": …, "certainty": … }`.
pub const VERIFIED_FROM: &str = "verified_from";

/// Something that is the case.
pub const FACT: &str = "Fact";
/// A choice made, with its reasons.
pub const DECISION: &str = "Decision";
/// Something that happened, as it was lived: an attempt, an incident.
pub const EXPERIENCE: &str = "Experience";
/// Something to do.
pub const TASK: &str = "Task";
/// The kinds of memory with a meaning. A memory may have any other label
/// too; these are the ones an agent can count on.
pub const KINDS: [&str; 4] = [FACT, DECISION, EXPERIENCE, TASK];

/// How a memory or a fact came to be known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Someone said or wrote it.
    Stated,
    /// The agent saw it: a file, a command's output, a page.
    Observed,
    /// The agent concluded it from other things.
    Inferred,
    /// Checked against a source.
    Verified,
}

impl Origin {
    pub const ALL: [Origin; 4] = [
        Origin::Stated,
        Origin::Observed,
        Origin::Inferred,
        Origin::Verified,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Stated => "stated",
            Origin::Observed => "observed",
            Origin::Inferred => "inferred",
            Origin::Verified => "verified",
        }
    }

    pub fn parse(text: &str) -> Result<Origin> {
        Origin::ALL
            .into_iter()
            .find(|origin| origin.as_str() == text)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown origin {text:?}: use \"stated\", \"observed\", \"inferred\" or \"verified\""
                ))
            })
    }
}

/// How a memory or a fact is known, as its properties say.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Assessment {
    /// Absent: not stated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// Absent: not stated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certainty: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_by: Option<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
}

impl Assessment {
    /// Neither origin nor certainty is stated.
    pub fn is_unstated(&self) -> bool {
        self.origin.is_none() && self.certainty.is_none()
    }

    /// A short note for a rendered line: `inferred, certainty 0.6`; empty
    /// when nothing is stated.
    pub fn note(&self) -> String {
        let mut parts = Vec::new();
        if let Some(origin) = self.origin {
            parts.push(origin.as_str().to_owned());
        }
        if let Some(certainty) = self.certainty {
            parts.push(format!("certainty {}", round2(certainty)));
        }
        parts.join(", ")
    }

    /// As JSON for a tool: what is stated, or `"not stated"`.
    pub fn to_json(&self) -> Value {
        if self.is_unstated() {
            return json!("not stated");
        }
        let mut value = serde_json::to_value(self).unwrap_or_default();
        if let Some(certainty) = self.certainty {
            value[CERTAINTY] = json!(round2(certainty));
        }
        value
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn of_props(props: &Props) -> Assessment {
    Assessment {
        origin: props
            .get(ORIGIN)
            .and_then(Value::as_str)
            .and_then(|text| Origin::parse(text).ok()),
        certainty: props
            .get(CERTAINTY)
            .and_then(Value::as_f64)
            .filter(|certainty| (0.0..=1.0).contains(certainty)),
        verified_by: props
            .get(VERIFIED_BY)
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<uuid::Uuid>().ok())
            .map(NodeId::from),
        verified_at: props
            .get(VERIFIED_AT)
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// How a memory is known.
pub fn of_node(node: &Node) -> Assessment {
    of_props(&node.props)
}

/// How a fact is known.
pub fn of_edge(edge: &Edge) -> Assessment {
    of_props(&edge.props)
}

/// The properties that state an origin and a certainty, checked: a
/// certainty outside 0 to 1 and an unknown origin are refused. Stating
/// `verified` here needs no source; [`verify`] is how a check is recorded
/// with one.
pub fn stated(origin: Option<&str>, certainty: Option<f64>) -> Result<Props> {
    let mut props = Props::new();
    if let Some(origin) = origin {
        props.insert(ORIGIN.into(), json!(Origin::parse(origin)?.as_str()));
    }
    if let Some(certainty) = certainty {
        if !(0.0..=1.0).contains(&certainty) || certainty.is_nan() {
            return Err(Error::Invalid("certainty must be between 0 and 1".into()));
        }
        props.insert(CERTAINTY.into(), json!(certainty));
    }
    Ok(props)
}

/// A memory or a fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Record {
    Node(NodeId),
    Edge(EdgeId),
}

/// Records that a memory or a fact was checked and holds: its origin
/// becomes [`Origin::Verified`], its certainty `certainty` (1 when not
/// given), with the source that supports it and when. What it was before
/// is kept in [`VERIFIED_FROM`]. The source must be a node of the same
/// namespace, and not the record itself.
pub fn verify(
    tx: &mut WriteTx,
    ns: &str,
    record: Record,
    source: Option<NodeId>,
    certainty: Option<f64>,
    at: Timestamp,
) -> Result<Assessment> {
    let before = match record {
        Record::Node(id) => of_node(&tx.node(ns, id)?.ok_or(Error::NodeNotFound(id))?),
        Record::Edge(id) => of_edge(&tx.edge(ns, id)?.ok_or(Error::EdgeNotFound(id))?),
    };
    if let Some(source) = source {
        if record == Record::Node(source) {
            return Err(Error::Invalid(
                "a memory cannot be the source that verifies itself".into(),
            ));
        }
        tx.node(ns, source)?.ok_or(Error::NodeNotFound(source))?;
    }
    let mut props = stated(
        Some(Origin::Verified.as_str()),
        Some(certainty.unwrap_or(1.0)),
    )?;
    props.insert(VERIFIED_AT.into(), json!(at.date()));
    if let Some(source) = source {
        props.insert(VERIFIED_BY.into(), json!(source.to_string()));
    }
    props.insert(
        VERIFIED_FROM.into(),
        json!({
            ORIGIN: before.origin.map(Origin::as_str),
            CERTAINTY: before.certainty,
        }),
    );
    match record {
        Record::Node(id) => {
            tx.set_node_props(ns, id, props)?;
            // A source named by an earlier check does not support this one.
            if source.is_none() {
                tx.remove_node_props(ns, id, &[VERIFIED_BY])?;
            }
            Ok(of_node(&tx.node(ns, id)?.ok_or(Error::NodeNotFound(id))?))
        }
        Record::Edge(id) => {
            tx.set_edge_props(ns, id, props)?;
            if source.is_none() {
                tx.remove_edge_props(ns, id, &[VERIFIED_BY])?;
            }
            Ok(of_edge(&tx.edge(ns, id)?.ok_or(Error::EdgeNotFound(id))?))
        }
    }
}
