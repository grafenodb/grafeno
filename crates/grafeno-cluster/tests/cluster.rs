//! Phase 13.2: members in one process, connected by the memory transport.
//! Writes go through the leader and reach every member; a new leader takes
//! over when the old one is cut off; members restart from what they
//! persisted; members join (from the log or a snapshot) and leave.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use grafeno::{Grafeno, NewEdge, NewNode, NodeId};
use grafeno_cluster::memory::Hub;
use grafeno_cluster::{Error, Node, NodeOptions, Storage};

const NS: &str = "c";

fn options(id: u64, dir: &std::path::Path) -> NodeOptions {
    let mut options = NodeOptions::new(id, dir.join(format!("snapshots-{id}")));
    // Windows timers tick every ~15 ms: shorter ticks come in bursts.
    options.tick = Duration::from_millis(20);
    options.write_timeout = Duration::from_secs(3);
    options
}

struct Cluster {
    hub: Arc<Hub>,
    dir: PathBuf,
    nodes: Vec<Option<Arc<Node>>>,
    dbs: Vec<Arc<Grafeno>>,
}

impl Cluster {
    fn new(founders: u64, total: u64) -> Self {
        let dir = std::env::temp_dir().join(format!("grafeno-cluster-{}", NodeId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cluster = Self {
            hub: Hub::new(),
            dir,
            nodes: Vec::new(),
            dbs: Vec::new(),
        };
        let voters: Vec<u64> = (1..=founders).collect();
        for id in 1..=total {
            cluster.dbs.push(Arc::new(Grafeno::in_memory().unwrap()));
            cluster.nodes.push(None);
            let start_voters = if id <= founders {
                voters.clone()
            } else {
                Vec::new()
            };
            cluster.start(id, &start_voters);
        }
        cluster
    }

    fn start(&mut self, id: u64, voters: &[u64]) {
        let storage = Storage::open(&self.dir.join(format!("raft-{id}.redb"))).unwrap();
        let node = Arc::new(
            Node::start(
                self.dbs[(id - 1) as usize].clone(),
                storage,
                self.hub.transport(),
                voters,
                options(id, &self.dir),
            )
            .unwrap(),
        );
        self.hub.register(&node);
        self.nodes[(id - 1) as usize] = Some(node);
    }

    fn stop(&mut self, id: u64) {
        if let Some(node) = self.nodes[(id - 1) as usize].take() {
            node.stop();
        }
    }

    fn node(&self, id: u64) -> &Arc<Node> {
        self.nodes[(id - 1) as usize].as_ref().unwrap()
    }

    fn running(&self) -> impl Iterator<Item = &Arc<Node>> {
        self.nodes.iter().flatten()
    }

    /// The leader among `among`, once there is one.
    fn leader(&self, among: &[u64]) -> u64 {
        for _ in 0..400 {
            for id in among {
                if let Some(Some(node)) = self.nodes.get((*id - 1) as usize)
                    && node.status().role == "leader"
                {
                    return *id;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no leader among {among:?}");
    }

    /// Writes on the leader among `among`, trying again across a change of
    /// leader, as a client would.
    fn write(
        &self,
        among: &[u64],
        f: impl Fn(&mut grafeno::WriteTx) -> grafeno::Result<()> + Copy,
    ) {
        for _ in 0..20 {
            let leader = self.leader(among);
            match self.node(leader).write(f) {
                Ok(()) => return,
                Err(Error::NotLeader(_) | Error::Lost | Error::Timeout) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => panic!("{error}"),
            }
        }
        panic!("the write never committed");
    }

    /// Waits until the members `ids` have all applied the same entries,
    /// up to the highest commit any of them knows.
    fn settle(&self, ids: &[u64]) {
        for _ in 0..1_000 {
            let statuses: Vec<_> = ids.iter().map(|id| self.node(*id).status()).collect();
            let commit = statuses.iter().map(|s| s.commit).max().unwrap_or(0);
            if statuses
                .iter()
                .all(|s| s.applied >= commit && s.applied == statuses[0].applied)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let statuses: Vec<_> = ids.iter().map(|id| self.node(*id).status()).collect();
        panic!("members did not settle: {statuses:#?}");
    }

    fn dump(&self, id: u64) -> String {
        let mut out = Vec::new();
        self.dbs[(id - 1) as usize]
            .begin_read()
            .unwrap()
            .export(&[], &mut out)
            .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_same(&self, ids: &[u64]) {
        self.settle(ids);
        let reference = self.dump(ids[0]);
        assert!(!reference.is_empty());
        for id in &ids[1..] {
            assert_eq!(
                self.dump(*id),
                reference,
                "member {id} differs from {}",
                ids[0]
            );
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let nodes: Vec<Arc<Node>> = self.nodes.iter_mut().filter_map(Option::take).collect();
        for node in nodes {
            node.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn add_people(tx: &mut grafeno::WriteTx) -> grafeno::Result<()> {
    let ana = tx.create_node(
        NS,
        NewNode::new("Person")
            .prop("name", "Ana")
            .text("Ana writes"),
    )?;
    let bea = tx.create_node(NS, NewNode::new("Person").prop("name", "Bea"))?;
    tx.create_edge(NS, NewEdge::new(ana, "knows", bea))?;
    Ok(())
}

#[test]
fn writes_reach_every_member_through_the_leader() {
    let cluster = Cluster::new(3, 3);
    let all = [1, 2, 3];
    for _ in 0..5 {
        cluster.write(&all, add_people);
    }
    cluster.assert_same(&all);
    assert_eq!(
        cluster.dbs[0]
            .begin_read()
            .unwrap()
            .stats(NS)
            .unwrap()
            .nodes,
        10
    );
    // A follower refuses writes and names the leader.
    let leader = cluster.leader(&all);
    let follower = all.iter().copied().find(|id| *id != leader).unwrap();
    match cluster.node(follower).write(add_people) {
        Err(Error::NotLeader(Some(named))) => assert_eq!(named, leader),
        other => panic!("{other:?}"),
    }
    // Every member's position is the same, and so are the change logs.
    let positions: Vec<_> = all
        .iter()
        .map(|id| {
            cluster.dbs[(*id - 1) as usize]
                .begin_read()
                .unwrap()
                .log_position()
                .unwrap()
                .applied
        })
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] == pair[1]),
        "{positions:?}"
    );
    let changes: Vec<_> = all
        .iter()
        .map(|id| {
            cluster.dbs[(*id - 1) as usize]
                .begin_read()
                .unwrap()
                .changes(NS, 0, 1000)
                .unwrap()
        })
        .collect();
    assert!(changes.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn a_new_leader_takes_over_and_the_old_one_catches_up() {
    let cluster = Cluster::new(3, 3);
    let all = [1, 2, 3];
    cluster.write(&all, add_people);
    let old = cluster.leader(&all);
    cluster.hub.isolate(old, true);
    // The old leader cannot commit anything alone.
    let alone = cluster.node(old).write(add_people);
    assert!(
        matches!(
            alone,
            Err(Error::Timeout | Error::NotLeader(_) | Error::Lost)
        ),
        "{alone:?}"
    );
    let rest: Vec<u64> = all.iter().copied().filter(|id| *id != old).collect();
    let new = cluster.leader(&rest);
    assert_ne!(new, old);
    for _ in 0..3 {
        cluster.write(&rest, add_people);
    }
    cluster.hub.isolate(old, false);
    cluster.assert_same(&all);
    // The old leader's lone write never happened anywhere.
    assert_eq!(
        cluster.dbs[(old - 1) as usize]
            .begin_read()
            .unwrap()
            .stats(NS)
            .unwrap()
            .nodes,
        8
    );
}

#[test]
fn a_member_restarts_from_what_it_persisted() {
    let mut cluster = Cluster::new(3, 3);
    let all = [1, 2, 3];
    cluster.write(&all, add_people);
    let leader = cluster.leader(&all);
    let follower = all.iter().copied().find(|id| *id != leader).unwrap();
    cluster.stop(follower);
    let rest: Vec<u64> = all.iter().copied().filter(|id| *id != follower).collect();
    for _ in 0..4 {
        cluster.write(&rest, add_people);
    }
    cluster.start(follower, &[1, 2, 3]);
    cluster.assert_same(&all);
}

#[test]
fn members_join_from_the_log_or_a_snapshot_and_leave() {
    let cluster = Cluster::new(3, 5);
    let founders = [1, 2, 3];
    for _ in 0..3 {
        cluster.write(&founders, add_people);
    }
    // Member 4 joins and catches up from the log.
    let leader = cluster.leader(&founders);
    cluster
        .node(leader)
        .change_membership(&[1, 2, 3, 4])
        .unwrap();
    let four = [1, 2, 3, 4];
    cluster.write(&four, add_people);
    cluster.assert_same(&four);
    // The leader compacts its log: member 5 can only catch up from the
    // snapshot.
    let leader = cluster.leader(&four);
    let index = cluster.node(leader).compact().unwrap();
    assert!(index > 0);
    assert_eq!(cluster.node(leader).status().snapshot_index, index);
    for _ in 0..30 {
        if cluster
            .node(leader)
            .change_membership(&[1, 2, 3, 4, 5])
            .is_ok()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let five = [1, 2, 3, 4, 5];
    cluster.write(&five, add_people);
    cluster.assert_same(&five);
    assert!(cluster.node(5).status().snapshot_index >= index);
    // Member 1 leaves; the others go on.
    let leader = cluster.leader(&five);
    for _ in 0..30 {
        if cluster
            .node(leader)
            .change_membership(&[2, 3, 4, 5])
            .is_ok()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let rest = [2, 3, 4, 5];
    for _ in 0..50 {
        let leader = cluster.leader(&rest);
        if cluster.node(leader).status().voters == vec![2, 3, 4, 5]
            && cluster.node(leader).status().outgoing.is_empty()
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cluster.write(&rest, add_people);
    cluster.assert_same(&rest);
    let _ = cluster.running().count();
}

/// Explicit transactions (16.4): statements across calls on the leader,
/// replicated as one entry at commit; nothing from a rollback; plain writes
/// wait for the transaction; a follower refuses to begin one.
#[test]
fn explicit_transactions_commit_as_one_entry() {
    let cluster = Cluster::new(3, 3);
    let all = [1, 2, 3];
    cluster.write(&all, add_people);
    cluster.assert_same(&all);
    let leader = cluster.leader(&all);
    let node = cluster.node(leader).clone();
    let before = node.status().last_index;

    let mut transaction = node.begin_transaction(Duration::from_secs(1)).unwrap();
    add_people(transaction.tx()).unwrap();
    add_people(transaction.tx()).unwrap();
    // A plain write waits for the transaction to end.
    let waiting = {
        let node = node.clone();
        std::thread::spawn(move || node.write(add_people))
    };
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !waiting.is_finished(),
        "a plain write runs after the transaction"
    );
    // A second transaction cannot begin meanwhile.
    assert!(matches!(
        node.begin_transaction(Duration::from_millis(100)),
        Err(Error::Timeout)
    ));
    node.commit_transaction(transaction).unwrap();
    waiting.join().unwrap().unwrap();
    cluster.assert_same(&all);
    let nodes = |id: u64| {
        cluster.dbs[(id - 1) as usize]
            .begin_read()
            .unwrap()
            .stats(NS)
            .unwrap()
            .nodes
    };
    for id in all {
        assert_eq!(nodes(id), 8, "member {id}");
    }
    // The transaction's two statements are one entry; the plain write another.
    assert_eq!(node.status().last_index, before + 2);

    // Rolled back: nothing anywhere.
    let mut transaction = node.begin_transaction(Duration::from_secs(1)).unwrap();
    add_people(transaction.tx()).unwrap();
    transaction.rollback().unwrap();
    cluster.write(&all, |_| Ok(()));
    cluster.assert_same(&all);
    for id in all {
        assert_eq!(nodes(id), 8, "member {id}");
    }

    let follower = all.iter().copied().find(|id| *id != leader).unwrap();
    assert!(matches!(
        cluster
            .node(follower)
            .begin_transaction(Duration::from_millis(100)),
        Err(Error::NotLeader(_))
    ));
}
