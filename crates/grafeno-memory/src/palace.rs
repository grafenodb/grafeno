//! Mental palaces: memories arranged into spaces.
//!
//! A palace ([`PALACE`]) has rooms ([`ROOM`], [`HAS_ROOM`]), a room has
//! places inside it ([`LOCUS`], [`HAS_LOCUS`]), and a memory is anchored at
//! any number of places ([`ANCHORED_AT`]). All of it is ordinary nodes and
//! edges in one namespace: the memories are the nodes that already exist,
//! and anchoring one neither copies nor changes it.
//!
//! Names are compared normalized ([`grafeno::normalize_name`]). A palace's
//! name is unique in its namespace, a room's in its palace and a locus's in
//! its room; creating one that exists returns the one there is.

use std::collections::{HashMap, HashSet};

use grafeno::{
    Direction, EdgeFilter, EdgeId, Error, NewEdge, NewNode, Node, NodeId, ReadTx, Result,
    Timestamp, Value, WriteTx, normalize_name,
};
use serde::Serialize;

use crate::checkpoint::{Checkpoint, ResumeQuery, resume};
use crate::context::{Context, ContextOptions, context_of, readable_name};
use crate::recall::{ARCHIVED, RecallQuery, recall};

/// The label of palaces.
pub const PALACE: &str = "Palace";
/// The label of rooms.
pub const ROOM: &str = "Room";
/// The label of the places inside a room.
pub const LOCUS: &str = "Locus";
/// A palace → one of its rooms.
pub const HAS_ROOM: &str = "has_room";
/// A room → one of its loci.
pub const HAS_LOCUS: &str = "has_locus";
/// A memory → a locus it is anchored at.
pub const ANCHORED_AT: &str = "anchored_at";
/// The locus every room is created with.
pub const ENTRANCE: &str = "entrance";

/// The labels of the nodes a palace is made of.
pub const STRUCTURE_LABELS: [&str; 3] = [PALACE, ROOM, LOCUS];

/// What the two kinds of transaction both do.
trait Graph {
    fn node_of(&self, ns: &str, id: NodeId) -> Result<Option<Node>>;
    fn edges_of(
        &self,
        ns: &str,
        node: NodeId,
        direction: Direction,
        label: &str,
    ) -> Result<Vec<(EdgeId, NodeId)>>;
    fn named(&self, ns: &str, name: &str) -> Result<Vec<NodeId>>;
}

macro_rules! graph {
    ($tx:ty) => {
        impl Graph for $tx {
            fn node_of(&self, ns: &str, id: NodeId) -> Result<Option<Node>> {
                self.node(ns, id)
            }

            fn edges_of(
                &self,
                ns: &str,
                node: NodeId,
                direction: Direction,
                label: &str,
            ) -> Result<Vec<(EdgeId, NodeId)>> {
                self.neighbor_ids(ns, node, direction, &EdgeFilter::any().label(label))
            }

            fn named(&self, ns: &str, name: &str) -> Result<Vec<NodeId>> {
                self.find_by_name(ns, name)
            }
        }
    };
}

graph!(ReadTx);
graph!(WriteTx);

/// What a `create_*` call found or made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Created {
    pub id: NodeId,
    /// False when it was already there.
    pub created: bool,
}

/// A room to create.
#[derive(Clone, Debug, Default)]
pub struct NewRoom {
    pub name: String,
    /// What the room is for: a project, a domain, a subject.
    pub topic: Option<String>,
    pub description: Option<String>,
    /// Loci to create besides the [`ENTRANCE`].
    pub loci: Vec<String>,
}

impl NewRoom {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn locus(mut self, name: impl Into<String>) -> Self {
        self.loci.push(name.into());
        self
    }
}

/// An anchor, as [`anchor`] left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Anchored {
    pub edge: EdgeId,
    /// False when the memory was already anchored there.
    pub created: bool,
}

/// Where a memory is anchored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Place {
    pub locus: NodeId,
    pub locus_name: String,
    /// Absent when the locus's room was deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_name: Option<String>,
    /// Absent when the room's palace was deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palace: Option<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palace_name: Option<String>,
    /// Why the memory is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub anchor: EdgeId,
}

/// Whether `node` is part of a palace's structure, and so not a memory.
pub fn is_structure(node: &Node) -> bool {
    STRUCTURE_LABELS.iter().any(|label| node.has_label(label))
}

/// The nodes palaces are made of in `ns`. What reads a namespace as
/// memories (recall, communities, maintenance, extraction) leaves them out.
pub fn structure(read: &ReadTx, ns: &str) -> Result<HashSet<NodeId>> {
    let mut ids = HashSet::new();
    for label in STRUCTURE_LABELS {
        ids.extend(read.nodes_with_label(ns, label)?);
    }
    Ok(ids)
}

fn name_of(node: &Node) -> String {
    node.prop("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn key(kind: &str, name: &str) -> Result<String> {
    let key = normalize_name(name);
    if key.is_empty() {
        return Err(Error::Invalid(format!("a {kind} needs a name")));
    }
    Ok(key)
}

/// The node `id` when it has `label`.
fn expect(graph: &impl Graph, ns: &str, id: NodeId, label: &str) -> Result<Node> {
    let node = graph.node_of(ns, id)?.ok_or(Error::NodeNotFound(id))?;
    if !node.has_label(label) {
        return Err(Error::Invalid(format!("{id} is not a {label}")));
    }
    Ok(node)
}

/// The palace named `name`: the oldest when a database holds several.
fn palace_named(graph: &impl Graph, ns: &str, name: &str) -> Result<Option<NodeId>> {
    let key = normalize_name(name);
    let mut best: Option<Node> = None;
    for id in graph.named(ns, name)? {
        let Some(node) = graph.node_of(ns, id)? else {
            continue;
        };
        // An alias or a title is not the palace's name.
        if node.has_label(PALACE)
            && normalize_name(&name_of(&node)) == key
            && best
                .as_ref()
                .is_none_or(|best| (node.created_at, node.id) < (best.created_at, best.id))
        {
            best = Some(node);
        }
    }
    Ok(best.map(|node| node.id))
}

/// The children of `parent` over `edge` that have `label`, as written.
fn children(
    graph: &impl Graph,
    ns: &str,
    parent: NodeId,
    edge: &str,
    label: &str,
) -> Result<Vec<Node>> {
    let mut out = Vec::new();
    for (_, id) in graph.edges_of(ns, parent, Direction::Out, edge)? {
        if let Some(node) = graph.node_of(ns, id)?
            && node.has_label(label)
        {
            out.push(node);
        }
    }
    Ok(out)
}

fn child_named(
    graph: &impl Graph,
    ns: &str,
    parent: NodeId,
    edge: &str,
    label: &str,
    name: &str,
) -> Result<Option<NodeId>> {
    let key = normalize_name(name);
    if key.is_empty() {
        return Ok(None);
    }
    Ok(children(graph, ns, parent, edge, label)?
        .into_iter()
        .filter(|node| normalize_name(&name_of(node)) == key)
        .min_by_key(|node| (node.created_at, node.id))
        .map(|node| node.id))
}

/// The first node `id` is joined to, coming in over `edge`, with `label`.
fn parent(
    graph: &impl Graph,
    ns: &str,
    id: NodeId,
    edge: &str,
    label: &str,
) -> Result<Option<Node>> {
    for (_, other) in graph.edges_of(ns, id, Direction::In, edge)? {
        if let Some(node) = graph.node_of(ns, other)?
            && node.has_label(label)
        {
            return Ok(Some(node));
        }
    }
    Ok(None)
}

/// The text a palace or a room is found by.
fn text(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .flatten()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(". ")
}

/// The palace named `name` in `ns`.
pub fn find_palace(read: &ReadTx, ns: &str, name: &str) -> Result<Option<NodeId>> {
    palace_named(read, ns, name)
}

/// The room named `name` in `palace`.
pub fn find_room(read: &ReadTx, ns: &str, palace: NodeId, name: &str) -> Result<Option<NodeId>> {
    child_named(read, ns, palace, HAS_ROOM, ROOM, name)
}

/// The locus named `name` in `room`.
pub fn find_locus(read: &ReadTx, ns: &str, room: NodeId, name: &str) -> Result<Option<NodeId>> {
    child_named(read, ns, room, HAS_LOCUS, LOCUS, name)
}

/// Creates the palace `name`, or finds it.
pub fn create_palace(
    tx: &mut WriteTx,
    ns: &str,
    name: &str,
    description: Option<&str>,
) -> Result<Created> {
    key("palace", name)?;
    if let Some(id) = palace_named(tx, ns, name)? {
        return Ok(Created { id, created: false });
    }
    let name = name.trim();
    let mut node = NewNode::new(PALACE)
        .prop("name", name)
        .text(text(&[Some(name), description]));
    if let Some(description) = description {
        node = node.prop("description", description);
    }
    let id = tx.create_node(ns, node)?;
    Ok(Created { id, created: true })
}

/// Creates a room in `palace`, with its [`ENTRANCE`] and the loci asked
/// for; or finds it, and adds the loci it lacks.
pub fn create_room(tx: &mut WriteTx, ns: &str, palace: NodeId, new: NewRoom) -> Result<Created> {
    key("room", &new.name)?;
    expect(tx, ns, palace, PALACE)?;
    let found = child_named(tx, ns, palace, HAS_ROOM, ROOM, &new.name)?;
    let room = match found {
        Some(id) => id,
        None => {
            let name = new.name.trim();
            let mut node = NewNode::new(ROOM).prop("name", name).text(text(&[
                Some(name),
                new.topic.as_deref(),
                new.description.as_deref(),
            ]));
            if let Some(topic) = &new.topic {
                node = node.prop("topic", topic.clone());
            }
            if let Some(description) = &new.description {
                node = node.prop("description", description.clone());
            }
            let id = tx.create_node(ns, node)?;
            tx.create_edge(ns, NewEdge::new(palace, HAS_ROOM, id))?;
            id
        }
    };
    create_locus(tx, ns, room, ENTRANCE)?;
    for locus in &new.loci {
        create_locus(tx, ns, room, locus)?;
    }
    Ok(Created {
        id: room,
        created: found.is_none(),
    })
}

/// Creates the locus `name` in `room`, after the ones it has; or finds it.
pub fn create_locus(tx: &mut WriteTx, ns: &str, room: NodeId, name: &str) -> Result<Created> {
    key("locus", name)?;
    expect(tx, ns, room, ROOM)?;
    if let Some(id) = child_named(tx, ns, room, HAS_LOCUS, LOCUS, name)? {
        return Ok(Created { id, created: false });
    }
    let position = children(tx, ns, room, HAS_LOCUS, LOCUS)?
        .iter()
        .filter_map(|node| node.prop("position").and_then(Value::as_u64))
        .max()
        .map_or(0, |last| last + 1);
    let name = name.trim();
    let id = tx.create_node(
        ns,
        NewNode::new(LOCUS)
            .prop("name", name)
            .prop("position", position)
            .text(name),
    )?;
    tx.create_edge(ns, NewEdge::new(room, HAS_LOCUS, id))?;
    Ok(Created { id, created: true })
}

/// Anchors `memory` at `locus`. The memory stays as it is, with its own
/// edges; anchoring it there again keeps the anchor there is.
pub fn anchor(
    tx: &mut WriteTx,
    ns: &str,
    memory: NodeId,
    locus: NodeId,
    note: Option<&str>,
) -> Result<Anchored> {
    let node = tx.node(ns, memory)?.ok_or(Error::NodeNotFound(memory))?;
    if is_structure(&node) {
        return Err(Error::Invalid(format!(
            "{memory} is part of a palace, not a memory"
        )));
    }
    expect(tx, ns, locus, LOCUS)?;
    let existing = tx
        .edges_of(ns, memory, Direction::Out, ANCHORED_AT)?
        .into_iter()
        .find(|(_, other)| *other == locus);
    if let Some((edge, _)) = existing {
        return Ok(Anchored {
            edge,
            created: false,
        });
    }
    let mut edge = NewEdge::new(memory, ANCHORED_AT, locus);
    if let Some(note) = note.map(str::trim).filter(|note| !note.is_empty()) {
        edge = edge.prop("note", note);
    }
    Ok(Anchored {
        edge: tx.create_edge(ns, edge)?,
        created: true,
    })
}

/// Removes the anchor of `memory` at `locus`; false when there was none.
pub fn unanchor(tx: &mut WriteTx, ns: &str, memory: NodeId, locus: NodeId) -> Result<bool> {
    let anchors: Vec<EdgeId> = tx
        .edges_of(ns, memory, Direction::Out, ANCHORED_AT)?
        .into_iter()
        .filter(|(_, other)| *other == locus)
        .map(|(edge, _)| edge)
        .collect();
    for edge in &anchors {
        tx.delete_edge(ns, *edge)?;
    }
    Ok(!anchors.is_empty())
}

/// The places `memory` is anchored at, by palace, room and locus name.
pub fn anchors_of(read: &ReadTx, ns: &str, memory: NodeId) -> Result<Vec<Place>> {
    let mut places = Vec::new();
    for (edge, locus) in read.edges_of(ns, memory, Direction::Out, ANCHORED_AT)? {
        let Some(locus) = read.node(ns, locus)?.filter(|node| node.has_label(LOCUS)) else {
            continue;
        };
        let room = parent(read, ns, locus.id, HAS_LOCUS, ROOM)?;
        let palace = match &room {
            Some(room) => parent(read, ns, room.id, HAS_ROOM, PALACE)?,
            None => None,
        };
        let note = read.edge(ns, edge)?.and_then(|edge| {
            edge.props
                .get("note")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        places.push(Place {
            locus: locus.id,
            locus_name: name_of(&locus),
            room: room.as_ref().map(|room| room.id),
            room_name: room.as_ref().map(name_of),
            palace: palace.as_ref().map(|palace| palace.id),
            palace_name: palace.as_ref().map(name_of),
            note,
            anchor: edge,
        });
    }
    places.sort_by(|a, b| {
        (&a.palace_name, &a.room_name, &a.locus_name, a.locus).cmp(&(
            &b.palace_name,
            &b.room_name,
            &b.locus_name,
            b.locus,
        ))
    });
    Ok(places)
}

/// What a map shows.
#[derive(Clone, Debug)]
pub struct MapOptions {
    /// List the memories at each locus, and not only how many there are.
    pub memories: bool,
    /// Memories listed per locus at most, the newest first.
    pub limit: usize,
}

impl Default for MapOptions {
    fn default() -> Self {
        Self {
            memories: false,
            limit: 20,
        }
    }
}

/// A palace and its rooms.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PalaceMap {
    pub id: NodeId,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// By name.
    pub rooms: Vec<RoomMap>,
}

/// A room and its loci.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RoomMap {
    pub id: NodeId,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Different memories anchored in the room, at any of its loci.
    pub memories: usize,
    /// In the order they were made, the entrance first.
    pub loci: Vec<LocusMap>,
}

/// A locus and what is anchored at it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LocusMap {
    pub id: NodeId,
    pub name: String,
    pub position: u64,
    /// Memories anchored here.
    pub count: usize,
    /// Those memories, when the map was asked for them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<AnchoredMemory>,
}

/// A memory as a map lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AnchoredMemory {
    pub id: NodeId,
    /// Its name, or the start of its text.
    pub name: String,
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn string(node: &Node, key: &str) -> Option<String> {
    node.prop(key).and_then(Value::as_str).map(str::to_owned)
}

fn position(node: &Node) -> u64 {
    node.prop("position")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX)
}

/// A memory's name, or the start of its text.
fn title(read: &ReadTx, ns: &str, node: &Node) -> Result<String> {
    let text = read.text(ns, node.id)?.unwrap_or_default();
    Ok(readable_name(node, &text))
}

fn note_of(read: &ReadTx, ns: &str, edge: EdgeId) -> Result<Option<String>> {
    Ok(read.edge(ns, edge)?.and_then(|edge| {
        edge.props
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }))
}

/// The memories anchored at `locus`, newest first, with their anchors.
fn anchored(read: &ReadTx, ns: &str, locus: NodeId) -> Result<Vec<(EdgeId, Node)>> {
    let mut out = Vec::new();
    for (edge, id) in read.edges_of(ns, locus, Direction::In, ANCHORED_AT)? {
        if let Some(node) = read.node(ns, id)?
            && !is_structure(&node)
        {
            out.push((edge, node));
        }
    }
    out.sort_by_key(|(_, node)| std::cmp::Reverse((node.created_at, node.id)));
    Ok(out)
}

fn locus_map(read: &ReadTx, ns: &str, locus: &Node, options: &MapOptions) -> Result<LocusMap> {
    let found = anchored(read, ns, locus.id)?;
    let mut memories = Vec::new();
    if options.memories {
        for (edge, node) in found.iter().take(options.limit) {
            memories.push(AnchoredMemory {
                id: node.id,
                name: title(read, ns, node)?,
                labels: node.labels.clone(),
                note: note_of(read, ns, *edge)?,
            });
        }
    }
    Ok(LocusMap {
        id: locus.id,
        name: name_of(locus),
        position: locus
            .prop("position")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        count: found.len(),
        memories,
    })
}

fn room_map(read: &ReadTx, ns: &str, room: &Node, options: &MapOptions) -> Result<RoomMap> {
    let mut nodes = children(read, ns, room.id, HAS_LOCUS, LOCUS)?;
    nodes.sort_by_key(|node| (position(node), node.id));
    let mut loci = Vec::with_capacity(nodes.len());
    let mut memories = HashSet::new();
    for locus in &nodes {
        let map = locus_map(read, ns, locus, options)?;
        memories.extend(
            anchored(read, ns, locus.id)?
                .into_iter()
                .map(|(_, memory)| memory.id),
        );
        loci.push(map);
    }
    Ok(RoomMap {
        id: room.id,
        name: name_of(room),
        topic: string(room, "topic"),
        description: string(room, "description"),
        memories: memories.len(),
        loci,
    })
}

/// A room: its loci and, when asked, the memories at each.
pub fn map_room(read: &ReadTx, ns: &str, room: NodeId, options: &MapOptions) -> Result<RoomMap> {
    let room = expect(read, ns, room, ROOM)?;
    room_map(read, ns, &room, options)
}

/// A palace: its rooms and their loci.
pub fn map_palace(
    read: &ReadTx,
    ns: &str,
    palace: NodeId,
    options: &MapOptions,
) -> Result<PalaceMap> {
    let palace = expect(read, ns, palace, PALACE)?;
    let mut rooms = Vec::new();
    for room in children(read, ns, palace.id, HAS_ROOM, ROOM)? {
        rooms.push(room_map(read, ns, &room, options)?);
    }
    rooms.sort_by(|a, b| {
        normalize_name(&a.name)
            .cmp(&normalize_name(&b.name))
            .then(a.id.cmp(&b.id))
    });
    Ok(PalaceMap {
        id: palace.id,
        name: name_of(&palace),
        description: string(&palace, "description"),
        rooms,
    })
}

/// The palaces of `ns`, by name, each with its rooms and their loci.
pub fn palaces(read: &ReadTx, ns: &str, options: &MapOptions) -> Result<Vec<PalaceMap>> {
    let mut out = Vec::new();
    for id in read.nodes_with_label(ns, PALACE)? {
        out.push(map_palace(read, ns, id, options)?);
    }
    out.sort_by(|a, b| {
        normalize_name(&a.name)
            .cmp(&normalize_name(&b.name))
            .then(a.id.cmp(&b.id))
    });
    Ok(out)
}

/// Hops a walk follows at most.
pub const MAX_HOPS: usize = 3;
/// Neighbors a walk follows from one memory at most: past it, the node is
/// a hub, and what it leads to is not about the topic.
const WALK_FANOUT: usize = 50;
/// What strength a walk orders memories by decays with: thirty days.
const HALF_LIFE_SECS: f64 = 30.0 * 86_400.0;

/// Where a walk starts and how far it goes.
#[derive(Clone, Debug)]
pub struct WalkQuery {
    /// What to look for: the rooms and loci whose name, topic or
    /// description match it, and the anchored memories that match it.
    pub topic: Option<String>,
    /// A node to start from: a palace, a room, a locus or a memory.
    pub from: Option<NodeId>,
    /// Only this palace; every palace of the namespace when absent.
    pub palace: Option<NodeId>,
    /// Relations followed from the memories found, up to [`MAX_HOPS`].
    pub hops: usize,
    /// Memories returned at most.
    pub limit: usize,
}

impl Default for WalkQuery {
    fn default() -> Self {
        Self {
            topic: None,
            from: None,
            palace: None,
            hops: 2,
            limit: 20,
        }
    }
}

impl WalkQuery {
    pub fn topic(topic: impl Into<String>) -> Self {
        Self {
            topic: Some(topic.into()),
            ..Self::default()
        }
    }

    pub fn from(node: NodeId) -> Self {
        Self {
            from: Some(node),
            ..Self::default()
        }
    }

    pub fn palace(mut self, palace: NodeId) -> Self {
        self.palace = Some(palace);
        self
    }

    pub fn hops(mut self, hops: usize) -> Self {
        self.hops = hops;
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// A palace, room or locus a walk went through.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WalkPlace {
    pub id: NodeId,
    /// [`PALACE`], [`ROOM`] or [`LOCUS`].
    pub kind: String,
    /// Where it is, as `Palace / Room / Locus`.
    pub path: String,
    /// How well it matched the topic; 0 when it was the node given.
    pub score: f32,
}

/// One relation followed by a walk.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Hop {
    pub edge: EdgeId,
    pub label: String,
    /// Whether the edge points at the node reached (or comes from it).
    pub forward: bool,
    pub node: NodeId,
    pub name: String,
}

/// A memory a walk reached.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reached {
    pub id: NodeId,
    pub name: String,
    pub labels: Vec<String>,
    /// Relations between it and a memory found in the palace: 0 for the
    /// ones found there.
    pub hops: usize,
    /// The memory found in the palace the walk came from; itself at 0 hops.
    pub origin: NodeId,
    /// The relations followed from `origin`, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub via: Vec<Hop>,
    /// Where it is anchored.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<Place>,
}

/// What a walk found.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Walk {
    /// The places the topic led to, best first.
    pub places: Vec<WalkPlace>,
    /// The memories reached: the nearest first, then the strongest.
    pub memories: Vec<Reached>,
}

/// A place as a walk shows it, and the palace it is in.
fn walk_place(
    read: &ReadTx,
    ns: &str,
    node: &Node,
    score: f32,
) -> Result<(WalkPlace, Option<NodeId>)> {
    let room = if node.has_label(LOCUS) {
        parent(read, ns, node.id, HAS_LOCUS, ROOM)?
    } else if node.has_label(ROOM) {
        Some(node.clone())
    } else {
        None
    };
    let palace = if node.has_label(PALACE) {
        Some(node.clone())
    } else {
        match &room {
            Some(room) => parent(read, ns, room.id, HAS_ROOM, PALACE)?,
            None => None,
        }
    };
    let mut path: Vec<String> = Vec::new();
    path.extend(palace.as_ref().map(name_of));
    path.extend(room.as_ref().map(name_of));
    if node.has_label(LOCUS) {
        path.push(name_of(node));
    }
    let kind = STRUCTURE_LABELS
        .iter()
        .find(|label| node.has_label(label))
        .copied()
        .unwrap_or(LOCUS);
    Ok((
        WalkPlace {
            id: node.id,
            kind: kind.to_owned(),
            path: path.join(" / "),
            score,
        },
        palace.map(|palace| palace.id),
    ))
}

/// The loci under a palace, a room or a locus.
fn loci_under(read: &ReadTx, ns: &str, node: &Node) -> Result<Vec<NodeId>> {
    if node.has_label(LOCUS) {
        return Ok(vec![node.id]);
    }
    let rooms = if node.has_label(ROOM) {
        vec![node.clone()]
    } else {
        children(read, ns, node.id, HAS_ROOM, ROOM)?
    };
    let mut loci = Vec::new();
    for room in rooms {
        let mut found = children(read, ns, room.id, HAS_LOCUS, LOCUS)?;
        found.sort_by_key(|node| (position(node), node.id));
        loci.extend(found.into_iter().map(|node| node.id));
    }
    Ok(loci)
}

fn strength(read: &ReadTx, ns: &str, node: &Node, now: Timestamp) -> Result<f32> {
    Ok(read.memory(ns, node)?.strength(now, HALF_LIFE_SECS))
}

/// Walks from a topic, or from a node, to the memories around it: the
/// places that match, the memories anchored there and the anchored
/// memories that match, and from those, over the graph's relations, the
/// memories within `hops`. A topic nothing matches gives an empty walk.
pub fn walk(read: &ReadTx, ns: &str, query: &WalkQuery) -> Result<Walk> {
    let topic = query
        .topic
        .as_deref()
        .map(str::trim)
        .filter(|topic| !topic.is_empty());
    if query.from.is_none() && topic.is_none() {
        return Err(Error::Invalid(
            "a walk needs a topic or a node to start from".into(),
        ));
    }
    if let Some(palace) = query.palace {
        expect(read, ns, palace, PALACE)?;
    }
    let inside = |palace: Option<NodeId>| query.palace.is_none() || palace == query.palace;
    let hops = query.hops.min(MAX_HOPS);
    let mut walk = Walk::default();
    // The places to look in, and the memories to start from, in order.
    let mut places: Vec<Node> = Vec::new();
    let mut origins: Vec<NodeId> = Vec::new();
    let mut seen_places = HashSet::new();

    if let Some(from) = query.from {
        let node = read.node(ns, from)?.ok_or(Error::NodeNotFound(from))?;
        if is_structure(&node) {
            let (place, palace) = walk_place(read, ns, &node, 0.0)?;
            if inside(palace) && seen_places.insert(node.id) {
                walk.places.push(place);
                places.push(node);
            }
        } else {
            origins.push(from);
        }
    }
    if let Some(topic) = topic {
        let mut places_query = RecallQuery::new().text(topic).limit(10).expand(None);
        places_query.ranking = None;
        places_query.labels = STRUCTURE_LABELS
            .iter()
            .map(|label| label.to_string())
            .collect();
        for hit in recall(read, ns, &places_query)? {
            let (place, palace) = walk_place(read, ns, &hit.node, hit.score)?;
            if inside(palace) && seen_places.insert(hit.node.id) {
                walk.places.push(place);
                places.push(hit.node);
            }
        }
        // Memories that match and have a place in the palace.
        let matching = RecallQuery::new()
            .text(topic)
            .limit(query.limit.max(1))
            .expand(None);
        for hit in recall(read, ns, &matching)? {
            let anchors = anchors_of(read, ns, hit.node.id)?;
            if anchors.iter().any(|place| inside(place.palace)) {
                origins.push(hit.node.id);
            }
        }
    }

    // What is anchored at the places found comes first, place by place.
    let mut level: Vec<NodeId> = Vec::new();
    let mut reached: HashMap<NodeId, (usize, NodeId, Vec<Hop>)> = HashMap::new();
    for place in &places {
        for locus in loci_under(read, ns, place)? {
            for (_, node) in anchored(read, ns, locus)? {
                if !node.has_label(ARCHIVED) && !reached.contains_key(&node.id) {
                    reached.insert(node.id, (0, node.id, Vec::new()));
                    level.push(node.id);
                }
            }
        }
    }
    for id in origins {
        if let std::collections::hash_map::Entry::Vacant(slot) = reached.entry(id) {
            slot.insert((0, id, Vec::new()));
            level.push(id);
        }
    }

    let now = Timestamp::now();
    let hidden = structure(read, ns)?;
    let mut order: Vec<NodeId> = Vec::new();
    for distance in 0..=hops {
        // What was found in the palace keeps its order; what was reached
        // from it is ordered by strength within its distance.
        if distance > 0 {
            let mut strengths = HashMap::new();
            for id in &level {
                if let Some(node) = read.node(ns, *id)? {
                    strengths.insert(*id, strength(read, ns, &node, now)?);
                }
            }
            level.sort_by(|a, b| {
                let of = |id: &NodeId| strengths.get(id).copied().unwrap_or(0.0);
                of(b).total_cmp(&of(a)).then(a.cmp(b))
            });
        }
        order.extend(level.iter().copied());
        if order.len() >= query.limit || distance == hops {
            break;
        }
        let mut next = Vec::new();
        for id in &level {
            let (_, origin, via) = reached[id].clone();
            let neighbors = read.neighbors(ns, *id, Direction::Both, &EdgeFilter::any())?;
            if neighbors.len() > WALK_FANOUT {
                continue;
            }
            for neighbor in neighbors {
                if hidden.contains(&neighbor.node) || reached.contains_key(&neighbor.node) {
                    continue;
                }
                let Some(node) = read.node(ns, neighbor.node)? else {
                    continue;
                };
                if node.has_label(ARCHIVED) || node.has_label(crate::communities::COMMUNITY) {
                    continue;
                }
                let mut via = via.clone();
                via.push(Hop {
                    edge: neighbor.edge.id,
                    label: neighbor.edge.label.clone(),
                    forward: neighbor.edge.to == node.id,
                    node: node.id,
                    name: title(read, ns, &node)?,
                });
                reached.insert(node.id, (distance + 1, origin, via));
                next.push(node.id);
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }

    for id in order.into_iter().take(query.limit) {
        let Some(node) = read.node(ns, id)? else {
            continue;
        };
        let (hops, origin, via) = reached.remove(&id).unwrap_or((0, id, Vec::new()));
        walk.memories.push(Reached {
            id,
            name: title(read, ns, &node)?,
            labels: node.labels.clone(),
            hops,
            origin,
            via,
            places: anchors_of(read, ns, id)?,
        });
    }
    Ok(walk)
}

/// How a room is entered.
#[derive(Clone, Debug)]
pub struct EnterOptions {
    /// How the room's memories are packed, and into how many tokens.
    pub context: ContextOptions,
}

impl EnterOptions {
    pub fn with_budget(budget: usize) -> Self {
        Self {
            context: ContextOptions::with_budget(budget),
        }
    }
}

impl Default for EnterOptions {
    fn default() -> Self {
        Self::with_budget(2_000)
    }
}

/// What an agent gets on entering a room.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Entered {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palace: Option<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palace_name: Option<String>,
    pub room: RoomMap,
    /// The room's memories, the strongest first, and the facts that
    /// connect them, within the budget.
    pub context: Context,
    /// The latest checkpoint about the room, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Checkpoint>,
}

/// Enters a room: its loci, its memories packed into a token budget, and
/// where the work in it was left (the latest checkpoint about the room).
pub fn enter(read: &ReadTx, ns: &str, room: NodeId, options: &EnterOptions) -> Result<Entered> {
    let node = expect(read, ns, room, ROOM)?;
    let palace = parent(read, ns, room, HAS_ROOM, PALACE)?;
    let map = room_map(read, ns, &node, &MapOptions::default())?;
    let now = Timestamp::now();
    let mut seen = HashSet::new();
    let mut memories: Vec<(Node, f32)> = Vec::new();
    for locus in &map.loci {
        for (_, memory) in anchored(read, ns, locus.id)? {
            if memory.has_label(ARCHIVED) || !seen.insert(memory.id) {
                continue;
            }
            let score = strength(read, ns, &memory, now)?;
            memories.push((memory, score));
        }
    }
    memories.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
    let context = context_of(read, ns, memories, &options.context)?;
    let checkpoint = resume(
        read,
        ns,
        &ResumeQuery {
            about: Some(room),
            ..ResumeQuery::default()
        },
    )?;
    Ok(Entered {
        palace: palace.as_ref().map(|palace| palace.id),
        palace_name: palace.as_ref().map(name_of),
        room: map,
        context,
        checkpoint,
    })
}
