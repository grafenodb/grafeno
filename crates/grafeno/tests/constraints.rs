//! Constraints on facts and properties: a contradicting fact ends the old
//! one in valid time (record time keeps what was believed), or is flagged,
//! or refused; functional properties keep their earlier values.

use grafeno::{
    Action, Actor, Constraint, Direction, EdgeFilter, Error, Grafeno, NewEdge, NewNode, NodeId,
    OnConflict, Scope, Timestamp, Value,
};

const NS: &str = "facts";

fn day(text: &str) -> Timestamp {
    Timestamp::parse(text).unwrap()
}

fn edge_constraint(scope: Scope, on_conflict: OnConflict) -> Constraint {
    Constraint::Edge {
        name: "works_at_one".into(),
        label: "works_at".into(),
        scope,
        on_conflict,
    }
}

fn setup() -> (Grafeno, NodeId, NodeId, NodeId) {
    let db = Grafeno::in_memory().unwrap();
    let ids = db
        .write(|tx| {
            let ana = tx.create_node(NS, NewNode::new("Person").prop("name", "Ana"))?;
            let acme = tx.create_node(NS, NewNode::new("Company").prop("name", "Acme"))?;
            let globex = tx.create_node(NS, NewNode::new("Company").prop("name", "Globex"))?;
            Ok((ana, acme, globex))
        })
        .unwrap();
    (db, ids.0, ids.1, ids.2)
}

fn employers(db: &Grafeno, ana: NodeId, filter: EdgeFilter) -> Vec<NodeId> {
    db.begin_read()
        .unwrap()
        .neighbors(NS, ana, Direction::Out, &filter.label("works_at"))
        .unwrap()
        .into_iter()
        .map(|n| n.node)
        .collect()
}

#[test]
fn a_new_fact_ends_the_old_one_and_history_stays() {
    let (db, ana, acme, globex) = setup();
    db.write(|tx| {
        tx.create_constraint(NS, edge_constraint(Scope::Source, OnConflict::Invalidate))
            .map(|_| ())
    })
    .unwrap();
    db.write(|tx| {
        tx.create_edge(
            NS,
            NewEdge::new(ana, "works_at", acme).valid_from(day("2020-01-01")),
        )
        .map(|_| ())
    })
    .unwrap();
    let before = Timestamp::now();
    let (_, events) = db
        .write(|tx| {
            let id = tx.create_edge(
                NS,
                NewEdge::new(ana, "works_at", globex).valid_from(day("2024-06-01")),
            )?;
            Ok((id, tx.take_constraint_events()))
        })
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].action, Action::Invalidated);
    assert_eq!(events[0].at, Some(day("2024-06-01")));

    // Now one employer; in 2022, the other; and what was believed before.
    assert_eq!(
        employers(&db, ana, EdgeFilter::any().valid_at(Timestamp::now())),
        vec![globex]
    );
    assert_eq!(
        employers(&db, ana, EdgeFilter::any().valid_at(day("2022-01-01"))),
        vec![acme]
    );
    let mut then = employers(
        &db,
        ana,
        EdgeFilter::any()
            .known_at(before)
            .valid_at(day("2025-01-01")),
    );
    then.sort();
    assert_eq!(
        then,
        vec![acme],
        "before the new fact was recorded, Acme held in 2025"
    );
    assert_eq!(
        db.begin_read()
            .unwrap()
            .constraint_events(NS, 10)
            .unwrap()
            .len(),
        1
    );

    // An older fact arriving late ends where the current one starts.
    let (id, events) = db
        .write(|tx| {
            let id = tx.create_edge(
                NS,
                NewEdge::new(ana, "works_at", acme).valid_from(day("2015-01-01")),
            )?;
            Ok((id, tx.take_constraint_events()))
        })
        .unwrap();
    assert!(
        events.iter().all(|e| e.action == Action::Shortened),
        "{events:?}"
    );
    let shortened = db.begin_read().unwrap().edge(NS, id).unwrap().unwrap();
    // Acme again is the same employer, no contradiction; Globex, from
    // 2024, is: the late fact ends where it starts.
    assert_eq!(shortened.valid_to, Some(day("2024-06-01")));
    assert!(db.begin_read().unwrap().check(NS).unwrap().is_ok());
}

#[test]
fn facts_without_dates_start_now_and_the_same_fact_is_no_contradiction() {
    let (db, ana, acme, globex) = setup();
    db.write(|tx| {
        tx.create_edge(NS, NewEdge::new(ana, "works_at", acme))?;
        tx.create_edge(NS, NewEdge::new(ana, "works_at", globex))?;
        // Declared afterwards: the existing contradiction is only flagged.
        let found =
            tx.create_constraint(NS, edge_constraint(Scope::Source, OnConflict::Invalidate))?;
        assert_eq!(found, 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        employers(&db, ana, EdgeFilter::any()).len(),
        2,
        "nothing changed"
    );
    let events = db.begin_read().unwrap().constraint_events(NS, 10).unwrap();
    assert_eq!(events[0].action, Action::Flagged);
    let fresh = Grafeno::in_memory().unwrap();
    let (ana, acme, globex) = fresh
        .write(|tx| {
            let ana = tx.create_node(NS, NewNode::new("Person"))?;
            let acme = tx.create_node(NS, NewNode::new("Company"))?;
            let globex = tx.create_node(NS, NewNode::new("Company"))?;
            tx.create_constraint(NS, edge_constraint(Scope::Source, OnConflict::Invalidate))?;
            tx.create_edge(NS, NewEdge::new(ana, "works_at", acme))?;
            // The same fact again: kept, no contradiction.
            tx.create_edge(NS, NewEdge::new(ana, "works_at", acme))?;
            assert!(tx.take_constraint_events().is_empty());
            Ok((ana, acme, globex))
        })
        .unwrap();
    let id = fresh
        .write(|tx| tx.create_edge(NS, NewEdge::new(ana, "works_at", globex)))
        .unwrap();
    let read = fresh.begin_read().unwrap();
    let new = read.edge(NS, id).unwrap().unwrap();
    assert_eq!(
        new.valid_from,
        Some(new.recorded_at),
        "starts when recorded"
    );
    let current: Vec<NodeId> = read
        .neighbors(
            NS,
            ana,
            Direction::Out,
            &EdgeFilter::any().valid_at(Timestamp::now()),
        )
        .unwrap()
        .into_iter()
        .map(|n| n.node)
        .collect();
    assert_eq!(current, vec![globex]);
    let _ = acme;
}

#[test]
fn flag_reject_target_pair_and_own_only() {
    let (db, ana, acme, globex) = setup();
    db.write(|tx| {
        tx.create_constraint(NS, edge_constraint(Scope::Source, OnConflict::Reject))
            .map(|_| ())
    })
    .unwrap();
    db.write(|tx| {
        tx.create_edge(NS, NewEdge::new(ana, "works_at", acme))
            .map(|_| ())
    })
    .unwrap();
    assert!(matches!(
        db.write(|tx| tx.create_edge(NS, NewEdge::new(ana, "works_at", globex))),
        Err(Error::Conflict(_))
    ));
    db.write(|tx| {
        tx.drop_constraint(NS, "works_at_one")?;
        tx.create_constraint(NS, edge_constraint(Scope::Source, OnConflict::Flag))?;
        tx.create_edge(NS, NewEdge::new(ana, "works_at", globex))?;
        assert_eq!(tx.take_constraint_events()[0].action, Action::Flagged);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        employers(&db, ana, EdgeFilter::any()).len(),
        2,
        "flagged: both kept"
    );

    // One occupant per seat; one rating per pair.
    let (seat, bob) = db
        .write(|tx| {
            tx.create_constraint(
                NS,
                Constraint::Edge {
                    name: "seat".into(),
                    label: "sits_in".into(),
                    scope: Scope::Target,
                    on_conflict: OnConflict::Invalidate,
                },
            )?;
            tx.create_constraint(
                NS,
                Constraint::Edge {
                    name: "rating".into(),
                    label: "rates".into(),
                    scope: Scope::Pair,
                    on_conflict: OnConflict::Invalidate,
                },
            )?;
            let seat = tx.create_node(NS, NewNode::new("Seat"))?;
            let bob = tx.create_node(NS, NewNode::new("Person"))?;
            tx.create_edge(NS, NewEdge::new(ana, "sits_in", seat))?;
            tx.create_edge(NS, NewEdge::new(bob, "sits_in", seat))?;
            tx.create_edge(NS, NewEdge::new(ana, "rates", acme).prop("stars", 3))?;
            tx.create_edge(NS, NewEdge::new(ana, "rates", acme).prop("stars", 5))?;
            // Another pair is no contradiction.
            tx.create_edge(NS, NewEdge::new(ana, "rates", globex).prop("stars", 1))?;
            let events = tx.take_constraint_events();
            assert_eq!(events.len(), 2, "{events:?}");
            Ok((seat, bob))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let occupants: Vec<NodeId> = read
        .neighbors(
            NS,
            seat,
            Direction::In,
            &EdgeFilter::any()
                .label("sits_in")
                .valid_at(Timestamp::now()),
        )
        .unwrap()
        .into_iter()
        .map(|n| n.node)
        .collect();
    assert_eq!(occupants, vec![bob]);
    let ratings: Vec<Value> = read
        .neighbors(
            NS,
            ana,
            Direction::Out,
            &EdgeFilter::any().label("rates").valid_at(Timestamp::now()),
        )
        .unwrap()
        .into_iter()
        .filter(|n| n.node == acme)
        .map(|n| n.edge.props["stars"].clone())
        .collect();
    assert_eq!(ratings, vec![Value::from(5)]);
    drop(read);

    // An agent limited to its own records cannot end someone else's fact:
    // the contradiction is flagged instead.
    db.write(|tx| {
        tx.act_as(Some(Actor::new("agent").own_only()));
        let carol = tx.create_node(NS, NewNode::new("Person"))?;
        tx.create_edge(NS, NewEdge::new(carol, "sits_in", seat))?;
        let events = tx.take_constraint_events();
        assert_eq!(events[0].action, Action::Flagged, "{events:?}");
        Ok(())
    })
    .unwrap();
}

#[test]
fn functional_properties_keep_their_history() {
    let db = Grafeno::in_memory().unwrap();
    let ana = db
        .write(|tx| {
            tx.create_constraint(
                NS,
                Constraint::Functional {
                    name: "city".into(),
                    label: "Person".into(),
                    key: "city".into(),
                    hyperedges: false,
                },
            )?;
            tx.create_node(NS, NewNode::new("Person").prop("city", "Lima"))
        })
        .unwrap();
    for city in ["Cusco", "Cusco", "Quito"] {
        db.write(|tx| {
            let mut props = grafeno::Props::new();
            props.insert("city".into(), Value::from(city));
            tx.set_node_props(NS, ana, props)
        })
        .unwrap();
    }
    let read = db.begin_read().unwrap();
    let history = read.property_history(NS, ana, "city").unwrap();
    let values: Vec<&Value> = history.iter().map(|h| &h.value).collect();
    assert_eq!(
        values,
        [&Value::from("Lima"), &Value::from("Cusco")],
        "no entry for an unchanged value"
    );
    assert!(history[0].to <= history[1].from && history[1].from <= history[1].to);
    assert_eq!(
        read.node(NS, ana).unwrap().unwrap().props["city"],
        Value::from("Quito")
    );
    assert!(read.property_history(NS, ana, "name").unwrap().is_empty());
}
