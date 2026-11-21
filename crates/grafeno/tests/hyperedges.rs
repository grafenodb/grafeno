//! Hyperedges: one record relating two or more nodes, each in a role, with
//! an incidence index per node and a label index. Creation, reads by id,
//! node, label and participants, updates, deletes, what deleting a node
//! does, namespaces, views, rollback, reopening, backups, the replication
//! log, concurrent writers, and a property test of the incidence invariant
//! against a model.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use grafeno::{
    Account, AccountKind, Act, Actor, Direction, EdgeFilter, Error, Grafeno, HyperEdgeFilter,
    HyperEdgeId, NewEdge, NewHyperEdge, NewNode, NodeId, Object, OnHyperEdges, Participant,
    ParticipantMatch, Privilege, Rights, RoleDef, Timestamp, View,
};
use proptest::prelude::*;

const NS: &str = "team";

fn nodes(db: &Grafeno, ns: &str, count: usize) -> Vec<NodeId> {
    db.write(|tx| {
        (0..count)
            .map(|i| tx.create_node(ns, NewNode::new("Agent").prop("name", format!("agent {i}"))))
            .collect()
    })
    .unwrap()
}

fn check(db: &Grafeno, ns: &str) {
    let report = db.begin_read().unwrap().check(ns).unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
}

fn ids(edges: &[grafeno::HyperEdge]) -> Vec<HyperEdgeId> {
    edges.iter().map(|edge| edge.id).collect()
}

#[test]
fn hyperedges_of_two_three_and_many_participants() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 60);
    let (pair, trio, many) = db
        .write(|tx| {
            let pair =
                tx.create_hyperedge(NS, NewHyperEdge::new("pairing").node(n[0]).node(n[1]))?;
            let trio = tx.create_hyperedge(
                NS,
                NewHyperEdge::new("decision")
                    .member(n[2], "proposer")
                    .member(n[0], "implementer")
                    .member(n[1], "reviewer")
                    .prop("topic", "storage")
                    .source(n[3]),
            )?;
            let mut big = NewHyperEdge::new("meeting");
            for &node in &n[..50] {
                big = big.member(node, "attendee");
            }
            let many = tx.create_hyperedge(NS, big)?;
            Ok((pair, trio, many))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let decision = read.hyperedge(NS, trio).unwrap().unwrap();
    assert_eq!(decision.label, "decision");
    assert_eq!(decision.props["topic"], "storage");
    assert_eq!(decision.sources, vec![n[3]]);
    // Stored sorted by node: the order given means nothing.
    let mut sorted = vec![n[0], n[1], n[2]];
    sorted.sort();
    assert_eq!(decision.nodes(), sorted);
    assert_eq!(decision.with_role("reviewer"), vec![n[1]]);
    assert_eq!(decision.roles_of(n[2]), vec![Some("proposer")]);
    assert_eq!(read.hyperedge(NS, pair).unwrap().unwrap().nodes().len(), 2);
    assert_eq!(
        read.hyperedge(NS, many)
            .unwrap()
            .unwrap()
            .participants
            .len(),
        50
    );
    assert!(read.hyperedge(NS, HyperEdgeId::new()).unwrap().is_none());
    // The node with most of them takes part in all three.
    let of_first = read
        .node_hyperedges(NS, n[0], &HyperEdgeFilter::any())
        .unwrap();
    assert_eq!(of_first.len(), 3);
    drop(read);
    check(&db, NS);
}

#[test]
fn invalid_hyperedges_are_refused() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 3);
    let try_create = |new: NewHyperEdge| db.write(|tx| tx.create_hyperedge(NS, new));
    let invalid = |result: Result<HyperEdgeId, Error>| matches!(result, Err(Error::Invalid(_)));

    assert!(invalid(try_create(NewHyperEdge::new("x"))));
    assert!(invalid(try_create(NewHyperEdge::new("x").node(n[0]))));
    // The same node twice is still one node.
    assert!(invalid(try_create(
        NewHyperEdge::new("x").member(n[0], "a").member(n[0], "b")
    )));
    // A (node, role) pair twice.
    assert!(invalid(try_create(
        NewHyperEdge::new("x")
            .member(n[0], "a")
            .member(n[0], "a")
            .node(n[1])
    )));
    assert!(invalid(try_create(
        NewHyperEdge::new("x").node(n[0]).node(n[0]).node(n[1])
    )));
    assert!(invalid(try_create(
        NewHyperEdge::new("").node(n[0]).node(n[1])
    )));
    assert!(invalid(try_create(
        NewHyperEdge::new(" x").node(n[0]).node(n[1])
    )));
    assert!(invalid(try_create(
        NewHyperEdge::new("x").member(n[0], "").node(n[1])
    )));
    let at = Timestamp::parse("2024-01-01").unwrap();
    assert!(invalid(try_create(
        NewHyperEdge::new("x")
            .node(n[0])
            .node(n[1])
            .valid_from(at)
            .valid_to(at)
    )));
    assert!(matches!(
        db.write(|tx| tx.create_hyperedge("", NewHyperEdge::new("x").node(n[0]).node(n[1]))),
        Err(Error::Invalid(_))
    ));
    // A node that does not exist.
    let ghost = NodeId::new();
    assert!(matches!(
        try_create(NewHyperEdge::new("x").node(n[0]).node(ghost)),
        Err(Error::NodeNotFound(id)) if id == ghost
    ));
    // The participant limit, configurable.
    db.set_max_participants(2).unwrap();
    assert!(invalid(try_create(
        NewHyperEdge::new("x").node(n[0]).node(n[1]).node(n[2])
    )));
    assert!(try_create(NewHyperEdge::new("x").node(n[0]).node(n[1])).is_ok());
    assert!(db.set_max_participants(1).is_err());
    db.set_max_participants(grafeno::DEFAULT_MAX_PARTICIPANTS)
        .unwrap();
    // A node may take part twice in different roles.
    assert!(
        try_create(
            NewHyperEdge::new("x")
                .member(n[0], "a")
                .member(n[0], "b")
                .node(n[1])
        )
        .is_ok()
    );
    // Nothing refused left a trace.
    let read = db.begin_read().unwrap();
    assert_eq!(read.hyperedges_page(NS, None, 100).unwrap().len(), 2);
    drop(read);
    check(&db, NS);
}

#[test]
fn reads_by_node_label_and_participants() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 5);
    let (abc, ab, abd, review) = db
        .write(|tx| {
            let abc = tx.create_hyperedge(
                NS,
                NewHyperEdge::new("collab").node(n[0]).node(n[1]).node(n[2]),
            )?;
            let ab = tx.create_hyperedge(NS, NewHyperEdge::new("collab").node(n[1]).node(n[0]))?;
            let abd = tx.create_hyperedge(
                NS,
                NewHyperEdge::new("decision")
                    .node(n[0])
                    .node(n[1])
                    .node(n[3]),
            )?;
            let review = tx.create_hyperedge(
                NS,
                NewHyperEdge::new("decision")
                    .member(n[0], "author")
                    .member(n[0], "reviewer")
                    .node(n[1]),
            )?;
            Ok((abc, ab, abd, review))
        })
        .unwrap();
    let read = db.begin_read().unwrap();
    let any = HyperEdgeFilter::any();

    // By node, all of them or one label's.
    assert_eq!(read.node_hyperedges(NS, n[0], &any).unwrap().len(), 4);
    assert_eq!(read.node_hyperedges(NS, n[2], &any).unwrap().len(), 1);
    assert!(read.node_hyperedges(NS, n[4], &any).unwrap().is_empty());
    let collabs = read
        .node_hyperedges(NS, n[1], &HyperEdgeFilter::any().label("collab"))
        .unwrap();
    assert_eq!(ids(&collabs), vec![abc, ab]);
    assert!(
        read.node_hyperedges(NS, NodeId::new(), &any)
            .unwrap()
            .is_empty()
    );

    // By label, in pages.
    let first = read
        .hyperedges_with_label(NS, "decision", &any, None, 1)
        .unwrap();
    assert_eq!(ids(&first), vec![abd]);
    let second = read
        .hyperedges_with_label(NS, "decision", &any, Some(abd), 1)
        .unwrap();
    assert_eq!(ids(&second), vec![review]);
    assert!(
        read.hyperedges_with_label(NS, "decision", &any, Some(review), 1)
            .unwrap()
            .is_empty()
    );
    assert!(
        read.hyperedges_with_label(NS, "nothing", &any, None, 10)
            .unwrap()
            .is_empty()
    );

    // By participants: contained or exactly these, roles aside.
    let contains = |nodes: &[NodeId]| {
        ids(&read
            .hyperedges_by_participants(NS, nodes, ParticipantMatch::Contains, &any)
            .unwrap())
    };
    let exact = |nodes: &[NodeId]| {
        ids(&read
            .hyperedges_by_participants(NS, nodes, ParticipantMatch::Exact, &any)
            .unwrap())
    };
    assert_eq!(contains(&[n[0], n[1]]), vec![abc, ab, abd, review]);
    assert_eq!(exact(&[n[1], n[0]]), vec![ab, review]);
    assert_eq!(exact(&[n[0], n[1], n[2]]), vec![abc]);
    assert_eq!(contains(&[n[2], n[3]]), Vec::<HyperEdgeId>::new());
    assert_eq!(contains(&[n[3]]), vec![abd]);
    assert_eq!(exact(&[n[0], n[0], n[1]]), vec![ab, review]);
    let decisions = read
        .hyperedges_by_participants(
            NS,
            &[n[0]],
            ParticipantMatch::Contains,
            &HyperEdgeFilter::any().label("decision"),
        )
        .unwrap();
    assert_eq!(ids(&decisions), vec![abd, review]);
    assert!(matches!(
        read.hyperedges_by_participants(NS, &[], ParticipantMatch::Contains, &any),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn updates_move_the_incidence_index() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 4);
    let id = db
        .write(|tx| {
            tx.create_hyperedge(
                NS,
                NewHyperEdge::new("task")
                    .member(n[0], "owner")
                    .node(n[1])
                    .prop("state", "open"),
            )
        })
        .unwrap();
    db.write(|tx| {
        let mut props = grafeno::Props::new();
        props.insert("state".into(), "done".into());
        props.insert("score".into(), 3.into());
        tx.set_hyperedge_props(NS, id, props)?;
        tx.remove_hyperedge_props(NS, id, &["score", "absent"])?;
        tx.set_hyperedge_participants(
            NS,
            id,
            vec![
                Participant::with_role(n[2], "owner"),
                Participant::new(n[3]),
                Participant::new(n[1]),
            ],
        )
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let edge = read.hyperedge(NS, id).unwrap().unwrap();
    assert_eq!(edge.props.get("state").unwrap(), "done");
    assert!(!edge.props.contains_key("score"));
    assert_eq!(edge.with_role("owner"), vec![n[2]]);
    let any = HyperEdgeFilter::any();
    assert!(read.node_hyperedges(NS, n[0], &any).unwrap().is_empty());
    for node in [n[1], n[2], n[3]] {
        assert_eq!(
            ids(&read.node_hyperedges(NS, node, &any).unwrap()),
            vec![id]
        );
    }
    drop(read);
    // Invalid participants change nothing.
    assert!(
        db.write(|tx| tx.set_hyperedge_participants(NS, id, vec![Participant::new(n[0])]))
            .is_err()
    );
    assert!(
        db.write(|tx| tx.set_hyperedge_props(NS, HyperEdgeId::new(), Default::default()))
            .is_err()
    );
    assert_eq!(
        db.begin_read()
            .unwrap()
            .hyperedge(NS, id)
            .unwrap()
            .unwrap()
            .nodes()
            .len(),
        3
    );
    check(&db, NS);
}

#[test]
fn time_works_as_for_edges() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 2);
    let (y2020, y2022, y2024) = (
        Timestamp::parse("2020-01-01").unwrap(),
        Timestamp::parse("2022-01-01").unwrap(),
        Timestamp::parse("2024-01-01").unwrap(),
    );
    let (old, wrong) = db
        .write(|tx| {
            let old = tx.create_hyperedge(
                NS,
                NewHyperEdge::new("team")
                    .node(n[0])
                    .node(n[1])
                    .valid_from(y2020),
            )?;
            let wrong = tx.create_hyperedge(NS, NewHyperEdge::new("team").node(n[0]).node(n[1]))?;
            Ok((old, wrong))
        })
        .unwrap();
    let before_retraction = Timestamp::now();
    db.write(|tx| {
        tx.invalidate_hyperedge(NS, old, y2022)?;
        tx.retract_hyperedge(NS, wrong)
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let in_year = |at| {
        ids(&read
            .node_hyperedges(NS, n[0], &HyperEdgeFilter::any().valid_at(at))
            .unwrap())
    };
    assert_eq!(in_year(y2020), vec![old]);
    assert!(in_year(y2024).is_empty());
    assert_eq!(
        ids(&read
            .node_hyperedges(NS, n[0], &HyperEdgeFilter::any())
            .unwrap()),
        vec![old]
    );
    let then = read
        .node_hyperedges(
            NS,
            n[0],
            &HyperEdgeFilter::any().known_at(before_retraction),
        )
        .unwrap();
    assert_eq!(ids(&then), vec![old, wrong]);
    // By id it is there, retracted.
    assert!(read.hyperedge(NS, wrong).unwrap().unwrap().is_retracted());
}

#[test]
fn deletes_leave_no_entries() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 3);
    let id = db
        .write(|tx| {
            tx.create_hyperedge(
                NS,
                NewHyperEdge::new("x")
                    .member(n[0], "a")
                    .member(n[0], "b")
                    .node(n[1])
                    .node(n[2]),
            )
        })
        .unwrap();
    db.write(|tx| tx.delete_hyperedge(NS, id)).unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.hyperedge(NS, id).unwrap().is_none());
    for node in &n {
        assert!(
            read.node_hyperedges(NS, *node, &HyperEdgeFilter::any())
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        read.hyperedges_with_label(NS, "x", &HyperEdgeFilter::any(), None, 10)
            .unwrap()
            .is_empty()
    );
    drop(read);
    assert!(matches!(
        db.write(|tx| tx.delete_hyperedge(NS, id)),
        Err(Error::HyperEdgeNotFound(gone)) if gone == id
    ));
    check(&db, NS);
}

#[test]
fn deleting_a_participant_is_refused_cascaded_or_detached() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 4);
    let (trio, pair) = db
        .write(|tx| {
            let trio =
                tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1]).node(n[2]))?;
            let pair = tx.create_hyperedge(NS, NewHyperEdge::new("y").node(n[0]).node(n[3]))?;
            // A binary edge too: it goes with the node, as always.
            tx.create_edge(NS, NewEdge::new(n[0], "knows", n[1]))?;
            Ok((trio, pair))
        })
        .unwrap();

    // Restrict (the default): refused, nothing changes.
    assert!(matches!(
        db.write(|tx| tx.delete_node(NS, n[0])),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        db.write(|tx| tx.delete_node_with(NS, n[0], OnHyperEdges::Restrict)),
        Err(Error::Conflict(_))
    ));
    assert!(db.begin_read().unwrap().node(NS, n[0]).unwrap().is_some());

    // Detach: the trio keeps two nodes, the pair is left with one and goes.
    db.write(|tx| tx.delete_node_with(NS, n[0], OnHyperEdges::Detach))
        .unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.node(NS, n[0]).unwrap().is_none());
    assert_eq!(read.hyperedge(NS, trio).unwrap().unwrap().nodes().len(), 2);
    assert!(read.hyperedge(NS, pair).unwrap().is_none());
    assert!(
        read.node_hyperedges(NS, n[3], &HyperEdgeFilter::any())
            .unwrap()
            .is_empty()
    );
    assert!(
        read.neighbors(NS, n[1], Direction::In, &EdgeFilter::any())
            .unwrap()
            .is_empty()
    );
    drop(read);
    check(&db, NS);

    // Cascade: the hyperedge goes with the node.
    db.write(|tx| tx.delete_node_with(NS, n[1], OnHyperEdges::Cascade))
        .unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.hyperedge(NS, trio).unwrap().is_none());
    assert!(
        read.node_hyperedges(NS, n[2], &HyperEdgeFilter::any())
            .unwrap()
            .is_empty()
    );
    drop(read);
    // A node in no hyperedge is deleted as before.
    db.write(|tx| tx.delete_node(NS, n[2])).unwrap();
    check(&db, NS);
}

#[test]
fn namespaces_are_apart() {
    let db = Grafeno::in_memory().unwrap();
    let a = nodes(&db, "a", 2);
    let b = nodes(&db, "b", 1);
    let id = db
        .write(|tx| tx.create_hyperedge("a", NewHyperEdge::new("x").node(a[0]).node(a[1])))
        .unwrap();
    let read = db.begin_read().unwrap();
    let any = HyperEdgeFilter::any();
    assert!(read.hyperedge("b", id).unwrap().is_none());
    assert!(read.node_hyperedges("b", a[0], &any).unwrap().is_empty());
    assert!(
        read.hyperedges_with_label("b", "x", &any, None, 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        read.hyperedges_by_participants("b", &[a[0]], ParticipantMatch::Contains, &any)
            .unwrap()
            .is_empty()
    );
    assert!(read.hyperedges_page("b", None, 10).unwrap().is_empty());
    drop(read);
    // Nodes of another namespace cannot take part, nor be changed from it.
    assert!(matches!(
        db.write(|tx| tx.create_hyperedge("b", NewHyperEdge::new("x").node(b[0]).node(a[0]))),
        Err(Error::NodeNotFound(_))
    ));
    assert!(matches!(
        db.write(|tx| tx.delete_hyperedge("b", id)),
        Err(Error::HyperEdgeNotFound(_))
    ));
    // Dropping a namespace takes its hyperedges and their entries.
    db.write(|tx| tx.drop_namespace("a").map(|_| ())).unwrap();
    let read = db.begin_read().unwrap();
    assert!(read.hyperedge("a", id).unwrap().is_none());
    assert!(read.node_hyperedges("a", a[0], &any).unwrap().is_empty());
    assert!(
        read.hyperedges_with_label("a", "x", &any, None, 10)
            .unwrap()
            .is_empty()
    );
}

fn view_hiding_secrets() -> Arc<View> {
    let namespace = || Object::Namespace {
        namespace: NS.into(),
    };
    let mut privileges: Vec<Privilege> = [
        Act::Read,
        Act::Create,
        Act::Update,
        Act::Delete,
        Act::Retract,
    ]
    .into_iter()
    .map(|act| Privilege::grant(act, namespace()))
    .collect();
    privileges.push(Privilege::deny(
        Act::Read,
        Object::Label {
            namespace: NS.into(),
            label: "Secret".into(),
            edges: false,
        },
    ));
    privileges.push(Privilege::deny(
        Act::Read,
        Object::Label {
            namespace: NS.into(),
            label: "classified".into(),
            edges: true,
        },
    ));
    let role = RoleDef {
        name: "r".into(),
        privileges,
        grants: Vec::new(),
        roles: Vec::new(),
        policies: Vec::new(),
        description: None,
        created_at: Timestamp::now(),
    };
    let account = Account {
        name: "me".into(),
        kind: AccountKind::Service,
        password_hash: None,
        grants: Vec::new(),
        roles: vec!["r".into()],
        superuser: false,
        own_only: false,
        disabled: false,
        expires_at: None,
        attributes: Default::default(),
        description: None,
        created_at: Timestamp::now(),
    };
    let rights = Rights::compute(&account, &HashMap::from([("r".to_owned(), role)]));
    Arc::new(View::new(Arc::new(rights), Some(&account)))
}

#[test]
fn views_hide_hyperedges_with_hidden_parts() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 2);
    let secret = db
        .write(|tx| tx.create_node(NS, NewNode::new("Secret")))
        .unwrap();
    let (open, with_secret, classified) = db
        .write(|tx| {
            let open = tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1]))?;
            let with_secret =
                tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(secret))?;
            let classified =
                tx.create_hyperedge(NS, NewHyperEdge::new("classified").node(n[0]).node(n[1]))?;
            Ok((open, with_secret, classified))
        })
        .unwrap();
    let view = view_hiding_secrets();
    let read = db.begin_read().unwrap().with_view(Some(view.clone()));
    let any = HyperEdgeFilter::any();
    assert!(read.hyperedge(NS, open).unwrap().is_some());
    assert!(read.hyperedge(NS, with_secret).unwrap().is_none());
    assert!(read.hyperedge(NS, classified).unwrap().is_none());
    assert_eq!(
        ids(&read.node_hyperedges(NS, n[0], &any).unwrap()),
        vec![open]
    );
    // The hidden node's incidence is not a way in.
    assert!(read.node_hyperedges(NS, secret, &any).unwrap().is_empty());
    assert!(
        read.hyperedges_by_participants(NS, &[secret], ParticipantMatch::Contains, &any)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ids(&read.hyperedges_page(NS, None, 10).unwrap()),
        vec![open]
    );
    drop(read);

    // Writes through the view: hidden nodes cannot take part, hidden
    // hyperedges cannot be changed (and are not named).
    let as_me = |f: &dyn Fn(&mut grafeno::WriteTx) -> Result<(), Error>| {
        db.write(|tx| {
            tx.act_as(Some(Actor::new("me").with_view(view.clone())));
            f(tx)
        })
    };
    assert!(matches!(
        as_me(&|tx| tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(secret)).map(|_| ())),
        Err(Error::NodeNotFound(id)) if id == secret
    ));
    assert!(matches!(
        as_me(&|tx| tx.delete_hyperedge(NS, with_secret)),
        Err(Error::HyperEdgeNotFound(_))
    ));
    assert!(matches!(
        as_me(&|tx| tx.delete_hyperedge(NS, classified)),
        Err(Error::HyperEdgeNotFound(_))
    ));
    as_me(&|tx| {
        tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1]))
            .map(|_| ())
    })
    .unwrap();
}

#[test]
fn own_only_actors_change_only_their_hyperedges() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 2);
    let theirs = db
        .write(|tx| {
            tx.act_as(Some(Actor::new("other")));
            tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1]))
        })
        .unwrap();
    let mine = db
        .write(|tx| {
            tx.act_as(Some(Actor::new("me").own_only()));
            tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1]))
        })
        .unwrap();
    assert_eq!(
        db.begin_read()
            .unwrap()
            .hyperedge(NS, mine)
            .unwrap()
            .unwrap()
            .recorded_by
            .as_deref(),
        Some("me")
    );
    let as_me = |id| {
        db.write(|tx| {
            tx.act_as(Some(Actor::new("me").own_only()));
            tx.delete_hyperedge(NS, id)
        })
    };
    assert!(matches!(as_me(theirs), Err(Error::Forbidden(_))));
    as_me(mine).unwrap();
}

#[test]
fn a_failed_transaction_leaves_nothing() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 3);
    let kept = db
        .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1])))
        .unwrap();
    let result: Result<(), Error> = db.write(|tx| {
        tx.create_hyperedge(NS, NewHyperEdge::new("y").node(n[1]).node(n[2]))?;
        tx.set_hyperedge_participants(
            NS,
            kept,
            vec![Participant::new(n[1]), Participant::new(n[2])],
        )?;
        tx.delete_node_with(NS, n[0], OnHyperEdges::Cascade)?;
        // Fails last: every write above is undone.
        tx.create_hyperedge(NS, NewHyperEdge::new("z").node(n[0]).node(NodeId::new()))?;
        Ok(())
    });
    assert!(matches!(result, Err(Error::NodeNotFound(_))));
    let read = db.begin_read().unwrap();
    assert_eq!(
        ids(&read.hyperedges_page(NS, None, 10).unwrap()),
        vec![kept]
    );
    assert_eq!(read.hyperedge(NS, kept).unwrap().unwrap().nodes(), {
        let mut both = vec![n[0], n[1]];
        both.sort();
        both
    });
    assert!(read.node(NS, n[0]).unwrap().is_some());
    assert!(
        read.node_hyperedges(NS, n[2], &HyperEdgeFilter::any())
            .unwrap()
            .is_empty()
    );
    drop(read);
    check(&db, NS);
}

#[test]
fn hyperedges_survive_reopening_and_backups() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hyper.redb");
    let (n, id) = {
        let db = Grafeno::open(&path).unwrap();
        let n = nodes(&db, NS, 3);
        let id = db
            .write(|tx| {
                tx.create_hyperedge(
                    NS,
                    NewHyperEdge::new("x")
                        .member(n[0], "a")
                        .node(n[1])
                        .node(n[2]),
                )
            })
            .unwrap();
        (n, id)
    };
    let db = Grafeno::open(&path).unwrap();
    let read = db.begin_read().unwrap();
    assert_eq!(
        read.hyperedge(NS, id).unwrap().unwrap().with_role("a"),
        vec![n[0]]
    );
    assert_eq!(
        ids(&read
            .node_hyperedges(NS, n[2], &HyperEdgeFilter::any())
            .unwrap()),
        vec![id]
    );
    let copy = dir.path().join("copy.redb");
    read.backup(&copy).unwrap();
    drop(read);
    drop(db);
    let restored = Grafeno::open(&copy).unwrap();
    let read = restored.begin_read().unwrap();
    assert_eq!(
        ids(&read
            .hyperedges_with_label(NS, "x", &HyperEdgeFilter::any(), None, 10)
            .unwrap()),
        vec![id]
    );
    drop(read);
    check(&restored, NS);
}

#[test]
fn check_finds_broken_entries_and_rebuild_repairs_them() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 3);
    db.write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1]).node(n[2])))
        .unwrap();
    let report = db.write(|tx| tx.rebuild_indexes(NS)).unwrap();
    assert_eq!((report.hyperedges, report.dropped_hyperedges), (1, 0));
    let checked = db.begin_read().unwrap().check(NS).unwrap();
    assert_eq!(checked.hyperedges, 1);
    assert!(checked.is_ok(), "{:#?}", checked.problems);
}

/// Every hyperedge of `ns`, as a reader sees it, for comparing databases.
fn hyperedges_of(db: &Grafeno, ns: &str) -> Vec<grafeno::HyperEdge> {
    db.begin_read()
        .unwrap()
        .hyperedges_page(ns, None, usize::MAX)
        .unwrap()
}

fn replay(from: &Grafeno, onto: &Grafeno, after: u64) -> usize {
    let entries = from
        .begin_read()
        .unwrap()
        .log_entries(after, usize::MAX)
        .unwrap();
    onto.apply_entries(&entries).unwrap()
}

#[test]
fn the_replication_log_carries_hyperedges() {
    let leader = Grafeno::in_memory().unwrap();
    // A hyperedge written before the log is on reaches members through
    // the backup they start from.
    let n = nodes(&leader, NS, 5);
    let early = leader
        .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("early").node(n[0]).node(n[1])))
        .unwrap();
    leader.enable_replication_log().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("member.redb");
    leader.begin_read().unwrap().backup(&copy).unwrap();
    let member = Grafeno::open(&copy).unwrap();
    let start = leader.begin_read().unwrap().log_position().unwrap().last;

    // Every kind of write, each in its own transaction.
    let id = leader
        .write(|tx| {
            tx.create_hyperedge(
                NS,
                NewHyperEdge::new("decision")
                    .member(n[0], "proposer")
                    .member(n[1], "reviewer")
                    .node(n[2])
                    .prop("status", "open"),
            )
        })
        .unwrap();
    leader
        .write(|tx| {
            let mut props = grafeno::Props::new();
            props.insert("status".into(), "done".into());
            tx.set_hyperedge_props(NS, id, props)?;
            tx.set_hyperedge_participants(
                NS,
                id,
                vec![
                    Participant::with_role(n[3], "proposer"),
                    Participant::new(n[1]),
                    Participant::new(n[2]),
                ],
            )
        })
        .unwrap();
    leader.write(|tx| tx.retract_hyperedge(NS, early)).unwrap();
    leader
        .write(|tx| tx.delete_node_with(NS, n[2], OnHyperEdges::Detach))
        .unwrap();
    let doomed = leader
        .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[3]).node(n[4])))
        .unwrap();
    leader.write(|tx| tx.delete_hyperedge(NS, doomed)).unwrap();
    leader
        .write(|tx| tx.delete_node_with(NS, n[4], OnHyperEdges::Cascade))
        .unwrap();

    // The entries that carry hyperedges have format 8; the rest keep theirs.
    let entries = leader
        .begin_read()
        .unwrap()
        .log_entries(start, usize::MAX)
        .unwrap();
    for (_, entry) in &entries {
        let carries = entry
            .ops
            .iter()
            .any(|op| matches!(op, grafeno::Op::HyperEdge { .. }));
        assert_eq!(entry.format == 8, carries, "{entry:?}");
    }
    assert_eq!(entries.iter().filter(|(_, e)| e.format == 8).count(), 6);
    let plain = leader
        .write(|tx| tx.create_node(NS, NewNode::new("Plain")))
        .unwrap();
    let last = leader
        .begin_read()
        .unwrap()
        .log_entries(start, usize::MAX)
        .unwrap();
    assert!(last.last().unwrap().1.format < 8, "{plain}");

    assert_eq!(replay(&leader, &member, start), entries.len() + 1);
    assert_eq!(hyperedges_of(&member, NS), hyperedges_of(&leader, NS));
    let read = member.begin_read().unwrap();
    for node in &n {
        assert_eq!(
            ids(&read
                .node_hyperedges(NS, *node, &HyperEdgeFilter::any())
                .unwrap()),
            ids(&leader
                .begin_read()
                .unwrap()
                .node_hyperedges(NS, *node, &HyperEdgeFilter::any())
                .unwrap()),
        );
    }
    assert!(read.hyperedge(NS, early).unwrap().unwrap().is_retracted());
    drop(read);
    check(&member, NS);

    // A member of the previous format refuses these entries instead of
    // losing them.
    let entry = &entries[0].1;
    assert_eq!(entry.format, grafeno::ENTRY_FORMAT);
    let text = serde_json::to_string(entry).unwrap();
    assert!(text.contains("\"op\":\"hyper_edge\""), "{text}");
}

#[test]
fn a_namespace_copy_takes_its_hyperedges() {
    let source = Grafeno::in_memory().unwrap();
    let n = nodes(&source, NS, 3);
    source
        .write(|tx| {
            tx.create_hyperedge(
                NS,
                NewHyperEdge::new("x")
                    .member(n[0], "a")
                    .node(n[1])
                    .node(n[2]),
            )
        })
        .unwrap();
    let mut ops = Vec::new();
    source
        .begin_read()
        .unwrap()
        .namespace_ops(NS, 2, |chunk| {
            ops.extend(chunk);
            Ok(())
        })
        .unwrap();
    let target = Grafeno::in_memory().unwrap();
    target
        .write(|tx| tx.import_ops(&ops, Vec::new(), None))
        .unwrap();
    assert_eq!(hyperedges_of(&target, NS), hyperedges_of(&source, NS));
    check(&target, NS);
}

#[test]
fn writes_captured_for_a_cluster_carry_hyperedges() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 2);
    // What a cluster leader does: run the write, keep its entry, apply it.
    let (id, entry) = db
        .dry_run(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1])))
        .unwrap();
    let entry = entry.expect("an entry");
    assert_eq!(entry.format, 8);
    assert!(
        db.begin_read()
            .unwrap()
            .hyperedge(NS, id)
            .unwrap()
            .is_none()
    );
    db.apply_entries(&[(1, entry)]).unwrap();
    assert!(
        db.begin_read()
            .unwrap()
            .hyperedge(NS, id)
            .unwrap()
            .is_some()
    );
    check(&db, NS);
}

#[test]
fn nodes_and_edges_are_untouched() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 3);
    db.write(|tx| {
        tx.create_edge(NS, NewEdge::new(n[0], "knows", n[1]))?;
        tx.create_hyperedge(
            NS,
            NewHyperEdge::new("knows").node(n[0]).node(n[1]).node(n[2]),
        )?;
        Ok(())
    })
    .unwrap();
    let read = db.begin_read().unwrap();
    let stats = read.stats(NS).unwrap();
    // A hyperedge is no edge and adds none.
    assert_eq!((stats.nodes, stats.edges), (3, 1));
    assert_eq!(stats.edge_labels["knows"], 1);
    let out = read
        .neighbors(NS, n[0], Direction::Out, &EdgeFilter::any())
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].node, n[1]);
    assert!(
        read.neighbors(NS, n[2], Direction::Both, &EdgeFilter::any())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn concurrent_writers_keep_the_indexes_whole() {
    let db = Arc::new(Grafeno::in_memory().unwrap());
    let n = nodes(&db, NS, 12);
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (db, done, n) = (db.clone(), done.clone(), n.clone());
        thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                let read = db.begin_read().unwrap();
                for &node in &n {
                    for edge in read
                        .node_hyperedges(NS, node, &HyperEdgeFilter::any())
                        .unwrap()
                    {
                        // Every entry leads to a hyperedge that has the node.
                        assert!(edge.has_node(node));
                    }
                }
            }
        })
    };
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let (db, n) = (db.clone(), n.clone());
            thread::spawn(move || {
                for i in 0..40usize {
                    let (a, b, c) = (
                        n[(w + i) % 12],
                        n[(w * 3 + i + 1) % 12],
                        n[(i * 5 + 2) % 12],
                    );
                    let id = db
                        .write(|tx| {
                            tx.create_hyperedge(
                                NS,
                                NewHyperEdge::new("work").node(a).node(b).node(c),
                            )
                        })
                        .or_else(|_| {
                            db.write(|tx| {
                                tx.create_hyperedge(
                                    NS,
                                    NewHyperEdge::new("work").node(n[0]).node(n[1]),
                                )
                            })
                        })
                        .unwrap();
                    if i % 3 == 0 {
                        db.write(|tx| tx.delete_hyperedge(NS, id)).unwrap();
                    } else if i % 3 == 1 {
                        db.write(|tx| {
                            tx.set_hyperedge_participants(
                                NS,
                                id,
                                vec![Participant::new(n[w]), Participant::new(n[11 - w])],
                            )
                        })
                        .unwrap();
                    }
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    done.store(true, Ordering::Release);
    reader.join().unwrap();
    let read = db.begin_read().unwrap();
    // 4 writers × 40, a third deleted.
    let all = read.hyperedges_page(NS, None, 1000).unwrap();
    assert_eq!(all.len(), 4 * 40 - 4 * 14);
    drop(read);
    check(&db, NS);
}

#[derive(Clone, Debug)]
enum Op {
    Create(Vec<(usize, u8)>),
    SetParticipants(usize, Vec<(usize, u8)>),
    DeleteHyperEdge(usize),
    DeleteNode(usize, u8),
}

fn members() -> impl Strategy<Value = Vec<(usize, u8)>> {
    prop::collection::vec((0..8usize, 0..3u8), 1..6)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => members().prop_map(Op::Create),
        2 => (0..20usize, members()).prop_map(|(i, m)| Op::SetParticipants(i, m)),
        1 => (0..20usize).prop_map(Op::DeleteHyperEdge),
        1 => (0..8usize, 0..3u8).prop_map(|(i, p)| Op::DeleteNode(i, p)),
    ]
}

fn participants(nodes: &[NodeId], list: &[(usize, u8)]) -> Vec<Participant> {
    list.iter()
        .map(|&(node, role)| match role {
            0 => Participant::new(nodes[node]),
            r => Participant::with_role(nodes[node], format!("r{r}")),
        })
        .collect()
}

/// What a valid participant list is, said independently of the engine.
fn model_valid(list: &[Participant], alive: &BTreeSet<NodeId>) -> Option<Vec<Participant>> {
    let set: BTreeSet<Participant> = list.iter().cloned().collect();
    if set.len() != list.len() || !list.iter().all(|p| alive.contains(&p.node)) {
        return None;
    }
    let distinct: BTreeSet<NodeId> = list.iter().map(|p| p.node).collect();
    (distinct.len() >= 2).then(|| set.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// After any sequence of writes, each node's hyperedges are those of
    /// the model, and check finds nothing wrong.
    #[test]
    fn incidence_follows_the_records(ops in prop::collection::vec(op(), 1..30)) {
        let db = Grafeno::in_memory().unwrap();
        let n = nodes(&db, NS, 8);
        let mut alive: BTreeSet<NodeId> = n.iter().copied().collect();
        let mut model: BTreeMap<HyperEdgeId, Vec<Participant>> = BTreeMap::new();
        let mut created: Vec<HyperEdgeId> = Vec::new();
        for op in ops {
            match op {
                Op::Create(list) => {
                    let list = participants(&n, &list);
                    let mut new = NewHyperEdge::new("h");
                    new.participants = list.clone();
                    let result = db.write(|tx| tx.create_hyperedge(NS, new));
                    match model_valid(&list, &alive) {
                        Some(sorted) => {
                            let id = result.unwrap();
                            model.insert(id, sorted);
                            created.push(id);
                        }
                        None => prop_assert!(result.is_err()),
                    }
                }
                Op::SetParticipants(i, list) => {
                    let Some(&id) = created.get(i) else { continue };
                    let list = participants(&n, &list);
                    let result = db.write(|tx| tx.set_hyperedge_participants(NS, id, list.clone()));
                    match (model.contains_key(&id), model_valid(&list, &alive)) {
                        (true, Some(sorted)) => {
                            result.unwrap();
                            model.insert(id, sorted);
                        }
                        _ => prop_assert!(result.is_err()),
                    }
                }
                Op::DeleteHyperEdge(i) => {
                    let Some(&id) = created.get(i) else { continue };
                    let result = db.write(|tx| tx.delete_hyperedge(NS, id));
                    prop_assert_eq!(result.is_ok(), model.remove(&id).is_some());
                }
                Op::DeleteNode(i, policy) => {
                    let node = n[i];
                    let policy = [OnHyperEdges::Restrict, OnHyperEdges::Cascade, OnHyperEdges::Detach][policy as usize];
                    let result = db.write(|tx| tx.delete_node_with(NS, node, policy));
                    let involved = model.values().any(|list| list.iter().any(|p| p.node == node));
                    if !alive.contains(&node) {
                        prop_assert!(result.is_err());
                        continue;
                    }
                    if involved && policy == OnHyperEdges::Restrict {
                        prop_assert!(matches!(result, Err(Error::Conflict(_))));
                        continue;
                    }
                    result.unwrap();
                    alive.remove(&node);
                    model.retain(|_, list| {
                        if !list.iter().any(|p| p.node == node) {
                            return true;
                        }
                        if policy == OnHyperEdges::Cascade {
                            return false;
                        }
                        list.retain(|p| p.node != node);
                        list.iter().map(|p| p.node).collect::<BTreeSet<_>>().len() >= 2
                    });
                }
            }
        }
        let read = db.begin_read().unwrap();
        for &node in &n {
            let found: Vec<HyperEdgeId> = read.node_hyperedges(NS, node, &HyperEdgeFilter::any()).unwrap().iter().map(|e| e.id).collect();
            let expected: Vec<HyperEdgeId> = model.iter().filter(|(_, list)| list.iter().any(|p| p.node == node)).map(|(id, _)| *id).collect();
            prop_assert_eq!(found, expected);
        }
        for (id, list) in &model {
            prop_assert_eq!(&read.hyperedge(NS, *id).unwrap().unwrap().participants, list);
        }
        prop_assert_eq!(read.hyperedges_page(NS, None, 1000).unwrap().len(), model.len());
        let report = read.check(NS).unwrap();
        prop_assert!(report.is_ok(), "{:#?}", report.problems);
    }
}

#[test]
fn merging_away_a_participant_is_refused_whole() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 3);
    db.write(|tx| {
        tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(n[1]))?;
        tx.create_edge(NS, NewEdge::new(n[1], "knows", n[2]))
    })
    .unwrap();
    // The merged node would be deleted from under its hyperedge.
    assert!(matches!(
        db.write(|tx| tx.merge_nodes(NS, n[2], n[1])),
        Err(Error::Conflict(_))
    ));
    let read = db.begin_read().unwrap();
    assert!(read.node(NS, n[1]).unwrap().is_some());
    assert_eq!(
        read.neighbors(NS, n[1], Direction::Out, &EdgeFilter::any())
            .unwrap()
            .len(),
        1
    );
    assert!(read.merges(NS, None).unwrap().is_empty());
    drop(read);
    // A node in no hyperedge merges as before.
    db.write(|tx| tx.merge_nodes(NS, n[1], n[2]).map(|_| ()))
        .unwrap();
    check(&db, NS);
}

#[test]
fn hidden_hyperedges_are_not_named_when_a_node_goes() {
    let db = Grafeno::in_memory().unwrap();
    let n = nodes(&db, NS, 2);
    let secret = db
        .write(|tx| tx.create_node(NS, NewNode::new("Secret")))
        .unwrap();
    let hidden = db
        .write(|tx| tx.create_hyperedge(NS, NewHyperEdge::new("x").node(n[0]).node(secret)))
        .unwrap();
    let view = view_hiding_secrets();
    for policy in [OnHyperEdges::Cascade, OnHyperEdges::Detach] {
        let result = db.write(|tx| {
            tx.act_as(Some(Actor::new("me").with_view(view.clone())));
            tx.delete_node_with(NS, n[0], policy)
        });
        match result {
            Err(Error::Forbidden(message)) => assert!(!message.contains(&hidden.to_string())),
            other => panic!("{policy:?}: {other:?}"),
        }
    }
    let read = db.begin_read().unwrap();
    assert!(read.hyperedge(NS, hidden).unwrap().is_some());
    assert!(read.node(NS, n[0]).unwrap().is_some());
}
