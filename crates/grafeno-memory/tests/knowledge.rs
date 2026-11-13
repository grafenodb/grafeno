//! How memories and facts are known: certainty and origin, an agent's
//! reading of a text in one call, and verifying.

use grafeno::{
    Constraint, Direction, EdgeFilter, EdgeId, Error, Grafeno, NewEdge, NewNode, NodeId,
    OnConflict, Scope, Timestamp,
};
use grafeno_memory::knowledge::{self, VERIFIED_FROM};
use grafeno_memory::{
    AgentMention, AgentRelation, ContextOptions, MentionRef, Origin, RecallQuery, Record,
    context_for, ingest_reading, verify,
};
use serde_json::json;

const NS: &str = "agent";

fn mention(surface: &str, label: &str) -> AgentMention {
    AgentMention {
        surface: surface.into(),
        label: Some(label.into()),
        ..AgentMention::default()
    }
}

fn relation(from: MentionRef, label: &str, to: MentionRef) -> AgentRelation {
    AgentRelation {
        from,
        label: label.into(),
        to,
        certainty: None,
        origin: None,
        valid_from: None,
        valid_to: None,
    }
}

#[test]
fn origin_and_certainty_are_checked_and_read_back() {
    let props = knowledge::stated(Some("inferred"), Some(0.6)).unwrap();
    assert_eq!(props["origin"], "inferred");
    assert_eq!(props["certainty"], 0.6);
    assert!(knowledge::stated(None, None).unwrap().is_empty());
    for (origin, certainty) in [
        (Some("guessed"), None),
        (None, Some(1.5)),
        (None, Some(-0.1)),
    ] {
        let result = knowledge::stated(origin, certainty);
        assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    }
    assert!(knowledge::stated(None, Some(f64::NAN)).is_err());

    let db = Grafeno::in_memory().unwrap();
    let (old, new) = db
        .write(|tx| {
            // Written before origins existed, or by someone who did not say.
            let old = tx.create_node(NS, NewNode::new("Note").text("Something said once"))?;
            let mut node = NewNode::new("Fact").text("The cache holds an hour");
            node.props = knowledge::stated(Some("observed"), Some(0.9))?;
            Ok((old, tx.create_node(NS, node)?))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let old = knowledge::of_node(&read.node(NS, old).unwrap().unwrap());
    assert!(old.is_unstated());
    assert_eq!(old.note(), "");
    assert_eq!(old.to_json(), json!("not stated"));
    let new = knowledge::of_node(&read.node(NS, new).unwrap().unwrap());
    assert_eq!(new.origin, Some(Origin::Observed));
    assert_eq!(new.certainty, Some(0.9));
    assert_eq!(new.note(), "observed, certainty 0.9");
    assert_eq!(
        new.to_json(),
        json!({ "origin": "observed", "certainty": 0.9 })
    );
}

/// A technical description, and an agent's reading of it.
fn described(db: &Grafeno) -> NodeId {
    db.write(|tx| {
        tx.create_node(
            NS,
            NewNode::new("Document").label("Fact").text(
                "The billing service runs on Ledger. Ana Torres maintains it, and it \
                 publishes invoices to the events queue.",
            ),
        )
    })
    .unwrap()
}

fn reading() -> (Vec<AgentMention>, Vec<AgentRelation>) {
    let mentions = vec![
        mention("billing service", "Service"),
        mention("Ledger", "Database"),
        mention("Ana Torres", "Person"),
        mention("events queue", "Queue"),
    ];
    let mut runs_on = relation(MentionRef::Index(0), "runs_on", MentionRef::Index(1));
    runs_on.origin = Some("stated".into());
    runs_on.certainty = Some(0.95);
    let maintains = relation(
        MentionRef::Surface("ana torres".into()),
        "maintains",
        MentionRef::Surface("Billing Service".into()),
    );
    let mut publishes = relation(MentionRef::Index(0), "publishes_to", MentionRef::Index(3));
    publishes.certainty = Some(0.6);
    (mentions, vec![runs_on, maintains, publishes])
}

fn out_fact(db: &Grafeno, from: NodeId, label: &str) -> (EdgeId, NodeId) {
    let read = db.begin_read().unwrap();
    let found = read
        .neighbor_ids(NS, from, Direction::Out, &EdgeFilter::any().label(label))
        .unwrap();
    assert_eq!(found.len(), 1, "{label}");
    found[0]
}

#[test]
fn an_agents_reading_stores_entities_and_relations_with_their_source() {
    let db = Grafeno::in_memory().unwrap();
    let doc = described(&db);
    let (mentions, relations) = reading();
    let read = db.begin_read().unwrap();
    let result = db
        .write(|tx| ingest_reading(tx, &read, NS, doc, &mentions, &relations, "Entity"))
        .unwrap();
    drop(read);
    assert_eq!(result.report.created, 4);
    assert_eq!(result.report.mentions, 4);
    assert_eq!(result.relations.len(), 3);
    assert!(result.skipped.is_empty());
    let [service, ledger, ana, queue] = [0, 1, 2, 3].map(|i| result.entities[i].unwrap());

    let read = db.begin_read().unwrap();
    let node = read.node(NS, service).unwrap().unwrap();
    assert!(node.has_label("Service"));
    assert_eq!(node.prop("name").unwrap(), "billing service");
    assert_eq!(node.sources, [doc]);

    // Each relation is a fact from one entity to the other, with the text
    // as its source and how it is known.
    let (runs_on, to) = out_fact(&db, service, "runs_on");
    assert_eq!(to, ledger);
    let edge = read.edge(NS, runs_on).unwrap().unwrap();
    assert_eq!(edge.sources, [doc]);
    let known = knowledge::of_edge(&edge);
    assert_eq!(
        (known.origin, known.certainty),
        (Some(Origin::Stated), Some(0.95))
    );

    let (maintains, to) = out_fact(&db, ana, "maintains");
    assert_eq!(to, service);
    let known = knowledge::of_edge(&read.edge(NS, maintains).unwrap().unwrap());
    // Inferred unless the agent says the text states it.
    assert_eq!(
        (known.origin, known.certainty),
        (Some(Origin::Inferred), None)
    );

    let (publishes, to) = out_fact(&db, service, "publishes_to");
    assert_eq!(to, queue);
    let known = knowledge::of_edge(&read.edge(NS, publishes).unwrap().unwrap());
    assert_eq!(known.note(), "inferred, certainty 0.6");

    // The text mentions each entity.
    let mentioned = read
        .neighbor_ids(
            NS,
            doc,
            Direction::Out,
            &EdgeFilter::any().label("mentions"),
        )
        .unwrap();
    assert_eq!(mentioned.len(), 4);

    // The context says how each fact is known; one nobody qualified reads
    // as before.
    let context = context_for(
        &read,
        NS,
        &RecallQuery::new().text("billing service"),
        &ContextOptions::with_budget(2_000),
    )
    .unwrap();
    let rendered = &context.rendered;
    assert!(
        rendered.contains("billing service runs_on Ledger [stated, certainty 0.95]"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Ana Torres maintains billing service [inferred]"),
        "{rendered}"
    );

    // Reading the same text again links the entities there are.
    drop(read);
    let read = db.begin_read().unwrap();
    let again = db
        .write(|tx| ingest_reading(tx, &read, NS, doc, &mentions, &[], "Entity"))
        .unwrap();
    assert_eq!((again.report.created, again.report.linked), (0, 4));
    assert_eq!(again.entities, result.entities);
}

#[test]
fn relations_that_cannot_be_placed_are_skipped_and_bad_ones_refused() {
    let db = Grafeno::in_memory().unwrap();
    let doc = described(&db);
    // Two people named Ana Torres: the mention waits in review.
    db.write(|tx| {
        tx.create_node(NS, NewNode::new("Person").prop("name", "Ana Torres"))?;
        tx.create_node(NS, NewNode::new("Person").prop("name", "Ana Torres"))
    })
    .unwrap();
    let (mentions, mut relations) = reading();
    relations.push(relation(
        MentionRef::Index(9),
        "depends_on",
        MentionRef::Index(1),
    ));
    let read = db.begin_read().unwrap();
    let result = db
        .write(|tx| ingest_reading(tx, &read, NS, doc, &mentions, &relations, "Entity"))
        .unwrap();
    drop(read);
    assert_eq!(result.report.queued, 1);
    assert_eq!(result.entities[2], None);
    assert_eq!(result.relations.len(), 2);
    let skipped: Vec<usize> = result.skipped.iter().map(|(index, _)| *index).collect();
    assert_eq!(skipped, [1, 3]);
    assert!(result.skipped[0].1.contains("review"));
    assert!(result.skipped[1].1.contains("not in this call"));

    // A bad relation refuses the whole call before anything is written.
    let before = db.begin_read().unwrap().nodes(NS).unwrap().len();
    for bad in [
        AgentRelation {
            certainty: Some(2.0),
            ..relation(MentionRef::Index(0), "runs_on", MentionRef::Index(1))
        },
        AgentRelation {
            origin: Some("rumoured".into()),
            ..relation(MentionRef::Index(0), "runs_on", MentionRef::Index(1))
        },
        relation(MentionRef::Index(0), "  ", MentionRef::Index(1)),
    ] {
        let read = db.begin_read().unwrap();
        let result = db.write(|tx| ingest_reading(tx, &read, NS, doc, &mentions, &[bad], "Entity"));
        assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    }
    assert_eq!(db.begin_read().unwrap().nodes(NS).unwrap().len(), before);
}

#[test]
fn a_relation_runs_into_the_constraints_there_are() {
    let db = Grafeno::in_memory().unwrap();
    let doc = described(&db);
    // A service runs on one thing at a time: a newer fact ends the older.
    db.write(|tx| {
        tx.create_constraint(
            NS,
            Constraint::Edge {
                name: "one_host".into(),
                label: "runs_on".into(),
                scope: Scope::Source,
                on_conflict: OnConflict::Invalidate,
            },
        )
    })
    .unwrap();
    let (mentions, mut relations) = reading();
    let mut moved = relation(MentionRef::Index(0), "runs_on", MentionRef::Index(3));
    moved.valid_from = Some(Timestamp::from_secs(1_700_000_000));
    relations.push(moved);
    let read = db.begin_read().unwrap();
    let result = db
        .write(|tx| ingest_reading(tx, &read, NS, doc, &mentions, &relations, "Entity"))
        .unwrap();
    // The clash is reported, and the earlier fact ends; neither is lost.
    assert_eq!(result.relations.len(), 4);
    assert_eq!(result.contradicted.len(), 1, "{:?}", result.contradicted);
    assert_eq!(result.contradicted[0].constraint, "one_host");
    let service = result.entities[0].unwrap();
    let read = db.begin_read().unwrap();
    let all = read
        .neighbors(
            NS,
            service,
            Direction::Out,
            &EdgeFilter::any().label("runs_on"),
        )
        .unwrap();
    assert_eq!(all.len(), 2);
    let ended: Vec<bool> = all.iter().map(|n| n.edge.valid_to.is_some()).collect();
    assert_eq!(ended.iter().filter(|ended| **ended).count(), 1);
}

#[test]
fn verifying_records_the_source_and_what_it_was() {
    let db = Grafeno::in_memory().unwrap();
    let (memory, other, fact, page) = db
        .write(|tx| {
            let mut node = NewNode::new("Fact").text("The cache holds an hour");
            node.props = knowledge::stated(Some("inferred"), Some(0.5))?;
            let memory = tx.create_node(NS, node)?;
            let other = tx.create_node(NS, NewNode::new("Note").text("Nobody said how"))?;
            let page = tx.create_node(NS, NewNode::new("Document").text("cache.ttl = 3600"))?;
            let mut edge = NewEdge::new(memory, "about", other);
            edge.props = knowledge::stated(Some("inferred"), None)?;
            let fact = tx.create_edge(NS, edge)?;
            Ok((memory, other, fact, page))
        })
        .unwrap();
    let now = Timestamp::now();

    let known = db
        .write(|tx| verify(tx, NS, Record::Node(memory), Some(page), None, now))
        .unwrap();
    assert_eq!(known.origin, Some(Origin::Verified));
    assert_eq!(known.certainty, Some(1.0));
    assert_eq!(known.verified_by, Some(page));
    assert_eq!(known.verified_at.as_deref(), Some(now.date().as_str()));
    let node = db.begin_read().unwrap().node(NS, memory).unwrap().unwrap();
    assert_eq!(
        node.props[VERIFIED_FROM],
        json!({ "origin": "inferred", "certainty": 0.5 })
    );

    // Verified again with no source: the earlier source does not carry over.
    let known = db
        .write(|tx| verify(tx, NS, Record::Node(memory), None, Some(0.8), now))
        .unwrap();
    assert_eq!((known.verified_by, known.certainty), (None, Some(0.8)));

    // A fact, and a memory that was never qualified.
    let known = db
        .write(|tx| verify(tx, NS, Record::Edge(fact), Some(page), None, now))
        .unwrap();
    assert_eq!(known.origin, Some(Origin::Verified));
    let edge = db.begin_read().unwrap().edge(NS, fact).unwrap().unwrap();
    assert_eq!(
        edge.props[VERIFIED_FROM],
        json!({ "origin": "inferred", "certainty": null })
    );
    db.write(|tx| verify(tx, NS, Record::Node(other), Some(page), None, now))
        .unwrap();
    let node = db.begin_read().unwrap().node(NS, other).unwrap().unwrap();
    assert_eq!(
        node.props[VERIFIED_FROM],
        json!({ "origin": null, "certainty": null })
    );

    // Refused: itself as source, a source or record that is not there, a
    // bad certainty.
    let result = db.write(|tx| verify(tx, NS, Record::Node(page), Some(page), None, now));
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
    let missing = NodeId::new();
    let result = db.write(|tx| verify(tx, NS, Record::Node(memory), Some(missing), None, now));
    assert!(matches!(result, Err(Error::NodeNotFound(id)) if id == missing));
    let result = db.write(|tx| verify(tx, NS, Record::Node(missing), None, None, now));
    assert!(matches!(result, Err(Error::NodeNotFound(_))));
    let result = db.write(|tx| verify(tx, "other", Record::Node(memory), None, None, now));
    assert!(matches!(result, Err(Error::NodeNotFound(_))));
    let result = db.write(|tx| verify(tx, NS, Record::Node(memory), None, Some(3.0), now));
    assert!(matches!(result, Err(Error::Invalid(_))));
}

#[test]
fn certainty_and_origin_survive_a_restart() {
    let path = std::env::temp_dir().join(format!(
        "grafeno-knowledge-{}-{}.redb",
        std::process::id(),
        NodeId::new()
    ));
    let (doc, entities) = {
        let db = Grafeno::open(&path).unwrap();
        let doc = described(&db);
        let (mentions, relations) = reading();
        let read = db.begin_read().unwrap();
        let result = db
            .write(|tx| ingest_reading(tx, &read, NS, doc, &mentions, &relations, "Entity"))
            .unwrap();
        (doc, result.entities)
    };
    let db = Grafeno::open(&path).unwrap();
    {
        let (runs_on, _) = out_fact(&db, entities[0].unwrap(), "runs_on");
        let read = db.begin_read().unwrap();
        let edge = read.edge(NS, runs_on).unwrap().unwrap();
        assert_eq!(edge.sources, [doc]);
        assert_eq!(knowledge::of_edge(&edge).note(), "stated, certainty 0.95");
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
