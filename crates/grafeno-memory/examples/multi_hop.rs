//! An answer that only the graph can find. Ana belongs to the Platform team,
//! the team works on Atlas, and Atlas made a decision whose text names
//! neither Ana nor her team. Asked what Ana's team decided, text search
//! finds Ana and the teams; recall follows their facts to the decision.
//! The Mobile team has a project and a decision of its own, and everyone
//! is part of Acme, a hub linked to 200 more people.
//!
//! Run with `cargo run -p grafeno-memory --example multi_hop`.

use grafeno::{Grafeno, NewEdge, NewNode, NodeId, ReadTx};
use grafeno_memory::{Expansion, RecallQuery, Recalled, Signal, recall};

pub const NS: &str = "memory";
pub const QUESTION: &str = "What did Ana's team decide about service communication?";

pub struct Memory {
    pub db: Grafeno,
    pub decision: NodeId,
    pub other_decision: NodeId,
    pub acme: NodeId,
}

/// The memory: a chain from Ana to the decision, another team with its own
/// decision, and a company everyone is linked to.
pub fn memory() -> grafeno::Result<Memory> {
    let db = Grafeno::in_memory()?;
    let (decision, other_decision, acme) = db.write(|tx| {
        let node =
            |label: &str, name: &str, text: &str| NewNode::new(label).prop("name", name).text(text);
        let ana = tx.create_node(NS, node("Person", "Ana", "Ana Ruiz, backend engineer"))?;
        let platform = tx.create_node(
            NS,
            node(
                "Team",
                "Platform team",
                "Platform team, owner of checkout and billing",
            ),
        )?;
        let atlas = tx.create_node(
            NS,
            node(
                "Project",
                "Atlas",
                "Atlas splits checkout into smaller parts",
            ),
        )?;
        let decision = tx.create_node(
            NS,
            node(
                "Decision",
                "Decision 42",
                "Event-driven messaging between the backends replaces synchronous calls",
            ),
        )?;
        tx.create_edge(NS, NewEdge::new(ana, "member_of", platform))?;
        tx.create_edge(NS, NewEdge::new(platform, "works_on", atlas))?;
        tx.create_edge(NS, NewEdge::new(atlas, "decided", decision))?;

        let mobile = tx.create_node(
            NS,
            node("Team", "Mobile team", "Mobile team, owner of the apps"),
        )?;
        let other_decision = tx.create_node(
            NS,
            node("Decision", "Decision 17", "Weekly releases for both apps"),
        )?;
        let pocket = tx.create_node(NS, node("Project", "Pocket", "Pocket rebuilds the apps"))?;
        tx.create_edge(NS, NewEdge::new(mobile, "works_on", pocket))?;
        tx.create_edge(NS, NewEdge::new(pocket, "decided", other_decision))?;

        let acme = tx.create_node(NS, NewNode::new("Company").prop("name", "Acme"))?;
        for member in [ana, platform, mobile] {
            tx.create_edge(NS, NewEdge::new(member, "part_of", acme))?;
        }
        for n in 0..200 {
            let person = tx.create_node(
                NS,
                NewNode::new("Person").prop("name", format!("Employee {n}")),
            )?;
            tx.create_edge(NS, NewEdge::new(person, "part_of", acme))?;
        }
        Ok((decision, other_decision, acme))
    })?;
    Ok(Memory {
        db,
        decision,
        other_decision,
        acme,
    })
}

/// The question with graph expansion of three hops, or none.
pub fn ask(read: &ReadTx, expand: Option<Expansion>) -> grafeno::Result<Vec<Recalled>> {
    recall(
        read,
        NS,
        &RecallQuery::new().text(QUESTION).limit(8).expand(expand),
    )
}

pub fn three_hops() -> Expansion {
    Expansion {
        depth: 3,
        ..Expansion::default()
    }
}

#[allow(dead_code)]
fn main() -> grafeno::Result<()> {
    let memory = memory()?;
    let read = memory.db.begin_read()?;
    println!("{QUESTION}\n");
    println!("Text search alone:");
    print(&read, &ask(&read, None)?)?;
    println!("\nWith the graph (three hops):");
    print(&read, &ask(&read, Some(three_hops()))?)?;
    println!("\nWith the graph, hubs not damped:");
    let undamped = Expansion {
        hubs: 0.0,
        fanout: 0,
        ..three_hops()
    };
    print(&read, &ask(&read, Some(undamped))?)?;
    Ok(())
}

fn print(read: &ReadTx, results: &[Recalled]) -> grafeno::Result<()> {
    for (rank, result) in results.iter().enumerate() {
        let name = |id: NodeId| -> grafeno::Result<String> {
            Ok(read
                .node(NS, id)?
                .and_then(|node| node.explicit_name().map(str::to_owned))
                .unwrap_or_default())
        };
        let mut why = Vec::new();
        for signal in &result.signals {
            match signal {
                Signal::Text { rank, .. } => why.push(format!("text #{rank}")),
                Signal::Vector { rank, .. } => why.push(format!("vector #{rank}")),
                Signal::Graph { seed, hops } => {
                    let unit = if *hops == 1 { "hop" } else { "hops" };
                    why.push(format!("{hops} {unit} from {}", name(*seed)?))
                }
                Signal::Memory { .. } => {}
            }
        }
        println!(
            "{}. {:<14} {:.4}  {}",
            rank + 1,
            result.node.explicit_name().unwrap_or_default(),
            result.score,
            why.join(", ")
        );
    }
    Ok(())
}
