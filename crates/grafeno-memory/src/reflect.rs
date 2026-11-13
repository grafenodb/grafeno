//! What is known about a topic, a room or a node, and what is not: the
//! memories and facts found, how they are known, what clashed, what may be
//! out of date, what is missing, and what to check next.
//!
//! Every item is computed from what is stored and names the record it
//! comes from; nothing is judged true or false, and nothing is written.

use std::collections::{BTreeMap, HashMap, HashSet};

use grafeno::{
    Direction, Edge, EdgeFilter, EdgeId, Node, NodeId, ReadTx, Result, Timestamp, Value,
};
use serde::{Deserialize, Serialize};

use crate::knowledge::{self, ANSWERS, Origin, QUESTION, SUPPORTED_BY};
use crate::palace::{self, ANCHORED_AT, HAS_LOCUS, HAS_ROOM, ROOM};
use crate::recall::ARCHIVED;

/// What to reflect on.
#[derive(Clone, Debug)]
pub enum Scope {
    /// The memories whose words match it, and those in the rooms it names.
    Topic(String),
    /// The memories anchored in a room.
    Room(NodeId),
    /// A node and what it is related to.
    Node(NodeId),
}

/// The label of a node that declares what a kind of memory is expected to
/// have: properties `kind`, `relations` and `properties` (lists of names).
pub const SHAPE: &str = "Shape";

/// What a kind of memory is expected to have, declared by an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    /// Relations (labels, either direction) each memory of the kind should
    /// have, such as `supported_by` for a Decision.
    #[serde(default)]
    pub relations: Vec<String>,
    /// Properties it should have, such as `reason`.
    #[serde(default)]
    pub properties: Vec<String>,
}

/// The shapes declared in a namespace: nodes labeled [`SHAPE`], by kind.
pub fn shapes(read: &ReadTx, ns: &str) -> Result<BTreeMap<String, Shape>> {
    let mut found = BTreeMap::new();
    for id in read.nodes_with_label(ns, SHAPE)? {
        let Some(node) = read.node(ns, id)? else {
            continue;
        };
        let Some(kind) = node.prop("kind").and_then(Value::as_str) else {
            continue;
        };
        let names = |key: &str| -> Vec<String> {
            node.prop(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        found.insert(
            kind.to_owned(),
            Shape {
                relations: names("relations"),
                properties: names("properties"),
            },
        );
    }
    Ok(found)
}

/// How a reflection looks.
#[derive(Clone, Debug)]
pub struct ReflectOptions {
    /// Items listed per section at most (each section says its total).
    pub limit: usize,
    /// Text matches a topic looks at.
    pub matches: usize,
    /// A stated certainty below this is uncertain.
    pub low_certainty: f64,
    /// A memory weaker than this (importance decayed since its last use)
    /// may be out of date.
    pub weak: f32,
    pub half_life_secs: f64,
    pub now: Timestamp,
    /// Shapes expected of kinds of memory, beside (and over) those
    /// declared in the namespace with [`SHAPE`] nodes.
    pub shapes: BTreeMap<String, Shape>,
}

impl Default for ReflectOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            matches: 50,
            low_certainty: 0.5,
            weak: 0.1,
            half_life_secs: 30.0 * 86_400.0,
            now: Timestamp::now(),
            shapes: BTreeMap::new(),
        }
    }
}

/// A memory or a fact a section points at.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Item {
    /// The node's or the fact's id.
    pub id: String,
    /// `memory`, `fact`, `entity`, `question` or `place`.
    pub kind: &'static str,
    /// Its name, its words, or the fact as a sentence.
    pub text: String,
    /// Why it is in this section.
    pub why: String,
}

/// A clash: recorded under a constraint, or by an agent with a
/// `contradicts` relation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Clash {
    /// `change` when the newer took the older's place (the constraint
    /// ended or shortened the older: the state changed, nothing is wrong);
    /// `contradiction` when both stand and say opposite things (flagged
    /// under a constraint, or recorded with `contradicts`).
    pub kind: &'static str,
    /// The constraint's name; `contradicts` for one an agent recorded.
    pub constraint: String,
    /// `invalidated`, `shortened`, `replaced` or `flagged` under a
    /// constraint; `recorded` for a `contradicts` relation.
    pub action: String,
    /// Whether the constraint settled it (an older fact ended or gave
    /// way); a flagged clash keeps both facts and waits for someone.
    pub settled: bool,
    pub new_fact: Item,
    pub old_fact: Item,
}

/// What is known, counted.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Known {
    pub memories: usize,
    pub facts: usize,
    /// Memories by their kind (Fact, Decision, Experience, Task, Question,
    /// or other).
    pub memories_by_kind: BTreeMap<String, usize>,
    /// Memories, then facts, by how they are known (stated, observed,
    /// inferred, verified, or not stated).
    pub memories_by_origin: BTreeMap<String, usize>,
    pub facts_by_origin: BTreeMap<String, usize>,
    /// What was checked against a source.
    pub verified: Vec<Item>,
    /// Memories and facts by the evidence behind them, each counted once,
    /// in this order: `conflicting` (in a clash not settled), `obsolete`
    /// (ended, archived), `verified`, `supported` (with sources or
    /// `supported_by`), then with nothing behind them: `inferred, no
    /// source`, `stated, no source` (stated or observed), `not stated, no
    /// source`. These are counts of records, not probabilities.
    pub by_evidence: BTreeMap<&'static str, usize>,
}

/// A thing to do next, about the items it names.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    pub what: String,
    pub how: &'static str,
    pub about: Vec<String>,
}

/// What a reflection could not see, and how it looked.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Limits {
    pub memories_looked_at: usize,
    pub facts_looked_at: usize,
    /// Text matches of the topic, and whether there were more than it
    /// looked at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_matches: Option<usize>,
    pub more_matches: bool,
    pub notes: Vec<String>,
}

/// What is known about a scope, and what is not.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Reflection {
    pub scope: String,
    /// Nothing is stored about it: the other sections are empty, and no
    /// unrelated memory is offered in their place.
    pub nothing: bool,
    pub known: Known,
    /// Inferred with no source, or of low certainty.
    pub uncertain: Vec<Item>,
    /// Said to be stated or observed, with no source and nothing
    /// supporting it: a claim whose evidence was never recorded.
    pub unsupported: Vec<Item>,
    pub conflicts: Vec<Clash>,
    /// Facts no longer true, archived or obsolete memories, memories long
    /// unused, and older memories a newer one may overtake.
    pub stale: Vec<Item>,
    /// Open questions, things mentioned and never described, empty
    /// places, and next steps left in a checkpoint.
    pub gaps: Vec<Item>,
    pub next: Vec<Step>,
    /// Totals of each list before `limit` cut them.
    pub totals: BTreeMap<&'static str, usize>,
    pub limits: Limits,
    /// The sources behind the items listed, by item id: a memory's or a
    /// fact's `sources`, and what a memory is `supported_by`. Items with
    /// none are not here.
    pub sources: BTreeMap<String, Vec<String>>,
}

fn snippet(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(120) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    }
}

fn describe(read: &ReadTx, ns: &str, node: &Node) -> Result<String> {
    if let Some(name) = node.explicit_name() {
        return Ok(name.to_owned());
    }
    let text = read.text(ns, node.id)?.unwrap_or_default();
    Ok(if text.trim().is_empty() {
        node.display_name()
    } else {
        snippet(&text)
    })
}

fn sentence(read: &ReadTx, ns: &str, edge: &Edge) -> Result<String> {
    let name = |id: NodeId| -> Result<String> {
        Ok(match read.node(ns, id)? {
            Some(node) => describe(read, ns, &node)?,
            None => id.to_string(),
        })
    };
    Ok(format!(
        "{} {} {}",
        name(edge.from)?,
        edge.label,
        name(edge.to)?
    ))
}

/// Edges that arrange memories or summarize the graph, not facts about
/// what the memories say.
const NOT_FACTS: [&str; 6] = [
    ANCHORED_AT,
    HAS_ROOM,
    HAS_LOCUS,
    crate::communities::MEMBER_OF,
    crate::communities::PART_OF,
    crate::reconcile::MENTIONS,
];

fn origin_name(origin: Option<Origin>) -> String {
    origin.map_or("not stated".into(), |origin| origin.as_str().into())
}

/// What is known about `scope`, and what is not. Reads only.
pub fn reflect(
    read: &ReadTx,
    ns: &str,
    scope: &Scope,
    options: &ReflectOptions,
) -> Result<Reflection> {
    let mut reflection = Reflection::default();
    let mut memories: Vec<Node> = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut add = |node: Node, memories: &mut Vec<Node>| {
        if !palace::is_structure(&node)
            && !node.has_label(crate::communities::COMMUNITY)
            && !node.has_label(SHAPE)
            && seen.insert(node.id)
        {
            memories.push(node);
        }
    };
    match scope {
        Scope::Topic(topic) => {
            reflection.scope = format!("the topic {topic:?}");
            let hits = read.search_text(ns, topic, options.matches)?;
            reflection.limits.text_matches = Some(hits.len());
            reflection.limits.more_matches = hits.len() >= options.matches;
            for hit in hits {
                if let Some(node) = read.node(ns, hit.node)? {
                    add(node, &mut memories);
                }
            }
            // The rooms it names: their memories too.
            let mut rooms = crate::RecallQuery::new().text(topic).limit(5).expand(None);
            rooms.ranking = None;
            rooms.labels = vec![ROOM.to_owned()];
            for room in crate::recall(read, ns, &rooms)? {
                for (_, locus) in read.neighbor_ids(
                    ns,
                    room.node.id,
                    Direction::Out,
                    &EdgeFilter::any().label(HAS_LOCUS),
                )? {
                    for (_, memory) in read.neighbor_ids(
                        ns,
                        locus,
                        Direction::In,
                        &EdgeFilter::any().label(ANCHORED_AT),
                    )? {
                        if let Some(node) = read.node(ns, memory)? {
                            add(node, &mut memories);
                        }
                    }
                }
            }
            reflection.limits.notes.push(
                "A topic is matched by its words, whole, in the memories' text: a memory that says \
                 it in other words, or only by meaning, is not found."
                    .into(),
            );
        }
        Scope::Room(room) => {
            let node = read
                .node(ns, *room)?
                .ok_or(grafeno::Error::NodeNotFound(*room))?;
            if !node.has_label(ROOM) {
                return Err(grafeno::Error::Invalid(format!("{room} is not a Room")));
            }
            reflection.scope = format!("the room {}", describe(read, ns, &node)?);
            for (_, locus) in read.neighbor_ids(
                ns,
                *room,
                Direction::Out,
                &EdgeFilter::any().label(HAS_LOCUS),
            )? {
                let Some(locus_node) = read.node(ns, locus)? else {
                    continue;
                };
                let anchored = read.neighbor_ids(
                    ns,
                    locus,
                    Direction::In,
                    &EdgeFilter::any().label(ANCHORED_AT),
                )?;
                if anchored.is_empty()
                    && locus_node.prop("name").and_then(Value::as_str) != Some(palace::ENTRANCE)
                {
                    reflection.gaps.push(Item {
                        id: locus.to_string(),
                        kind: "place",
                        text: describe(read, ns, &locus_node)?,
                        why: "a place in the room with nothing at it".into(),
                    });
                }
                for (_, memory) in anchored {
                    if let Some(node) = read.node(ns, memory)? {
                        add(node, &mut memories);
                    }
                }
            }
        }
        Scope::Node(id) => {
            let node = read
                .node(ns, *id)?
                .ok_or(grafeno::Error::NodeNotFound(*id))?;
            reflection.scope = describe(read, ns, &node)?;
            if palace::is_structure(&node) {
                return Err(grafeno::Error::Invalid(format!(
                    "{id} is part of a palace: reflect on a room, or on a topic"
                )));
            }
            add(node, &mut memories);
            for (_, other) in read.neighbor_ids(ns, *id, Direction::Both, &EdgeFilter::any())? {
                if let Some(node) = read.node(ns, other)? {
                    add(node, &mut memories);
                }
            }
        }
    }
    if memories.is_empty() {
        reflection.nothing = true;
        reflection.limits.notes.insert(
            0,
            "Nothing stored matches: no memory is offered in place of one that is not there."
                .into(),
        );
        return Ok(reflection);
    }

    // The facts around them, each once.
    let scope_ids: HashSet<NodeId> = memories.iter().map(|node| node.id).collect();
    let mut facts: Vec<Edge> = Vec::new();
    let mut fact_ids: HashSet<EdgeId> = HashSet::new();
    let mut neighbors: HashMap<NodeId, Vec<Edge>> = HashMap::new();
    for memory in &memories {
        for neighbor in read.neighbors(ns, memory.id, Direction::Both, &EdgeFilter::any())? {
            neighbors
                .entry(neighbor.node)
                .or_default()
                .push(neighbor.edge.clone());
            let edge = neighbor.edge;
            if NOT_FACTS.contains(&edge.label.as_str()) || edge.label == "summarizes" {
                continue;
            }
            let other = read.node(ns, neighbor.node)?;
            if other.as_ref().is_some_and(palace::is_structure) {
                continue;
            }
            if fact_ids.insert(edge.id) {
                facts.push(edge);
            }
        }
    }
    reflection.limits.memories_looked_at = memories.len();
    reflection.limits.facts_looked_at = facts.len();

    // Known.
    let known = &mut reflection.known;
    known.memories = memories.len();
    known.facts = facts.len();
    let mut verified = Vec::new();
    for memory in &memories {
        let kind = knowledge::KINDS
            .iter()
            .find(|kind| memory.has_label(kind))
            .map_or("other", |kind| *kind);
        *known.memories_by_kind.entry(kind.into()).or_default() += 1;
        let assessed = knowledge::of_node(memory);
        *known
            .memories_by_origin
            .entry(origin_name(assessed.origin))
            .or_default() += 1;
        if assessed.origin == Some(Origin::Verified) {
            verified.push(Item {
                id: memory.id.to_string(),
                kind: "memory",
                text: describe(read, ns, memory)?,
                why: format!(
                    "verified{}",
                    assessed
                        .verified_at
                        .map(|at| format!(" on {at}"))
                        .unwrap_or_default()
                ),
            });
        }
    }
    for fact in &facts {
        let assessed = knowledge::of_edge(fact);
        *known
            .facts_by_origin
            .entry(origin_name(assessed.origin))
            .or_default() += 1;
        if assessed.origin == Some(Origin::Verified) {
            verified.push(Item {
                id: fact.id.to_string(),
                kind: "fact",
                text: sentence(read, ns, fact)?,
                why: format!(
                    "verified{}",
                    assessed
                        .verified_at
                        .map(|at| format!(" on {at}"))
                        .unwrap_or_default()
                ),
            });
        }
    }

    // Uncertain.
    let mut uncertain = Vec::new();
    let mut unsourced = Vec::new();
    for memory in &memories {
        let assessed = knowledge::of_node(memory);
        if assessed.origin == Some(Origin::Inferred) && memory.sources.is_empty() {
            unsourced.push(memory.id.to_string());
            uncertain.push(Item {
                id: memory.id.to_string(),
                kind: "memory",
                text: describe(read, ns, memory)?,
                why: "inferred, with no source".into(),
            });
        } else if let Some(certainty) = assessed.certainty.filter(|c| *c < options.low_certainty) {
            uncertain.push(Item {
                id: memory.id.to_string(),
                kind: "memory",
                text: describe(read, ns, memory)?,
                why: format!("certainty {certainty}"),
            });
        }
    }
    for fact in &facts {
        let assessed = knowledge::of_edge(fact);
        if assessed.origin == Some(Origin::Inferred) && fact.sources.is_empty() {
            unsourced.push(fact.id.to_string());
            uncertain.push(Item {
                id: fact.id.to_string(),
                kind: "fact",
                text: sentence(read, ns, fact)?,
                why: "inferred, with no source".into(),
            });
        } else if let Some(certainty) = assessed.certainty.filter(|c| *c < options.low_certainty) {
            uncertain.push(Item {
                id: fact.id.to_string(),
                kind: "fact",
                text: sentence(read, ns, fact)?,
                why: format!("certainty {certainty}"),
            });
        }
    }

    // What was supported, and what was claimed with nothing behind it.
    let mut support: HashMap<String, Vec<String>> = HashMap::new();
    for memory in &memories {
        let mut behind: Vec<String> = memory.sources.iter().map(|id| id.to_string()).collect();
        for (_, other) in read.neighbor_ids(
            ns,
            memory.id,
            Direction::Out,
            &EdgeFilter::any().label(SUPPORTED_BY),
        )? {
            behind.push(other.to_string());
        }
        if !behind.is_empty() {
            support.insert(memory.id.to_string(), behind);
        }
    }
    for fact in &facts {
        if !fact.sources.is_empty() {
            support.insert(
                fact.id.to_string(),
                fact.sources.iter().map(|id| id.to_string()).collect(),
            );
        }
    }
    let mut unsupported = Vec::new();
    let mut claimed = Vec::new();
    let said = |origin: Option<Origin>| matches!(origin, Some(Origin::Stated | Origin::Observed));
    for memory in &memories {
        let assessed = knowledge::of_node(memory);
        if said(assessed.origin) && !support.contains_key(&memory.id.to_string()) {
            claimed.push(memory.id.to_string());
            unsupported.push(Item {
                id: memory.id.to_string(),
                kind: "memory",
                text: describe(read, ns, memory)?,
                why: format!("{}, with no source", origin_name(assessed.origin)),
            });
        }
    }
    for fact in &facts {
        let assessed = knowledge::of_edge(fact);
        if said(assessed.origin) && fact.sources.is_empty() {
            claimed.push(fact.id.to_string());
            unsupported.push(Item {
                id: fact.id.to_string(),
                kind: "fact",
                text: sentence(read, ns, fact)?,
                why: format!("{}, with no source", origin_name(assessed.origin)),
            });
        }
    }

    // Conflicts recorded under constraints, on these facts or their
    // nodes.
    let mut conflicts = Vec::new();
    let mut unsettled = Vec::new();
    for event in read.constraint_events(ns, usize::MAX)? {
        let mut involved = [
            Some(event.new_edge),
            Some(event.old_edge),
            event.replacement,
        ]
        .into_iter()
        .flatten()
        .any(|edge| fact_ids.contains(&edge));
        let (Some(new_edge), Some(old_edge)) = (
            read.edge(ns, event.new_edge)?,
            read.edge(ns, event.old_edge)?,
        ) else {
            continue;
        };
        involved |= [new_edge.from, new_edge.to, old_edge.from, old_edge.to]
            .iter()
            .any(|node| scope_ids.contains(node));
        if !involved {
            continue;
        }
        let action = serde_json::to_value(event.action)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let settled = action != "flagged";
        if !settled {
            unsettled.push(new_edge.id.to_string());
        }
        conflicts.push(Clash {
            kind: if settled { "change" } else { "contradiction" },
            constraint: event.constraint,
            action,
            settled,
            new_fact: Item {
                id: new_edge.id.to_string(),
                kind: "fact",
                text: sentence(read, ns, &new_edge)?,
                why: format!("recorded {}", event.recorded_at.date()),
            },
            old_fact: Item {
                id: old_edge.id.to_string(),
                kind: "fact",
                text: sentence(read, ns, &old_edge)?,
                why: "the fact it clashed with".into(),
            },
        });
    }
    // Contradictions an agent recorded between two memories or facts.
    for fact in &facts {
        if fact.label != knowledge::CONTRADICTS || fact.valid_to.is_some_and(|to| to <= options.now)
        {
            continue;
        }
        let item = |id: NodeId| -> Result<Item> {
            Ok(match read.node(ns, id)? {
                Some(node) => Item {
                    id: id.to_string(),
                    kind: "memory",
                    text: describe(read, ns, &node)?,
                    why: String::new(),
                },
                None => Item {
                    id: id.to_string(),
                    kind: "memory",
                    text: id.to_string(),
                    why: String::new(),
                },
            })
        };
        let mut new_fact = item(fact.from)?;
        new_fact.why = format!(
            "recorded {} as contradicting the other",
            fact.recorded_at.date()
        );
        let mut old_fact = item(fact.to)?;
        old_fact.why = "what it contradicts".into();
        unsettled.push(fact.id.to_string());
        conflicts.push(Clash {
            kind: "contradiction",
            constraint: knowledge::CONTRADICTS.into(),
            action: "recorded".into(),
            settled: false,
            new_fact,
            old_fact,
        });
    }
    // Unsettled first.
    conflicts.sort_by_key(|clash| clash.settled);

    // Possibly out of date.
    let mut stale = Vec::new();
    let mut obsolete = Vec::new();
    for fact in &facts {
        if let Some(until) = fact.valid_to.filter(|until| *until <= options.now) {
            stale.push(Item {
                id: fact.id.to_string(),
                kind: "fact",
                text: sentence(read, ns, fact)?,
                why: format!("no longer true since {}", until.date()),
            });
        }
    }
    for memory in &memories {
        let feedback = memory.prop("feedback").and_then(Value::as_str);
        let why = if memory.has_label(ARCHIVED) {
            Some(match feedback {
                Some("obsolete") => "marked obsolete, archived".to_owned(),
                Some("wrong") => "marked wrong, archived".to_owned(),
                _ => "archived (summarized, or left out of recall)".to_owned(),
            })
        } else {
            let state = read.memory(ns, memory)?;
            let strength = state.strength(options.now, options.half_life_secs);
            (strength < options.weak).then(|| {
                format!(
                    "weak: not used since {} (strength {:.2})",
                    state.last_access.date(),
                    strength
                )
            })
        };
        if let Some(why) = why {
            stale.push(Item {
                id: memory.id.to_string(),
                kind: "memory",
                text: describe(read, ns, memory)?,
                why,
            });
        }
    }
    for pair in crate::superseded(
        read,
        ns,
        &crate::SupersededOptions {
            limit: usize::MAX,
            among: memories.iter().map(|memory| memory.id).collect(),
            ..Default::default()
        },
    )? {
        if scope_ids.contains(&pair.older.id) {
            obsolete.push(pair.older.id.to_string());
            stale.push(Item {
                id: pair.older.id.to_string(),
                kind: "memory",
                text: describe(read, ns, &pair.older)?,
                why: format!(
                    "a newer memory about the same may say it as it is now: {}",
                    pair.newer.id
                ),
            });
        }
    }

    // Gaps.
    let mut gaps = std::mem::take(&mut reflection.gaps);
    let mut open = Vec::new();
    let mut questions: Vec<Node> = memories
        .iter()
        .filter(|m| m.has_label(QUESTION))
        .cloned()
        .collect();
    for other in neighbors.keys() {
        if !scope_ids.contains(other)
            && let Some(node) = read.node(ns, *other)?
            && node.has_label(QUESTION)
        {
            questions.push(node);
        }
    }
    for question in questions {
        let answered = !read
            .neighbor_ids(
                ns,
                question.id,
                Direction::In,
                &EdgeFilter::any().label(ANSWERS),
            )?
            .is_empty();
        if !answered {
            open.push(question.id.to_string());
            gaps.push(Item {
                id: question.id.to_string(),
                kind: "question",
                text: describe(read, ns, &question)?,
                why: "an open question: nothing answers it".into(),
            });
        }
    }
    let mut undescribed = Vec::new();
    for (other, edges) in &neighbors {
        if scope_ids.contains(other) || !edges.iter().all(|e| e.label == crate::reconcile::MENTIONS)
        {
            continue;
        }
        let Some(node) = read.node(ns, *other)? else {
            continue;
        };
        if palace::is_structure(&node) || node.has_label(QUESTION) {
            continue;
        }
        let described = read
            .text(ns, node.id)?
            .is_some_and(|text| !text.trim().is_empty())
            || node
                .props
                .keys()
                .any(|key| !matches!(key.as_str(), "name" | "title" | "label"));
        let related = read
            .neighbors(ns, node.id, Direction::Both, &EdgeFilter::any())?
            .iter()
            .any(|n| n.edge.label != crate::reconcile::MENTIONS);
        if !described && !related {
            undescribed.push(node.id.to_string());
            gaps.push(Item {
                id: node.id.to_string(),
                kind: "entity",
                text: describe(read, ns, &node)?,
                why: "mentioned, and never described or related to anything".into(),
            });
        }
    }
    // What the kinds' expected shapes ask for and a memory lacks.
    let mut expected = shapes(read, ns)?;
    expected.extend(options.shapes.clone());
    let mut incomplete = Vec::new();
    if !expected.is_empty() {
        for memory in &memories {
            for (kind, shape) in &expected {
                if !memory.has_label(kind) {
                    continue;
                }
                let mut lacking: Vec<String> = Vec::new();
                for label in &shape.relations {
                    if read
                        .neighbor_ids(
                            ns,
                            memory.id,
                            Direction::Both,
                            &EdgeFilter::any().label(label),
                        )?
                        .is_empty()
                    {
                        lacking.push(format!("a `{label}` relation"));
                    }
                }
                for key in &shape.properties {
                    if memory.prop(key).is_none() {
                        lacking.push(format!("its `{key}`"));
                    }
                }
                if !lacking.is_empty() {
                    incomplete.push(memory.id.to_string());
                    gaps.push(Item {
                        id: memory.id.to_string(),
                        kind: "memory",
                        text: describe(read, ns, memory)?,
                        why: format!(
                            "a {kind} is expected to have {}; this one lacks {}",
                            shape
                                .relations
                                .iter()
                                .map(|label| format!("`{label}`"))
                                .chain(shape.properties.iter().map(|key| format!("`{key}`")))
                                .collect::<Vec<_>>()
                                .join(", "),
                            lacking.join(" and ")
                        ),
                    });
                }
            }
        }
    }
    let about = match scope {
        Scope::Room(room) => Some(*room),
        Scope::Node(node) => Some(*node),
        Scope::Topic(_) => None,
    };
    if let Some(about) = about
        && let Some(checkpoint) = crate::resume(
            read,
            ns,
            &crate::ResumeQuery {
                about: Some(about),
                ..Default::default()
            },
        )?
        && !checkpoint.next.is_empty()
    {
        gaps.push(Item {
            id: checkpoint.id.to_string(),
            kind: "memory",
            text: checkpoint.next.join("; "),
            why: format!(
                "left to do in the checkpoint of {:?} of {}",
                checkpoint.task,
                checkpoint.at.date()
            ),
        });
    }

    // What to check next.
    let mut next = Vec::new();
    if !unsettled.is_empty() {
        next.push(Step {
            what: "settle the facts flagged as clashing".into(),
            how: "keep one: forget the other with mode \"invalidate\" (it ended) or \"retract\" (it was a mistake)",
            about: unsettled,
        });
    }
    if !unsourced.is_empty() {
        next.push(Step {
            what: "check what was inferred against a source".into(),
            how: "feedback with verdict \"verified\" and the source, or \"wrong\"",
            about: unsourced,
        });
    }
    if !claimed.is_empty() {
        next.push(Step {
            what: "find the evidence of what was stated with no source".into(),
            how: "relate it `supported_by` the evidence, or feedback \"verified\" with the source",
            about: claimed,
        });
    }
    if !incomplete.is_empty() {
        next.push(Step {
            what: "complete what the expected shapes ask for".into(),
            how: "relate or update each memory with what it lacks, if it is known; if not, keep a Question",
            about: incomplete,
        });
    }
    if !open.is_empty() {
        next.push(Step {
            what: "answer the open questions".into(),
            how: "remember the answer and relate it to the question with \"answers\"",
            about: open,
        });
    }
    if !undescribed.is_empty() {
        next.push(Step {
            what: "describe what is mentioned and never described".into(),
            how: "remember what it is, with its name",
            about: undescribed,
        });
    }
    if !obsolete.is_empty() {
        next.push(Step {
            what: "decide whether newer memories overtake older ones".into(),
            how: "feedback \"obsolete\" on the older one if it is, or leave both",
            about: obsolete,
        });
    }

    // Each record once, by the evidence behind it.
    let in_clash: HashSet<String> = unsettled_ids(&conflicts);
    let ended: HashSet<String> = stale.iter().map(|item| item.id.clone()).collect();
    let mut by_evidence: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut count = |id: String, origin: Option<Origin>, archived: bool| {
        let category = if in_clash.contains(&id) {
            "conflicting"
        } else if archived || ended.contains(&id) {
            "obsolete"
        } else if origin == Some(Origin::Verified) {
            "verified"
        } else if support.contains_key(&id) {
            "supported"
        } else {
            match origin {
                Some(Origin::Inferred) => "inferred, no source",
                Some(_) => "stated, no source",
                None => "not stated, no source",
            }
        };
        *by_evidence.entry(category).or_default() += 1;
    };
    for memory in &memories {
        count(
            memory.id.to_string(),
            knowledge::of_node(memory).origin,
            memory.has_label(ARCHIVED),
        );
    }
    for fact in &facts {
        count(fact.id.to_string(), knowledge::of_edge(fact).origin, false);
    }
    reflection.known.by_evidence = by_evidence;

    reflection.limits.notes.push(
        "Evidence categories count records by what is stored behind them; they are not probabilities, and a supported claim is not thereby true."
            .into(),
    );
    reflection.limits.notes.push(
        "Origin and certainty are what agents stated; a record with neither is counted as not stated."
            .into(),
    );
    reflection.limits.notes.push(
        "Conflicts are those recorded under constraints or with a `contradicts` relation; facts that clash with neither are not found. Facts from different sources or contexts are not taken for contradictions unless recorded as such."
            .into(),
    );
    reflection.limits.notes.push(
        "A change is a newer fact taking an older one's place under a constraint: the state changed, and the older one is its history, not an error."
            .into(),
    );
    let mut totals = BTreeMap::new();
    let mut cut = |name: &'static str, items: &mut Vec<Item>| {
        totals.insert(name, items.len());
        items.truncate(options.limit);
    };
    cut("verified", &mut verified);
    cut("uncertain", &mut uncertain);
    cut("unsupported", &mut unsupported);
    cut("stale", &mut stale);
    cut("gaps", &mut gaps);
    totals.insert("conflicts", conflicts.len());
    conflicts.truncate(options.limit);
    // The sources behind each item listed.
    let mut listed: Vec<&Item> = Vec::new();
    listed.extend(verified.iter());
    listed.extend(uncertain.iter());
    listed.extend(stale.iter());
    listed.extend(gaps.iter());
    for clash in &conflicts {
        listed.push(&clash.new_fact);
        listed.push(&clash.old_fact);
    }
    for item in listed {
        if let Some(behind) = support.get(&item.id) {
            reflection.sources.insert(item.id.clone(), behind.clone());
        }
    }
    reflection.known.verified = verified;
    reflection.uncertain = uncertain;
    reflection.unsupported = unsupported;
    reflection.conflicts = conflicts;
    reflection.stale = stale;
    reflection.gaps = gaps;
    reflection.next = next;
    reflection.totals = totals;
    Ok(reflection)
}

/// The ids of the records in clashes not settled.
fn unsettled_ids(conflicts: &[Clash]) -> HashSet<String> {
    conflicts
        .iter()
        .filter(|clash| !clash.settled)
        .flat_map(|clash| [clash.new_fact.id.clone(), clash.old_fact.id.clone()])
        .collect()
}
