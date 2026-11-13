//! Reading a palace: its map, walks from a topic or a node, and entering
//! a room.

use grafeno::{Error, Grafeno, NewEdge, NewNode, NodeId};
use grafeno_memory::palace::ENTRANCE;
use grafeno_memory::{
    EnterOptions, MapOptions, NewCheckpoint, NewRoom, WalkQuery, anchor, archive, create_palace,
    create_room, enter, find_locus, map_palace, map_room, palaces, save_checkpoint, walk,
};

const NS: &str = "agent";

/// Two palaces. In `Work`: the room `Storage` (about the storage engine)
/// with the notes `writer` and `file`, and the empty room `Queries`. In
/// `Home`: the room `Kitchen` with `pasta`. `writer` is also in `Kitchen`.
/// Outside any palace: `ana`, who wrote `writer`; `raft`, which `writer`
/// leads to; and `leader`, one relation further.
struct Fixture {
    db: Grafeno,
    work: NodeId,
    home: NodeId,
    storage: NodeId,
    queries: NodeId,
    kitchen: NodeId,
    decisions: NodeId,
    writer: NodeId,
    file: NodeId,
    pasta: NodeId,
    ana: NodeId,
    raft: NodeId,
    leader: NodeId,
}

fn fixture_in(db: Grafeno) -> Fixture {
    let ids = db
        .write(|tx| {
            let work = create_palace(tx, NS, "Work", Some("Projects"))?.id;
            let home = create_palace(tx, NS, "Home", None)?.id;
            let storage = create_room(
                tx,
                NS,
                work,
                NewRoom::new("Storage")
                    .topic("the storage engine")
                    .locus("Decisions"),
            )?
            .id;
            let queries = create_room(tx, NS, work, NewRoom::new("Queries"))?.id;
            let kitchen = create_room(tx, NS, home, NewRoom::new("Kitchen").topic("cooking"))?.id;
            let note = |tx: &mut grafeno::WriteTx, title: &str, text: &str| {
                tx.create_node(NS, NewNode::new("Note").prop("title", title).text(text))
            };
            let writer = note(tx, "One writer", "Only one transaction writes at a time")?;
            let file = note(tx, "Single file", "The whole database is one file")?;
            let pasta = note(tx, "Pasta", "Boil the pasta in salted water")?;
            let raft = note(tx, "Raft", "A replicated log orders the writes")?;
            let leader = note(tx, "Leader", "The leader coordinates the group")?;
            let ana = tx.create_node(NS, NewNode::new("Person").prop("name", "Ana"))?;
            tx.create_edge(NS, NewEdge::new(ana, "wrote", writer))?;
            tx.create_edge(NS, NewEdge::new(writer, "leads_to", raft))?;
            tx.create_edge(NS, NewEdge::new(raft, "needs", leader))?;
            Ok((
                work, home, storage, queries, kitchen, writer, file, pasta, ana, raft, leader,
            ))
        })
        .unwrap();
    let (work, home, storage, queries, kitchen, writer, file, pasta, ana, raft, leader) = ids;
    let (decisions, storage_entrance, kitchen_entrance) = {
        let read = db.begin_read().unwrap();
        (
            find_locus(&read, NS, storage, "Decisions")
                .unwrap()
                .unwrap(),
            find_locus(&read, NS, storage, ENTRANCE).unwrap().unwrap(),
            find_locus(&read, NS, kitchen, ENTRANCE).unwrap().unwrap(),
        )
    };
    db.write(|tx| {
        anchor(tx, NS, writer, decisions, Some("why writes are serial"))?;
        anchor(tx, NS, file, storage_entrance, None)?;
        anchor(tx, NS, pasta, kitchen_entrance, None)?;
        anchor(tx, NS, writer, kitchen_entrance, None)?;
        Ok(())
    })
    .unwrap();
    Fixture {
        db,
        work,
        home,
        storage,
        queries,
        kitchen,
        decisions,
        writer,
        file,
        pasta,
        ana,
        raft,
        leader,
    }
}

fn fixture() -> Fixture {
    fixture_in(Grafeno::in_memory().unwrap())
}

fn ids(walk: &grafeno_memory::Walk) -> Vec<NodeId> {
    walk.memories.iter().map(|memory| memory.id).collect()
}

#[test]
fn a_map_shows_palaces_rooms_loci_and_counts() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();

    let all = palaces(&read, NS, &MapOptions::default()).unwrap();
    let names: Vec<&str> = all.iter().map(|palace| palace.name.as_str()).collect();
    assert_eq!(names, ["Home", "Work"]);
    assert!(
        palaces(&read, "other", &MapOptions::default())
            .unwrap()
            .is_empty()
    );

    let work = map_palace(&read, NS, f.work, &MapOptions::default()).unwrap();
    assert_eq!(work.description.as_deref(), Some("Projects"));
    let rooms: Vec<(&str, usize)> = work
        .rooms
        .iter()
        .map(|room| (room.name.as_str(), room.memories))
        .collect();
    // By name; an empty room is there, with its entrance.
    assert_eq!(rooms, [("Queries", 0), ("Storage", 2)]);
    assert_eq!(work.rooms[0].id, f.queries);
    assert_eq!(work.rooms[0].loci.len(), 1);
    assert_eq!(work.rooms[0].loci[0].name, ENTRANCE);
    let storage = &work.rooms[1];
    assert_eq!(storage.topic.as_deref(), Some("the storage engine"));
    let loci: Vec<(&str, u64, usize)> = storage
        .loci
        .iter()
        .map(|locus| (locus.name.as_str(), locus.position, locus.count))
        .collect();
    assert_eq!(loci, [(ENTRANCE, 0, 1), ("Decisions", 1, 1)]);
    // Counts only, unless the memories are asked for.
    assert!(storage.loci.iter().all(|locus| locus.memories.is_empty()));
}

#[test]
fn a_room_map_lists_its_memories() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();
    let options = MapOptions {
        memories: true,
        ..MapOptions::default()
    };
    let storage = map_room(&read, NS, f.storage, &options).unwrap();
    assert_eq!(storage.loci[1].id, f.decisions);
    let at = &storage.loci[1].memories;
    assert_eq!(at.len(), 1);
    assert_eq!(at[0].id, f.writer);
    assert_eq!(at[0].name, "One writer");
    assert_eq!(at[0].labels, ["Note"]);
    assert_eq!(at[0].note.as_deref(), Some("why writes are serial"));
    assert_eq!(storage.loci[0].memories[0].id, f.file);

    // The same memory shows in the other room it is anchored in.
    let kitchen = map_room(&read, NS, f.kitchen, &options).unwrap();
    assert_eq!(kitchen.memories, 2);
    let there: Vec<NodeId> = kitchen.loci[0].memories.iter().map(|m| m.id).collect();
    assert!(there.contains(&f.writer) && there.contains(&f.pasta));

    // A limit cuts the list, not the count.
    let one = map_room(
        &read,
        NS,
        f.kitchen,
        &MapOptions {
            memories: true,
            limit: 1,
        },
    )
    .unwrap();
    assert_eq!(one.loci[0].count, 2);
    assert_eq!(one.loci[0].memories.len(), 1);

    // A palace is not a room.
    let result = map_room(&read, NS, f.work, &options);
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    let result = map_palace(&read, "other", f.work, &options);
    assert!(matches!(result, Err(Error::NodeNotFound(_))), "{result:?}");
}

#[test]
fn a_topic_that_names_a_room_leads_to_its_memories_and_beyond() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();

    let found = walk(&read, NS, &WalkQuery::topic("storage engine").hops(0)).unwrap();
    assert_eq!(found.places[0].id, f.storage);
    assert_eq!(found.places[0].kind, "Room");
    assert_eq!(found.places[0].path, "Work / Storage");
    assert!(found.places[0].score > 0.0);
    // The room's memories, the entrance's first, and nothing further.
    assert_eq!(ids(&found), [f.file, f.writer]);
    assert!(found.memories.iter().all(|memory| memory.hops == 0));
    let writer = &found.memories[1];
    assert_eq!(writer.origin, f.writer);
    assert!(writer.via.is_empty());
    let paths: Vec<(Option<&str>, &str)> = writer
        .places
        .iter()
        .map(|place| (place.room_name.as_deref(), place.locus_name.as_str()))
        .collect();
    assert_eq!(
        paths,
        [(Some("Kitchen"), ENTRANCE), (Some("Storage"), "Decisions")]
    );

    // One relation further: who wrote it and what it leads to, neither
    // anchored anywhere, each with the way there.
    let found = walk(&read, NS, &WalkQuery::topic("storage engine").hops(1)).unwrap();
    assert_eq!(ids(&found)[..2], [f.file, f.writer]);
    let near: Vec<NodeId> = ids(&found)[2..].to_vec();
    assert_eq!(near.len(), 2);
    assert!(near.contains(&f.ana) && near.contains(&f.raft));
    let raft = found.memories.iter().find(|m| m.id == f.raft).unwrap();
    assert_eq!((raft.hops, raft.origin), (1, f.writer));
    assert_eq!(raft.via.len(), 1);
    assert_eq!(raft.via[0].label, "leads_to");
    assert!(raft.via[0].forward);
    assert_eq!(raft.via[0].name, "Raft");
    assert!(raft.places.is_empty());
    let ana = found.memories.iter().find(|m| m.id == f.ana).unwrap();
    assert_eq!(ana.via[0].label, "wrote");
    assert!(!ana.via[0].forward);
    // The walk does not pass through the kitchen to the pasta.
    assert!(!ids(&found).contains(&f.pasta));

    // Two relations, the default: the leader, through Raft.
    let found = walk(&read, NS, &WalkQuery::topic("storage engine")).unwrap();
    let leader = found.memories.iter().find(|m| m.id == f.leader).unwrap();
    assert_eq!(leader.hops, 2);
    let way: Vec<&str> = leader.via.iter().map(|hop| hop.label.as_str()).collect();
    assert_eq!(way, ["leads_to", "needs"]);
    assert_eq!(found.memories.last().unwrap().id, f.leader);

    // A limit keeps the nearest.
    let found = walk(&read, NS, &WalkQuery::topic("storage engine").limit(3)).unwrap();
    assert_eq!(found.memories.len(), 3);
    assert_eq!(ids(&found)[..2], [f.file, f.writer]);
}

#[test]
fn a_topic_only_a_memory_matches_starts_from_that_memory() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();

    // No room or locus is about pasta; the note is, and it is in a room.
    let found = walk(&read, NS, &WalkQuery::topic("salted water").hops(0)).unwrap();
    assert!(found.places.is_empty());
    assert_eq!(ids(&found), [f.pasta]);
    assert_eq!(
        found.memories[0].places[0].room_name.as_deref(),
        Some("Kitchen")
    );

    // A memory that matches but has no place in a palace is recall's to
    // find, not a walk's.
    let found = walk(&read, NS, &WalkQuery::topic("replicated log").hops(0)).unwrap();
    assert!(found.places.is_empty() && found.memories.is_empty());

    // A topic nothing matches is an empty walk, not an error.
    let found = walk(&read, NS, &WalkQuery::topic("volcanoes")).unwrap();
    assert!(found.places.is_empty() && found.memories.is_empty());
}

#[test]
fn a_walk_stays_in_the_palace_it_is_given() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();

    // "cooking" is the kitchen's topic, and the kitchen is in Home.
    let anywhere = walk(&read, NS, &WalkQuery::topic("cooking").hops(0)).unwrap();
    assert_eq!(anywhere.places[0].path, "Home / Kitchen");
    assert_eq!(anywhere.memories.len(), 2);
    let in_work = walk(
        &read,
        NS,
        &WalkQuery::topic("cooking").hops(0).palace(f.work),
    )
    .unwrap();
    assert!(in_work.places.is_empty() && in_work.memories.is_empty());
    let in_home = walk(
        &read,
        NS,
        &WalkQuery::topic("cooking").hops(0).palace(f.home),
    )
    .unwrap();
    assert_eq!(in_home, anywhere);

    // A matching memory counts when it has a place in the palace given.
    let in_work = walk(
        &read,
        NS,
        &WalkQuery::topic("salted water").hops(0).palace(f.work),
    )
    .unwrap();
    assert!(in_work.memories.is_empty());

    let result = walk(&read, NS, &WalkQuery::topic("cooking").palace(f.kitchen));
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
}

#[test]
fn a_walk_starts_from_a_node() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();

    // From a palace: everything in its rooms.
    let found = walk(&read, NS, &WalkQuery::from(f.work).hops(0)).unwrap();
    assert_eq!(found.places[0].kind, "Palace");
    assert_eq!(found.places[0].path, "Work");
    assert_eq!(ids(&found), [f.file, f.writer]);

    // From a locus: what is anchored there.
    let found = walk(&read, NS, &WalkQuery::from(f.decisions).hops(0)).unwrap();
    assert_eq!(found.places[0].path, "Work / Storage / Decisions");
    assert_eq!(ids(&found), [f.writer]);

    // From a memory: itself, then its relations.
    let found = walk(&read, NS, &WalkQuery::from(f.raft).hops(1)).unwrap();
    assert!(found.places.is_empty());
    assert_eq!(found.memories[0].id, f.raft);
    let near: Vec<NodeId> = ids(&found)[1..].to_vec();
    assert_eq!(near.len(), 2);
    assert!(near.contains(&f.writer) && near.contains(&f.leader));

    // Hops are capped.
    let far = walk(&read, NS, &WalkQuery::from(f.leader).hops(99)).unwrap();
    assert!(far.memories.iter().all(|memory| memory.hops <= 3));
    assert!(ids(&far).contains(&f.ana));

    let missing = NodeId::new();
    let result = walk(&read, NS, &WalkQuery::from(missing));
    assert!(matches!(result, Err(Error::NodeNotFound(id)) if id == missing));
    let result = walk(&read, NS, &WalkQuery::default());
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    let result = walk(&read, NS, &WalkQuery::topic("   "));
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
}

#[test]
fn archived_memories_are_left_out_of_walks_and_rooms() {
    let f = fixture();
    f.db.write(|tx| archive(tx, NS, &[f.file, f.raft])).unwrap();
    let read = f.db.begin_read().unwrap();
    let found = walk(&read, NS, &WalkQuery::from(f.storage)).unwrap();
    assert!(!ids(&found).contains(&f.file));
    assert!(!ids(&found).contains(&f.raft));
    // Nor what was only reachable through one.
    assert!(!ids(&found).contains(&f.leader));
    assert!(ids(&found).contains(&f.writer));

    let entered = enter(&read, NS, f.storage, &EnterOptions::default()).unwrap();
    let items: Vec<NodeId> = entered.context.items.iter().map(|item| item.node).collect();
    assert_eq!(items, [f.writer]);
}

#[test]
fn entering_a_room_gives_its_memories_within_a_budget() {
    let f = fixture();
    f.db.write(|tx| tx.set_importance(NS, f.writer, 0.9))
        .unwrap();
    let read = f.db.begin_read().unwrap();

    let entered = enter(&read, NS, f.storage, &EnterOptions::default()).unwrap();
    assert_eq!(entered.palace, Some(f.work));
    assert_eq!(entered.palace_name.as_deref(), Some("Work"));
    assert_eq!(entered.room.name, "Storage");
    assert_eq!(entered.room.memories, 2);
    assert_eq!(entered.room.loci.len(), 2);
    // The strongest first.
    let items: Vec<NodeId> = entered.context.items.iter().map(|item| item.node).collect();
    assert_eq!(items, [f.writer, f.file]);
    assert!(
        entered
            .context
            .rendered
            .contains("Only one transaction writes")
    );
    assert!(
        entered
            .context
            .rendered
            .contains("The whole database is one file")
    );
    // The memories' own facts are there; where they are anchored is not.
    let facts: Vec<&str> = entered
        .context
        .facts
        .iter()
        .map(|fact| fact.label.as_str())
        .collect();
    assert!(facts.contains(&"wrote") && facts.contains(&"leads_to"));
    assert!(!facts.contains(&"anchored_at"));
    assert_eq!(entered.checkpoint, None);

    // A small budget keeps what fits and never goes over.
    let small = enter(&read, NS, f.storage, &EnterOptions::with_budget(30)).unwrap();
    assert!(small.context.tokens <= 30);
    assert!(small.context.items.len() < 2 || small.context.items.iter().any(|i| i.truncated));

    // An empty room is entered too.
    let empty = enter(&read, NS, f.queries, &EnterOptions::default()).unwrap();
    assert!(empty.context.items.is_empty());
    assert_eq!(empty.context.rendered, "");

    let result = enter(&read, NS, f.work, &EnterOptions::default());
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    let result = enter(&read, "other", f.storage, &EnterOptions::default());
    assert!(matches!(result, Err(Error::NodeNotFound(_))), "{result:?}");
}

#[test]
fn entering_a_room_after_a_restart_gives_its_memories_and_its_checkpoint() {
    let path = std::env::temp_dir().join(format!(
        "grafeno-palace-enter-{}-{}.redb",
        std::process::id(),
        NodeId::new()
    ));
    let (storage, kitchen, writer, file) = {
        let f = fixture_in(Grafeno::open(&path).unwrap());
        f.db.write(|tx| {
            save_checkpoint(
                tx,
                NS,
                NewCheckpoint {
                    task: "Storage design".into(),
                    summary: "First pass".into(),
                    about: Some(f.storage),
                    ..NewCheckpoint::default()
                },
            )?;
            save_checkpoint(
                tx,
                NS,
                NewCheckpoint {
                    task: "Dinner".into(),
                    summary: "Shopping done".into(),
                    about: Some(f.kitchen),
                    ..NewCheckpoint::default()
                },
            )
        })
        .unwrap();
        f.db.write(|tx| {
            save_checkpoint(
                tx,
                NS,
                NewCheckpoint {
                    task: "storage  DESIGN".into(),
                    summary: "One writer settled".into(),
                    next: vec!["Measure the file".into()],
                    about: Some(f.storage),
                    ..NewCheckpoint::default()
                },
            )
        })
        .unwrap();
        (f.storage, f.kitchen, f.writer, f.file)
    };

    let db = Grafeno::open(&path).unwrap();
    {
        let read = db.begin_read().unwrap();
        let entered = enter(&read, NS, storage, &EnterOptions::default()).unwrap();
        let mut items: Vec<NodeId> = entered.context.items.iter().map(|item| item.node).collect();
        items.sort();
        let mut expected = vec![writer, file];
        expected.sort();
        assert_eq!(items, expected);
        // The latest checkpoint about this room, not the other room's.
        let checkpoint = entered.checkpoint.unwrap();
        assert_eq!(checkpoint.summary, "One writer settled");
        assert_eq!(checkpoint.next, ["Measure the file"]);
        assert_eq!(checkpoint.about, Some(storage));
        assert!(checkpoint.previous.is_some());

        let other = enter(&read, NS, kitchen, &EnterOptions::default()).unwrap();
        assert_eq!(other.checkpoint.unwrap().summary, "Shopping done");

        // And the walk finds the same after the restart.
        let found = walk(&read, NS, &WalkQuery::topic("storage engine").hops(0)).unwrap();
        assert_eq!(found.places[0].id, storage);
        assert_eq!(found.memories.len(), 2);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
