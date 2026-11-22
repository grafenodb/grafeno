//! A memory something newer replaced (`supersedes`) is read as replaced:
//! entering its room puts the current one first and says, on the replaced
//! item, what replaced it; reflecting on the room lists it as stale with
//! the reason; a context made by recall marks it too. When the replacement
//! no longer holds, the older memory is current again.

use grafeno::{Grafeno, NewEdge, NewNode, NodeId, Timestamp};
use grafeno_memory::palace::ENTRANCE;
use grafeno_memory::{
    ContextOptions, EnterOptions, NewRoom, RecallQuery, ReflectOptions, Scope, anchor, context_for,
    create_palace, create_room, enter, find_locus, reflect,
};

const NS: &str = "pruebas";

struct Room {
    db: Grafeno,
    room: NodeId,
    older: NodeId,
    newer: NodeId,
    replacement: grafeno::EdgeId,
}

/// Two billing decisions in one room; the newer replaces the older, which
/// is the more important one (so strength alone would put it first).
fn room() -> Room {
    let db = Grafeno::in_memory().unwrap();
    let (room, older, newer, replacement) = db
        .write(|tx| {
            let palace = create_palace(tx, NS, "Trabajo", None)?.id;
            let room = create_room(tx, NS, palace, NewRoom::new("Facturación"))?.id;
            let older = tx.create_node(
                NS,
                NewNode::new("Decision")
                    .prop("name", "Monthly invoices")
                    .text("Invoices are sent once a month, on the first day"),
            )?;
            let newer = tx.create_node(
                NS,
                NewNode::new("Decision")
                    .prop("name", "Weekly invoices")
                    .text("Invoices are sent every Monday"),
            )?;
            tx.set_importance(NS, older, 0.9)?;
            tx.set_importance(NS, newer, 0.2)?;
            let replacement = tx.create_edge(
                NS,
                NewEdge::new(newer, "supersedes", older)
                    .prop("reason", "customers asked for weekly invoices"),
            )?;
            Ok((room, older, newer, replacement))
        })
        .unwrap();
    let entrance = find_locus(&db.begin_read().unwrap(), NS, room, ENTRANCE)
        .unwrap()
        .unwrap();
    db.write(|tx| {
        anchor(tx, NS, older, entrance, None)?;
        anchor(tx, NS, newer, entrance, None)?;
        Ok(())
    })
    .unwrap();
    Room {
        db,
        room,
        older,
        newer,
        replacement,
    }
}

#[test]
fn entering_a_room_puts_the_current_memory_first_and_marks_the_replaced_one() {
    let r = room();
    let read = r.db.begin_read().unwrap();
    let entered = enter(&read, NS, r.room, &EnterOptions::default()).unwrap();
    let items: Vec<NodeId> = entered.context.items.iter().map(|i| i.node).collect();
    assert_eq!(items, [r.newer, r.older]);
    assert!(entered.context.items[0].superseded_by.is_empty());
    assert_eq!(entered.context.items[1].superseded_by, [r.newer]);
    let rendered = &entered.context.rendered;
    assert!(
        rendered.contains("[2] Monthly invoices (Decision; superseded by Weekly invoices)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("[1] Weekly invoices (Decision)"),
        "{rendered}"
    );
}

#[test]
fn reflecting_on_the_room_lists_the_replaced_memory_as_stale() {
    let r = room();
    let read = r.db.begin_read().unwrap();
    let found = reflect(&read, NS, &Scope::Room(r.room), &ReflectOptions::default()).unwrap();
    let older = r.older.to_string();
    let stale: Vec<_> = found.stale.iter().filter(|item| item.id == older).collect();
    assert_eq!(stale.len(), 1, "{:#?}", found.stale);
    assert!(stale[0].why.starts_with("replaced by"), "{}", stale[0].why);
    assert!(
        stale[0].why.contains(&r.newer.to_string()),
        "{}",
        stale[0].why
    );
    assert!(
        stale[0].why.contains("customers asked for weekly invoices"),
        "{}",
        stale[0].why
    );
    // The current one is not stale, and replacing it was decided already:
    // no step asks whether it is overtaken.
    assert!(
        !found
            .stale
            .iter()
            .any(|item| item.id == r.newer.to_string())
    );
    assert!(
        !found
            .next
            .iter()
            .any(|step| step.about.contains(&older) && step.what.contains("overtake")),
        "{:#?}",
        found.next
    );
}

#[test]
fn a_recalled_context_marks_it_too() {
    let r = room();
    let read = r.db.begin_read().unwrap();
    let context = context_for(
        &read,
        NS,
        &RecallQuery::new().text("invoices"),
        &ContextOptions::default(),
    )
    .unwrap();
    let older = context
        .items
        .iter()
        .find(|item| item.node == r.older)
        .expect("recalled");
    assert_eq!(older.superseded_by, [r.newer]);
    assert!(context.rendered.contains("superseded by Weekly invoices"));
}

#[test]
fn when_the_replacement_ends_the_older_memory_is_current_again() {
    let r = room();
    r.db.write(|tx| tx.invalidate_edge(NS, r.replacement, Timestamp::now()))
        .unwrap();
    let read = r.db.begin_read().unwrap();
    let entered = enter(&read, NS, r.room, &EnterOptions::default()).unwrap();
    // Strength decides again: the older one is the more important.
    let items: Vec<NodeId> = entered.context.items.iter().map(|i| i.node).collect();
    assert_eq!(items, [r.older, r.newer]);
    assert!(
        entered
            .context
            .items
            .iter()
            .all(|i| i.superseded_by.is_empty())
    );
    assert!(!entered.context.rendered.contains("superseded by"));
    let found = reflect(&read, NS, &Scope::Room(r.room), &ReflectOptions::default()).unwrap();
    assert!(
        !found
            .stale
            .iter()
            .any(|item| item.why.starts_with("replaced by"))
    );
}
