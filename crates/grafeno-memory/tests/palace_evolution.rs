//! Phase 4 of the plan: consolidation and evolution. A review proposes how
//! to arrange a palace and changes nothing; a room or a palace is removed
//! with what is under it and never a memory; newer memories that may
//! overtake older ones are listed; a consolidation is undone; successive
//! facts keep their times and who recorded them.

use grafeno::{
    Actor, Constraint, Direction, EdgeFilter, Grafeno, NewEdge, NewNode, NodeId, OnConflict, Scope,
    Timestamp,
};
use grafeno_memory::palace::ENTRANCE;
use grafeno_memory::{
    ARCHIVED, CommunityOptions, DOOR, NewRoom, Proposal, ReviewOptions, Summary, SupersededOptions,
    anchor, anchors_of, archive, create_locus, create_palace, create_room, find_locus, find_palace,
    find_room, refresh_communities, remove, review, superseded, timeline, undo_summary,
    write_summary,
};

const NS: &str = "agent";

fn counts(db: &Grafeno) -> (u64, u64) {
    let stats = db.begin_read().unwrap().stats(NS).unwrap();
    (stats.nodes, stats.edges)
}

fn note(tx: &mut grafeno::WriteTx, text: &str) -> grafeno::Result<NodeId> {
    tx.create_node(NS, NewNode::new("Note").text(text))
}

/// Two clusters of notes, joined inside and not across: one about billing
/// and one about search. The billing notes are in the room Billing but for
/// one, put in Search; Archive holds what Billing holds; Plans is empty and
/// Search has a locus with nothing at it. Six more notes about deploys are
/// in no room.
struct Messy {
    db: Grafeno,
    palace: NodeId,
    billing: NodeId,
    search: NodeId,
    archive_room: NodeId,
    plans: NodeId,
    misplaced: NodeId,
    idle: NodeId,
}

fn messy() -> Messy {
    let db = Grafeno::in_memory().unwrap();
    let ids = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Work", None)?.id;
            let billing = create_room(tx, NS, palace, NewRoom::new("Billing"))?.id;
            let search = create_room(tx, NS, palace, NewRoom::new("Search").locus("Idle"))?.id;
            let archive_room = create_room(tx, NS, palace, NewRoom::new("Archive"))?.id;
            let plans = create_room(tx, NS, palace, NewRoom::new("Plans"))?.id;
            let mut clusters = Vec::new();
            for subject in ["billing invoices", "search ranking", "deploy rollout"] {
                let notes: Vec<NodeId> = (0..6)
                    .map(|i| note(tx, &format!("{subject} note {i}")))
                    .collect::<grafeno::Result<_>>()?;
                for i in 0..notes.len() {
                    for j in i + 1..notes.len() {
                        tx.create_edge(NS, NewEdge::new(notes[i], "relates_to", notes[j]))?;
                    }
                }
                clusters.push(notes);
            }
            Ok((palace, billing, search, archive_room, plans, clusters))
        })
        .unwrap();
    let (palace, billing, search, archive_room, plans, clusters) = ids;
    let read = db.begin_read().unwrap();
    let entrance = |room| find_locus(&read, NS, room, ENTRANCE).unwrap().unwrap();
    let (at_billing, at_search, at_archive) =
        (entrance(billing), entrance(search), entrance(archive_room));
    let idle = find_locus(&read, NS, search, "Idle").unwrap().unwrap();
    drop(read);
    let misplaced = clusters[0][5];
    db.write(|tx| {
        for &memory in &clusters[0][..5] {
            anchor(tx, NS, memory, at_billing, None)?;
            anchor(tx, NS, memory, at_archive, None)?;
        }
        anchor(tx, NS, misplaced, at_search, None)?;
        for &memory in &clusters[1] {
            anchor(tx, NS, memory, at_search, None)?;
        }
        Ok(())
    })
    .unwrap();
    refresh_communities(&db, NS, &CommunityOptions::default()).unwrap();
    Messy {
        db,
        palace,
        billing,
        search,
        archive_room,
        plans,
        misplaced,
        idle,
    }
}

#[test]
fn a_review_proposes_and_changes_nothing() {
    let m = messy();
    let before = counts(&m.db);
    let read = m.db.begin_read().unwrap();
    let options = ReviewOptions {
        crowded: 6,
        ..ReviewOptions::default()
    };
    let found = review(&read, NS, m.palace, &options).unwrap();
    assert!(found.communities);
    let proposals = &found.proposals;

    assert!(proposals.contains(&Proposal::EmptyRoom {
        room: m.plans,
        name: "Plans".into()
    }));
    assert!(proposals.contains(&Proposal::EmptyLocus {
        locus: m.idle,
        room: "Search".into(),
        name: "Idle".into()
    }));
    // Search holds its six and the misplaced one: past six.
    assert!(
        proposals.iter().any(|p| matches!(
            p,
            Proposal::SplitRoom { room, memories: 7, by_community, .. }
                if *room == m.search && by_community.len() == 2
        )),
        "{proposals:#?}"
    );
    // The billing note in Search belongs with the others, in Billing (and
    // in Archive, which holds the same).
    let moves: Vec<NodeId> = proposals
        .iter()
        .filter_map(|p| match p {
            Proposal::MoveMemory {
                memory,
                to_room,
                share,
                ..
            } if *memory == m.misplaced => {
                assert!(*share >= 0.66);
                Some(*to_room)
            }
            _ => None,
        })
        .collect();
    assert!(
        moves.contains(&m.billing) && moves.contains(&m.archive_room),
        "{proposals:#?}"
    );
    assert!(
        proposals.iter().any(|p| matches!(
            p,
            Proposal::MergeRooms { rooms, overlap, .. }
                if rooms.contains(&m.billing) && rooms.contains(&m.archive_room) && *overlap == 1.0
        )),
        "{proposals:#?}"
    );
    // The deploy notes have no room.
    assert!(
        proposals.iter().any(|p| matches!(
            p,
            Proposal::NewRoom { memories, .. } if memories.len() == 6
        )),
        "{proposals:#?}"
    );
    // Nothing that is in order is proposed: no move for well-placed notes.
    assert_eq!(
        proposals
            .iter()
            .filter(|p| matches!(p, Proposal::MoveMemory { .. }))
            .count(),
        2
    );
    drop(read);
    assert_eq!(counts(&m.db), before, "a review changes nothing");

    // Without community summaries, only what needs none.
    let db = Grafeno::in_memory().unwrap();
    let palace = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Home", None)?.id;
            create_room(tx, NS, palace, NewRoom::new("Empty"))?;
            Ok(palace)
        })
        .unwrap();
    let found = review(
        &db.begin_read().unwrap(),
        NS,
        palace,
        &ReviewOptions::default(),
    )
    .unwrap();
    assert!(!found.communities);
    assert_eq!(found.proposals.len(), 1);
}

#[test]
fn removing_a_room_or_a_palace_takes_its_structure_and_no_memory() {
    let m = messy();
    let (nodes, _) = counts(&m.db);
    m.db.write(|tx| {
        tx.create_edge(NS, NewEdge::new(m.billing, DOOR, m.search))
            .map(drop)
    })
    .unwrap();

    // Said first, then done.
    let said = m.db.write(|tx| remove(tx, NS, m.billing, true)).unwrap();
    assert!(!said.done);
    assert_eq!(said.rooms, [m.billing]);
    assert_eq!(said.loci.len(), 1);
    assert_eq!(said.anchors, 5);
    // Those five are in Archive too: none is left without a place.
    assert!(said.unplaced.is_empty());
    assert_eq!(counts(&m.db).0, nodes, "a dry run changes nothing");
    let done = m.db.write(|tx| remove(tx, NS, m.billing, false)).unwrap();
    assert!(done.done);
    let read = m.db.begin_read().unwrap();
    assert_eq!(find_room(&read, NS, m.palace, "Billing").unwrap(), None);
    assert_eq!(counts(&m.db).0, nodes - 2, "the room and its entrance");
    // The door went with the room.
    assert!(
        read.neighbor_ids(NS, m.search, Direction::In, &EdgeFilter::any().label(DOOR))
            .unwrap()
            .is_empty()
    );
    drop(read);

    // The palace: its rooms, their loci, and memories left in no place.
    let said = m.db.write(|tx| remove(tx, NS, m.palace, true)).unwrap();
    assert_eq!(said.palaces, [m.palace]);
    assert_eq!(said.rooms.len(), 3);
    assert_eq!(
        said.unplaced.len(),
        12,
        "the six of Search, the misplaced one and five of Archive"
    );
    m.db.write(|tx| remove(tx, NS, m.palace, false)).unwrap();
    let read = m.db.begin_read().unwrap();
    assert_eq!(find_palace(&read, NS, "Work").unwrap(), None);
    assert!(read.nodes_with_label(NS, "Locus").unwrap().is_empty());
    assert_eq!(read.nodes_with_label(NS, "Note").unwrap().len(), 18);
    for memory in &said.unplaced {
        assert!(read.node(NS, *memory).unwrap().is_some());
        assert!(anchors_of(&read, NS, *memory).unwrap().is_empty());
    }
    drop(read);

    // A memory is not removed this way.
    let memory = m.misplaced;
    let refused = m.db.write(|tx| remove(tx, NS, memory, false));
    assert!(
        matches!(refused, Err(grafeno::Error::Invalid(_))),
        "{refused:?}"
    );
}

#[test]
fn a_locus_alone_is_removed_too() {
    let m = messy();
    let lone =
        m.db.write(|tx| create_locus(tx, NS, m.plans, "Corner").map(|made| made.id))
            .unwrap();
    let removed = m.db.write(|tx| remove(tx, NS, lone, false)).unwrap();
    assert_eq!((removed.loci.clone(), removed.rooms.len()), (vec![lone], 0));
    let read = m.db.begin_read().unwrap();
    assert!(read.node(NS, lone).unwrap().is_none());
    assert!(read.node(NS, m.plans).unwrap().is_some());
}

#[test]
fn newer_memories_that_may_overtake_older_ones_are_listed() {
    let db = Grafeno::in_memory().unwrap();
    let (old, new, repeat, other) = db
        .write(|tx| {
            let ana = tx.create_node(NS, NewNode::new("Person").prop("name", "Ana"))?;
            let old = note(tx, "Ana works at Acme as a backend engineer in Lima")?;
            std::thread::sleep(std::time::Duration::from_millis(2));
            let new = note(tx, "Ana works at Globex as a backend engineer in Lima now")?;
            let repeat = note(tx, "Ana works at Globex as a backend engineer in Lima now.")?;
            let other = note(tx, "Ana likes soup")?;
            for memory in [old, new, repeat, other] {
                tx.create_edge(NS, NewEdge::new(memory, "about", ana))?;
            }
            Ok((old, new, repeat, other))
        })
        .unwrap();
    let before = counts(&db);
    let read = db.begin_read().unwrap();
    let found = superseded(&read, NS, &SupersededOptions::default()).unwrap();
    let pairs: Vec<(NodeId, NodeId)> = found.iter().map(|p| (p.older.id, p.newer.id)).collect();
    assert!(pairs.contains(&(old, new)), "{pairs:?}");
    // A repeat is not an update, and an unrelated memory is not either.
    assert!(
        !pairs
            .iter()
            .any(|(a, b)| (*a == new && *b == repeat) || (*a == repeat && *b == new))
    );
    assert!(!pairs.iter().any(|(a, b)| *a == other || *b == other));
    let pair = found
        .iter()
        .find(|p| p.older.id == old && p.newer.id == new)
        .unwrap();
    assert_eq!(pair.about[0].prop("name").unwrap(), "Ana");
    assert!(pair.overlap > 0.4 && pair.overlap < 0.85);
    drop(read);
    assert_eq!(counts(&db), before, "listing changes nothing");
}

#[test]
fn a_consolidation_is_undone() {
    let db = Grafeno::in_memory().unwrap();
    let (summary, memories) = db
        .write(|tx| {
            let memories: Vec<NodeId> = (0..3)
                .map(|i| note(tx, &format!("Billing retry note {i}")))
                .collect::<grafeno::Result<_>>()?;
            // One was archived before, for another reason.
            archive(tx, NS, &memories[2..])?;
            let summary = write_summary(
                tx,
                NS,
                Summary {
                    text: "Billing retries, in short".into(),
                    sources: memories.clone(),
                    archive_sources: true,
                    ..Summary::default()
                },
            )?;
            Ok((summary, memories))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    for memory in &memories {
        assert!(read.node(NS, *memory).unwrap().unwrap().has_label(ARCHIVED));
    }
    // Its timeline says it was summarized, and when.
    let moments = timeline(&read, NS, memories[0], 100).unwrap();
    assert!(
        moments.iter().any(|m| m.text.contains("summarizes")),
        "{moments:?}"
    );
    drop(read);

    let undone = db.write(|tx| undo_summary(tx, NS, summary)).unwrap();
    assert_eq!(undone.restored, memories[..2]);
    assert_eq!(undone.kept_archived, memories[2..]);
    let read = db.begin_read().unwrap();
    assert!(read.node(NS, summary).unwrap().is_none());
    assert!(
        !read
            .node(NS, memories[0])
            .unwrap()
            .unwrap()
            .has_label(ARCHIVED)
    );
    assert!(
        read.node(NS, memories[2])
            .unwrap()
            .unwrap()
            .has_label(ARCHIVED)
    );
    drop(read);
    // Only a summary is undone this way.
    let refused = db.write(|tx| undo_summary(tx, NS, memories[0]));
    assert!(refused.is_err());
}

/// Phase 4's acceptance: successive facts about one thing keep their
/// times, which one holds when, and who recorded each.
#[test]
fn successive_facts_keep_their_time_and_who_recorded_them() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_constraint(
            NS,
            Constraint::Edge {
                name: "one_employer".into(),
                label: "works_at".into(),
                scope: Scope::Source,
                on_conflict: OnConflict::Invalidate,
            },
        )
        .map(drop)
    })
    .unwrap();
    let at = |date: &str| Timestamp::parse(date).unwrap();
    let (ana, acme, globex) = db
        .write(|tx| {
            tx.act_as(Some(Actor::new("first-agent")));
            let ana = tx.create_node(NS, NewNode::new("Person").prop("name", "Ana"))?;
            let acme = tx.create_node(NS, NewNode::new("Company").prop("name", "Acme"))?;
            let globex = tx.create_node(NS, NewNode::new("Company").prop("name", "Globex"))?;
            tx.create_edge(
                NS,
                NewEdge::new(ana, "works_at", acme).valid_from(at("2021-03-01")),
            )?;
            Ok((ana, acme, globex))
        })
        .unwrap();
    db.write(|tx| {
        tx.act_as(Some(Actor::new("second-agent")));
        tx.create_edge(
            NS,
            NewEdge::new(ana, "works_at", globex).valid_from(at("2024-01-15")),
        )
        .map(drop)
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let both = read
        .neighbors(
            NS,
            ana,
            Direction::Out,
            &EdgeFilter::any().label("works_at"),
        )
        .unwrap();
    assert_eq!(both.len(), 2, "neither is lost");
    for neighbor in &both {
        let edge = &neighbor.edge;
        if neighbor.node == acme {
            assert_eq!(edge.valid_to, Some(at("2024-01-15")));
            assert_eq!(edge.recorded_by.as_deref(), Some("first-agent"));
        } else {
            assert_eq!(neighbor.node, globex);
            assert_eq!(edge.valid_to, None);
            assert_eq!(edge.recorded_by.as_deref(), Some("second-agent"));
        }
    }
    let then = read
        .neighbors(
            NS,
            ana,
            Direction::Out,
            &EdgeFilter::any()
                .label("works_at")
                .valid_at(at("2022-06-01")),
        )
        .unwrap();
    assert_eq!(then.len(), 1);
    assert_eq!(then[0].node, acme);
    let moments = timeline(&read, NS, ana, 100).unwrap();
    let order: Vec<&str> = moments
        .iter()
        .filter(|m| matches!(m.kind, "fact_began" | "fact_ended"))
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(
        order,
        [
            "began: Ana works_at Acme",
            "began: Ana works_at Globex",
            "ended: Ana works_at Acme"
        ]
    );
}
