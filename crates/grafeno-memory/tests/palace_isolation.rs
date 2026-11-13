//! A palace arranges memories and is not one: recall, context,
//! communities, maintenance and extraction answer the same over a
//! namespace with a palace as over the same memories without it.

use std::collections::BTreeSet;

use grafeno::{Direction, EdgeFilter, Grafeno, NewEdge, NewNode, NodeId, Timestamp};
use grafeno_memory::palace::{ENTRANCE, is_structure};
use grafeno_memory::{
    COMMUNITY, CommunityOptions, ConsolidationOptions, ContextOptions, DecayOptions,
    DuplicateOptions, Expansion, ExtractOptions, MENTIONS, NewRoom, RecallQuery, anchor,
    community_members, consolidation_groups, context_for, create_palace, create_room, extract,
    find_duplicates, find_locus, recall, refresh_communities, weak_memories,
};

const NS: &str = "agent";

struct Memories {
    rust: NodeId,
    lifetimes: NodeId,
    pasta: NodeId,
    trip: NodeId,
}

/// Notes on three subjects, the people who wrote them and a project.
fn memories(db: &Grafeno, ns: &str) -> Memories {
    db.write(|tx| {
        let note = |tx: &mut grafeno::WriteTx, title: &str, text: &str| {
            tx.create_node(ns, NewNode::new("Note").prop("title", title).text(text))
        };
        let rust = note(
            tx,
            "Rust notes",
            "Rust ownership rules and the borrow checker",
        )?;
        let lifetimes = note(
            tx,
            "Lifetimes",
            "Rust references must not outlive the data they borrow",
        )?;
        let pasta = note(tx, "Pasta", "Boil the pasta in salted water")?;
        let trip = note(tx, "Lisbon", "The trip to Lisbon is in March")?;
        let ana = tx.create_node(ns, NewNode::new("Person").prop("name", "Ana"))?;
        let bob = tx.create_node(ns, NewNode::new("Person").prop("name", "Bob"))?;
        let project = tx.create_node(ns, NewNode::new("Project").prop("name", "Compiler"))?;
        tx.create_edge(ns, NewEdge::new(ana, "wrote", rust))?;
        tx.create_edge(ns, NewEdge::new(ana, "wrote", lifetimes))?;
        tx.create_edge(ns, NewEdge::new(rust, "about", project))?;
        tx.create_edge(ns, NewEdge::new(lifetimes, "about", project))?;
        tx.create_edge(ns, NewEdge::new(bob, "wrote", pasta))?;
        tx.create_edge(ns, NewEdge::new(bob, "wrote", trip))?;
        Ok(Memories {
            rust,
            lifetimes,
            pasta,
            trip,
        })
    })
    .unwrap()
}

/// Two palaces with a room of the same name, named and described with the
/// words of the notes, and every note anchored in the first one.
fn build_palace(db: &Grafeno, ns: &str, m: &Memories) {
    db.write(|tx| {
        let work = create_palace(
            tx,
            ns,
            "Rust",
            Some("Rust ownership and the borrow checker"),
        )?
        .id;
        let home = create_palace(tx, ns, "Home", Some("Pasta and trips"))?.id;
        let room = create_room(
            tx,
            ns,
            work,
            NewRoom::new("Compiler")
                .topic("Rust ownership rules, borrow checker, pasta")
                .locus("Rust notes"),
        )?
        .id;
        create_room(tx, ns, home, NewRoom::new("Compiler").topic("Lisbon"))?;
        Ok(room)
    })
    .and_then(|room| {
        let read = db.begin_read()?;
        let entrance = find_locus(&read, ns, room, ENTRANCE)?.unwrap();
        let notes = find_locus(&read, ns, room, "Rust notes")?.unwrap();
        drop(read);
        db.write(|tx| {
            for memory in [m.rust, m.lifetimes, m.pasta, m.trip] {
                anchor(tx, ns, memory, entrance, None)?;
            }
            anchor(tx, ns, m.rust, notes, Some("the first one"))?;
            Ok(())
        })
    })
    .unwrap();
}

fn later() -> Timestamp {
    Timestamp::from_micros(Timestamp::now().as_micros() + 400 * 86_400 * 1_000_000)
}

/// What the read-only memory features answer.
#[derive(Debug, PartialEq)]
struct Seen {
    recalled: Vec<Vec<NodeId>>,
    context: Vec<(Vec<NodeId>, Vec<String>)>,
    duplicates: Vec<(NodeId, NodeId)>,
    weak: Vec<NodeId>,
    groups: Vec<(NodeId, Vec<NodeId>)>,
}

fn see(db: &Grafeno, ns: &str) -> Seen {
    let read = db.begin_read().unwrap();
    let queries = [
        RecallQuery::new().text("rust ownership borrow checker"),
        RecallQuery::new().text("pasta"),
        RecallQuery::new().text("compiler"),
        RecallQuery::new().text("entrance"),
        RecallQuery::new().text("rust").label("Note"),
        RecallQuery::new()
            .text("rust ownership")
            .expand(Some(Expansion {
                depth: 3,
                ..Expansion::default()
            })),
        RecallQuery::new().text("lisbon").expand(Some(Expansion {
            depth: 2,
            hubs: 0.0,
            ..Expansion::default()
        })),
    ];
    let recalled = queries
        .iter()
        .map(|query| {
            recall(&read, ns, query)
                .unwrap()
                .into_iter()
                .map(|hit| hit.node.id)
                .collect()
        })
        .collect();
    let context = queries
        .iter()
        .map(|query| {
            let context =
                context_for(&read, ns, query, &ContextOptions::with_budget(2_000)).unwrap();
            (
                context.items.iter().map(|item| item.node).collect(),
                context.facts.iter().map(|fact| fact.text.clone()).collect(),
            )
        })
        .collect();
    let mut duplicates: Vec<(NodeId, NodeId)> = find_duplicates(
        &read,
        ns,
        &DuplicateOptions {
            min_score: 0.0,
            ..DuplicateOptions::default()
        },
    )
    .unwrap()
    .into_iter()
    .map(|pair| (pair.a.id.min(pair.b.id), pair.a.id.max(pair.b.id)))
    .collect();
    duplicates.sort();
    let weak = weak_memories(
        &read,
        ns,
        &DecayOptions {
            threshold: 1.0,
            limit: 1_000,
            now: later(),
            ..DecayOptions::default()
        },
    )
    .unwrap()
    .into_iter()
    .map(|weak| weak.node.id)
    .collect();
    let mut groups: Vec<(NodeId, Vec<NodeId>)> = consolidation_groups(
        &read,
        ns,
        &ConsolidationOptions {
            older_than_secs: 0,
            min_size: 2,
            limit: 1_000,
            now: later(),
            ..ConsolidationOptions::default()
        },
    )
    .unwrap()
    .into_iter()
    .map(|group| {
        let mut memories: Vec<NodeId> = group.memories.iter().map(|node| node.id).collect();
        memories.sort();
        (group.entity.id, memories)
    })
    .collect();
    groups.sort();
    Seen {
        recalled,
        context,
        duplicates,
        weak,
        groups,
    }
}

#[test]
fn recall_context_and_maintenance_are_the_same_with_a_palace() {
    let db = Grafeno::in_memory().unwrap();
    let m = memories(&db, NS);
    let before = see(&db, NS);
    // The fixture gives every feature something to answer.
    assert!(
        before
            .recalled
            .iter()
            .filter(|hits| !hits.is_empty())
            .count()
            >= 5
    );
    assert!(before.context.iter().any(|(_, facts)| !facts.is_empty()));
    assert_eq!(before.weak.len(), 7);
    assert_eq!(before.groups.len(), 3, "{:?}", before.groups);

    build_palace(&db, NS, &m);
    let after = see(&db, NS);
    assert_eq!(after, before);
}

#[test]
fn rooms_and_loci_are_recalled_when_asked_for_by_label() {
    let db = Grafeno::in_memory().unwrap();
    let m = memories(&db, NS);
    build_palace(&db, NS, &m);
    let read = db.begin_read().unwrap();

    let hits = recall(&read, NS, &RecallQuery::new().text("rust")).unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|hit| !is_structure(&hit.node)));

    let rooms = recall(
        &read,
        NS,
        &RecallQuery::new().text("compiler").label("Room"),
    )
    .unwrap();
    assert_eq!(rooms.len(), 2);
    assert!(rooms.iter().all(|hit| hit.node.has_label("Room")));
    let loci = recall(
        &read,
        NS,
        &RecallQuery::new().text("rust notes").label("Locus"),
    )
    .unwrap();
    // The locus named so first; others of its room may follow it.
    assert_eq!(loci[0].node.prop("name").unwrap(), "Rust notes");
    assert!(loci.iter().all(|hit| hit.node.has_label("Locus")));
}

#[test]
fn communities_are_the_same_with_a_palace() {
    let groups = |db: &Grafeno| {
        let read = db.begin_read().unwrap();
        let mut groups: Vec<BTreeSet<NodeId>> = read
            .nodes_with_label(NS, COMMUNITY)
            .unwrap()
            .into_iter()
            .map(|id| {
                community_members(&read, NS, id, 1_000)
                    .unwrap()
                    .members
                    .into_iter()
                    .map(|node| node.id)
                    .collect()
            })
            .collect();
        groups.sort();
        groups
    };
    let options = CommunityOptions {
        min_size: 2,
        ..CommunityOptions::default()
    };

    let db = Grafeno::in_memory().unwrap();
    let m = memories(&db, NS);
    let first = refresh_communities(&db, NS, &options).unwrap();
    let before = groups(&db);
    assert_eq!(first.nodes, 7);
    assert!(before.len() >= 2, "{before:?}");

    build_palace(&db, NS, &m);
    let second = refresh_communities(&db, NS, &options).unwrap();
    assert_eq!(second.nodes, first.nodes);
    assert_eq!(second.communities, first.communities);
    assert_eq!((second.created, second.removed), (0, 0));
    assert_eq!(groups(&db), before);
}

#[test]
fn extraction_reads_no_palace_and_links_to_none() {
    let db = Grafeno::in_memory().unwrap();
    let texts = [
        "Grafeno keeps one writer at a time",
        "The storage of Grafeno is a single file",
        "Ana works on Grafeno with Bob",
    ];
    let fill = |ns: &str| {
        db.write(|tx| {
            tx.create_node(ns, NewNode::new("Person").prop("name", "Ana"))?;
            texts
                .iter()
                .map(|text| tx.create_node(ns, NewNode::new("Memory").text(*text)))
                .collect::<grafeno::Result<Vec<NodeId>>>()
        })
        .unwrap()
    };
    let plain = fill("plain");
    let housed = fill("housed");
    // A palace, a room and a locus named as what the texts mention.
    db.write(|tx| {
        let palace = create_palace(tx, "housed", "Grafeno", Some("Ana and Bob work here"))?.id;
        let room = create_room(tx, "housed", palace, NewRoom::new("Grafeno").locus("Ana"))?.id;
        let _ = room;
        Ok(())
    })
    .unwrap();
    let room = {
        let read = db.begin_read().unwrap();
        let palace = grafeno_memory::find_palace(&read, "housed", "Grafeno")
            .unwrap()
            .unwrap();
        let room = grafeno_memory::find_room(&read, "housed", palace, "Grafeno")
            .unwrap()
            .unwrap();
        find_locus(&read, "housed", room, ENTRANCE)
            .unwrap()
            .unwrap()
    };
    db.write(|tx| {
        for memory in &housed {
            anchor(tx, "housed", *memory, room, None)?;
        }
        Ok(())
    })
    .unwrap();

    // Memories are read and every other named node is an entity: the
    // room named Grafeno and the locus named Ana are not.
    let a = extract(&db, "plain", &ExtractOptions::default()).unwrap();
    let b = extract(&db, "housed", &ExtractOptions::default()).unwrap();
    assert_eq!(b, a);
    assert_eq!(a.read, 3);
    assert!(a.linked > 0, "{a:?}");

    let mentions = |ns: &str, sources: &[NodeId]| {
        let read = db.begin_read().unwrap();
        let mut out = Vec::new();
        for (index, source) in sources.iter().enumerate() {
            for neighbor in read
                .neighbors(
                    ns,
                    *source,
                    Direction::Out,
                    &EdgeFilter::any().label(MENTIONS),
                )
                .unwrap()
            {
                let node = read.node(ns, neighbor.node).unwrap().unwrap();
                assert!(!is_structure(&node), "a mention of {node:?}");
                out.push((index, node.labels.clone(), node.display_name()));
            }
        }
        out.sort();
        out
    };
    let found = mentions("plain", &plain);
    assert!(!found.is_empty());
    assert_eq!(mentions("housed", &housed), found);

    // Every node with a text is a source: the palace's texts are not read.
    let options = ExtractOptions {
        source_labels: Vec::new(),
        force: true,
        ..ExtractOptions::default()
    };
    let a = extract(&db, "plain", &options).unwrap();
    let b = extract(&db, "housed", &options).unwrap();
    assert_eq!(b, a);
    assert_eq!(a.read, 3);
    assert_eq!(mentions("housed", &housed), mentions("plain", &plain));
}
