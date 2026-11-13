use grafeno::{Direction, EdgeFilter, Error, Grafeno, NewEdge, NewNode, NodeId};
use grafeno_memory::palace::{ENTRANCE, HAS_LOCUS, HAS_ROOM};
use grafeno_memory::{
    ANCHORED_AT, LOCUS, NewRoom, PALACE, ROOM, anchor, anchors_of, create_locus, create_palace,
    create_room, find_locus, find_palace, find_room, unanchor,
};

const NS: &str = "agent";

fn count(db: &Grafeno, ns: &str, label: &str) -> usize {
    db.begin_read()
        .unwrap()
        .nodes_with_label(ns, label)
        .unwrap()
        .len()
}

/// A palace with one room, and two memories joined by a fact.
struct Fixture {
    db: Grafeno,
    palace: NodeId,
    room: NodeId,
    note: NodeId,
    ana: NodeId,
}

fn fixture_in(db: Grafeno) -> Fixture {
    let (palace, room, note, ana) = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Work", Some("Everything about work"))?.id;
            let room = create_room(
                tx,
                NS,
                palace,
                NewRoom::new("Grafeno")
                    .topic("graph database")
                    .locus("Decisions")
                    .locus("Open questions"),
            )?
            .id;
            let note = tx.create_node(
                NS,
                NewNode::new("Note")
                    .prop("title", "Storage choice")
                    .text("The storage engine keeps one writer"),
            )?;
            let ana = tx.create_node(NS, NewNode::new("Person").prop("name", "Ana"))?;
            tx.create_edge(NS, NewEdge::new(ana, "wrote", note))?;
            Ok((palace, room, note, ana))
        })
        .unwrap();
    Fixture {
        db,
        palace,
        room,
        note,
        ana,
    }
}

fn fixture() -> Fixture {
    fixture_in(Grafeno::in_memory().unwrap())
}

#[test]
fn palaces_rooms_and_loci_are_created_and_found() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();

    assert_eq!(find_palace(&read, NS, "work").unwrap(), Some(f.palace));
    assert_eq!(find_palace(&read, NS, "  WORK ").unwrap(), Some(f.palace));
    assert_eq!(find_palace(&read, NS, "Home").unwrap(), None);
    assert_eq!(
        find_room(&read, NS, f.palace, "grafeno").unwrap(),
        Some(f.room)
    );
    assert_eq!(find_room(&read, NS, f.palace, "Other").unwrap(), None);

    let palace = read.node(NS, f.palace).unwrap().unwrap();
    assert!(palace.has_label(PALACE));
    assert_eq!(palace.prop("name").unwrap(), "Work");
    assert_eq!(palace.prop("description").unwrap(), "Everything about work");
    let room = read.node(NS, f.room).unwrap().unwrap();
    assert!(room.has_label(ROOM));
    assert_eq!(room.prop("topic").unwrap(), "graph database");
    assert_eq!(
        read.text(NS, f.room).unwrap().as_deref(),
        Some("Grafeno. graph database")
    );

    // The entrance first, then the loci asked for, in order.
    let mut loci = Vec::new();
    for name in [ENTRANCE, "decisions", "open questions"] {
        let id = find_locus(&read, NS, f.room, name).unwrap().unwrap();
        let node = read.node(NS, id).unwrap().unwrap();
        assert!(node.has_label(LOCUS));
        loci.push(node.prop("position").unwrap().as_u64().unwrap());
    }
    assert_eq!(loci, [0, 1, 2]);

    let rooms = read
        .neighbor_ids(
            NS,
            f.palace,
            Direction::Out,
            &EdgeFilter::any().label(HAS_ROOM),
        )
        .unwrap();
    assert_eq!(rooms.len(), 1);
    let places = read
        .neighbor_ids(
            NS,
            f.room,
            Direction::Out,
            &EdgeFilter::any().label(HAS_LOCUS),
        )
        .unwrap();
    assert_eq!(places.len(), 3);
}

#[test]
fn creating_twice_gives_one() {
    let f = fixture();
    let (palace, room, locus) =
        f.db.write(|tx| {
            let palace = create_palace(tx, NS, "WORK", Some("Another description"))?;
            let room = create_room(
                tx,
                NS,
                palace.id,
                NewRoom::new("grafeno").locus("decisions").locus("Ideas"),
            )?;
            let locus = create_locus(tx, NS, room.id, "Open  Questions")?;
            Ok((palace, room, locus))
        })
        .unwrap();
    assert_eq!((palace.id, palace.created), (f.palace, false));
    assert_eq!((room.id, room.created), (f.room, false));
    assert!(!locus.created);

    assert_eq!(count(&f.db, NS, PALACE), 1);
    assert_eq!(count(&f.db, NS, ROOM), 1);
    // The three it had and the one the second call added.
    assert_eq!(count(&f.db, NS, LOCUS), 4);

    let read = f.db.begin_read().unwrap();
    // What was there is not rewritten.
    let node = read.node(NS, f.palace).unwrap().unwrap();
    assert_eq!(node.prop("description").unwrap(), "Everything about work");
    let ideas = find_locus(&read, NS, f.room, "ideas").unwrap().unwrap();
    let ideas = read.node(NS, ideas).unwrap().unwrap();
    assert_eq!(ideas.prop("position").unwrap(), 3);
}

#[test]
fn names_are_unique_under_their_parent_only() {
    let f = fixture();
    let (home, other_room) =
        f.db.write(|tx| {
            let home = create_palace(tx, NS, "Home", None)?.id;
            // The same room name in another palace is another room.
            let room = create_room(tx, NS, home, NewRoom::new("Grafeno"))?;
            assert!(room.created);
            Ok((home, room.id))
        })
        .unwrap();
    assert_ne!(other_room, f.room);
    assert_eq!(count(&f.db, NS, ROOM), 2);

    let read = f.db.begin_read().unwrap();
    assert_eq!(
        find_room(&read, NS, home, "Grafeno").unwrap(),
        Some(other_room)
    );
    // Each room has an entrance of its own.
    let a = find_locus(&read, NS, f.room, ENTRANCE).unwrap().unwrap();
    let b = find_locus(&read, NS, other_room, ENTRANCE)
        .unwrap()
        .unwrap();
    assert_ne!(a, b);
}

#[test]
fn a_name_is_needed_and_parents_are_checked() {
    let f = fixture();
    let invalid = |result: grafeno::Result<()>| {
        assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    };
    invalid(f.db.write(|tx| create_palace(tx, NS, "  ", None).map(drop)));
    invalid(f.db.write(|tx| create_room(tx, NS, f.palace, NewRoom::new("?!")).map(drop)));
    invalid(f.db.write(|tx| create_locus(tx, NS, f.room, "").map(drop)));
    // A room is made in a palace and a locus in a room.
    invalid(f.db.write(|tx| create_room(tx, NS, f.room, NewRoom::new("Inner")).map(drop)));
    invalid(f.db.write(|tx| create_locus(tx, NS, f.palace, "Desk").map(drop)));
    let missing = NodeId::new();
    let result =
        f.db.write(|tx| create_room(tx, NS, missing, NewRoom::new("Lost")).map(drop));
    assert!(matches!(result, Err(Error::NodeNotFound(id)) if id == missing));
    // A failed call leaves nothing behind.
    assert_eq!(count(&f.db, NS, ROOM), 1);
    assert_eq!(count(&f.db, NS, LOCUS), 3);
}

#[test]
fn a_memory_is_anchored_at_several_loci_and_stays_as_it_was() {
    let f = fixture();
    let before = {
        let read = f.db.begin_read().unwrap();
        (
            read.node(NS, f.note).unwrap().unwrap(),
            read.text(NS, f.note).unwrap(),
            read.neighbor_ids(NS, f.note, Direction::In, &EdgeFilter::any().label("wrote"))
                .unwrap(),
            read.memory(NS, &read.node(NS, f.note).unwrap().unwrap())
                .unwrap(),
        )
    };
    let nodes = f.db.begin_read().unwrap().nodes(NS).unwrap().len();

    let (entrance, decisions, first, again) = {
        let read = f.db.begin_read().unwrap();
        let entrance = find_locus(&read, NS, f.room, ENTRANCE).unwrap().unwrap();
        let decisions = find_locus(&read, NS, f.room, "Decisions").unwrap().unwrap();
        drop(read);
        let (first, again) =
            f.db.write(|tx| {
                let first = anchor(tx, NS, f.note, decisions, Some("why one writer"))?;
                anchor(tx, NS, f.note, entrance, None)?;
                let again = anchor(tx, NS, f.note, decisions, Some("another note"))?;
                Ok((first, again))
            })
            .unwrap();
        (entrance, decisions, first, again)
    };
    assert!(first.created);
    assert!(!again.created);
    assert_eq!(first.edge, again.edge);

    let read = f.db.begin_read().unwrap();
    // No node was added: the memory is the one there was.
    assert_eq!(read.nodes(NS).unwrap().len(), nodes);
    let node = read.node(NS, f.note).unwrap().unwrap();
    assert_eq!(node, before.0);
    assert_eq!(read.text(NS, f.note).unwrap(), before.1);
    assert_eq!(
        read.neighbor_ids(NS, f.note, Direction::In, &EdgeFilter::any().label("wrote"))
            .unwrap(),
        before.2
    );
    assert_eq!(read.memory(NS, &node).unwrap(), before.3);

    let anchors = read
        .neighbor_ids(
            NS,
            f.note,
            Direction::Out,
            &EdgeFilter::any().label(ANCHORED_AT),
        )
        .unwrap();
    assert_eq!(anchors.len(), 2);

    let places = anchors_of(&read, NS, f.note).unwrap();
    assert_eq!(places.len(), 2);
    assert_eq!(places[0].locus, decisions);
    assert_eq!(places[0].locus_name, "Decisions");
    assert_eq!(places[0].room, Some(f.room));
    assert_eq!(places[0].room_name.as_deref(), Some("Grafeno"));
    assert_eq!(places[0].palace, Some(f.palace));
    assert_eq!(places[0].palace_name.as_deref(), Some("Work"));
    assert_eq!(places[0].note.as_deref(), Some("why one writer"));
    assert_eq!(places[1].locus, entrance);
    assert_eq!(places[1].note, None);
    assert!(anchors_of(&read, NS, f.ana).unwrap().is_empty());
}

#[test]
fn only_memories_are_anchored_and_only_at_loci() {
    let f = fixture();
    let entrance = {
        let read = f.db.begin_read().unwrap();
        find_locus(&read, NS, f.room, ENTRANCE).unwrap().unwrap()
    };
    // At a room, not a locus.
    let result = f.db.write(|tx| anchor(tx, NS, f.note, f.room, None));
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    // A room is not a memory.
    let result = f.db.write(|tx| anchor(tx, NS, f.room, entrance, None));
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    let missing = NodeId::new();
    let result = f.db.write(|tx| anchor(tx, NS, missing, entrance, None));
    assert!(matches!(result, Err(Error::NodeNotFound(id)) if id == missing));
}

#[test]
fn removing_an_anchor_removes_the_edge_only() {
    let f = fixture();
    let entrance = {
        let read = f.db.begin_read().unwrap();
        find_locus(&read, NS, f.room, ENTRANCE).unwrap().unwrap()
    };
    f.db.write(|tx| anchor(tx, NS, f.note, entrance, None))
        .unwrap();
    assert!(f.db.write(|tx| unanchor(tx, NS, f.note, entrance)).unwrap());
    assert!(!f.db.write(|tx| unanchor(tx, NS, f.note, entrance)).unwrap());

    let read = f.db.begin_read().unwrap();
    assert!(anchors_of(&read, NS, f.note).unwrap().is_empty());
    assert!(read.node(NS, f.note).unwrap().is_some());
    assert!(read.node(NS, entrance).unwrap().is_some());
    assert_eq!(
        read.neighbor_ids(NS, f.note, Direction::In, &EdgeFilter::any().label("wrote"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn deleting_a_memory_takes_its_anchors_and_leaves_the_palace() {
    let f = fixture();
    let entrance = {
        let read = f.db.begin_read().unwrap();
        find_locus(&read, NS, f.room, ENTRANCE).unwrap().unwrap()
    };
    f.db.write(|tx| {
        anchor(tx, NS, f.note, entrance, None)?;
        tx.delete_node(NS, f.note)
    })
    .unwrap();
    let read = f.db.begin_read().unwrap();
    assert!(
        read.neighbor_ids(
            NS,
            entrance,
            Direction::In,
            &EdgeFilter::any().label(ANCHORED_AT)
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        find_room(&read, NS, f.palace, "Grafeno").unwrap(),
        Some(f.room)
    );
}

#[test]
fn namespaces_do_not_see_each_other() {
    let f = fixture();
    let read = f.db.begin_read().unwrap();
    assert_eq!(find_palace(&read, "other", "Work").unwrap(), None);
    drop(read);

    // The same name in another namespace is another palace.
    let other =
        f.db.write(|tx| {
            let palace = create_palace(tx, "other", "Work", None)?;
            assert!(palace.created);
            create_room(tx, "other", palace.id, NewRoom::new("Grafeno"))
        })
        .unwrap();
    assert!(other.created);
    assert_ne!(other.id, f.room);
    assert_eq!(count(&f.db, NS, PALACE), 1);
    assert_eq!(count(&f.db, "other", PALACE), 1);

    let read = f.db.begin_read().unwrap();
    let theirs = find_locus(&read, "other", other.id, ENTRANCE)
        .unwrap()
        .unwrap();
    let ours = find_locus(&read, NS, f.room, ENTRANCE).unwrap().unwrap();
    drop(read);

    // A palace of one namespace cannot be reached from the other, and a
    // memory of one cannot be anchored in the other.
    let result =
        f.db.write(|tx| create_room(tx, "other", f.palace, NewRoom::new("Leak")).map(drop));
    assert!(matches!(result, Err(Error::NodeNotFound(_))), "{result:?}");
    let result = f.db.write(|tx| anchor(tx, "other", f.note, theirs, None));
    assert!(matches!(result, Err(Error::NodeNotFound(_))), "{result:?}");
    let result = f.db.write(|tx| anchor(tx, NS, f.note, theirs, None));
    assert!(matches!(result, Err(Error::NodeNotFound(_))), "{result:?}");
    let result = f.db.write(|tx| anchor(tx, "other", f.note, ours, None));
    assert!(matches!(result, Err(Error::NodeNotFound(_))), "{result:?}");

    let read = f.db.begin_read().unwrap();
    assert!(anchors_of(&read, NS, f.note).unwrap().is_empty());
    assert_eq!(count(&f.db, "other", ROOM), 1);
}

#[test]
fn a_palace_survives_closing_and_reopening_the_file() {
    let path = std::env::temp_dir().join(format!(
        "grafeno-palace-{}-{}.redb",
        std::process::id(),
        NodeId::new()
    ));
    let (palace, room, note, decisions) = {
        let f = fixture_in(Grafeno::open(&path).unwrap());
        let decisions = {
            let read = f.db.begin_read().unwrap();
            find_locus(&read, NS, f.room, "Decisions").unwrap().unwrap()
        };
        f.db.write(|tx| anchor(tx, NS, f.note, decisions, Some("kept")))
            .unwrap();
        (f.palace, f.room, f.note, decisions)
    };

    let db = Grafeno::open(&path).unwrap();
    {
        let read = db.begin_read().unwrap();
        assert_eq!(find_palace(&read, NS, "Work").unwrap(), Some(palace));
        assert_eq!(find_room(&read, NS, palace, "Grafeno").unwrap(), Some(room));
        assert_eq!(
            find_locus(&read, NS, room, "Decisions").unwrap(),
            Some(decisions)
        );
        let places = anchors_of(&read, NS, note).unwrap();
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].locus, decisions);
        assert_eq!(places[0].note.as_deref(), Some("kept"));
    }
    // And it is still the one palace: creating it again finds it.
    let again = db.write(|tx| create_palace(tx, NS, "Work", None)).unwrap();
    assert_eq!((again.id, again.created), (palace, false));
    drop(db);
    let _ = std::fs::remove_file(&path);
}
