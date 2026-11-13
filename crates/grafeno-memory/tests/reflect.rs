//! Phase 6 of the plan: metacognition. On a topic with partial
//! information, a reflection tells apart what is known (and verified), what
//! is uncertain, what clashed, what may be out of date and what is
//! missing, each with the record it comes from; on a topic with nothing it
//! says so; and it writes nothing.

use grafeno::{Constraint, Grafeno, NewEdge, NewNode, NodeId, OnConflict, Scope, Timestamp};
use grafeno_memory::knowledge::{self, ANSWERS, QUESTION};
use grafeno_memory::palace::ENTRANCE;
use grafeno_memory::{
    NewCheckpoint, NewRoom, Record, ReflectOptions, Scope as Reflect, anchor, archive,
    create_palace, create_room, find_locus, reflect, save_checkpoint, verify,
};

const NS: &str = "agent";

struct Billing {
    db: Grafeno,
    room: NodeId,
    verified_fact: String,
    inferred_fact: String,
    unsure: NodeId,
    ended_fact: String,
    obsolete: NodeId,
    older: NodeId,
    open_question: NodeId,
    answered_question: NodeId,
    undescribed: NodeId,
}

fn billing() -> Billing {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| {
        tx.create_constraint(
            NS,
            Constraint::Edge {
                name: "one_owner".into(),
                label: "owned_by".into(),
                scope: Scope::Source,
                on_conflict: OnConflict::Flag,
            },
        )
        .map(drop)
    })
    .unwrap();
    let past = Timestamp::parse("2020-01-01").unwrap();
    let ended = Timestamp::parse("2023-06-01").unwrap();
    let ids = db
        .write(|tx| {
            let note = |tx: &mut grafeno::WriteTx, text: &str| {
                tx.create_node(NS, NewNode::new("Note").text(text))
            };
            let doc = tx.create_node(
                NS,
                NewNode::new("Document").text("The billing runbook: the service runs on Ledger"),
            )?;
            let service = tx.create_node(
                NS,
                NewNode::new("Service")
                    .prop("name", "Billing service")
                    .text("The billing service sends invoices"),
            )?;
            let ledger = tx.create_node(NS, NewNode::new("Database").prop("name", "Ledger"))?;
            let old_db = tx.create_node(NS, NewNode::new("Database").prop("name", "Oldbase"))?;
            let ana = tx.create_node(NS, NewNode::new("Person").prop("name", "Ana"))?;
            let bob = tx.create_node(NS, NewNode::new("Person").prop("name", "Bob"))?;
            let verified_fact = tx.create_edge(NS, NewEdge::new(service, "runs_on", ledger))?;
            let mut inferred = NewEdge::new(ana, "maintains", service);
            inferred.props = knowledge::stated(Some("inferred"), None)?;
            let inferred_fact = tx.create_edge(NS, inferred)?;
            let ended_fact = tx.create_edge(
                NS,
                NewEdge::new(service, "ran_on", old_db)
                    .valid_from(past)
                    .valid_to(ended),
            )?;
            // Two owners, under "one owner": flagged, both kept.
            tx.create_edge(NS, NewEdge::new(service, "owned_by", ana))?;
            tx.create_edge(NS, NewEdge::new(service, "owned_by", bob))?;
            let mut unsure = NewNode::new("Decision").text("Billing may move to monthly invoices");
            unsure.props = knowledge::stated(Some("stated"), Some(0.3))?;
            let unsure = tx.create_node(NS, unsure)?;
            let obsolete = note(tx, "Billing retries every hour")?;
            tx.set_node_props(
                NS,
                obsolete,
                [("feedback".to_owned(), serde_json::json!("obsolete"))]
                    .into_iter()
                    .collect(),
            )?;
            archive(tx, NS, &[obsolete])?;
            let older = note(tx, "Billing invoices go out at midnight from Lima")?;
            tx.create_edge(NS, NewEdge::new(older, "about", service))?;
            std::thread::sleep(std::time::Duration::from_millis(2));
            let newer = note(tx, "Billing invoices go out at six from Lima now")?;
            tx.create_edge(NS, NewEdge::new(newer, "about", service))?;
            let open_question = tx.create_node(
                NS,
                NewNode::new(QUESTION).text("Who approves billing refunds?"),
            )?;
            let answered_question =
                tx.create_node(NS, NewNode::new(QUESTION).text("Where do billing logs go?"))?;
            let answer = note(tx, "Billing logs go to the central store")?;
            tx.create_edge(NS, NewEdge::new(answer, ANSWERS, answered_question))?;
            let undescribed =
                tx.create_node(NS, NewNode::new("Entity").prop("name", "Refund desk"))?;
            tx.create_edge(
                NS,
                NewEdge::new(open_question, "mentions", undescribed).prop("kind", "agent"),
            )?;
            Ok((
                doc,
                verified_fact,
                inferred_fact,
                unsure,
                ended_fact,
                obsolete,
                older,
                open_question,
                answered_question,
                undescribed,
                service,
            ))
        })
        .unwrap();
    let (
        doc,
        verified_fact,
        inferred_fact,
        unsure,
        ended_fact,
        obsolete,
        older,
        open_question,
        answered_question,
        undescribed,
        service,
    ) = ids;
    db.write(|tx| {
        verify(
            tx,
            NS,
            Record::Edge(verified_fact),
            Some(doc),
            None,
            Timestamp::now(),
        )
        .map(drop)
    })
    .unwrap();
    let room = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Work", None)?.id;
            create_room(
                tx,
                NS,
                palace,
                NewRoom::new("Billing").topic("billing").locus("Refunds"),
            )
            .map(|made| made.id)
        })
        .unwrap();
    let entrance = find_locus(&db.begin_read().unwrap(), NS, room, ENTRANCE)
        .unwrap()
        .unwrap();
    db.write(|tx| {
        anchor(tx, NS, service, entrance, None)?;
        save_checkpoint(
            tx,
            NS,
            NewCheckpoint {
                task: "Billing cleanup".into(),
                summary: "Half done".into(),
                next: vec!["Find who approves refunds".into()],
                about: Some(room),
                ..NewCheckpoint::default()
            },
        )
        .map(drop)
    })
    .unwrap();
    Billing {
        db,
        room,
        verified_fact: verified_fact.to_string(),
        inferred_fact: inferred_fact.to_string(),
        unsure,
        ended_fact: ended_fact.to_string(),
        obsolete,
        older,
        open_question,
        answered_question,
        undescribed,
    }
}

fn ids(items: &[grafeno_memory::Item]) -> Vec<&str> {
    items.iter().map(|item| item.id.as_str()).collect()
}

#[test]
fn a_reflection_tells_known_uncertain_clashing_stale_and_missing_apart() {
    let b = billing();
    let before = b.db.begin_read().unwrap().stats(NS).unwrap();
    let read = b.db.begin_read().unwrap();
    let found = reflect(
        &read,
        NS,
        &Reflect::Topic("billing".into()),
        &ReflectOptions::default(),
    )
    .unwrap();
    assert!(!found.nothing);

    // Known: counted by kind and by how they are known, the verified named.
    assert!(found.known.memories >= 6, "{:?}", found.known);
    assert_eq!(found.known.memories_by_kind.get("Decision"), Some(&1));
    assert_eq!(found.known.memories_by_kind.get(QUESTION), Some(&2));
    assert_eq!(found.known.facts_by_origin.get("verified"), Some(&1));
    assert_eq!(found.known.facts_by_origin.get("inferred"), Some(&1));
    assert_eq!(ids(&found.known.verified), [b.verified_fact.as_str()]);
    assert!(found.known.verified[0].text.contains("runs_on Ledger"));

    // Uncertain: the inferred fact with no source, and the unsure decision.
    let uncertain = ids(&found.uncertain);
    assert!(
        uncertain.contains(&b.inferred_fact.as_str()),
        "{uncertain:?}"
    );
    assert!(uncertain.contains(&b.unsure.to_string().as_str()));
    assert!(!uncertain.contains(&b.verified_fact.as_str()));

    // Conflicts: the two owners, flagged and not settled.
    assert_eq!(found.conflicts.len(), 1, "{:?}", found.conflicts);
    let clash = &found.conflicts[0];
    assert_eq!(
        (clash.constraint.as_str(), clash.settled),
        ("one_owner", false)
    );
    assert!(clash.new_fact.text.contains("owned_by Bob"));
    assert!(clash.old_fact.text.contains("owned_by Ana"));

    // Stale: the fact that ended, the obsolete memory, the older note.
    let stale: Vec<(&str, &str)> = found
        .stale
        .iter()
        .map(|item| (item.id.as_str(), item.why.as_str()))
        .collect();
    let why = |id: &str| stale.iter().find(|(x, _)| *x == id).map(|(_, why)| *why);
    assert!(
        why(&b.ended_fact)
            .unwrap()
            .contains("no longer true since 2023-06-01")
    );
    assert!(why(&b.obsolete.to_string()).unwrap().contains("obsolete"));
    assert!(why(&b.older.to_string()).unwrap().contains("newer memory"));

    // Gaps: the open question (not the answered one), and the desk it
    // mentions that nothing describes.
    let gaps = ids(&found.gaps);
    assert!(
        gaps.contains(&b.open_question.to_string().as_str()),
        "{gaps:?}"
    );
    assert!(!gaps.contains(&b.answered_question.to_string().as_str()));
    assert!(
        gaps.contains(&b.undescribed.to_string().as_str()),
        "{gaps:?}"
    );

    // Next: one step per kind of thing to check, each naming its records.
    let steps: Vec<&str> = found.next.iter().map(|step| step.what.as_str()).collect();
    for want in [
        "settle",
        "inferred",
        "open questions",
        "never described",
        "overtake",
    ] {
        assert!(
            steps.iter().any(|step| step.contains(want)),
            "{want}: {steps:?}"
        );
    }
    assert!(found.next.iter().all(|step| !step.about.is_empty()));

    // Every item names a record that is there.
    for item in found
        .uncertain
        .iter()
        .chain(&found.stale)
        .chain(&found.gaps)
        .chain(&found.known.verified)
    {
        let id: uuid::Uuid = item.id.parse().unwrap();
        let node = read.node(NS, NodeId::from(id)).unwrap();
        let edge = read
            .edge(NS, grafeno::EdgeId::from_u128(id.as_u128()))
            .unwrap();
        assert!(node.is_some() || edge.is_some(), "{item:?}");
    }
    assert!(!found.limits.notes.is_empty());
    drop(read);
    // Nothing written.
    let after = b.db.begin_read().unwrap().stats(NS).unwrap();
    assert_eq!((after.nodes, after.edges), (before.nodes, before.edges));
}

#[test]
fn a_topic_with_nothing_says_so() {
    let b = billing();
    let read = b.db.begin_read().unwrap();
    let found = reflect(
        &read,
        NS,
        &Reflect::Topic("volcanoes".into()),
        &ReflectOptions::default(),
    )
    .unwrap();
    assert!(found.nothing);
    assert_eq!(found.known.memories, 0);
    assert!(found.uncertain.is_empty() && found.stale.is_empty() && found.gaps.is_empty());
    assert!(found.conflicts.is_empty() && found.next.is_empty());
    assert!(found.limits.notes[0].contains("Nothing stored"));
}

#[test]
fn a_room_and_a_node_are_reflected_on_too() {
    let b = billing();
    let read = b.db.begin_read().unwrap();
    let room = reflect(
        &read,
        NS,
        &Reflect::Room(b.room),
        &ReflectOptions::default(),
    )
    .unwrap();
    assert_eq!(room.known.memories, 1);
    // Its empty place and what its checkpoint left to do.
    let gaps: Vec<&str> = room.gaps.iter().map(|item| item.why.as_str()).collect();
    assert!(
        gaps.iter().any(|why| why.contains("nothing at it")),
        "{gaps:?}"
    );
    assert!(
        gaps.iter().any(|why| why.contains("left to do")),
        "{gaps:?}"
    );
    // The service is in the room: its flagged owners show here too.
    assert_eq!(room.conflicts.len(), 1);

    let node = reflect(
        &read,
        NS,
        &Reflect::Node(b.open_question),
        &ReflectOptions::default(),
    )
    .unwrap();
    assert!(ids(&node.gaps).contains(&b.open_question.to_string().as_str()));
    let wrong = reflect(
        &read,
        NS,
        &Reflect::Room(b.open_question),
        &ReflectOptions::default(),
    );
    assert!(wrong.is_err());
    // Lists are cut, totals kept.
    let small = reflect(
        &read,
        NS,
        &Reflect::Topic("billing".into()),
        &ReflectOptions {
            limit: 1,
            ..ReflectOptions::default()
        },
    )
    .unwrap();
    assert_eq!(small.stale.len(), 1);
    assert!(small.totals["stale"] >= 3);
}
