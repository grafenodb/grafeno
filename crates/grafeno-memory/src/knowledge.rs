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
/// Something not known yet, kept to be answered: open while nothing
/// [`ANSWERS`] it.
pub const QUESTION: &str = "Question";
/// A memory → the question it answers.
pub const ANSWERS: &str = "answers";
/// Raw material as it came: a conversation, a session, a document read.
/// Facts drawn from it name it in their `sources`.
pub const EPISODE: &str = "Episode";
/// An idea, not a thing in the world: "eventual consistency".
pub const CONCEPT: &str = "Concept";
/// Something proposed and not settled. Reflection lists hypotheses that
/// nothing supports.
pub const HYPOTHESIS: &str = "Hypothesis";
/// What supports a claim: an output, a page, a measurement. Linked with
/// [`SUPPORTED_BY`].
pub const EVIDENCE: &str = "Evidence";
/// A rule of the domain, written by an agent ("a service has one owner").
/// The engine's constraints, which decide on writes, are something else.
pub const CONSTRAINT: &str = "Constraint";
/// What a task is for.
pub const GOAL: &str = "Goal";
/// The kinds of memory with a meaning. A memory may have any other label
/// too; these are the ones an agent can count on.
pub const KINDS: [&str; 11] = [
    FACT, DECISION, EXPERIENCE, TASK, QUESTION, EPISODE, CONCEPT, HYPOTHESIS, EVIDENCE, CONSTRAINT,
    GOAL,
];

/// Relations with a meaning. Any other label stays allowed: these add
/// meaning, they do not close the list.
pub const RELATED_TO: &str = "related_to";
/// Needs the other to work, to happen or to be true.
pub const DEPENDS_ON: &str = "depends_on";
/// Is a piece of the other. (Between communities, the same label joins a
/// summary to the one above it.)
pub const PART_OF: &str = "part_of";
/// Was made from the other: a summary from notes, a figure from a table.
pub const DERIVED_FROM: &str = "derived_from";
/// Is supported by the other, as evidence.
pub const SUPPORTED_BY: &str = "supported_by";
/// Says the opposite of the other, where no constraint can tell.
pub const CONTRADICTS: &str = "contradicts";
/// Replaces the other, which was true or decided before.
pub const SUPERSEDES: &str = "supersedes";
/// Happened because of the other. Recorded only with evidence: the
/// `sources` of the fact.
pub const CAUSED_BY: &str = "caused_by";
/// The relations with a meaning.
pub const RELATIONS: [&str; 8] = [
    RELATED_TO,
    DEPENDS_ON,
    PART_OF,
    DERIVED_FROM,
    SUPPORTED_BY,
    CONTRADICTS,
    SUPERSEDES,
    CAUSED_BY,
];

/// How a memory or a fact was obtained: `agent` (an agent's own reading),
/// `rule` (extraction's rules), `stated` (told), `imported`, or any other
/// word. Free, as long as it is short.
pub const METHOD: &str = "method";

/// On a node that took others in by a merge: what each one was and how it
/// was known, so a merge loses no provenance.
pub const MERGED_FROM: &str = "merged_from";

/// Checks a fact before it is recorded: a causal claim needs evidence.
pub fn check_fact(label: &str, sources: &[NodeId]) -> Result<()> {
    if label == CAUSED_BY && sources.is_empty() {
        return Err(Error::Invalid(
            "a caused_by fact needs evidence: name in `sources` what shows the cause \
             (or record it as related_to, or as a Hypothesis)"
                .into(),
        ));
    }
    Ok(())
}

/// The `method` property, checked: a short word or phrase.
pub fn method(method: Option<&str>) -> Result<Props> {
    let mut props = Props::new();
    if let Some(method) = method.map(str::trim).filter(|m| !m.is_empty()) {
        if method.chars().count() > 64 {
            return Err(Error::Invalid(
                "a method is a short word, 64 characters at most".into(),
            ));
        }
        props.insert(METHOD.into(), json!(method));
    }
    Ok(props)
}

/// Before `merged` is merged into `kept`: what `merged` was and how it was
/// known, appended to `kept`'s [`MERGED_FROM`]. Undoing the merge brings
/// both back as they were, this list included.
pub fn note_merge(tx: &mut WriteTx, ns: &str, kept: NodeId, merged: NodeId) -> Result<()> {
    let keep = tx.node(ns, kept)?.ok_or(Error::NodeNotFound(kept))?;
    let gone = tx.node(ns, merged)?.ok_or(Error::NodeNotFound(merged))?;
    let assessed = of_node(&gone);
    let mut entry = json!({
        "id": merged.to_string(),
        "name": gone.display_name(),
        "labels": gone.labels,
        "created": gone.created_at.date(),
    });
    for (key, value) in [
        (ORIGIN, assessed.origin.map(|o| json!(o.as_str()))),
        (CERTAINTY, assessed.certainty.map(|c| json!(c))),
        (
            VERIFIED_BY,
            assessed.verified_by.map(|id| json!(id.to_string())),
        ),
        (METHOD, gone.prop(METHOD).cloned()),
        ("recorded_by", gone.recorded_by.clone().map(|by| json!(by))),
    ] {
        if let Some(value) = value {
            entry[key] = value;
        }
    }
    let mut list = keep
        .prop(MERGED_FROM)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    list.push(entry);
    let mut props = Props::new();
    props.insert(MERGED_FROM.into(), Value::Array(list));
    tx.set_node_props(ns, kept, props)
}

/// The same current fact recorded more than once: same ends, label and
/// valid times, neither retracted nor expired.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DuplicateFacts {
    /// The first one recorded.
    pub kept: EdgeId,
    pub duplicates: Vec<EdgeId>,
    pub from: NodeId,
    pub label: String,
    pub to: NodeId,
}

fn same_fact(a: &Edge, b: &Edge) -> bool {
    a.label == b.label && a.valid_from == b.valid_from && a.valid_to == b.valid_to
}

/// The current fact `from -label-> to` with these valid times, if it is
/// already recorded.
pub fn existing_fact(
    tx: &WriteTx,
    ns: &str,
    from: NodeId,
    label: &str,
    to: NodeId,
    valid_from: Option<Timestamp>,
    valid_to: Option<Timestamp>,
) -> Result<Option<EdgeId>> {
    let probe = Edge {
        id: EdgeId::from_u128(0),
        label: label.to_owned(),
        from,
        to,
        props: Props::new(),
        sources: Vec::new(),
        valid_from,
        valid_to,
        recorded_at: Timestamp::from_micros(0),
        expired_at: None,
        recorded_by: None,
    };
    for (id, other) in tx.neighbor_ids(
        ns,
        from,
        grafeno::Direction::Out,
        &grafeno::EdgeFilter::any().label(label),
    )? {
        if other != to {
            continue;
        }
        if let Some(edge) = tx.edge(ns, id)?
            && edge.expired_at.is_none()
            && same_fact(&edge, &probe)
        {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// What makes two facts the same one: ends, label and valid times.
type FactKey = (NodeId, String, NodeId, Option<i64>, Option<i64>);

/// Facts recorded more than once in `ns`, the first of each kept.
pub fn duplicate_facts(
    read: &grafeno::ReadTx,
    ns: &str,
    limit: usize,
) -> Result<Vec<DuplicateFacts>> {
    let mut groups: std::collections::BTreeMap<FactKey, Vec<Edge>> = Default::default();
    let mut after = None;
    loop {
        let page = read.edges_page(ns, None, after, 5_000)?;
        let Some(last) = page.last() else { break };
        after = Some(last.id);
        for edge in page {
            if edge.expired_at.is_some() || edge.is_retracted() {
                continue;
            }
            groups
                .entry((
                    edge.from,
                    edge.label.clone(),
                    edge.to,
                    edge.valid_from.map(Timestamp::as_micros),
                    edge.valid_to.map(Timestamp::as_micros),
                ))
                .or_default()
                .push(edge);
        }
    }
    let mut out = Vec::new();
    for ((from, label, to, _, _), mut edges) in groups {
        if edges.len() < 2 {
            continue;
        }
        edges.sort_by_key(|edge| (edge.recorded_at, edge.id));
        out.push(DuplicateFacts {
            kept: edges[0].id,
            duplicates: edges[1..].iter().map(|edge| edge.id).collect(),
            from,
            label,
            to,
        });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

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

/// What supports a memory, and what puts it in doubt, as stored: the
/// factual side of a result, apart from how the search found it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Justification {
    /// What it came from.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<NodeId>,
    /// How it is known: origin, certainty, verification.
    pub known: Value,
    /// What supports it ([`SUPPORTED_BY`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported_by: Vec<NodeId>,
    /// What says the opposite ([`CONTRADICTS`], either way).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contradicted_by: Vec<NodeId>,
    /// What replaced it ([`SUPERSEDES`] pointing at it).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded_by: Vec<NodeId>,
    /// Archived (summarized, obsolete or wrong).
    pub archived: bool,
}

/// What supports `node` and what puts it in doubt.
pub fn justify(read: &grafeno::ReadTx, ns: &str, node: &Node) -> Result<Justification> {
    use grafeno::{Direction, EdgeFilter};
    let ends = |direction: Direction, label: &str| -> Result<Vec<NodeId>> {
        Ok(read
            .neighbor_ids(ns, node.id, direction, &EdgeFilter::any().label(label))?
            .into_iter()
            .map(|(_, other)| other)
            .collect())
    };
    Ok(Justification {
        sources: node.sources.clone(),
        known: of_node(node).to_json(),
        supported_by: ends(Direction::Out, SUPPORTED_BY)?,
        contradicted_by: ends(Direction::Both, CONTRADICTS)?,
        superseded_by: ends(Direction::In, SUPERSEDES)?,
        archived: node.has_label(crate::recall::ARCHIVED),
    })
}
