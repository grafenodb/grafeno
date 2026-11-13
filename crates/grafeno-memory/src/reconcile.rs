//! Extraction runs and reconciliation: what a text mentions becomes
//! `mentions` edges from the text's node to entities, found, created, or
//! queued for someone to decide.
//!
//! - A known name with one node links to it. With several (namesakes), the
//!   other entities the text links settle it when exactly one candidate is
//!   linked to them; otherwise the name is queued (`ambiguous`).
//! - An email links to the node whose `email` it is (through an exact index
//!   on `email`), else to an `Email` node; URLs, handles, hashtags and
//!   identifiers link to a node per value (`Url`, `Handle`, `Hashtag`, the
//!   identifier's label). Dates and amounts are kept on the text's node
//!   (`mentioned_dates`, `mentioned_amounts`).
//! - A capitalized phrase nobody knows is queued (`phrase`); once it shows
//!   up in `min_phrase_sources` different texts it becomes an entity.
//! - Decisions made on the queue are remembered: a name resolved to a node
//!   links there next time, a dismissed one is not proposed again.
//!
//! Runs are incremental: each text's hash is kept, and only new or changed
//! texts are read. Extracted edges carry `extracted: true` and are replaced
//! when their text changes; an agent's edges ([`ingest`]) are not.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
use std::sync::Arc;

use grafeno::{
    Decision, Direction, EdgeFilter, EdgeId, Error, Grafeno, IndexRange, NewEdge, NewNode, NodeId,
    Probe, Props, ReadTx, Result, Timestamp, Value, WriteTx, normalize_name,
};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::extract::{Found, Gazetteer, IdentifierPattern, Kind, patterns, phrases};

/// The label of the edges from a text to what it mentions.
pub const MENTIONS: &str = "mentions";

/// The labels whose texts are read by default.
pub const SOURCE_LABELS: [&str; 7] = [
    "Memory", "Note", "Episode", "Document", "Chunk", "Message", "Summary",
];

const HASHES: &str = "extract.hash";
const QUEUE: &str = "extract.review";
const DECISIONS: &str = "extract.decision";
const IGNORED: &str = "extract.ignored";

/// What an extraction run reads and how it decides.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractOptions {
    /// Texts of nodes with these labels (memories, notes, documents by
    /// default); every node with a text when empty.
    pub source_labels: Vec<String>,
    /// The gazetteer: names of nodes with these labels; when empty, every
    /// named node that is not a source.
    pub entity_labels: Vec<String>,
    pub patterns: bool,
    pub phrases: bool,
    pub identifiers: Vec<IdentifierPattern>,
    /// Read `1/2/2026` as January 2 (default: 1 February).
    pub month_first: bool,
    /// Different texts a new phrase must appear in to become an entity.
    pub min_phrase_sources: usize,
    /// The label of entities made from phrases.
    pub new_entity_label: String,
    /// Known names shorter than this are not looked for.
    pub min_name_len: usize,
    /// Texts per transaction.
    pub batch: usize,
    /// Read every text again, changed or not.
    pub force: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            source_labels: SOURCE_LABELS
                .iter()
                .map(|label| label.to_string())
                .collect(),
            entity_labels: Vec::new(),
            patterns: true,
            phrases: true,
            identifiers: Vec::new(),
            month_first: false,
            min_phrase_sources: 2,
            new_entity_label: "Entity".into(),
            min_name_len: 3,
            batch: 100,
            force: false,
        }
    }
}

/// What a run (or an ingestion) did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ExtractReport {
    /// Texts read, and texts skipped because they did not change.
    pub read: u64,
    pub unchanged: u64,
    /// `mentions` edges written, of which to entities that existed, and
    /// entities created.
    pub mentions: u64,
    pub linked: u64,
    pub created: u64,
    /// Review items added or grown.
    pub queued: u64,
    /// Finds by kind (`name`, `email`, `phrase`, …).
    pub found: BTreeMap<String, u64>,
}

impl ExtractReport {
    fn add(&mut self, other: &ExtractReport) {
        self.read += other.read;
        self.unchanged += other.unchanged;
        self.mentions += other.mentions;
        self.linked += other.linked;
        self.created += other.created;
        self.queued += other.queued;
        for (kind, count) in &other.found {
            *self.found.entry(kind.clone()).or_default() += count;
        }
    }
}

/// One place a review item was found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub source: NodeId,
    pub start: usize,
    pub end: usize,
    pub surface: String,
}

/// Something extraction could not decide on its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewItem {
    /// `phrase:<value>` or `ambiguous:<value>`.
    pub key: String,
    /// `phrase`: maybe a new entity; `ambiguous`: a name several nodes have.
    pub kind: String,
    pub value: String,
    pub occurrences: Vec<Occurrence>,
    /// For `ambiguous`: the nodes it could be.
    #[serde(default)]
    pub candidates: Vec<NodeId>,
    /// For an agent's mentions: the label and properties it gave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Props::is_empty")]
    pub props: Props,
}

/// How to settle a review item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum Resolution {
    /// It is this node: link every occurrence, and future ones.
    Link { node: NodeId },
    /// It is a new entity: create it (with `label`, or the item's or the
    /// default) and link every occurrence.
    Create {
        #[serde(default)]
        label: Option<String>,
    },
    /// Not an entity: forget it, and do not propose it again.
    Dismiss,
}

/// What a run needs that does not change while it runs: the gazetteer and
/// the compiled identifier patterns.
pub struct Plan {
    gazetteer: Arc<Gazetteer>,
    identifiers: Arc<Vec<(IdentifierPattern, Regex)>>,
    /// The texts to read (new or changed).
    pub sources: Vec<NodeId>,
    pub unchanged: u64,
}

fn text_hash(text: &str) -> [u8; 8] {
    BuildHasherDefault::<DefaultHasher>::default()
        .hash_one(text)
        .to_le_bytes()
}

fn is_source(node: &grafeno::Node, options: &ExtractOptions) -> bool {
    options.source_labels.is_empty()
        || options
            .source_labels
            .iter()
            .any(|label| node.has_label(label))
}

/// Builds the gazetteer and lists the texts to read.
pub fn plan(read: &ReadTx, ns: &str, options: &ExtractOptions) -> Result<Plan> {
    let mut identifiers = Vec::new();
    for pattern in &options.identifiers {
        let regex = Regex::new(&pattern.pattern).map_err(|error| {
            Error::Invalid(format!("identifier pattern for {}: {error}", pattern.label))
        })?;
        identifiers.push((pattern.clone(), regex));
    }
    let mut names = Vec::new();
    let mut sources = Vec::new();
    let mut unchanged = 0;
    let mut after = None;
    loop {
        let page = read.nodes_page(ns, None, after, 2_000)?;
        let Some(last) = page.last() else {
            break;
        };
        after = Some(last.id);
        for node in page {
            // A room named as an entity is not that entity, and its text is
            // not a memory to read.
            if crate::palace::is_structure(&node) {
                continue;
            }
            let entity = if options.entity_labels.is_empty() {
                !is_source(&node, options)
            } else {
                options
                    .entity_labels
                    .iter()
                    .any(|label| node.has_label(label))
            };
            if entity {
                for name in grafeno::names_of(&node) {
                    names.push((name, node.id));
                }
            }
            if !is_source(&node, options) {
                continue;
            }
            let Some(text) = read.text(ns, node.id)? else {
                continue;
            };
            let key = node.id.to_string();
            let seen = read.state(ns, HASHES, &key)?;
            if !options.force && seen.as_deref() == Some(&text_hash(&text)[..]) {
                unchanged += 1;
            } else {
                sources.push(node.id);
            }
        }
    }
    Ok(Plan {
        gazetteer: Arc::new(Gazetteer::new(names, options.min_name_len)),
        identifiers: Arc::new(identifiers),
        sources,
        unchanged,
    })
}

/// Reads every new or changed text of `ns` and reconciles what it
/// mentions, a batch of texts per transaction.
pub fn extract(db: &Grafeno, ns: &str, options: &ExtractOptions) -> Result<ExtractReport> {
    let plan = plan(&db.begin_read()?, ns, options)?;
    let mut report = ExtractReport {
        unchanged: plan.unchanged,
        ..ExtractReport::default()
    };
    for batch in plan.sources.chunks(options.batch.max(1)) {
        let read = db.begin_read()?;
        let done = db.write(|tx| extract_batch(tx, &read, ns, &plan, batch, options))?;
        report.add(&done);
    }
    Ok(report)
}

fn mention(found: &Found, target: NodeId, source: NodeId, extracted: bool, kind: &str) -> NewEdge {
    NewEdge::new(source, MENTIONS, target)
        .prop("surface", found.surface.as_str())
        .prop("start", found.start as u64)
        .prop("end", found.end as u64)
        .prop("kind", kind)
        .prop("extracted", extracted)
}

fn load(tx: &WriteTx, ns: &str, key: &str) -> Result<Option<ReviewItem>> {
    tx.state(ns, QUEUE, key)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|e| Error::Serialization(e.to_string()))
        })
        .transpose()
}

fn save(tx: &mut WriteTx, ns: &str, item: &ReviewItem) -> Result<()> {
    let bytes = serde_json::to_vec(item).map_err(|e| Error::Serialization(e.to_string()))?;
    tx.put_state(ns, QUEUE, &item.key, &bytes)
}

/// Adds an occurrence to a review item (made if missing).
#[allow(clippy::too_many_arguments)]
fn queue(
    tx: &mut WriteTx,
    ns: &str,
    kind: &str,
    value: &str,
    occurrence: Occurrence,
    candidates: &[NodeId],
    label: Option<String>,
    props: Props,
) -> Result<ReviewItem> {
    let key = format!("{kind}:{value}");
    let mut item = load(tx, ns, &key)?.unwrap_or(ReviewItem {
        key,
        kind: kind.into(),
        value: value.into(),
        occurrences: Vec::new(),
        candidates: Vec::new(),
        label,
        props,
    });
    if !item
        .occurrences
        .iter()
        .any(|o| o.source == occurrence.source && o.start == occurrence.start)
    {
        item.occurrences.push(occurrence);
    }
    for candidate in candidates {
        if !item.candidates.contains(candidate) {
            item.candidates.push(*candidate);
        }
    }
    save(tx, ns, &item)?;
    Ok(item)
}

/// Removes what earlier runs wrote for these texts: their extracted edges
/// and their occurrences in the review queue.
fn forget(tx: &mut WriteTx, ns: &str, sources: &BTreeSet<NodeId>) -> Result<()> {
    let filter = EdgeFilter::any().label(MENTIONS);
    for source in sources {
        for (edge, _) in tx.neighbor_ids(ns, *source, Direction::Out, &filter)? {
            if tx
                .edge(ns, edge)?
                .is_some_and(|edge| edge.props.get("extracted") == Some(&Value::Bool(true)))
            {
                tx.delete_edge(ns, edge)?;
            }
        }
    }
    for (key, bytes) in tx.state_entries(ns, QUEUE, "", usize::MAX)? {
        let mut item: ReviewItem =
            serde_json::from_slice(&bytes).map_err(|e| Error::Serialization(e.to_string()))?;
        let before = item.occurrences.len();
        item.occurrences.retain(|o| !sources.contains(&o.source));
        if item.occurrences.len() != before {
            if item.occurrences.is_empty() {
                tx.delete_state(ns, QUEUE, &key)?;
            } else {
                save(tx, ns, &item)?;
            }
        }
    }
    Ok(())
}

fn decision(tx: &WriteTx, ns: &str, value: &str) -> Result<Option<NodeId>> {
    Ok(tx
        .state(ns, DECISIONS, value)?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|id| id.parse::<uuid::Uuid>().ok())
        .map(NodeId::from))
}

/// A node for a value (an email, a hashtag): reused by name, else made.
fn value_node(
    tx: &mut WriteTx,
    ns: &str,
    label: &str,
    value: &str,
    made: &mut HashMap<(String, String), NodeId>,
    report: &mut ExtractReport,
) -> Result<NodeId> {
    if let Some(id) = made.get(&(label.to_owned(), value.to_owned())) {
        return Ok(*id);
    }
    for id in tx.find_by_name(ns, value)? {
        if tx.node(ns, id)?.is_some_and(|node| node.has_label(label)) {
            report.linked += 1;
            return Ok(id);
        }
    }
    let id = tx.create_node(ns, NewNode::new(label).prop("name", value))?;
    report.created += 1;
    made.insert((label.to_owned(), value.to_owned()), id);
    Ok(id)
}

/// The node whose `email` property is this, through exact indexes on
/// `email`.
fn by_email(tx: &WriteTx, ns: &str, email: &str, surface: &str) -> Result<Option<NodeId>> {
    for def in tx.index_defs(ns)? {
        if def.key == "email" && !def.fuzzy && def.is_simple() {
            for value in [email, surface] {
                let found =
                    tx.index_lookup(ns, &def.name, &IndexRange::Equal(Value::from(value)), 2)?;
                if found.len() == 1 {
                    return Ok(Some(found[0]));
                }
            }
        }
    }
    Ok(None)
}

fn neighbors(tx: &WriteTx, ns: &str, node: NodeId) -> Result<BTreeSet<NodeId>> {
    Ok(tx
        .neighbor_ids(ns, node, Direction::Both, &EdgeFilter::any())?
        .into_iter()
        .map(|(_, other)| other)
        .collect())
}

/// Extracts and reconciles a batch of texts in one transaction. `read` is
/// a snapshot from before it (for match rules).
pub fn extract_batch(
    tx: &mut WriteTx,
    _read: &ReadTx,
    ns: &str,
    plan: &Plan,
    sources: &[NodeId],
    options: &ExtractOptions,
) -> Result<ExtractReport> {
    let mut report = ExtractReport::default();
    let batch: BTreeSet<NodeId> = sources.iter().copied().collect();
    forget(tx, ns, &batch)?;
    let mut made: HashMap<(String, String), NodeId> = HashMap::new();
    for &source in sources {
        let Some(text) = tx.text(ns, source)? else {
            tx.delete_state(ns, HASHES, &source.to_string())?;
            continue;
        };
        report.read += 1;
        let mut finds: Vec<(Found, Vec<NodeId>)> = plan.gazetteer.find(&text);
        if options.patterns {
            finds.extend(
                patterns(&text, &plan.identifiers, options.month_first)
                    .into_iter()
                    .map(|f| (f, Vec::new())),
            );
        }
        let covered = |start: usize, end: usize, finds: &[(Found, Vec<NodeId>)]| {
            finds.iter().any(|(f, _)| f.start < end && start < f.end)
        };
        if options.phrases {
            for phrase in phrases(&text) {
                if !covered(phrase.start, phrase.end, &finds) {
                    finds.push((phrase, Vec::new()));
                }
            }
        }
        // Names with one node first: they are the context for the others.
        finds.sort_by_key(|(f, nodes)| (f.kind != Kind::Name || nodes.len() != 1, f.start));
        let mut linked_here: BTreeSet<NodeId> = BTreeSet::new();
        let (mut dates, mut amounts) = (BTreeSet::new(), BTreeSet::new());
        for (found, nodes) in finds {
            *report.found.entry(found.kind.name()).or_default() += 1;
            let occurrence = Occurrence {
                source,
                start: found.start,
                end: found.end,
                surface: found.surface.clone(),
            };
            let link = |tx: &mut WriteTx,
                        target: NodeId,
                        kind: &str,
                        report: &mut ExtractReport,
                        linked_here: &mut BTreeSet<NodeId>|
             -> Result<()> {
                if target != source {
                    tx.create_edge(ns, mention(&found, target, source, true, kind))?;
                    report.mentions += 1;
                    linked_here.insert(target);
                }
                Ok(())
            };
            match &found.kind {
                Kind::Date => {
                    dates.insert(found.value.clone());
                }
                Kind::Amount => {
                    amounts.insert(found.value.clone());
                }
                Kind::Email => {
                    let target = match by_email(tx, ns, &found.value, &found.surface)? {
                        Some(id) => {
                            report.linked += 1;
                            id
                        }
                        None => value_node(tx, ns, "Email", &found.value, &mut made, &mut report)?,
                    };
                    link(tx, target, "email", &mut report, &mut linked_here)?;
                }
                Kind::Url | Kind::Handle | Kind::Hashtag | Kind::Identifier(_) => {
                    let label = match &found.kind {
                        Kind::Url => "Url".to_owned(),
                        Kind::Handle => "Handle".to_owned(),
                        Kind::Hashtag => "Hashtag".to_owned(),
                        Kind::Identifier(label) => label.clone(),
                        _ => unreachable!(),
                    };
                    let target = value_node(tx, ns, &label, &found.value, &mut made, &mut report)?;
                    link(
                        tx,
                        target,
                        &found.kind.name(),
                        &mut report,
                        &mut linked_here,
                    )?;
                }
                Kind::Name | Kind::Phrase => {
                    if tx.state(ns, IGNORED, &found.value)?.is_some() {
                        continue;
                    }
                    // A phrase may have become known since the run began.
                    let mut candidates: Vec<NodeId> = if found.kind == Kind::Name {
                        nodes
                    } else {
                        tx.find_by_name(ns, &found.value)?
                    };
                    candidates.retain(|id| *id != source);
                    if found.kind != Kind::Name {
                        let mut entities = Vec::with_capacity(candidates.len());
                        for id in candidates {
                            if tx
                                .node(ns, id)?
                                .is_some_and(|node| !crate::palace::is_structure(&node))
                            {
                                entities.push(id);
                            }
                        }
                        candidates = entities;
                    }
                    if let Some(decided) = decision(tx, ns, &found.value)? {
                        candidates = vec![decided];
                    }
                    match candidates.len() {
                        1 => {
                            report.linked += 1;
                            link(tx, candidates[0], "name", &mut report, &mut linked_here)?;
                        }
                        0 => {
                            let item = queue(
                                tx,
                                ns,
                                "phrase",
                                &found.value,
                                occurrence,
                                &[],
                                None,
                                Props::new(),
                            )?;
                            let texts: BTreeSet<NodeId> =
                                item.occurrences.iter().map(|o| o.source).collect();
                            if texts.len() >= options.min_phrase_sources.max(1) {
                                let label = options.new_entity_label.clone();
                                settle(
                                    tx,
                                    ns,
                                    &item,
                                    Resolution::Create { label: Some(label) },
                                    &mut report,
                                )?;
                            } else {
                                report.queued += 1;
                            }
                        }
                        _ => {
                            // The candidate linked to what this text already links.
                            let mut scored = Vec::new();
                            for candidate in &candidates {
                                let shared = neighbors(tx, ns, *candidate)?
                                    .intersection(&linked_here)
                                    .count();
                                scored.push((shared, *candidate));
                            }
                            scored.sort_by_key(|s| std::cmp::Reverse(s.0));
                            if scored[0].0 > 0 && scored[0].0 > scored[1].0 {
                                report.linked += 1;
                                link(tx, scored[0].1, "name", &mut report, &mut linked_here)?;
                            } else {
                                queue(
                                    tx,
                                    ns,
                                    "ambiguous",
                                    &found.value,
                                    occurrence,
                                    &candidates,
                                    None,
                                    Props::new(),
                                )?;
                                report.queued += 1;
                            }
                        }
                    }
                }
            }
        }
        // Dates and amounts on the text's node, when they change.
        let node = tx.node(ns, source)?.ok_or(Error::NodeNotFound(source))?;
        let mut props = Props::new();
        let mut gone: Vec<&str> = Vec::new();
        for (key, values) in [("mentioned_dates", &dates), ("mentioned_amounts", &amounts)] {
            let new = Value::from(values.iter().cloned().collect::<Vec<_>>());
            let old = node.props.get(key).cloned().unwrap_or(Value::Null);
            if values.is_empty() && node.props.contains_key(key) {
                gone.push(key);
            } else if !values.is_empty() && old != new {
                props.insert(key.into(), new);
            }
        }
        if !props.is_empty() {
            tx.set_node_props(ns, source, props)?;
        }
        if !gone.is_empty() {
            tx.remove_node_props(ns, source, &gone)?;
        }
        tx.put_state(ns, HASHES, &source.to_string(), &text_hash(&text))?;
    }
    Ok(report)
}

/// Settles a review item: links its occurrences, makes its entity, or
/// dismisses it, and remembers the decision for later runs.
fn settle(
    tx: &mut WriteTx,
    ns: &str,
    item: &ReviewItem,
    resolution: Resolution,
    report: &mut ExtractReport,
) -> Result<Option<NodeId>> {
    let target = match resolution {
        Resolution::Dismiss => {
            tx.put_state(ns, IGNORED, &item.value, b"1")?;
            tx.delete_state(ns, QUEUE, &item.key)?;
            return Ok(None);
        }
        Resolution::Link { node } => {
            tx.node(ns, node)?.ok_or(Error::NodeNotFound(node))?;
            report.linked += 1;
            node
        }
        Resolution::Create { label } => {
            // The surface written most often names it.
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for o in &item.occurrences {
                *counts.entry(o.surface.as_str()).or_default() += 1;
            }
            let name = counts
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
                .map_or(item.value.clone(), |(surface, _)| surface.to_owned());
            let label = label
                .or_else(|| item.label.clone())
                .unwrap_or_else(|| "Entity".into());
            let mut new = NewNode::new(label.as_str()).prop("name", name);
            for (key, value) in &item.props {
                new = new.prop(key.as_str(), value.clone());
            }
            for o in &item.occurrences {
                new = new.source(o.source);
            }
            report.created += 1;
            tx.create_node(ns, new)?
        }
    };
    for o in &item.occurrences {
        if o.source == target {
            continue;
        }
        let found = Found {
            kind: Kind::Name,
            surface: o.surface.clone(),
            start: o.start,
            end: o.end,
            value: item.value.clone(),
        };
        // Agent mentions stay when the text is extracted again.
        let extracted = item.label.is_none();
        tx.create_edge(ns, mention(&found, target, o.source, extracted, &item.kind))?;
        report.mentions += 1;
    }
    tx.put_state(ns, DECISIONS, &item.value, target.to_string().as_bytes())?;
    tx.delete_state(ns, QUEUE, &item.key)?;
    Ok(Some(target))
}

/// The review queue, in key order.
pub fn review_queue(read: &ReadTx, ns: &str, limit: usize) -> Result<Vec<ReviewItem>> {
    read.state_entries(ns, QUEUE, "", limit)?
        .into_iter()
        .map(|(_, bytes)| {
            serde_json::from_slice(&bytes).map_err(|e| Error::Serialization(e.to_string()))
        })
        .collect()
}

/// Settles the review item `key`; the node it now points to, if any.
pub fn resolve(
    tx: &mut WriteTx,
    ns: &str,
    key: &str,
    resolution: Resolution,
) -> Result<(Option<NodeId>, ExtractReport)> {
    let item =
        load(tx, ns, key)?.ok_or_else(|| Error::Invalid(format!("no review item {key:?}")))?;
    let mut report = ExtractReport::default();
    let target = settle(tx, ns, &item, resolution, &mut report)?;
    Ok((target, report))
}

/// An entity an agent found in a text (with its own model, or by hand).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentMention {
    /// As written in the text.
    pub surface: String,
    /// What it is (`Person`, `Company`); the default label when missing.
    #[serde(default)]
    pub label: Option<String>,
    /// What the agent knows of it (`email`, `born_on`, …), for match rules
    /// and for a new node.
    #[serde(default)]
    pub props: Props,
    /// Where it is in the text, if known (character offsets).
    #[serde(default)]
    pub start: Option<usize>,
    #[serde(default)]
    pub end: Option<usize>,
}

/// Reconciles an agent's mentions of a text like extraction's own: by the
/// label's match rule when there is one (same → link, review → queued,
/// different → a new node), else by name. Its edges have `kind: agent`
/// and stay when the text is extracted again.
pub fn ingest(
    tx: &mut WriteTx,
    read: &ReadTx,
    ns: &str,
    source: NodeId,
    mentions: &[AgentMention],
    default_label: &str,
) -> Result<ExtractReport> {
    Ok(ingest_targets(tx, read, ns, source, mentions, default_label)?.0)
}

/// [`ingest`], with the node each mention was linked to, in order: `None`
/// for one that waits in the review queue.
fn ingest_targets(
    tx: &mut WriteTx,
    read: &ReadTx,
    ns: &str,
    source: NodeId,
    mentions: &[AgentMention],
    default_label: &str,
) -> Result<(ExtractReport, Vec<Option<NodeId>>)> {
    tx.node(ns, source)?.ok_or(Error::NodeNotFound(source))?;
    let mut report = ExtractReport::default();
    let mut targets = Vec::with_capacity(mentions.len());
    for m in mentions {
        *report.found.entry("agent".into()).or_default() += 1;
        let value = normalize_name(&m.surface);
        let label = m.label.clone().unwrap_or_else(|| default_label.to_owned());
        let found = Found {
            kind: Kind::Name,
            surface: m.surface.clone(),
            start: m.start.unwrap_or(0),
            end: m.end.unwrap_or(0),
            value: value.clone(),
        };
        let occurrence = Occurrence {
            source,
            start: found.start,
            end: found.end,
            surface: m.surface.clone(),
        };
        let mut props = m.props.clone();
        props
            .entry("name".to_owned())
            .or_insert_with(|| Value::from(m.surface.as_str()));
        // By rule, by an earlier decision, or by name.
        let target = if let Some(decided) = decision(tx, ns, &value)? {
            Some(decided)
        } else if read.match_rule(ns, &label)?.is_some() {
            let probe = Probe {
                label: label.clone(),
                props: props.clone(),
                neighbors: Vec::new(),
            };
            let matches = read.find_matches(ns, &probe, None, 5, Some(source))?;
            match matches.first() {
                Some(best) if best.decision == Decision::Same => Some(best.node),
                Some(best) if best.decision == Decision::Review => {
                    let candidates: Vec<NodeId> = matches
                        .iter()
                        .filter(|m| m.decision != Decision::Different)
                        .map(|m| m.node)
                        .collect();
                    queue(
                        tx,
                        ns,
                        "ambiguous",
                        &value,
                        occurrence,
                        &candidates,
                        Some(label.clone()),
                        props.clone(),
                    )?;
                    report.queued += 1;
                    let _ = best;
                    targets.push(None);
                    continue;
                }
                _ => None,
            }
        } else {
            let mut named = Vec::new();
            for id in tx.find_by_name(ns, &value)? {
                if id != source && tx.node(ns, id)?.is_some_and(|node| node.has_label(&label)) {
                    named.push(id);
                }
            }
            match named.len() {
                0 => None,
                1 => Some(named[0]),
                _ => {
                    queue(
                        tx,
                        ns,
                        "ambiguous",
                        &value,
                        occurrence,
                        &named,
                        Some(label.clone()),
                        props.clone(),
                    )?;
                    report.queued += 1;
                    targets.push(None);
                    continue;
                }
            }
        };
        let target = match target {
            Some(id) => {
                report.linked += 1;
                id
            }
            None => {
                let mut new = NewNode::new(label.as_str()).source(source);
                for (key, value) in &props {
                    new = new.prop(key.as_str(), value.clone());
                }
                report.created += 1;
                tx.create_node(ns, new)?
            }
        };
        tx.create_edge(ns, mention(&found, target, source, false, "agent"))?;
        report.mentions += 1;
        targets.push(Some(target));
    }
    Ok((report, targets))
}

/// A relation an agent read between two of the entities it mentions in a
/// text: `from` and `to` are the mentions' positions in the same call, or
/// their surfaces as written there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentRelation {
    pub from: MentionRef,
    pub label: String,
    pub to: MentionRef,
    /// How sure the agent is, from 0 to 1.
    #[serde(default)]
    pub certainty: Option<f64>,
    /// How it is known: `inferred` (the default) when the agent concluded
    /// it, `stated` when the text says it in so many words.
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub valid_from: Option<Timestamp>,
    #[serde(default)]
    pub valid_to: Option<Timestamp>,
}

/// A mention of the same call, by position or by its surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MentionRef {
    Index(usize),
    Surface(String),
}

/// What became of an agent's reading of a text.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Reading {
    pub report: ExtractReport,
    /// The node each mention was linked to, in order; `None` while it
    /// waits in the review queue.
    pub entities: Vec<Option<NodeId>>,
    /// The facts recorded, in the order of the relations.
    pub relations: Vec<EdgeId>,
    /// Relations not recorded, each with its position and why.
    pub skipped: Vec<(usize, String)>,
    /// Earlier facts the new ones contradicted under a constraint.
    pub contradicted: Vec<grafeno::ConstraintEvent>,
}

/// An agent's reading of a text in one call: the entities it mentions
/// ([`ingest`]) and the relations between them, each a fact from one
/// entity to another with the text as its source, `inferred` unless said
/// otherwise, and the certainty given. A relation whose entity waits in
/// review, or that names a mention not in the call, is skipped and said
/// why; the rest are recorded.
pub fn ingest_reading(
    tx: &mut WriteTx,
    read: &ReadTx,
    ns: &str,
    source: NodeId,
    mentions: &[AgentMention],
    relations: &[AgentRelation],
    default_label: &str,
) -> Result<Reading> {
    // Every relation is checked before anything is written.
    let mut checked = Vec::with_capacity(relations.len());
    for relation in relations {
        if relation.label.trim().is_empty() {
            return Err(Error::Invalid("a relation needs a label".into()));
        }
        let origin = relation.origin.as_deref().unwrap_or("inferred");
        let props = crate::knowledge::stated(Some(origin), relation.certainty)?;
        checked.push(props);
    }
    let position = |reference: &MentionRef| -> Option<usize> {
        match reference {
            MentionRef::Index(index) => (*index < mentions.len()).then_some(*index),
            MentionRef::Surface(surface) => {
                let key = normalize_name(surface);
                mentions
                    .iter()
                    .position(|m| normalize_name(&m.surface) == key)
            }
        }
    };
    let (report, entities) = ingest_targets(tx, read, ns, source, mentions, default_label)?;
    let mut reading = Reading {
        report,
        entities,
        ..Reading::default()
    };
    for (index, (relation, props)) in relations.iter().zip(checked).enumerate() {
        let (Some(from), Some(to)) = (position(&relation.from), position(&relation.to)) else {
            reading
                .skipped
                .push((index, "it names a mention that is not in this call".into()));
            continue;
        };
        let (Some(from), Some(to)) = (reading.entities[from], reading.entities[to]) else {
            reading.skipped.push((
                index,
                "one of its entities waits in the review queue".into(),
            ));
            continue;
        };
        let label = relation.label.trim();
        // The same current fact again: the one there is stands.
        if let Some(existing) = crate::knowledge::existing_fact(
            tx,
            ns,
            from,
            label,
            to,
            relation.valid_from,
            relation.valid_to,
        )? {
            reading.relations.push(existing);
            continue;
        }
        let mut edge = NewEdge::new(from, label, to).source(source);
        edge.props = props;
        edge.props
            .insert(crate::knowledge::METHOD.into(), Value::from("agent"));
        edge.valid_from = relation.valid_from;
        edge.valid_to = relation.valid_to;
        reading.relations.push(tx.create_edge(ns, edge)?);
        reading.contradicted.extend(tx.take_constraint_events());
    }
    Ok(reading)
}
