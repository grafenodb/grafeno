//! Latency and tokens of retrieval over a palace: recall, context, walks
//! (by distance, by relevance, with a context) and entering a room, over a
//! namespace of synthetic notes in rooms, joined by relations.
//!
//! `cargo run --release -p grafeno-memory --example palace_bench -- 5000`
//!
//! The argument is the number of notes (5 000 by default). They go into 20
//! rooms of 5 loci each, two relations per note to others of its room or a
//! neighbor's, and one door from each room to the next. Each measure is
//! taken 50 times after one warm-up; the median and the 95th percentile
//! are printed, with the tokens of what a context packed.

use std::time::Instant;

use grafeno::{Grafeno, NewEdge, NewNode, NodeId};
use grafeno_memory::{
    ContextOptions, DOOR, EnterOptions, NewRoom, REDUNDANT, RecallQuery, WalkOrder, WalkQuery,
    anchor, context_for, create_palace, create_room, enter, find_locus, recall, walk,
};

const NS: &str = "bench";
const ROOMS: usize = 20;
const LOCI: usize = 5;
const RUNS: usize = 50;

/// Words the notes are made of: a subject per room and common ones.
const SUBJECTS: [&str; ROOMS] = [
    "billing",
    "storage",
    "network",
    "search",
    "deploy",
    "auth",
    "cache",
    "queue",
    "reports",
    "mobile",
    "payments",
    "email",
    "backup",
    "metrics",
    "logging",
    "pricing",
    "support",
    "onboarding",
    "export",
    "schema",
];
const COMMON: [&str; 12] = [
    "service", "error", "latency", "release", "customer", "config", "timeout", "retry", "decision",
    "incident", "owner", "limit",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn timed<T>(mut work: impl FnMut() -> T) -> (f64, f64, T) {
    let mut last = work();
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let started = Instant::now();
        last = work();
        times.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    times.sort_by(f64::total_cmp);
    (times[RUNS / 2], times[RUNS * 95 / 100], last)
}

fn main() -> grafeno::Result<()> {
    let notes: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(5_000);
    let db = Grafeno::in_memory()?;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);

    let started = Instant::now();
    let (rooms, loci) = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Work", None)?.id;
            let mut rooms = Vec::new();
            for subject in SUBJECTS {
                let mut room = NewRoom::new(subject).topic(format!("{subject} work"));
                for locus in 1..LOCI {
                    room = room.locus(format!("{subject} part {locus}"));
                }
                rooms.push(create_room(tx, NS, palace, room)?.id);
            }
            for pair in rooms.windows(2) {
                tx.create_edge(NS, NewEdge::new(pair[0], DOOR, pair[1]))?;
            }
            Ok(rooms)
        })
        .and_then(|rooms| {
            let read = db.begin_read()?;
            let mut loci = Vec::new();
            for (room, subject) in rooms.iter().zip(SUBJECTS) {
                let mut here = vec![find_locus(&read, NS, *room, "entrance")?.unwrap()];
                for locus in 1..LOCI {
                    here.push(
                        find_locus(&read, NS, *room, &format!("{subject} part {locus}"))?.unwrap(),
                    );
                }
                loci.push(here);
            }
            Ok((rooms, loci))
        })?;
    let mut ids: Vec<(usize, NodeId)> = Vec::with_capacity(notes);
    for batch in (0..notes).collect::<Vec<_>>().chunks(1_000) {
        let made = db.write(|tx| {
            let mut made = Vec::new();
            for &i in batch {
                let room = i % ROOMS;
                let words: Vec<&str> = (0..8).map(|_| COMMON[rng.below(COMMON.len())]).collect();
                let text = format!(
                    "Note {i} on {}: {} {}",
                    SUBJECTS[room],
                    words.join(" "),
                    SUBJECTS[(room + 1) % ROOMS]
                );
                let id = tx.create_node(NS, NewNode::new("Note").text(text))?;
                anchor(tx, NS, id, loci[room][i % LOCI], None)?;
                made.push((room, id));
            }
            Ok(made)
        })?;
        ids.extend(made);
    }
    db.write(|tx| {
        for (at, (room, id)) in ids.iter().enumerate() {
            for _ in 0..2 {
                let near = (room + rng.below(2)) % ROOMS;
                let other = ids[(near + ROOMS * rng.below(notes / ROOMS)) % notes].1;
                if other != *id {
                    tx.create_edge(NS, NewEdge::new(*id, "relates_to", other))?;
                }
            }
            let _ = at;
        }
        Ok(())
    })?;
    println!(
        "{notes} notes in {ROOMS} rooms, built in {:.2} s",
        started.elapsed().as_secs_f64()
    );
    println!();
    println!("| What | Median (ms) | p95 (ms) | Returned | Tokens |");
    println!("| --- | ---: | ---: | ---: | ---: |");

    let read = db.begin_read()?;
    let topic = "billing timeout incident";
    let row = |what: &str, (median, p95): (f64, f64), returned: usize, tokens: Option<usize>| {
        println!(
            "| {what} | {median:.2} | {p95:.2} | {returned} | {} |",
            tokens.map_or("—".into(), |t| t.to_string())
        );
    };

    let (m, p, found) = timed(|| recall(&read, NS, &RecallQuery::new().text(topic)).unwrap());
    row("`recall`, 10 results", (m, p), found.len(), None);
    let (m, p, context) = timed(|| {
        context_for(
            &read,
            NS,
            &RecallQuery::new().text(topic).limit(20),
            &ContextOptions::with_budget(1_500),
        )
        .unwrap()
    });
    row(
        "`context`, 1 500 tokens",
        (m, p),
        context.items.len(),
        Some(context.tokens),
    );
    let (m, p, context) = timed(|| {
        let mut options = ContextOptions::with_budget(1_500);
        options.redundancy = Some(REDUNDANT);
        context_for(
            &read,
            NS,
            &RecallQuery::new().text(topic).limit(20),
            &options,
        )
        .unwrap()
    });
    row(
        "`context`, 1 500 tokens, redundancy left out",
        (m, p),
        context.items.len(),
        Some(context.tokens),
    );
    for (what, query) in [
        ("`palace_walk` by distance, 2 hops", WalkQuery::topic(topic)),
        (
            "`palace_walk` by relevance, 2 hops",
            WalkQuery::topic(topic).order(WalkOrder::Relevance),
        ),
        (
            "`palace_walk` by relevance, 2 hops, 1 500 tokens",
            WalkQuery::topic(topic)
                .order(WalkOrder::Relevance)
                .budget(1_500),
        ),
        (
            "`palace_walk` by distance, 3 hops",
            WalkQuery::topic(topic).hops(3),
        ),
    ] {
        let (m, p, found) = timed(|| walk(&read, NS, &query).unwrap());
        row(
            what,
            (m, p),
            found.memories.len(),
            found.context.as_ref().map(|c| c.tokens),
        );
    }
    let (m, p, entered) =
        timed(|| enter(&read, NS, rooms[0], &EnterOptions::with_budget(1_500)).unwrap());
    row(
        &format!(
            "`palace_enter`, a room of {} notes, 1 500 tokens",
            entered.room.memories
        ),
        (m, p),
        entered.context.items.len(),
        Some(entered.context.tokens),
    );
    Ok(())
}
