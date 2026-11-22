//! Phase 13.6: operating a cluster in one process. A backup taken on a
//! follower starts a new cluster; a member joins from an older backup and
//! catches up from the log, without a snapshot; a member started that way
//! restarts from what it persisted; the leader writes no entry format a
//! voter cannot read; the status counts elections, leader changes and
//! snapshots, and shows each member's lag.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use grafeno::{Grafeno, NewNode, NodeId, Op};
use grafeno_cluster::memory::Hub;
use grafeno_cluster::{Error, MemberInfo, Node, NodeOptions, Storage};

const NS: &str = "c";

fn options(id: u64, dir: &Path) -> NodeOptions {
    let mut options = NodeOptions::new(id, dir.join(format!("snapshots-{id}")));
    options.tick = Duration::from_millis(20);
    options.write_timeout = Duration::from_secs(3);
    options
}

fn start(hub: &Arc<Hub>, dir: &Path, db: &Arc<Grafeno>, id: u64, voters: &[u64]) -> Arc<Node> {
    let storage = Storage::open(&dir.join(format!("raft-{id}.redb"))).unwrap();
    let node = Arc::new(
        Node::start(
            db.clone(),
            storage,
            hub.transport(),
            voters,
            options(id, dir),
        )
        .unwrap(),
    );
    hub.register(&node);
    node
}

fn leader(nodes: &[&Arc<Node>]) -> Arc<Node> {
    for _ in 0..500 {
        if let Some(node) = nodes.iter().find(|n| n.status().role == "leader") {
            return (*node).clone();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no leader");
}

fn write(nodes: &[&Arc<Node>], name: &str) {
    for _ in 0..50 {
        let name = name.to_owned();
        match leader(nodes).write(move |tx| {
            tx.create_node(NS, NewNode::new("Note").prop("name", name.as_str()))
                .map(|_| ())
        }) {
            Ok(()) => return,
            Err(Error::NotLeader(_) | Error::Lost | Error::Timeout) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => panic!("{error}"),
        }
    }
    panic!("the write never committed");
}

fn names(db: &Grafeno) -> Vec<String> {
    let mut names: Vec<String> = db
        .begin_read()
        .unwrap()
        .nodes(NS)
        .unwrap()
        .into_iter()
        .filter_map(|n| {
            n.props
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

fn settle(nodes: &[&Arc<Node>]) {
    for _ in 0..500 {
        let statuses: Vec<_> = nodes.iter().map(|n| n.status()).collect();
        let commit = statuses.iter().map(|s| s.commit).max().unwrap_or(0);
        if statuses
            .iter()
            .all(|s| s.applied >= commit && s.applied == statuses[0].applied)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("members did not settle");
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("grafeno-ops-{name}-{}", NodeId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn clusters_start_and_members_join_from_backups() {
    // The first cluster, on files.
    let first = temp("first");
    let hub = Hub::new();
    let dbs: Vec<Arc<Grafeno>> = (1..=3)
        .map(|id| Arc::new(Grafeno::open(first.join(format!("db-{id}.redb"))).unwrap()))
        .collect();
    let nodes: Vec<Arc<Node>> = (1..=3)
        .map(|id| start(&hub, &first, &dbs[id as usize - 1], id, &[1, 2, 3]))
        .collect();
    let refs: Vec<&Arc<Node>> = nodes.iter().collect();
    for i in 0..5 {
        write(&refs, &format!("before {i}"));
    }
    settle(&refs);
    // A backup taken on a follower.
    let follower = nodes
        .iter()
        .position(|n| n.status().role != "leader")
        .unwrap();
    let backup = first.join("backup.redb");
    dbs[follower].begin_read().unwrap().backup(&backup).unwrap();
    let backed_up = dbs[follower]
        .begin_read()
        .unwrap()
        .log_position()
        .unwrap()
        .applied;
    assert!(backed_up > 5);
    for node in &nodes {
        node.stop();
    }

    // A new cluster from the backup: every member starts from a copy.
    let second = temp("second");
    let hub = Hub::new();
    let open_copy = |id: u64| {
        let path = second.join(format!("db-{id}.redb"));
        std::fs::copy(&backup, &path).unwrap();
        Arc::new(Grafeno::open(path).unwrap())
    };
    let dbs: Vec<Arc<Grafeno>> = (1..=3).map(open_copy).collect();
    let mut nodes: Vec<Arc<Node>> = (1..=3)
        .map(|id| start(&hub, &second, &dbs[id as usize - 1], id, &[1, 2, 3]))
        .collect();
    {
        let refs: Vec<&Arc<Node>> = nodes.iter().collect();
        for i in 0..5 {
            write(&refs, &format!("after {i}"));
        }
        settle(&refs);
        assert!(refs[0].status().applied > backed_up);
    }
    let expected: Vec<String> = (0..5)
        .map(|i| format!("after {i}"))
        .chain((0..5).map(|i| format!("before {i}")))
        .collect();
    for db in &dbs {
        assert_eq!(names(db), expected);
    }

    // A member joins from the (older) backup and catches up from the log.
    let joining_db = open_copy(4);
    let joining = start(&hub, &second, &joining_db, 4, &[]);
    nodes.push(joining.clone());
    let refs: Vec<&Arc<Node>> = nodes.iter().collect();
    leader(&refs[..3]).change_membership(&[1, 2, 3, 4]).unwrap();
    write(&refs, "with four");
    settle(&refs);
    let mut four = expected.clone();
    four.insert(0, "with four".into());
    four.sort();
    assert_eq!(names(&joining_db), four);
    assert_eq!(
        joining.status().snapshots_installed,
        0,
        "it caught up from the log"
    );
    assert_eq!(joining.status().voters, vec![1, 2, 3, 4]);

    // A member started from a backup restarts from what it persisted.
    let at = 0;
    nodes[at].stop();
    nodes[at] = start(&hub, &second, &dbs[at], 1, &[1, 2, 3]);
    let refs: Vec<&Arc<Node>> = nodes.iter().collect();
    write(&refs, "after restart");
    settle(&refs);
    for db in dbs.iter().chain([&joining_db]) {
        assert!(names(db).contains(&"after restart".to_owned()));
    }

    // The status counts what happened and shows the lag on the leader.
    let lead = leader(&refs);
    let status = lead.status();
    assert!(status.leader_changes >= 1, "{status:?}");
    assert_eq!(status.peers.len(), 3, "{status:?}");
    assert!(status.peers.iter().all(|p| p.lag == 0), "{status:?}");
    assert!(refs.iter().map(|n| n.status().elections).sum::<u64>() >= 1);
    for node in &nodes {
        node.stop();
    }
    let _ = std::fs::remove_dir_all(&first);
    let _ = std::fs::remove_dir_all(&second);
}

#[test]
fn the_leader_writes_no_format_a_member_cannot_read() {
    let dir = temp("format");
    let hub = Hub::new();
    let dbs: Vec<Arc<Grafeno>> = (0..3)
        .map(|_| Arc::new(Grafeno::in_memory().unwrap()))
        .collect();
    let nodes: Vec<Arc<Node>> = (1..=3)
        .map(|id| start(&hub, &dir, &dbs[id as usize - 1], id, &[1, 2, 3]))
        .collect();
    let refs: Vec<&Arc<Node>> = nodes.iter().collect();
    write(&refs, "first");
    let lead = leader(&refs);
    // A change log entry copied as it is needs format 2 (a namespace move).
    let copy = || {
        let ops = vec![Op::Change {
            ns: NS.into(),
            seq: 99,
            change: None,
        }];
        move |tx: &mut grafeno::WriteTx| tx.import_ops(&ops, Vec::new(), None)
    };
    // Members not heard from with a format are taken to read only 1.
    assert_eq!(lead.cluster_format(), 1);
    let refused = lead.write(copy()).unwrap_err();
    assert!(
        refused.to_string().contains("upgrade every member"),
        "{refused}"
    );
    // Plain writes are format 1 and go on.
    write(&refs, "second");
    // Once every voter says it reads format 2, the leader writes it.
    for id in 1..=3 {
        lead.note_member(id, MemberInfo::here());
    }
    assert_eq!(lead.cluster_format(), grafeno::ENTRY_FORMAT);
    lead.write(copy()).unwrap();
    // A member of the version before brings it back to 1.
    let old = MemberInfo {
        version: "0.0.0".into(),
        format: 1,
        layout: grafeno::SCHEMA_VERSION,
    };
    let other = (1..=3).find(|id| *id != lead.id()).unwrap();
    lead.note_member(other, old);
    assert_eq!(lead.cluster_format(), 1);
    assert!(lead.write(copy()).is_err());
    for node in &nodes {
        node.stop();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Waits until every database holds `count` nodes of `NS`.
fn all_hold(dbs: &[Arc<Grafeno>], count: u64) {
    for _ in 0..500 {
        if dbs
            .iter()
            .all(|db| db.begin_read().unwrap().stats(NS).unwrap().nodes == count)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let counts: Vec<u64> = dbs
        .iter()
        .map(|db| db.begin_read().unwrap().stats(NS).unwrap().nodes)
        .collect();
    panic!("members hold {counts:?} nodes, not {count}");
}

#[test]
fn a_member_of_the_version_before_holds_back_hyperedges_only() {
    let dir = temp("rolling");
    let hub = Hub::new();
    let dbs: Vec<Arc<Grafeno>> = (0..3)
        .map(|_| Arc::new(Grafeno::in_memory().unwrap()))
        .collect();
    let nodes: Vec<Arc<Node>> = (1..=3)
        .map(|id| start(&hub, &dir, &dbs[id as usize - 1], id, &[1, 2, 3]))
        .collect();
    let refs: Vec<&Arc<Node>> = nodes.iter().collect();
    write(&refs, "a");
    write(&refs, "b");
    let lead = leader(&refs);
    // Every member upgraded but one, which still runs 0.4 (format 7).
    for id in 1..=3 {
        lead.note_member(id, MemberInfo::here());
    }
    let old = (1..=3).find(|id| *id != lead.id()).unwrap();
    lead.note_member(
        old,
        MemberInfo {
            version: "0.4.0".into(),
            format: 7,
            layout: 18,
        },
    );
    assert_eq!(lead.cluster_format(), 7);
    all_hold(&dbs, 2);
    let ids = dbs[0]
        .begin_read()
        .unwrap()
        .nodes_page(NS, None, None, 10)
        .unwrap()
        .iter()
        .map(|node| node.id)
        .collect::<Vec<NodeId>>();
    let (a, b) = (ids[0], ids[1]);
    // Hyperedges, and indexes on them, wait for the upgrade.
    let refused = lead
        .write(move |tx| {
            tx.create_hyperedge(NS, grafeno::NewHyperEdge::new("pair").node(a).node(b))
                .map(|_| ())
        })
        .unwrap_err();
    assert!(
        refused.to_string().contains("upgrade every member"),
        "{refused}"
    );
    let refused = lead
        .write(|tx| {
            tx.create_index(NS, grafeno::IndexDef::on_hyperedges("pair", &["k"]))
                .map(|_| ())
        })
        .unwrap_err();
    assert!(
        refused.to_string().contains("upgrade every member"),
        "{refused}"
    );
    // Everything else goes on, to every member, the old one too.
    write(&refs, "c");
    lead.write(move |tx| {
        tx.create_edge(NS, grafeno::NewEdge::new(a, "knows", b))
            .map(|_| ())
    })
    .unwrap();
    lead.write(|tx| {
        tx.create_index(NS, grafeno::IndexDef::new("Note", "name"))
            .map(|_| ())
    })
    .unwrap();
    all_hold(&dbs, 3);
    for db in &dbs {
        let read = db.begin_read().unwrap();
        assert_eq!(read.stats(NS).unwrap().edges, 1);
        assert!(read.hyperedges_page(NS, None, 10).unwrap().is_empty());
    }
    // Upgraded, it reads format 8: hyperedges are written, everywhere.
    lead.note_member(old, MemberInfo::here());
    assert_eq!(lead.cluster_format(), grafeno::ENTRY_FORMAT);
    lead.write(move |tx| {
        tx.create_hyperedge(NS, grafeno::NewHyperEdge::new("pair").node(a).node(b))
            .map(|_| ())
    })
    .unwrap();
    for _ in 0..500 {
        if dbs.iter().all(|db| {
            db.begin_read()
                .unwrap()
                .hyperedges_page(NS, None, 10)
                .unwrap()
                .len()
                == 1
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for db in &dbs {
        assert_eq!(
            db.begin_read()
                .unwrap()
                .hyperedges_page(NS, None, 10)
                .unwrap()
                .len(),
            1
        );
    }
    for node in &nodes {
        node.stop();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
