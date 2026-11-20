use grafeno::{Direction, EdgeFilter, Grafeno, NewEdge, NewNode, NodeId, Timestamp};
use grafeno_memory::{
    ARCHIVED, ConsolidationOptions, DISTINCT_FROM, DecayOptions, DuplicateOptions, RecallQuery,
    Signal, Summary, archive, consolidation_groups, find_duplicates, recall, weak_memories,
    write_summary,
};

const NS: &str = "agent";

fn person(db: &Grafeno, name: &str) -> NodeId {
    db.write(|tx| tx.create_node(NS, NewNode::new("Person").prop("name", name)))
        .unwrap()
}

#[test]
fn duplicates_by_name() {
    let db = Grafeno::in_memory().unwrap();
    let ana = person(&db, "Ana García");
    let ana2 = person(&db, "ana  garcia");
    let ana3 = person(&db, "Ana García López");
    person(&db, "Bob Smith");
    // Same name, but a company: not a duplicate of the person.
    db.write(|tx| tx.create_node(NS, NewNode::new("Company").prop("name", "Ana García")))
        .unwrap();

    let read = db.begin_read().unwrap();
    let found = find_duplicates(&read, NS, &DuplicateOptions::default()).unwrap();
    let pairs: Vec<(NodeId, NodeId, f32)> =
        found.iter().map(|d| (d.a.id, d.b.id, d.score)).collect();
    let has = |x: NodeId, y: NodeId| {
        pairs
            .iter()
            .find(|(a, b, _)| (*a == x && *b == y) || (*a == y && *b == x))
            .map(|p| p.2)
    };
    assert_eq!(has(ana, ana2), Some(1.0));
    assert!(
        has(ana, ana3).is_some_and(|s| (0.6..1.0).contains(&s)),
        "{pairs:?}"
    );
    assert_eq!(found[0].score, 1.0, "best first");
    assert!(found[0].reasons[0].starts_with("same name"));
    assert_eq!(pairs.len(), 3, "no person-company pair, no Bob: {pairs:?}");
    drop(read);

    // Marked as different: not proposed again.
    db.write(|tx| tx.create_edge(NS, NewEdge::new(ana, DISTINCT_FROM, ana3)))
        .unwrap();
    let read = db.begin_read().unwrap();
    let found = find_duplicates(&read, NS, &DuplicateOptions::default()).unwrap();
    assert!(
        found
            .iter()
            .all(|d| !(d.a.id == ana && d.b.id == ana3 || d.a.id == ana3 && d.b.id == ana))
    );
}

#[test]
fn duplicates_by_meaning() {
    let db = Grafeno::in_memory().unwrap();
    let (a, b) = db
        .write(|tx| {
            let a = tx.create_node(
                NS,
                NewNode::new("Fact")
                    .text("prefers tea")
                    .embedding(vec![1.0, 0.0, 0.01]),
            )?;
            let b = tx.create_node(
                NS,
                NewNode::new("Fact")
                    .text("likes tea")
                    .embedding(vec![1.0, 0.02, 0.0]),
            )?;
            tx.create_node(
                NS,
                NewNode::new("Fact")
                    .text("hates noise")
                    .embedding(vec![0.0, 1.0, 0.0]),
            )?;
            Ok((a, b))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let found = find_duplicates(&read, NS, &DuplicateOptions::default()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        [found[0].a.id, found[0].b.id]
            .iter()
            .filter(|id| **id == a || **id == b)
            .count(),
        2
    );
    assert!(found[0].reasons[0].starts_with("similar meaning"));
    let without = DuplicateOptions {
        vector_threshold: None,
        ..DuplicateOptions::default()
    };
    assert!(find_duplicates(&read, NS, &without).unwrap().is_empty());
}

#[test]
fn recall_weighs_importance_and_skips_archived() {
    let db = Grafeno::in_memory().unwrap();
    let (low, high, old) = db
        .write(|tx| {
            let low =
                tx.create_node(NS, NewNode::new("Memory").text("the user likes green tea"))?;
            let high =
                tx.create_node(NS, NewNode::new("Memory").text("the user likes black tea"))?;
            let old = tx.create_node(NS, NewNode::new("Memory").text("the user liked tea once"))?;
            tx.set_importance(NS, low, 0.1)?;
            tx.set_importance(NS, high, 1.0)?;
            Ok((low, high, old))
        })
        .unwrap();
    db.write(|tx| archive(tx, NS, &[old]).map(|_| ())).unwrap();

    let read = db.begin_read().unwrap();
    let query = RecallQuery::new().text("likes tea").expand(None);
    let results = recall(&read, NS, &query).unwrap();
    let ids: Vec<NodeId> = results.iter().map(|r| r.node.id).collect();
    assert_eq!(ids, vec![high, low], "important first, archived left out");
    assert!(
        results[0]
            .signals
            .iter()
            .any(|s| matches!(s, Signal::Memory { importance, .. } if *importance == 1.0))
    );

    let all = recall(
        &read,
        NS,
        &RecallQuery {
            include_archived: true,
            ..query.clone()
        },
    )
    .unwrap();
    assert_eq!(all.len(), 3);
    let plain = recall(
        &read,
        NS,
        &RecallQuery {
            ranking: None,
            ..query
        },
    )
    .unwrap();
    assert!(
        plain
            .iter()
            .all(|r| !r.signals.iter().any(|s| matches!(s, Signal::Memory { .. })))
    );
}

#[test]
fn weak_memories_and_archiving() {
    let db = Grafeno::in_memory().unwrap();
    let (keep, fade) = db
        .write(|tx| {
            let keep = tx.create_node(NS, NewNode::new("Memory").text("birthday is in May"))?;
            let fade = tx.create_node(NS, NewNode::new("Memory").text("ate pasta on a Tuesday"))?;
            tx.set_importance(NS, keep, 0.9)?;
            tx.set_importance(NS, fade, 0.2)?;
            Ok((keep, fade))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    // A year later, with a 180-day half-life, recency is about 0.25: 0.9 →
    // 0.22 stays, 0.2 → 0.05 falls below 0.1.
    let later = Timestamp::from_micros(Timestamp::now().as_micros() + 365 * 86_400 * 1_000_000);
    let options = DecayOptions {
        now: later,
        half_life_secs: 180.0 * 86_400.0,
        threshold: 0.1,
        ..DecayOptions::default()
    };
    let weak = weak_memories(&read, NS, &options).unwrap();
    assert!(weak.iter().any(|w| w.node.id == fade));
    assert!(
        weak.iter().all(|w| w.node.id != keep),
        "importance 0.9 decays slower than the threshold"
    );
    assert!(
        weak_memories(&read, NS, &DecayOptions::default())
            .unwrap()
            .is_empty(),
        "nothing is weak today"
    );
    drop(read);

    assert_eq!(db.write(|tx| archive(tx, NS, &[fade, fade])).unwrap(), 1);
    let read = db.begin_read().unwrap();
    assert!(read.node(NS, fade).unwrap().unwrap().has_label(ARCHIVED));
    assert!(
        weak_memories(&read, NS, &options)
            .unwrap()
            .iter()
            .all(|w| w.node.id != fade)
    );
}

#[test]
fn consolidation_groups_and_summaries() {
    let db = Grafeno::in_memory().unwrap();
    let (ana, memories) = db
        .write(|tx| {
            let ana = tx.create_node(NS, NewNode::new("Person").prop("name", "Ana"))?;
            let mut memories = Vec::new();
            for text in ["Ana likes tea", "Ana moved to Lima", "Ana started at Acme"] {
                let id = tx.create_node(NS, NewNode::new("Memory").text(text))?;
                tx.create_edge(NS, NewEdge::new(id, "about", ana))?;
                memories.push(id);
            }
            tx.set_importance(NS, memories[1], 0.8)?;
            // Two memories about Bob: fewer than the minimum.
            let bob = tx.create_node(NS, NewNode::new("Person").prop("name", "Bob"))?;
            for text in ["Bob likes coffee", "Bob plays chess"] {
                let id = tx.create_node(NS, NewNode::new("Memory").text(text))?;
                tx.create_edge(NS, NewEdge::new(id, "about", bob))?;
            }
            Ok((ana, memories))
        })
        .unwrap();

    let read = db.begin_read().unwrap();
    let in_a_month = Timestamp::from_micros(Timestamp::now().as_micros() + 30 * 86_400 * 1_000_000);
    let options = ConsolidationOptions {
        now: in_a_month,
        ..ConsolidationOptions::default()
    };
    let groups = consolidation_groups(&read, NS, &options).unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].entity.id, ana);
    assert_eq!(groups[0].memories.len(), 3);
    assert!(
        consolidation_groups(&read, NS, &ConsolidationOptions::default())
            .unwrap()
            .is_empty(),
        "too recent"
    );
    drop(read);

    let summary = db
        .write(|tx| {
            write_summary(
                tx,
                NS,
                Summary {
                    text: "Ana likes tea, lives in Lima and works at Acme.".into(),
                    sources: memories.clone(),
                    about: Some(ana),
                    archive_sources: true,
                    ..Summary::default()
                },
            )
        })
        .unwrap();

    let read = db.begin_read().unwrap();
    let node = read.node(NS, summary).unwrap().unwrap();
    assert_eq!(node.labels, ["Summary"]);
    assert_eq!(node.sources, memories);
    assert_eq!(read.memory(NS, &node).unwrap().importance, 0.8);
    let summarized = read
        .neighbors(
            NS,
            summary,
            Direction::Out,
            &EdgeFilter::any().label("summarizes"),
        )
        .unwrap();
    assert_eq!(summarized.len(), 3);
    assert!(
        memories
            .iter()
            .all(|id| read.node(NS, *id).unwrap().unwrap().has_label(ARCHIVED))
    );
    // Recall now finds the summary instead of the archived memories.
    let results = recall(&read, NS, &RecallQuery::new().text("Lima").expand(None)).unwrap();
    assert_eq!(
        results.iter().map(|r| r.node.id).collect::<Vec<_>>(),
        vec![summary]
    );
    assert!(
        consolidation_groups(&read, NS, &options)
            .unwrap()
            .is_empty(),
        "archived memories are not grouped again"
    );
    assert!(read.check(NS).unwrap().is_ok());
}

#[test]
fn duplicates_follow_identity_rules_in_tricky_cases() {
    let db = Grafeno::in_memory().unwrap();
    let node = |labels: &[&str], name: &str, props: &[(&str, &str)], aliases: &[&str]| {
        let mut new = NewNode::new(labels[0]).prop("name", name);
        for label in &labels[1..] {
            new = new.label(*label);
        }
        for (key, value) in props {
            new = new.prop(*key, *value);
        }
        if !aliases.is_empty() {
            new = new.prop("aliases", aliases.to_vec());
        }
        db.write(|tx| tx.create_node(NS, new)).unwrap()
    };
    // Namesakes: the same name, different emails.
    let ana_a = node(&["Person"], "Ana García", &[("email", "ana@a.com")], &[]);
    let ana_b = node(&["Person"], "Ana Garcia", &[("email", "ana@b.org")], &[]);
    // The same email under another name: the same person.
    let ana_c = node(&["Person"], "A. G. Ruiz", &[("email", "ANA@a.com")], &[]);
    // A company with a person's name, both entities.
    node(&["Entity", "Company"], "Ana García", &[], &[]);
    // Typos.
    let jonh = node(&["Person"], "Jonh Smith", &[], &[]);
    let john = node(&["Person"], "John Smith", &[], &[]);
    let grafneo = node(&["Project"], "Grafneo", &[], &[]);
    let grafeno = node(&["Project"], "Grafeno", &[], &[]);
    // Not typos: other names.
    node(&["Person"], "Mario Rossi", &[], &[]);
    node(&["Person"], "Maria Rossi", &[], &[]);
    node(&["Person"], "Daniel", &[], &[]);
    node(&["Person"], "Daniela", &[], &[]);
    // A nickname and a translation, known as aliases.
    let robert = node(&["Person"], "Robert Brown", &[], &["Bobby"]);
    let bobby = node(&["Person"], "Bobby", &[], &[]);
    let germany = node(&["Country"], "Germany", &[], &["Alemania", "Deutschland"]);
    let alemania = node(&["Country"], "Alemania", &[], &[]);
    node(&["Team"], "Germany", &[], &[]);
    // Two birth dates: still proposed, less likely, and the conflict said.
    let luis_a = node(&["Person"], "Luis Pérez", &[("born_on", "1990-02-01")], &[]);
    let luis_b = node(&["Person"], "luis perez", &[("born_on", "1991-07-12")], &[]);

    let read = db.begin_read().unwrap();
    let found = find_duplicates(&read, NS, &DuplicateOptions::default()).unwrap();
    let pair = |x: NodeId, y: NodeId| {
        found
            .iter()
            .find(|d| (d.a.id == x && d.b.id == y) || (d.a.id == y && d.b.id == x))
    };
    let listed: Vec<String> = found
        .iter()
        .map(|d| {
            format!(
                "{} / {}: {:.2} {:?}",
                d.a.display_name(),
                d.b.display_name(),
                d.score,
                d.reasons
            )
        })
        .collect();
    assert!(pair(ana_a, ana_b).is_none(), "namesakes: {listed:#?}");
    let same = pair(ana_a, ana_c).expect("same email");
    assert_eq!(same.score, 1.0);
    assert!(
        same.reasons[0].starts_with("same email"),
        "{:?}",
        same.reasons
    );
    assert!(pair(ana_b, ana_c).is_none(), "different emails");
    assert!(
        pair(jonh, john).is_some_and(|d| d.score >= 0.8),
        "{listed:#?}"
    );
    assert!(pair(grafneo, grafeno).is_some_and(|d| d.reasons[0].contains("typo")));
    assert!(pair(robert, bobby).is_some_and(|d| d.score == 1.0));
    assert!(pair(germany, alemania).is_some_and(|d| d.score == 1.0));
    let luis = pair(luis_a, luis_b).expect("proposed");
    assert!((luis.score - 0.85).abs() < 1e-6, "{}", luis.score);
    assert_eq!(luis.conflicts.len(), 1);
    assert!(
        luis.reasons
            .iter()
            .any(|r| r.contains("conflicting born_on"))
    );
    // Nothing else: no person and company, no country and team, no
    // Mario/Maria, no Daniel/Daniela.
    assert_eq!(found.len(), 6, "{listed:#?}");
}

/// Two memories with the same words (case, accents and punctuation aside)
/// are duplicates without embeddings; a very short text, or another kind of
/// node with the same words, is not.
#[test]
fn duplicates_by_the_same_text_without_embeddings() {
    let db = Grafeno::in_memory().unwrap();
    let memory = |text: &str, label: &str| {
        db.write(|tx| tx.create_node(NS, NewNode::new(label).text(text)))
            .unwrap()
    };
    let a = memory("We ship the release on Fridays.", "Memory");
    let b = memory("we ship the release on fridays", "Memory");
    let c = memory("We ship the release on Mondays.", "Memory");
    let task = memory("We ship the release on Fridays.", "Task");
    let (ok1, ok2) = (memory("Ok", "Memory"), memory("ok.", "Memory"));

    let read = db.begin_read().unwrap();
    let found = find_duplicates(&read, NS, &DuplicateOptions::default()).unwrap();
    let pair = |x: NodeId, y: NodeId| {
        found
            .iter()
            .find(|d| (d.a.id == x && d.b.id == y) || (d.a.id == y && d.b.id == x))
    };
    let same = pair(a, b).expect("the same text is a duplicate");
    assert_eq!(same.score, 1.0);
    assert!(
        same.reasons.iter().any(|r| r == "same text"),
        "{:?}",
        same.reasons
    );
    assert!(pair(a, c).is_none(), "another day is another memory");
    assert!(pair(a, task).is_none(), "a task is not a memory");
    assert!(pair(ok1, ok2).is_none(), "too short to tell");
}
