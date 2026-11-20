//! Phase 13.1: the replication log. A database with the log on is written
//! in every way it can be; its entries, applied to an empty database, give
//! the same data, and a member restored from a backup catches up from the
//! entries after it.

use std::sync::Arc;

use grafeno::textsim::Measure;
use grafeno::{
    Constraint, Direction, EdgeFilter, FieldRule, Grafeno, Grant, IndexDef, MatchRule, NewEdge,
    NewNode, NodeId, OnConflict, Principal, Role, Scope, Timestamp, Writer, WriterOptions,
};

const NS: &str = "r";

/// Everything a reader can see, for comparing two databases.
fn snapshot(db: &Grafeno) -> String {
    let read = db.begin_read().unwrap();
    let mut out = Vec::new();
    let namespaces: Vec<String> = read
        .namespaces()
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    read.export(&namespaces, &mut out).unwrap();
    let mut text = String::from_utf8(out).unwrap();
    // The export's header carries the time it was taken.
    text = text.lines().skip(1).collect::<Vec<_>>().join("\n");
    let mut lines = vec![text];
    lines.push(format!("{:?}", read.namespaces().unwrap()));
    for ns in &namespaces {
        lines.push(format!("{:?}", read.stats(ns).unwrap()));
        lines.push(format!("{:?}", read.constraints(ns).unwrap()));
        lines.push(format!("{:?}", read.match_rules(ns).unwrap()));
        lines.push(format!("{:?}", read.merges(ns, None).unwrap()));
        lines.push(format!("{:?}", read.indexes(ns).unwrap()));
        lines.push(format!(
            "{:?}",
            read.state_entries(ns, "", "", usize::MAX).unwrap()
        ));
        for kind in ["history", "events", "import.id", "app"] {
            lines.push(format!(
                "{kind}: {:?}",
                read.state_entries(ns, kind, "", usize::MAX).unwrap()
            ));
        }
        lines.push(format!("{:?}", read.changes(ns, 0, usize::MAX).unwrap()));
    }
    lines.push(format!("{:?}", read.accounts().unwrap()));
    lines.push(format!("{:?}", read.api_keys().unwrap()));
    lines.push(format!("{:?}", read.sessions(None).unwrap()));
    lines.push(format!("{:?}", read.roles().unwrap()));
    lines.join("\n")
}

fn replay(from: &Grafeno, onto: &Grafeno, after: u64) -> usize {
    let entries = from
        .begin_read()
        .unwrap()
        .log_entries(after, usize::MAX)
        .unwrap();
    onto.apply_entries(&entries).unwrap()
}

fn rule() -> MatchRule {
    MatchRule {
        label: "Person".into(),
        strong_keys: vec!["email".into()],
        fields: vec![FieldRule {
            key: "name".into(),
            measure: Measure::JaroWinkler,
            weight: 1.0,
        }],
        neighbors: 0.0,
        same: 0.9,
        review: 0.7,
    }
}

/// Writes of every kind, over several transactions.
fn write_everything(db: &Grafeno) -> (NodeId, NodeId) {
    let (ana, acme) = db
        .write(|tx| {
            tx.create_namespace(NS, Some("replicated"))?;
            tx.create_index(NS, IndexDef::new("Person", "email"))?;
            tx.create_constraint(
                NS,
                Constraint::Functional {
                    name: "Person_city".into(),
                    label: "Person".into(),
                    key: "city".into(),
                },
            )?;
            tx.create_constraint(
                NS,
                Constraint::Edge {
                    name: "one_employer".into(),
                    label: "works_at".into(),
                    scope: Scope::Source,
                    on_conflict: OnConflict::Invalidate,
                },
            )?;
            let ana = tx.create_node(
                NS,
                NewNode::new("Person")
                    .prop("name", "Ana")
                    .prop("email", "ana@x.org")
                    .prop("city", "Lima")
                    .text("Ana works on graph engines")
                    .embedding(vec![1.0, 0.0, 0.5]),
            )?;
            let acme = tx.create_node(NS, NewNode::new("Company").prop("name", "Acme"))?;
            tx.create_edge(
                NS,
                NewEdge::new(ana, "works_at", acme).valid_from(Timestamp::from_micros(1_000_000)),
            )?;
            Ok((ana, acme))
        })
        .unwrap();
    db.write(|tx| {
        // A functional property changes: its history is kept.
        let mut props = grafeno::Props::new();
        props.insert("city".into(), "Quito".into());
        tx.set_node_props(NS, ana, props)?;
        tx.add_label(NS, ana, "Engineer")?;
        tx.set_importance(NS, ana, 0.8)?;
        // A second employer: the constraint ends the first fact.
        let globex = tx.create_node(NS, NewNode::new("Company").prop("name", "Globex"))?;
        tx.create_edge(NS, NewEdge::new(ana, "works_at", globex))?;
        tx.set_match_rule(NS, &rule())?;
        tx.put_state(NS, "app", "cursor", b"42")?;
        let (account, key) = Principal {
            name: "agent".into(),
            secret_hash: "ab".repeat(32),
            grants: vec![Grant {
                namespace: NS.into(),
                role: Role::Write,
            }],
            owner: false,
            own_only: true,
            description: None,
            created_at: Timestamp::from_micros(5),
        }
        .split();
        tx.create_account(&account)?;
        tx.create_api_key(&key)?;
        // A user who signed in, with a session.
        tx.create_account(&grafeno::Account {
            name: "ana".into(),
            kind: grafeno::AccountKind::User,
            password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into()),
            grants: vec![Grant {
                namespace: NS.into(),
                role: Role::Read,
            }],
            roles: Vec::new(),
            superuser: false,
            own_only: false,
            disabled: false,
            expires_at: None,
            attributes: Default::default(),
            description: None,
            created_at: Timestamp::from_micros(6),
        })?;
        // A role, granted to her (14.2).
        tx.put_role(&grafeno::RoleDef {
            name: "auditor".into(),
            privileges: vec![grafeno::Privilege::grant(
                grafeno::Act::Metrics,
                grafeno::Object::Server,
            )],
            grants: Vec::new(),
            roles: Vec::new(),
            policies: Vec::new(),
            description: None,
            created_at: Timestamp::from_micros(8),
        })?;
        let mut ana = tx.account("ana")?.unwrap();
        ana.roles.push("auditor".into());
        tx.put_account(&ana)?;
        tx.put_session(&grafeno::Session {
            id: "s1".into(),
            account: "ana".into(),
            token_hash: "cd".repeat(32),
            expires_at: Timestamp::from_micros(900),
            refresh_hash: "ef".repeat(32),
            refresh_expires_at: Timestamp::from_micros(9_000),
            replaced_refresh_hash: None,
            created_at: Timestamp::from_micros(7),
            refreshed_at: None,
            address: Some("127.0.0.1".into()),
        })?;
        Ok(())
    })
    .unwrap();
    db.write(|tx| {
        // Duplicates merged; an edge retracted; texts and embeddings changed.
        let dup = tx.create_node(
            NS,
            NewNode::new("Person")
                .prop("name", "Ana P.")
                .text("duplicate of Ana")
                .embedding(vec![0.9, 0.1, 0.5]),
        )?;
        let note = tx.create_node(NS, NewNode::new("Note").text("to be deleted"))?;
        let link = tx.create_edge(NS, NewEdge::new(note, "about", acme))?;
        tx.merge_nodes(NS, ana, dup)?;
        tx.retract_edge(NS, link)?;
        tx.delete_node(NS, note)?;
        tx.set_text(NS, acme, "Acme builds rockets")?;
        tx.set_embedding(NS, acme, &[0.0, 1.0, 0.2])?;
        tx.delete_state(NS, "app", "cursor")?;
        tx.create_namespace("scratch", None)?;
        tx.create_node("scratch", NewNode::new("Tmp"))?;
        Ok(())
    })
    .unwrap();
    db.write(|tx| {
        tx.drop_namespace("scratch")?;
        tx.create_node(
            "implicit",
            NewNode::new("Thing").text("a namespace made by its first write"),
        )?;
        tx.delete_account("agent")?;
        tx.drop_index(NS, "Person_email")?;
        tx.create_index(NS, IndexDef::new("Company", "name"))?;
        Ok(())
    })
    .unwrap();
    // A failing transaction leaves nothing in the log.
    let _ = db.write(|tx| {
        tx.create_node(NS, NewNode::new("Ghost"))?;
        Err::<(), _>(grafeno::Error::Invalid("on purpose".into()))
    });
    db.write(|tx| tx.compact_vectors(NS).map(|_| ())).unwrap();
    (ana, acme)
}

#[test]
fn replaying_the_log_gives_the_same_data() {
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    let (ana, acme) = write_everything(&leader);
    let position = leader.begin_read().unwrap().log_position().unwrap();
    assert!(position.enabled);
    assert_eq!((position.first, position.last, position.applied), (1, 5, 5));

    let follower = Grafeno::in_memory().unwrap();
    assert_eq!(replay(&leader, &follower, 0), 5);
    assert_eq!(snapshot(&follower), snapshot(&leader));
    let read = follower.begin_read().unwrap();
    assert_eq!(read.log_position().unwrap(), position);
    for ns in [NS, "implicit"] {
        assert!(read.check(ns).unwrap().is_ok(), "{ns}");
    }
    // Derived structures work: traversals, text and vector search, history.
    assert_eq!(
        read.neighbors(NS, ana, Direction::Out, &EdgeFilter::any())
            .unwrap()
            .len(),
        leader
            .begin_read()
            .unwrap()
            .neighbors(NS, ana, Direction::Out, &EdgeFilter::any())
            .unwrap()
            .len()
    );
    assert_eq!(read.search_text(NS, "rockets", 1).unwrap()[0].node, acme);
    assert_eq!(
        read.search_vector(NS, &[0.0, 1.0, 0.2], 1).unwrap()[0].node,
        acme
    );
    assert_eq!(read.property_history(NS, ana, "city").unwrap().len(), 1);
    drop(read);

    // Applying again changes nothing; a gap is refused.
    assert_eq!(replay(&leader, &follower, 0), 0);
    let later = leader.begin_read().unwrap().log_entries(4, 1).unwrap();
    let fresh = Grafeno::in_memory().unwrap();
    assert!(fresh.apply_entries(&later).is_err());
}

#[test]
fn a_member_catches_up_from_a_backup_and_the_entries_after_it() {
    let leader = Grafeno::in_memory().unwrap();
    leader.enable_replication_log().unwrap();
    write_everything(&leader);
    let path = std::env::temp_dir().join(format!("grafeno-repl-{}.redb", NodeId::new()));
    leader.begin_read().unwrap().backup(&path).unwrap();
    // The leader goes on, and drops the entries the backup holds.
    leader
        .write(|tx| {
            tx.create_node(
                NS,
                NewNode::new("Person")
                    .prop("name", "Bea")
                    .text("after the backup"),
            )?;
            Ok(())
        })
        .unwrap();
    leader.write(|tx| tx.truncate_log(5).map(|_| ())).unwrap();
    let position = leader.begin_read().unwrap().log_position().unwrap();
    assert_eq!((position.first, position.last), (6, 6));
    assert!(leader.begin_read().unwrap().log_entries(0, 10).unwrap()[0].0 == 6);

    {
        let member = Grafeno::open(&path).unwrap();
        let at = member.begin_read().unwrap().log_position().unwrap().applied;
        assert_eq!(at, 5);
        assert_eq!(replay(&leader, &member, at), 1);
        assert_eq!(snapshot(&member), snapshot(&leader));
        // The member keeps logging its own writes after the leader's entries.
        member
            .write(|tx| tx.create_node(NS, NewNode::new("Local")).map(|_| ()))
            .unwrap();
        assert_eq!(member.begin_read().unwrap().log_position().unwrap().last, 7);
    }
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn group_commits_log_one_entry_with_the_writes_that_stayed() {
    let leader = Arc::new(Grafeno::in_memory().unwrap());
    leader.enable_replication_log().unwrap();
    let writer = Writer::new(leader.clone(), WriterOptions::default());
    let handles: Vec<_> = (0..20)
        .map(|i| {
            let writer = &writer;
            std::thread::scope(|_| {
                writer.write(move |tx| {
                    if i % 7 == 3 {
                        return Err(grafeno::Error::Invalid("refused".into()));
                    }
                    tx.create_node(NS, NewNode::new("N").prop("i", i))
                        .map(|_| ())
                })
            })
        })
        .collect();
    let failed = handles.iter().filter(|outcome| outcome.is_err()).count();
    assert_eq!(failed, 3);
    drop(writer);
    let follower = Grafeno::in_memory().unwrap();
    replay(&leader, &follower, 0);
    assert_eq!(snapshot(&follower), snapshot(&leader));
    assert_eq!(follower.begin_read().unwrap().stats(NS).unwrap().nodes, 17);
}

#[test]
fn without_the_log_nothing_is_written() {
    let db = Grafeno::in_memory().unwrap();
    db.write(|tx| tx.create_node(NS, NewNode::new("N")).map(|_| ()))
        .unwrap();
    let position = db.begin_read().unwrap().log_position().unwrap();
    assert_eq!(position, grafeno::LogPosition::default());
    assert!(
        db.begin_read()
            .unwrap()
            .log_entries(0, 10)
            .unwrap()
            .is_empty()
    );
}

/// Everything a reader can see of one namespace.
fn namespace_snapshot(db: &Grafeno, ns: &str) -> String {
    let read = db.begin_read().unwrap();
    let mut out = Vec::new();
    read.export(&[ns.to_owned()], &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    let mut lines: Vec<String> = text.lines().skip(1).map(str::to_owned).collect();
    lines.push(format!("{:?}", read.namespace(ns).unwrap()));
    lines.push(format!("{:?}", read.stats(ns).unwrap()));
    lines.push(format!("{:?}", read.constraints(ns).unwrap()));
    lines.push(format!("{:?}", read.match_rules(ns).unwrap()));
    lines.push(format!("{:?}", read.merges(ns, None).unwrap()));
    lines.push(format!("{:?}", read.indexes(ns).unwrap()));
    lines.push(format!(
        "{:?}",
        read.state_entries(ns, "", "", usize::MAX).unwrap()
    ));
    lines.push(format!("{:?}", read.changes(ns, 0, usize::MAX).unwrap()));
    lines.join("\n")
}

/// Phase 13.5: a namespace copied to another database (as a namespace
/// moving between cluster groups is): its records and change log at one
/// position, then the effects on it of the entries after, give the same
/// namespace, change log sequence numbers included; the copy's own log
/// replays it onto a third database.
#[test]
fn a_namespace_copies_to_another_database_and_catches_up() {
    let source = Grafeno::in_memory().unwrap();
    source.enable_replication_log().unwrap();
    write_everything(&source);
    let copied_at = source.begin_read().unwrap().log_position().unwrap().last;

    let target = Grafeno::in_memory().unwrap();
    target.enable_replication_log().unwrap();
    // Another namespace already there stays as it is.
    target
        .write(|tx| tx.create_node("other", NewNode::new("Kept")).map(|_| ()))
        .unwrap();
    let read = source.begin_read().unwrap();
    let mut chunks = Vec::new();
    read.namespace_ops(NS, 7, |ops| {
        chunks.push(ops);
        Ok(())
    })
    .unwrap();
    drop(read);
    assert!(chunks.len() > 2, "copied in {} chunks", chunks.len());
    for ops in &chunks {
        assert!(ops.iter().all(|op| op.namespace() == Some(NS)));
        target
            .write(|tx| tx.import_ops(ops, Vec::new(), None))
            .unwrap();
    }
    assert_eq!(namespace_snapshot(&target, NS), {
        // The source as it was when copied.
        namespace_snapshot(&source, NS)
    });

    // Writes go on at the source while the copy catches up.
    let made = source
        .write(|tx| {
            let a = tx.create_node(
                NS,
                NewNode::new("Person")
                    .prop("name", "Zoe")
                    .text("Zoe writes"),
            )?;
            let b = tx.create_node("implicit", NewNode::new("Elsewhere"))?;
            tx.create_edge(NS, NewEdge::new(a, "knows", a))?;
            Ok((a, b))
        })
        .unwrap();
    source
        .write(|tx| {
            tx.set_embedding(NS, made.0, &[0.5, 0.5, 0.0])?;
            tx.put_state(NS, "app", "cursor", b"42")?;
            Ok(())
        })
        .unwrap();
    source
        .write(|tx| tx.delete_node(NS, made.0).map(|_| ()))
        .unwrap();
    let entries = source
        .begin_read()
        .unwrap()
        .log_entries(copied_at, usize::MAX)
        .unwrap();
    assert_eq!(entries.len(), 3);
    for (_, entry) in &entries {
        target
            .write(|tx| tx.import_ops(&entry.ops_of(NS), entry.changes_of(NS), Some(entry.at)))
            .unwrap();
    }
    assert_eq!(
        namespace_snapshot(&target, NS),
        namespace_snapshot(&source, NS)
    );
    assert!(target.begin_read().unwrap().check(NS).unwrap().is_ok());
    assert_eq!(
        target.begin_read().unwrap().stats("other").unwrap().nodes,
        1
    );
    // The change log goes on from the same sequence number.
    let last = |db: &Grafeno| {
        db.begin_read()
            .unwrap()
            .changes(NS, 0, usize::MAX)
            .unwrap()
            .last
    };
    assert_eq!(last(&target), last(&source));

    // What the copy logged replays it onto a third database.
    let third = Grafeno::in_memory().unwrap();
    replay(&target, &third, 0);
    assert_eq!(
        namespace_snapshot(&third, NS),
        namespace_snapshot(&source, NS)
    );
}

/// Phase 13.5: a fenced namespace refuses writes (busy) while the others
/// take them; the move's own writes pass, and a dropped moved namespace
/// keeps refusing late writes.
#[test]
fn a_fenced_namespace_refuses_writes() {
    let db = Grafeno::in_memory().unwrap();
    db.enable_replication_log().unwrap();
    db.write(|tx| tx.create_node(NS, NewNode::new("N")).map(|_| ()))
        .unwrap();
    db.write(|tx| tx.fence_namespace(NS, "2")).unwrap();
    let refused = db
        .write(|tx| tx.create_node(NS, NewNode::new("N")).map(|_| ()))
        .unwrap_err();
    assert!(matches!(refused, grafeno::Error::Busy(_)), "{refused}");
    assert!(refused.to_string().contains("moving"));
    db.write(|tx| tx.create_node("other", NewNode::new("N")).map(|_| ()))
        .unwrap();
    assert_eq!(db.begin_read().unwrap().stats(NS).unwrap().nodes, 1);
    // The move's own writes go through: copies into it, and dropping it.
    let mut ops = Vec::new();
    db.begin_read()
        .unwrap()
        .namespace_ops(NS, 100, |chunk| {
            ops.extend(chunk);
            Ok(())
        })
        .unwrap();
    db.write(|tx| tx.import_ops(&ops, Vec::new(), None))
        .unwrap();
    db.write(|tx| tx.drop_moved_namespace(NS, "2")).unwrap();
    assert_eq!(db.begin_read().unwrap().stats(NS).unwrap().nodes, 0);
    assert!(
        db.write(|tx| tx.create_node(NS, NewNode::new("N")).map(|_| ()))
            .is_err()
    );
    db.write(|tx| tx.unfence_namespace(NS)).unwrap();
    db.write(|tx| tx.create_node(NS, NewNode::new("N")).map(|_| ()))
        .unwrap();
}
