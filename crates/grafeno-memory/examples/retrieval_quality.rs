//! Retrieval quality on questions that need four memories joined: an
//! incident, the service it hit, the database that service depends on, and
//! who maintains that database. Each question names only the incident.
//!
//! `cargo run --release -p grafeno-memory --example retrieval_quality -- 200`
//!
//! The argument is the number of incidents (200 by default); each has its
//! own chain, and every service and database also has notes that share its
//! words, as distractors. Measured for each way of retrieving: how often
//! the whole chain is in what comes back, the share of the chain found,
//! the latency and the tokens of the context.

use std::time::Instant;

use grafeno::{Fusion, Grafeno, NewEdge, NewNode, NodeId};
use grafeno_memory::{
    ContextOptions, Expansion, NewRoom, RecallQuery, WalkOrder, WalkQuery, anchor, context_for,
    create_palace, create_room, find_locus, walk,
};

const NS: &str = "quality";
const NAMES: [&str; 20] = [
    "orion", "vega", "lyra", "draco", "hydra", "pyxis", "carina", "lupus", "norma", "ara",
    "volans", "dorado", "mensa", "musca", "apus", "pavo", "indus", "grus", "tucana", "phoenix",
];

struct Chain {
    incident: NodeId,
    needed: [NodeId; 4],
    word: String,
}

fn build(db: &Grafeno, incidents: usize) -> grafeno::Result<Vec<Chain>> {
    let room = db.write(|tx| {
        let palace = create_palace(tx, NS, "Operations", None)?.id;
        Ok(create_room(tx, NS, palace, NewRoom::new("Incidents").topic("incidents"))?.id)
    })?;
    let entrance = find_locus(&db.begin_read()?, NS, room, "entrance")?.unwrap();
    let mut chains = Vec::new();
    for batch in (0..incidents).collect::<Vec<_>>().chunks(100) {
        let made = db.write(|tx| {
            let mut made = Vec::new();
            for &i in batch {
                let word = format!("{}{}", NAMES[i % NAMES.len()], i);
                let incident = tx.create_node(
                    NS,
                    NewNode::new("Experience").text(format!(
                        "The {word} outage stopped the night jobs for an hour"
                    )),
                )?;
                let service = tx.create_node(
                    NS,
                    NewNode::new("Service")
                        .prop("name", format!("service {i}"))
                        .text(format!("Service {i} sends the reports of team {}", i % 7)),
                )?;
                let database = tx.create_node(
                    NS,
                    NewNode::new("Database")
                        .prop("name", format!("store {i}"))
                        .text(format!("Store {i} keeps the reports of team {}", i % 7)),
                )?;
                let person = tx.create_node(
                    NS,
                    NewNode::new("Person")
                        .prop("name", format!("person {i}"))
                        .text(format!("Person {i} keeps stores running")),
                )?;
                tx.create_edge(NS, NewEdge::new(incident, "hit", service))?;
                tx.create_edge(NS, NewEdge::new(service, "depends_on", database))?;
                tx.create_edge(NS, NewEdge::new(person, "maintains", database))?;
                // Distractors: notes that share the service's and the
                // store's words, related to them.
                for d in 0..3 {
                    let note = tx.create_node(
                        NS,
                        NewNode::new("Note").text(format!(
                            "Reports of team {} are sent by service {i}, note {d}",
                            i % 7
                        )),
                    )?;
                    tx.create_edge(NS, NewEdge::new(note, "about", service))?;
                }
                anchor(tx, NS, incident, entrance, None)?;
                made.push(Chain {
                    incident,
                    needed: [incident, service, database, person],
                    word,
                });
            }
            Ok(made)
        })?;
        chains.extend(made);
    }
    Ok(chains)
}

struct Score {
    complete: usize,
    found: usize,
    times: Vec<f64>,
    tokens: Vec<usize>,
}

impl Score {
    fn new() -> Self {
        Self {
            complete: 0,
            found: 0,
            times: Vec::new(),
            tokens: Vec::new(),
        }
    }

    fn add(&mut self, chain: &Chain, got: &[NodeId], seconds: f64, tokens: usize) {
        let found = chain.needed.iter().filter(|id| got.contains(id)).count();
        self.found += found;
        if found == chain.needed.len() {
            self.complete += 1;
        }
        self.times.push(seconds * 1_000.0);
        self.tokens.push(tokens);
    }

    fn row(&mut self, what: &str, chains: usize) {
        self.times.sort_by(f64::total_cmp);
        self.tokens.sort_unstable();
        let at = |p: usize| self.times[(self.times.len() * p / 100).min(self.times.len() - 1)];
        println!(
            "| {what} | {:.0} % | {:.0} % | {:.2} | {:.2} | {} |",
            100.0 * self.complete as f64 / chains as f64,
            100.0 * self.found as f64 / (chains * 4) as f64,
            at(50),
            at(95),
            self.tokens[self.tokens.len() / 2],
        );
    }
}

fn main() -> grafeno::Result<()> {
    let incidents: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(200);
    let db = Grafeno::in_memory()?;
    let chains = build(&db, incidents)?;
    let read = db.begin_read()?;
    println!("{incidents} incidents, {} memories", read.stats(NS)?.nodes);
    println!();
    println!("| Way | Whole chain | Chain found | p50 ms | p95 ms | Tokens (median) |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    let budget = 1_500;
    for (depth, fusion) in [
        (1, Fusion::default()),
        (2, Fusion::default()),
        (3, Fusion::default()),
        (3, Fusion::Weighted { text: 1.0 }),
    ] {
        let mut score = Score::new();
        for chain in &chains {
            let question = format!("who keeps the store behind the {} outage", chain.word);
            let mut query = RecallQuery::new().text(question).limit(20);
            query.fusion = fusion;
            query.expand = Some(Expansion {
                depth,
                ..Expansion::default()
            });
            let started = Instant::now();
            let context = context_for(&read, NS, &query, &ContextOptions::with_budget(budget))?;
            let seconds = started.elapsed().as_secs_f64();
            let got: Vec<NodeId> = context.items.iter().map(|item| item.node).collect();
            score.add(chain, &got, seconds, context.tokens);
        }
        let fused = match fusion {
            Fusion::Rank { .. } => "rank fusion",
            Fusion::Weighted { .. } => "scores scaled",
        };
        score.row(
            &format!("`context`, recall at depth {depth}, {fused}"),
            chains.len(),
        );
    }
    for (what, words, order, relations) in [
        (
            "`palace_walk` from the question's words, by relevance, 3 hops",
            true,
            WalkOrder::Relevance,
            Vec::new(),
        ),
        (
            "`palace_walk` from the question's words, by distance, 3 hops",
            true,
            WalkOrder::Distance,
            Vec::new(),
        ),
        (
            "`palace_walk` from the incident, 3 hops",
            false,
            WalkOrder::Distance,
            Vec::new(),
        ),
        (
            "`palace_walk` from the incident, 3 hops, only `hit`, `depends_on`, `maintains`",
            false,
            WalkOrder::Distance,
            vec!["hit", "depends_on", "maintains"],
        ),
    ] {
        let mut score = Score::new();
        for chain in &chains {
            let mut query = if words {
                WalkQuery::topic(format!(
                    "who keeps the store behind the {} outage",
                    chain.word
                ))
            } else {
                WalkQuery::from(chain.incident)
            }
            .hops(3)
            .order(order)
            .budget(budget);
            for label in &relations {
                query = query.relation(*label);
            }
            let started = Instant::now();
            let found = walk(&read, NS, &query)?;
            let seconds = started.elapsed().as_secs_f64();
            let context = found.context.unwrap_or_default();
            let got: Vec<NodeId> = context.items.iter().map(|item| item.node).collect();
            score.add(chain, &got, seconds, context.tokens);
        }
        score.row(what, chains.len());
    }
    Ok(())
}
