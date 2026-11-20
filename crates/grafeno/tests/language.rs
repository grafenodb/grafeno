//! A namespace's language: node texts and questions are reduced to their
//! stem in it ("decisiones" finds "decisión"), changing it indexes the
//! texts again, and a replica does the same.

use grafeno::{Grafeno, Language, NewNode, NodeId};

const NS: &str = "notas";

fn found(db: &Grafeno, query: &str) -> Vec<NodeId> {
    db.begin_read()
        .unwrap()
        .search_text(NS, query, 10)
        .unwrap()
        .into_iter()
        .map(|hit| hit.node)
        .collect()
}

fn notes(db: &Grafeno) -> (NodeId, NodeId) {
    db.write(|tx| {
        tx.create_namespace(NS, None)?;
        let decision = tx.create_node(
            NS,
            NewNode::new("Decision").text("La decisión: migramos la facturación al servidor nuevo"),
        )?;
        let other = tx.create_node(
            NS,
            NewNode::new("Memory").text("El equipo prefiere revisiones cortas"),
        )?;
        Ok((decision, other))
    })
    .unwrap()
}

fn healthy(db: &Grafeno) {
    let report = db.begin_read().unwrap().check(NS).unwrap();
    assert!(report.is_ok(), "{report:?}");
}

#[test]
fn spanish_words_find_their_other_forms() {
    let db = Grafeno::in_memory().unwrap();
    let (decision, other) = notes(&db);

    // Without a language, words are kept whole.
    assert!(found(&db, "decisiones").is_empty());
    assert!(found(&db, "revisión").is_empty());

    // In Spanish, plurals and accents meet.
    let indexed = db
        .write(|tx| tx.set_namespace_language(NS, Some(Language::Spanish)))
        .unwrap();
    assert_eq!(indexed, 2);
    healthy(&db);
    assert_eq!(found(&db, "decisiones"), vec![decision]);
    assert_eq!(found(&db, "revisión"), vec![other]);
    assert_eq!(found(&db, "migrar facturacion"), vec![decision]);
    let info = db.begin_read().unwrap().namespace(NS).unwrap().unwrap();
    assert_eq!(info.language, Some(Language::Spanish));

    // Texts written, changed and removed afterwards are indexed in it.
    let third = db
        .write(|tx| {
            tx.create_node(
                NS,
                NewNode::new("Decision").text("Decidimos publicar los viernes"),
            )
        })
        .unwrap();
    assert_eq!(found(&db, "publicaremos"), vec![third]);
    db.write(|tx| tx.set_text(NS, other, "Revisiones largas, nunca"))
        .unwrap();
    assert_eq!(found(&db, "revisión"), vec![other]);
    db.write(|tx| tx.remove_text(NS, third)).unwrap();
    assert!(found(&db, "publicaremos").is_empty());
    healthy(&db);

    // Back to none: whole words again.
    db.write(|tx| tx.set_namespace_language(NS, None)).unwrap();
    healthy(&db);
    assert!(found(&db, "decisiones").is_empty());
    assert_eq!(found(&db, "decisión"), vec![decision]);
}

#[test]
fn a_replica_indexes_its_texts_in_the_new_language() {
    let source = Grafeno::in_memory().unwrap();
    source.enable_replication_log().unwrap();
    let (decision, _) = notes(&source);
    source
        .write(|tx| tx.set_namespace_language(NS, Some(Language::Spanish)))
        .unwrap();

    let replica = Grafeno::in_memory().unwrap();
    let entries = source
        .begin_read()
        .unwrap()
        .log_entries(0, usize::MAX)
        .unwrap();
    replica.apply_entries(&entries).unwrap();
    healthy(&replica);
    assert_eq!(found(&replica, "decisiones"), vec![decision]);
}
