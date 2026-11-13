//! Checkpoints: where a task stands, saved by an agent so that it, or
//! another one, can take the task up again in a later session.
//!
//! A checkpoint is a node labeled [`CHECKPOINT`] with what the task is,
//! a summary, what is done and what comes next, the decisions taken and
//! the files touched. It follows the task's previous checkpoint
//! ([`FOLLOWS`]), points at the node the task is about ([`ABOUT`]) and
//! notes the change log's position, so [`resume`] can say what was written
//! since.

use grafeno::{
    Direction, EdgeFilter, NewEdge, NewNode, Node, NodeId, ReadTx, Result, Timestamp, WriteTx,
};
use serde::Serialize;
use serde_json::{Value, json};

/// The label of checkpoints.
pub const CHECKPOINT: &str = "Checkpoint";
/// A checkpoint → the previous checkpoint of the same task.
pub const FOLLOWS: &str = "follows";
/// A checkpoint → the node its task is about (a project, a document).
pub const ABOUT: &str = "about";

/// What to save.
#[derive(Clone, Debug, Default)]
pub struct NewCheckpoint {
    /// What the work is; checkpoints of one task share it.
    pub task: String,
    /// Where it stands, in a few sentences.
    pub summary: String,
    pub done: Vec<String>,
    pub next: Vec<String>,
    pub decisions: Vec<String>,
    pub files: Vec<String>,
    /// The node the task is about.
    pub about: Option<NodeId>,
    /// Who saved it.
    pub agent: Option<String>,
    /// The namespace's change log position when it was saved.
    pub change: u64,
}

/// A saved checkpoint.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Checkpoint {
    pub id: NodeId,
    pub task: String,
    pub summary: String,
    pub done: Vec<String>,
    pub next: Vec<String>,
    pub decisions: Vec<String>,
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<NodeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub at: Timestamp,
    pub change: u64,
    /// The checkpoint before it in the same task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<NodeId>,
}

/// Tasks match by their words, whatever the case and spacing.
pub fn task_key(task: &str) -> String {
    task.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn strings(node: &Node, key: &str) -> Vec<String> {
    node.prop(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn of_node(node: &Node, about: Option<NodeId>, previous: Option<NodeId>) -> Checkpoint {
    let text = |key: &str| {
        node.prop(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Checkpoint {
        id: node.id,
        task: text("task"),
        summary: text("summary"),
        done: strings(node, "done"),
        next: strings(node, "next"),
        decisions: strings(node, "decisions"),
        files: strings(node, "files"),
        about,
        agent: node
            .prop("agent")
            .and_then(Value::as_str)
            .map(str::to_owned),
        at: node.created_at,
        change: node.prop("change").and_then(Value::as_u64).unwrap_or(0),
        previous,
    }
}

/// The text a checkpoint is found by, and read as.
fn render(new: &NewCheckpoint) -> String {
    let mut out = format!("Checkpoint of {}: {}", new.task, new.summary);
    for (title, items) in [
        ("Done", &new.done),
        ("Next", &new.next),
        ("Decisions", &new.decisions),
        ("Files", &new.files),
    ] {
        if !items.is_empty() {
            out.push_str(&format!("\n{title}: {}", items.join("; ")));
        }
    }
    out
}

/// The latest checkpoint of `task` in `tx` (any agent's), by time then id.
fn latest_in_tx(tx: &WriteTx, ns: &str, key: &str) -> Result<Option<Node>> {
    let mut best: Option<Node> = None;
    let mut after = None;
    loop {
        let page = tx.nodes_page(ns, Some(CHECKPOINT), after, 1_000)?;
        let Some(last) = page.last() else { break };
        after = Some(last.id);
        for node in page {
            if node.prop("task_key").and_then(Value::as_str) == Some(key)
                && best
                    .as_ref()
                    .is_none_or(|best| (node.created_at, node.id) > (best.created_at, best.id))
            {
                best = Some(node);
            }
        }
    }
    Ok(best)
}

/// Saves a checkpoint, after the task's latest one.
pub fn save_checkpoint(tx: &mut WriteTx, ns: &str, new: NewCheckpoint) -> Result<NodeId> {
    let key = task_key(&new.task);
    if key.is_empty() {
        return Err(grafeno::Error::Invalid("a checkpoint needs a task".into()));
    }
    let previous = latest_in_tx(tx, ns, &key)?.map(|node| node.id);
    let mut node = NewNode::new(CHECKPOINT)
        .prop("task", new.task.clone())
        .prop("task_key", key)
        .prop("summary", new.summary.clone())
        .prop("done", json!(new.done))
        .prop("next", json!(new.next))
        .prop("decisions", json!(new.decisions))
        .prop("files", json!(new.files))
        .prop("change", new.change)
        .text(render(&new));
    if let Some(agent) = &new.agent {
        node = node.prop("agent", agent.clone());
    }
    let id = tx.create_node(ns, node)?;
    if let Some(previous) = previous {
        tx.create_edge(ns, NewEdge::new(id, FOLLOWS, previous))?;
    }
    if let Some(about) = new.about {
        tx.create_edge(ns, NewEdge::new(id, ABOUT, about))?;
    }
    Ok(id)
}

/// The node a checkpoint points at with `label`, if any.
fn target(read: &ReadTx, ns: &str, id: NodeId, label: &str) -> Result<Option<NodeId>> {
    Ok(read
        .neighbor_ids(ns, id, Direction::Out, &EdgeFilter::any().label(label))?
        .first()
        .map(|(_, other)| *other))
}

/// What to resume.
#[derive(Clone, Debug, Default)]
pub struct ResumeQuery {
    /// The task, matched by its words; any task when absent.
    pub task: Option<String>,
    /// Only checkpoints about this node.
    pub about: Option<NodeId>,
    /// Only checkpoints saved by this agent.
    pub agent: Option<String>,
}

/// Every checkpoint that matches, newest first.
pub fn checkpoints(read: &ReadTx, ns: &str, query: &ResumeQuery) -> Result<Vec<Checkpoint>> {
    let key = query.task.as_deref().map(task_key);
    let mut found = Vec::new();
    let mut after = None;
    loop {
        let page = read.nodes_page(ns, Some(CHECKPOINT), after, 1_000)?;
        let Some(last) = page.last() else { break };
        after = Some(last.id);
        for node in page {
            if key
                .as_deref()
                .is_some_and(|key| node.prop("task_key").and_then(Value::as_str) != Some(key))
            {
                continue;
            }
            if query
                .agent
                .as_deref()
                .is_some_and(|agent| node.prop("agent").and_then(Value::as_str) != Some(agent))
            {
                continue;
            }
            let about = target(read, ns, node.id, ABOUT)?;
            if query.about.is_some() && about != query.about {
                continue;
            }
            let previous = target(read, ns, node.id, FOLLOWS)?;
            found.push(of_node(&node, about, previous));
        }
    }
    found.sort_by_key(|checkpoint| std::cmp::Reverse((checkpoint.at, checkpoint.id)));
    Ok(found)
}

/// The latest checkpoint that matches.
pub fn resume(read: &ReadTx, ns: &str, query: &ResumeQuery) -> Result<Option<Checkpoint>> {
    Ok(checkpoints(read, ns, query)?.into_iter().next())
}

/// The latest checkpoint of each task, newest first.
pub fn tasks(read: &ReadTx, ns: &str, query: &ResumeQuery) -> Result<Vec<Checkpoint>> {
    let mut seen = std::collections::HashSet::new();
    Ok(checkpoints(read, ns, query)?
        .into_iter()
        .filter(|checkpoint| seen.insert(task_key(&checkpoint.task)))
        .collect())
}
