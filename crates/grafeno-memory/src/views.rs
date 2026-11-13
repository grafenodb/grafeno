//! The palace as a view (phase 11): routes that follow given relations in
//! order towards a goal, and the context of one node rebuilt in one call.
//!
//! Both read only. A palace never copies a memory, so what these return is
//! the memory as it is now, whichever palace it was reached from.

use std::collections::HashSet;

use grafeno::{Direction, EdgeFilter, Node, NodeId, ReadTx, Result, Timestamp};
use serde::{Deserialize, Serialize};

use crate::history::{History, StateFact};
use crate::knowledge::Justification;
use crate::palace::{ANCHORED_AT, HAS_LOCUS, HAS_ROOM, Hop, LOCUS, PALACE, Place, ROOM};
use crate::reflect::{ReflectOptions, Reflection, Scope};
use crate::timeline::Moment;

/// Relations a route follows from one node at most.
const FANOUT: usize = 50;
/// Hops a route takes at most.
pub const MAX_ROUTE_HOPS: usize = 12;

/// One leg of a route: a relation to follow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteStep {
    pub label: String,
    /// Follow it against its direction: from the node it points at, to the
    /// one it comes from.
    #[serde(default)]
    pub backward: bool,
    /// Follow it as far as it goes (one hop at least): a line of versions,
    /// a chain of dependencies.
    #[serde(default)]
    pub repeat: bool,
}

impl RouteStep {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            backward: false,
            repeat: false,
        }
    }

    pub fn backward(mut self) -> Self {
        self.backward = true;
        self
    }

    pub fn repeat(mut self) -> Self {
        self.repeat = true;
        self
    }
}

/// Where a route starts, the relations it follows in order, and where it
/// should end.
#[derive(Clone, Debug)]
pub struct RouteQuery {
    pub from: NodeId,
    pub steps: Vec<RouteStep>,
    /// Only routes that end here.
    pub to: Option<NodeId>,
    /// Follow only facts true at this time.
    pub valid_at: Option<Timestamp>,
    /// Routes returned at most.
    pub limit: usize,
    /// Hops at most, up to [`MAX_ROUTE_HOPS`].
    pub max_hops: usize,
}

impl RouteQuery {
    pub fn new(from: NodeId, steps: Vec<RouteStep>) -> Self {
        Self {
            from,
            steps,
            to: None,
            valid_at: None,
            limit: 20,
            max_hops: 8,
        }
    }
}

/// A route found: the nodes in order, and the relation taken at each hop.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Route {
    pub nodes: Vec<NodeId>,
    pub hops: Vec<Hop>,
}

/// The routes a query found.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Routes {
    pub from: NodeId,
    pub routes: Vec<Route>,
    /// Whether the search stopped at the limit, or at the hops allowed,
    /// with ways left to follow.
    pub cut: bool,
}

struct Search<'a> {
    read: &'a ReadTx,
    ns: &'a str,
    query: &'a RouteQuery,
    max_hops: usize,
    path: Vec<NodeId>,
    hops: Vec<Hop>,
    found: Vec<Route>,
    cut: bool,
}

impl Search<'_> {
    fn emit(&mut self) {
        if self
            .query
            .to
            .is_some_and(|to| self.path.last() != Some(&to))
        {
            return;
        }
        if self.found.len() >= self.query.limit {
            self.cut = true;
            return;
        }
        self.found.push(Route {
            nodes: self.path.clone(),
            hops: self.hops.clone(),
        });
    }

    /// At `node`, about to take step `at`; `taken` says whether the step
    /// (a repeated one) was taken already.
    fn go(&mut self, node: NodeId, at: usize, taken: bool) -> Result<()> {
        if at == self.query.steps.len() {
            self.emit();
            return Ok(());
        }
        let step = self.query.steps[at].clone();
        let last = at + 1 == self.query.steps.len();
        // A repeated step taken once may end here, unless it is the last
        // and nothing is aimed at: then only the longest line is a route.
        if step.repeat && taken && (!last || self.query.to.is_some()) {
            self.go(node, at + 1, false)?;
        }
        if self.hops.len() >= self.max_hops {
            if step.repeat && taken && last && self.query.to.is_none() {
                self.emit();
            }
            self.cut = true;
            return Ok(());
        }
        let mut filter = EdgeFilter::any().label(&step.label);
        if let Some(at) = self.query.valid_at {
            filter = filter.valid_at(at);
        }
        let direction = if step.backward {
            Direction::In
        } else {
            Direction::Out
        };
        let mut extended = false;
        let neighbors = self.read.neighbors(self.ns, node, direction, &filter)?;
        if neighbors.len() > FANOUT {
            self.cut = true;
        }
        for neighbor in neighbors.into_iter().take(FANOUT) {
            if self.path.contains(&neighbor.node) {
                continue;
            }
            extended = true;
            let name = match self.read.node(self.ns, neighbor.node)? {
                Some(other) => crate::history::name_of(self.read, self.ns, &other)?,
                None => neighbor.node.to_string(),
            };
            self.path.push(neighbor.node);
            self.hops.push(Hop {
                edge: neighbor.edge.id,
                label: neighbor.edge.label.clone(),
                forward: !step.backward,
                node: neighbor.node,
                name,
            });
            if step.repeat {
                self.go(neighbor.node, at, true)?;
            } else {
                self.go(neighbor.node, at + 1, false)?;
            }
            self.path.pop();
            self.hops.pop();
        }
        if !extended && step.repeat && taken && last && self.query.to.is_none() {
            self.emit();
        }
        Ok(())
    }
}

/// The routes from `query.from` that follow its steps in order: each step
/// a relation, forwards or backwards, once or as far as it goes. A node is
/// visited once per route. With `to`, only the routes that end there.
pub fn route(read: &ReadTx, ns: &str, query: &RouteQuery) -> Result<Routes> {
    if query.steps.is_empty() {
        return Err(grafeno::Error::Invalid(
            "a route needs at least one step".into(),
        ));
    }
    if query.steps.iter().any(|step| step.label.trim().is_empty()) {
        return Err(grafeno::Error::Invalid("a step needs a label".into()));
    }
    if read.node(ns, query.from)?.is_none() {
        return Err(grafeno::Error::NodeNotFound(query.from));
    }
    let mut search = Search {
        read,
        ns,
        query,
        max_hops: query.max_hops.clamp(1, MAX_ROUTE_HOPS),
        path: vec![query.from],
        hops: Vec::new(),
        found: Vec::new(),
        cut: false,
    };
    search.go(query.from, 0, false)?;
    Ok(Routes {
        from: query.from,
        routes: search.found,
        cut: search.cut,
    })
}

/// A node's context, rebuilt: what it is, where it is kept, what it is
/// related to now, how it changed, the versions it belongs to, and what is
/// known and not known around it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reconstruction {
    pub id: NodeId,
    pub name: String,
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Where it is anchored, in every palace.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<Place>,
    /// The facts that hold now, palaces' own edges left out.
    pub facts: Vec<StateFact>,
    /// The latest moments of its timeline, oldest first.
    pub timeline: Vec<Moment>,
    /// The line of versions it belongs to, when something replaced it or
    /// it replaced something.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<History>,
    pub justification: Justification,
    pub reflection: Reflection,
}

/// Rebuilds the context of a memory, an entity, a decision or a room in
/// one call: its places, current facts, timeline (the latest `moments`),
/// versions, justification and reflection. A palace or a locus is refused:
/// map it instead.
pub fn reconstruct(read: &ReadTx, ns: &str, id: NodeId, moments: usize) -> Result<Reconstruction> {
    let node: Node = read.node(ns, id)?.ok_or(grafeno::Error::NodeNotFound(id))?;
    if node.has_label(PALACE) || node.has_label(LOCUS) {
        return Err(grafeno::Error::Invalid(format!(
            "{id} is a palace or a locus: map it, or reconstruct a room or a memory"
        )));
    }
    let scope = if node.has_label(ROOM) {
        Scope::Room(id)
    } else {
        Scope::Node(id)
    };
    let now = Timestamp::now();
    let mut facts = Vec::new();
    let mut seen = HashSet::new();
    for neighbor in read.neighbors(ns, id, Direction::Both, &EdgeFilter::any().valid_at(now))? {
        let edge = neighbor.edge;
        if [ANCHORED_AT, HAS_ROOM, HAS_LOCUS].contains(&edge.label.as_str())
            || !seen.insert(edge.id)
        {
            continue;
        }
        facts.push(crate::history::state_fact(read, ns, &edge)?);
    }
    let history = crate::history::history(read, ns, id)?;
    Ok(Reconstruction {
        id,
        name: crate::history::name_of(read, ns, &node)?,
        labels: node.labels.clone(),
        text: read.text(ns, id)?,
        places: crate::palace::anchors_of(read, ns, id)?,
        facts,
        timeline: crate::timeline(read, ns, id, moments)?,
        history: (history.versions.len() > 1).then_some(history),
        justification: crate::knowledge::justify(read, ns, &node)?,
        reflection: crate::reflect(read, ns, &scope, &ReflectOptions::default())?,
    })
}
