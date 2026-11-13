//! A node's timeline: what happened to it and around it, oldest first —
//! when it was recorded, the facts it is part of (when each began to hold,
//! stopped holding and was recorded; facts taken back as mistakes are left
//! out, as they never held), the values
//! its properties had before, the merges it took part in, and the
//! contradictions its facts ran into.

use grafeno::{Direction, Edge, EdgeFilter, EdgeId, NodeId, ReadTx, Result, Timestamp};
use serde::Serialize;

/// One moment of a timeline.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Moment {
    pub at: Timestamp,
    /// `created`, `updated`, `fact_began`, `fact_ended`, `fact_recorded`,
    /// `property`, `merged`, `contradiction`.
    pub kind: &'static str,
    /// What happened, in a sentence.
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fact: Option<EdgeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// A node's name, or the start of its text, quoted, when it has none (a
/// chunk, a memory).
fn name(read: &ReadTx, ns: &str, id: NodeId) -> Result<String> {
    let Some(node) = read.node(ns, id)? else {
        return Ok(id.to_string());
    };
    if node.explicit_name().is_none()
        && let Some(text) = read.text(ns, id)?
    {
        let line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let mut short: String = line.chars().take(60).collect();
        if line.chars().count() > 60 {
            short.push('…');
        }
        return Ok(format!("“{short}”"));
    }
    Ok(node.display_name())
}

fn sentence(read: &ReadTx, ns: &str, edge: &Edge) -> Result<String> {
    Ok(format!(
        "{} {} {}",
        name(read, ns, edge.from)?,
        edge.label,
        name(read, ns, edge.to)?
    ))
}

/// The timeline of `node`, oldest first, at most `limit` moments (the
/// latest kept).
pub fn timeline(read: &ReadTx, ns: &str, node: NodeId, limit: usize) -> Result<Vec<Moment>> {
    let Some(record) = read.node(ns, node)? else {
        return Err(grafeno::Error::NodeNotFound(node));
    };
    let mut moments = vec![Moment {
        at: record.created_at,
        kind: "created",
        text: format!("{} was recorded", record.display_name()),
        fact: None,
        by: record.recorded_by.clone(),
    }];
    if record.updated_at > record.created_at {
        moments.push(Moment {
            at: record.updated_at,
            kind: "updated",
            text: format!("{} was last changed", record.display_name()),
            fact: None,
            by: None,
        });
    }

    // Facts, those that ended included.
    {
        for neighbor in read.neighbors(ns, node, Direction::Both, &EdgeFilter::any())? {
            let edge = neighbor.edge;
            let said = sentence(read, ns, &edge)?;
            moments.push(Moment {
                at: edge.recorded_at,
                kind: "fact_recorded",
                text: format!("recorded: {said}"),
                fact: Some(edge.id),
                by: edge.recorded_by.clone(),
            });
            if let Some(from) = edge.valid_from {
                moments.push(Moment {
                    at: from,
                    kind: "fact_began",
                    text: format!("began: {said}"),
                    fact: Some(edge.id),
                    by: None,
                });
            }
            if let Some(to) = edge.valid_to {
                moments.push(Moment {
                    at: to,
                    kind: "fact_ended",
                    text: format!("ended: {said}"),
                    fact: Some(edge.id),
                    by: None,
                });
            }
        }
    }

    // Values its properties had before.
    for key in record.props.keys() {
        for entry in read.property_history(ns, node, key)? {
            moments.push(Moment {
                at: entry.to,
                kind: "property",
                text: format!("{key} stopped being {}", entry.value),
                fact: None,
                by: entry.by.clone(),
            });
        }
    }

    // Merges it took part in.
    for merge in read.merges(ns, Some(node))? {
        let other = if merge.kept == node {
            merge.merged_before.node.display_name()
        } else {
            name(read, ns, merge.kept)?
        };
        let text = if merge.kept == node {
            format!("{other} was merged into it")
        } else {
            format!("it was merged into {other}")
        };
        moments.push(Moment {
            at: merge.at,
            kind: "merged",
            text,
            fact: None,
            by: merge.by.clone(),
        });
    }

    // Contradictions its facts ran into.
    for event in read.constraint_events(ns, 10_000)? {
        let mine = |id: EdgeId| -> Result<Option<Edge>> {
            Ok(read
                .edge(ns, id)?
                .filter(|edge| edge.from == node || edge.to == node))
        };
        let (Some(new), old) = (mine(event.new_edge)?, read.edge(ns, event.old_edge)?) else {
            continue;
        };
        let old = match old {
            Some(old) => sentence(read, ns, &old)?,
            None => "an earlier fact".to_owned(),
        };
        moments.push(Moment {
            at: event.recorded_at,
            kind: "contradiction",
            text: format!(
                "{} contradicted {old} under {} ({})",
                sentence(read, ns, &new)?,
                event.constraint,
                serde_json::to_value(event.action)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
            fact: Some(event.new_edge),
            by: event.by.clone(),
        });
    }

    moments.sort_by(|a, b| a.at.cmp(&b.at).then(a.kind.cmp(b.kind)));
    if moments.len() > limit {
        moments.drain(..moments.len() - limit);
    }
    Ok(moments)
}
