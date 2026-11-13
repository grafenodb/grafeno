//! Phase 3 of the plan: retrieval. Contexts leave out what repeats; walks
//! order by relevance and pack what they reach, with the facts that join
//! it; doors lead from a room to another.

use grafeno::{Grafeno, NewEdge, NewNode, NodeId};
use grafeno_memory::palace::ENTRANCE;
use grafeno_memory::{
    ContextOptions, DOOR, MapOptions, NewRoom, REDUNDANT, RecallQuery, WalkOrder, WalkQuery,
    anchor, context_for, context_of, create_palace, create_room, find_locus, map_room, walk,
};

const NS: &str = "agent";

fn note(tx: &mut grafeno::WriteTx, text: &str) -> grafeno::Result<NodeId> {
    tx.create_node(NS, NewNode::new("Note").text(text))
}

#[test]
fn a_context_leaves_out_what_repeats_what_it_holds() {
    let db = Grafeno::in_memory().unwrap();
    let (first, again, other) = db
        .write(|tx| {
            let first = note(tx, "Restart the billing consumer with systemctl restart")?;
            // The same chunk, ingested twice with other punctuation.
            let again = note(tx, "Restart the billing consumer, with systemctl restart.")?;
            let other = note(tx, "Billing invoices go out at night")?;
            Ok((first, again, other))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let query = RecallQuery::new().text("billing consumer restart invoices");

    // As before by default: every item.
    let all = context_for(&read, NS, &query, &ContextOptions::with_budget(2_000)).unwrap();
    assert_eq!(all.items.len(), 3);
    assert!(all.redundant.is_empty());

    let mut options = ContextOptions::with_budget(2_000);
    options.redundancy = Some(REDUNDANT);
    let one = context_for(&read, NS, &query, &options).unwrap();
    let items: Vec<NodeId> = one.items.iter().map(|item| item.node).collect();
    assert_eq!(items.len(), 2, "{items:?}");
    assert!(items.contains(&other));
    assert_eq!(one.redundant.len(), 1);
    let (left_out, kept) = one.redundant[0];
    assert!(items.contains(&kept));
    assert!([first, again].contains(&left_out) && left_out != kept);
    assert!(one.tokens < all.tokens);

    // An item that did not fit is not what a later one repeats.
    let nodes: Vec<(grafeno::Node, f32)> = [first, again]
        .iter()
        .map(|id| (read.node(NS, *id).unwrap().unwrap(), 1.0))
        .collect();
    let mut tiny = ContextOptions::with_budget(10);
    tiny.redundancy = Some(REDUNDANT);
    tiny.fact_share = 0.0;
    let packed = context_of(&read, NS, nodes, &tiny).unwrap();
    assert!(packed.items.is_empty());
    assert!(packed.redundant.is_empty());
}

/// An incident in one room, the service it hit two relations away and who
/// maintains that service: a question that needs three memories joined.
struct Incident {
    db: Grafeno,
    palace: NodeId,
    ops: NodeId,
    outage: NodeId,
    service: NodeId,
    ana: NodeId,
    lunch: NodeId,
}

fn incident() -> Incident {
    let db = Grafeno::in_memory().unwrap();
    let ids = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Work", None)?.id;
            let ops = create_room(
                tx,
                NS,
                palace,
                NewRoom::new("Operations").topic("incidents"),
            )?
            .id;
            let outage = note(tx, "The night outage stopped the invoices")?;
            let service = tx.create_node(
                NS,
                NewNode::new("Service")
                    .prop("name", "Billing service")
                    .text("Billing service: sends the invoices"),
            )?;
            let ana = tx.create_node(
                NS,
                NewNode::new("Person")
                    .prop("name", "Ana")
                    .text("Ana keeps the billing service and its invoices running"),
            )?;
            let lunch = note(tx, "The team lunch is on Friday")?;
            tx.create_edge(NS, NewEdge::new(outage, "hit", service))?;
            tx.create_edge(NS, NewEdge::new(ana, "maintains", service))?;
            tx.create_edge(NS, NewEdge::new(outage, "reported_at", lunch))?;
            Ok((palace, ops, outage, service, ana, lunch))
        })
        .unwrap();
    let (palace, ops, outage, service, ana, lunch) = ids;
    let entrance = find_locus(&db.begin_read().unwrap(), NS, ops, ENTRANCE)
        .unwrap()
        .unwrap();
    db.write(|tx| anchor(tx, NS, outage, entrance, None).map(drop))
        .unwrap();
    Incident {
        db,
        palace,
        ops,
        outage,
        service,
        ana,
        lunch,
    }
}

#[test]
fn a_walk_by_relevance_puts_what_matches_the_topic_first() {
    let f = incident();
    let read = f.db.begin_read().unwrap();
    let ids = |walk: &grafeno_memory::Walk| -> Vec<NodeId> {
        walk.memories.iter().map(|memory| memory.id).collect()
    };

    // By distance, as before: the lunch, one relation away, comes before
    // Ana, two away.
    let near = walk(&read, NS, &WalkQuery::topic("incidents invoices")).unwrap();
    assert_eq!(ids(&near)[0], f.outage);
    let at = |id| ids(&near).iter().position(|x| *x == id).unwrap();
    assert!(at(f.lunch) < at(f.ana));
    assert!(
        near.memories
            .iter()
            .all(|memory| memory.relevance.is_none())
    );

    // By relevance: what speaks of invoices first, wherever it is, and
    // the lunch, which does not, last.
    let relevant = walk(
        &read,
        NS,
        &WalkQuery::topic("incidents invoices").order(WalkOrder::Relevance),
    )
    .unwrap();
    let order = ids(&relevant);
    assert_eq!(order.len(), 4);
    assert_eq!(*order.last().unwrap(), f.lunch);
    // Its words match nothing: its relevance is what it inherits from the
    // outage it was reached from, half per relation, below the others'.
    let lunch = relevant.memories.last().unwrap();
    let outage = relevant.memories.iter().find(|m| m.id == f.outage).unwrap();
    assert_eq!(lunch.relevance, Some(outage.relevance.unwrap() * 0.5));
    for memory in &relevant.memories[..3] {
        assert!(
            memory.relevance.unwrap() > lunch.relevance.unwrap(),
            "{memory:?}"
        );
    }
    // The way each was reached is kept.
    let ana = relevant.memories.iter().find(|m| m.id == f.ana).unwrap();
    assert_eq!(ana.hops, 2);
    let way: Vec<&str> = ana.via.iter().map(|hop| hop.label.as_str()).collect();
    assert_eq!(way, ["hit", "maintains"]);

    // A limit keeps the most relevant.
    let two = walk(
        &read,
        NS,
        &WalkQuery::topic("incidents invoices")
            .order(WalkOrder::Relevance)
            .limit(2),
    )
    .unwrap();
    assert_eq!(two.memories.len(), 2);
    assert!(!ids(&two).contains(&f.lunch));
}

#[test]
fn a_walk_packs_the_memories_it_joined_with_the_facts_between_them() {
    let f = incident();
    let read = f.db.begin_read().unwrap();
    // "Who maintains what the outage hit?": three memories, two facts.
    let found = walk(
        &read,
        NS,
        &WalkQuery::topic("incidents")
            .order(WalkOrder::Relevance)
            .budget(400),
    )
    .unwrap();
    let context = found.context.expect("a context");
    assert!(context.tokens <= 400, "{}", context.tokens);
    let items: Vec<NodeId> = context.items.iter().map(|item| item.node).collect();
    for id in [f.outage, f.service, f.ana] {
        assert!(items.contains(&id), "{items:?}");
    }
    let facts: Vec<&str> = context
        .facts
        .iter()
        .map(|fact| fact.label.as_str())
        .collect();
    assert!(
        facts.contains(&"hit") && facts.contains(&"maintains"),
        "{facts:?}"
    );
    let rendered = &context.rendered;
    assert!(
        rendered.contains("Ana maintains Billing service"),
        "{rendered}"
    );
    // Without a budget, no context, as before.
    let plain = walk(&read, NS, &WalkQuery::topic("incidents")).unwrap();
    assert!(plain.context.is_none());
    // A small budget is kept.
    let small = walk(&read, NS, &WalkQuery::topic("incidents").budget(40)).unwrap();
    assert!(small.context.unwrap().tokens <= 40);
}

#[test]
fn doors_lead_a_walk_from_a_room_to_another() {
    let f = incident();
    let (billing, invoices, home, cook) =
        f.db.write(|tx| {
            let billing = create_room(tx, NS, f.palace, NewRoom::new("Billing"))?.id;
            let home = create_palace(tx, NS, "Home", None)?.id;
            let kitchen = create_room(tx, NS, home, NewRoom::new("Kitchen"))?.id;
            let invoices = note(tx, "Invoices are numbered by year")?;
            let cook = note(tx, "Soup on Sundays")?;
            // A door from Operations to Billing, and one to another palace.
            tx.create_edge(NS, NewEdge::new(f.ops, DOOR, billing))?;
            tx.create_edge(NS, NewEdge::new(f.ops, DOOR, kitchen))?;
            Ok((billing, invoices, home, cook))
        })
        .unwrap();
    let read = f.db.begin_read().unwrap();
    let billing_entrance = find_locus(&read, NS, billing, ENTRANCE).unwrap().unwrap();
    let kitchen = grafeno_memory::find_room(&read, NS, home, "Kitchen")
        .unwrap()
        .unwrap();
    let kitchen_entrance = find_locus(&read, NS, kitchen, ENTRANCE).unwrap().unwrap();
    drop(read);
    f.db.write(|tx| {
        anchor(tx, NS, invoices, billing_entrance, None)?;
        anchor(tx, NS, cook, kitchen_entrance, None)?;
        Ok(())
    })
    .unwrap();
    let read = f.db.begin_read().unwrap();

    // The map of a room names its doors.
    let ops = map_room(&read, NS, f.ops, &MapOptions::default()).unwrap();
    let doors: Vec<&str> = ops.doors.iter().map(|door| door.path.as_str()).collect();
    assert_eq!(doors, ["Work / Billing", "Home / Kitchen"]);

    // A walk to Operations goes through its doors, one step.
    let found = walk(&read, NS, &WalkQuery::topic("incidents").hops(1)).unwrap();
    let through: Vec<(&str, Option<NodeId>)> = found
        .places
        .iter()
        .map(|place| (place.path.as_str(), place.through))
        .collect();
    assert!(
        through.contains(&("Work / Operations", None)),
        "{through:?}"
    );
    assert!(
        through.contains(&("Work / Billing", Some(f.ops))),
        "{through:?}"
    );
    let invoice = found.memories.iter().find(|m| m.id == invoices).unwrap();
    assert_eq!(invoice.hops, 1);
    assert_eq!(invoice.origin, f.ops);
    assert_eq!(invoice.via[0].label, DOOR);
    assert_eq!(invoice.via[0].node, billing);

    // Inside one palace, its doors to another are not taken.
    let inside = walk(
        &read,
        NS,
        &WalkQuery::topic("incidents").hops(1).palace(f.palace),
    )
    .unwrap();
    assert!(inside.memories.iter().any(|m| m.id == invoices));
    assert!(!inside.memories.iter().any(|m| m.id == cook));
    let anywhere = walk(&read, NS, &WalkQuery::topic("incidents").hops(1)).unwrap();
    assert!(anywhere.memories.iter().any(|m| m.id == cook));

    // With no hops, no doors either.
    let here = walk(&read, NS, &WalkQuery::topic("incidents").hops(0)).unwrap();
    assert_eq!(
        here.memories.iter().map(|m| m.id).collect::<Vec<_>>(),
        [f.outage]
    );
    // A `leads_to` between memories is an ordinary fact, followed as one.
    let _ = f.service;
}
