//! A deterministic simulation of a Raft cluster: five members (three
//! voting at first), a network that drops, delays, duplicates and reorders
//! messages and splits into partitions, members that crash and restart from
//! what they persisted, membership changes and log compaction with
//! snapshots, all drawn from a seed. Every schedule checks that:
//!
//! - no term has two leaders;
//! - no two members apply different entries at the same index;
//! - every proposal acknowledged to a client (applied where it was
//!   proposed) is applied by every member of the final configuration;
//! - once the network heals and nobody crashes, a leader is elected and a
//!   new proposal commits everywhere.
//!
//! `RAFT_SEEDS=2000 cargo test --release -p grafeno-raft --test simulation`
//! runs more schedules (the default is 300).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

/// What every schedule exercised, summed: terms with a leader, snapshots
/// installed, configurations committed, acknowledged writes, crashes.
static LEADERS: AtomicU64 = AtomicU64::new(0);
static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
static CONFIGS: AtomicU64 = AtomicU64::new(0);
static ACKED: AtomicU64 = AtomicU64::new(0);
static CRASHES: AtomicU64 = AtomicU64::new(0);
static READS: AtomicU64 = AtomicU64::new(0);

use grafeno_raft::{
    Entry, Envelope, HardState, Index, Message, NodeId, Options, Payload, Raft, Role, SnapshotMeta,
};

const POOL: [NodeId; 5] = [1, 2, 3, 4, 5];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

/// What a member keeps across crashes: Raft's persisted state and the
/// state machine (the applied data, as the database is), with its snapshot.
#[derive(Clone, Default)]
struct Disk {
    hard: HardState,
    snapshot: SnapshotMeta,
    snapshot_state: Vec<(Index, Vec<u8>)>,
    log: Vec<Entry>,
    state: Vec<(Index, Vec<u8>)>,
    applied: Index,
}

struct Member {
    raft: Option<Raft>,
    disk: Disk,
    down_until: u64,
    /// Snapshot data that arrived with a message, by its index.
    incoming: BTreeMap<Index, Vec<(Index, Vec<u8>)>>,
}

struct InFlight {
    at: u64,
    envelope: Envelope,
    data: Option<Vec<(Index, Vec<u8>)>>,
}

struct Sim {
    rng: Rng,
    now: u64,
    members: BTreeMap<NodeId, Member>,
    network: Vec<InFlight>,
    /// Which side of a partition each member is on (all 0: no partition).
    side: HashMap<NodeId, u8>,
    faults: bool,
    leaders: HashMap<u64, NodeId>,
    committed: HashMap<Index, (u64, Payload)>,
    /// Proposals: value -> (member, index) where proposed.
    proposed: HashMap<Vec<u8>, (NodeId, Index)>,
    acked: BTreeSet<Vec<u8>>,
    /// The highest index of an acknowledged write.
    acked_max: Index,
    /// Reads asked: context -> the highest acknowledged index when asked.
    reads: HashMap<u64, Index>,
    next_read: u64,
    committed_configs: BTreeSet<Index>,
    next_value: u64,
    options: Options,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let options = Options {
            seed,
            ..Options::default()
        };
        let mut members = BTreeMap::new();
        for id in POOL {
            // Members 4 and 5 start outside the configuration, waiting.
            let voters: Vec<NodeId> = if id <= 3 { vec![1, 2, 3] } else { vec![] };
            let raft = Raft::new(id, voters.clone(), options);
            let disk = Disk {
                snapshot: raft.snapshot_meta().clone(),
                ..Disk::default()
            };
            members.insert(
                id,
                Member {
                    raft: Some(raft),
                    disk,
                    down_until: 0,
                    incoming: BTreeMap::new(),
                },
            );
        }
        Self {
            rng: Rng(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1),
            now: 0,
            members,
            network: Vec::new(),
            side: HashMap::new(),
            faults: true,
            leaders: HashMap::new(),
            committed: HashMap::new(),
            proposed: HashMap::new(),
            acked: BTreeSet::new(),
            acked_max: 0,
            reads: HashMap::new(),
            next_read: 0,
            committed_configs: BTreeSet::new(),
            next_value: 0,
            options,
        }
    }

    fn connected(&self, a: NodeId, b: NodeId) -> bool {
        self.side.get(&a).copied().unwrap_or(0) == self.side.get(&b).copied().unwrap_or(0)
    }

    /// Persists, sends and applies what a member's Raft asks for.
    fn process(&mut self, id: NodeId) {
        let Some(member) = self.members.get_mut(&id) else {
            return;
        };
        let Some(raft) = member.raft.as_mut() else {
            return;
        };
        let ready = raft.ready();
        let (role, term) = (raft.role(), raft.term());
        if let Some(hard) = ready.hard_state {
            member.disk.hard = hard;
        }
        if let Some(meta) = ready.snapshot {
            let data = member
                .incoming
                .remove(&meta.index)
                .expect("snapshot data arrived with its message");
            member.disk.state = data.clone();
            member.disk.snapshot_state = data;
            member.disk.applied = meta.index;
            member.disk.log.retain(|entry| entry.index > meta.index);
            member.disk.snapshot = meta;
            SNAPSHOTS.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(from) = ready.truncate_from {
            member.disk.log.retain(|entry| entry.index < from);
        }
        member.disk.log.extend(ready.entries);
        let snapshot_state = member.disk.snapshot_state.clone();
        for envelope in ready.messages {
            let data = matches!(envelope.message, Message::Snapshot { .. })
                .then(|| snapshot_state.clone());
            self.network.push(InFlight {
                at: self.now,
                envelope,
                data,
            });
        }
        let member = self.members.get_mut(&id).expect("member");
        for entry in ready.committed {
            match self.committed.get(&entry.index) {
                Some(seen) => assert_eq!(
                    seen,
                    &(entry.term, entry.payload.clone()),
                    "member {id} applied a different entry at {}",
                    entry.index
                ),
                None => {
                    self.committed
                        .insert(entry.index, (entry.term, entry.payload.clone()));
                }
            }
            if matches!(entry.payload, Payload::Config(_))
                && !self.committed_configs.contains(&entry.index)
            {
                self.committed_configs.insert(entry.index);
                CONFIGS.fetch_add(1, Ordering::Relaxed);
            }
            if let Payload::Data(data) = &entry.payload {
                member.disk.state.push((entry.index, data.clone()));
                if self.proposed.get(data) == Some(&(id, entry.index)) {
                    self.acked.insert(data.clone());
                    self.acked_max = self.acked_max.max(entry.index);
                }
            }
            member.disk.applied = entry.index;
        }
        // A linearizable read waits for an index covering every write
        // acknowledged before it was asked.
        for (context, index) in ready.reads {
            let asked = self.reads.remove(&context).expect("a read asked");
            if let Some(index) = index {
                assert!(
                    index >= asked,
                    "a read at {index} misses a write acknowledged at {asked}"
                );
                READS.fetch_add(1, Ordering::Relaxed);
            }
        }
        if role == Role::Leader {
            match self.leaders.get(&term) {
                Some(leader) => assert_eq!(*leader, id, "two leaders in term {term}"),
                None => {
                    self.leaders.insert(term, id);
                    LEADERS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn deliver(&mut self) {
        // Messages due now, in a random order.
        let mut due = Vec::new();
        let mut later = Vec::new();
        for message in self.network.drain(..) {
            if message.at <= self.now {
                due.push(message);
            } else {
                later.push(message);
            }
        }
        self.network = later;
        for i in (1..due.len()).rev() {
            let j = self.rng.below(i as u64 + 1) as usize;
            due.swap(i, j);
        }
        for message in due {
            let (from, to) = (message.envelope.from, message.envelope.to);
            if self.faults {
                if !self.connected(from, to) || self.rng.chance(50) {
                    continue;
                }
                if self.rng.chance(30) {
                    // Delay it.
                    let at = self.now + 1 + self.rng.below(8);
                    self.network.push(InFlight { at, ..message });
                    continue;
                }
                if self.rng.chance(20) {
                    // Duplicate it, later.
                    let at = self.now + 1 + self.rng.below(5);
                    self.network.push(InFlight {
                        at,
                        envelope: message.envelope.clone(),
                        data: message.data.clone(),
                    });
                }
            }
            let Some(member) = self.members.get_mut(&to) else {
                continue;
            };
            let Some(raft) = member.raft.as_mut() else {
                continue;
            };
            if let (Message::Snapshot { meta, .. }, Some(data)) =
                (&message.envelope.message, message.data)
            {
                member.incoming.insert(meta.index, data);
            }
            raft.step(message.envelope);
            self.process(to);
        }
    }

    fn propose(&mut self) {
        let leaders: Vec<NodeId> = self
            .members
            .iter()
            .filter(|(_, m)| m.raft.as_ref().is_some_and(|r| r.role() == Role::Leader))
            .map(|(id, _)| *id)
            .collect();
        let Some(&id) = leaders.get(self.rng.below(leaders.len() as u64 + 1) as usize) else {
            return;
        };
        self.next_value += 1;
        let value = format!("v{}", self.next_value).into_bytes();
        let raft = self.members.get_mut(&id).unwrap().raft.as_mut().unwrap();
        if self.rng.chance(300) {
            self.next_read += 1;
            let context = self.next_read;
            if raft.read_index(context).is_ok() {
                self.reads.insert(context, self.acked_max);
                self.process(id);
            }
            return;
        }
        if let Ok(index) = raft.propose(value.clone()) {
            self.proposed.insert(value, (id, index));
            self.process(id);
        }
    }

    fn chaos(&mut self) {
        // Crash a member now and then.
        if self.rng.chance(4) {
            let id = POOL[self.rng.below(5) as usize];
            let member = self.members.get_mut(&id).unwrap();
            if member.raft.is_some() {
                CRASHES.fetch_add(1, Ordering::Relaxed);
                member.raft = None;
                member.incoming.clear();
                member.down_until = self.now + 5 + self.rng.below(60);
            }
        }
        // Restart those whose time came.
        for id in POOL {
            let member = self.members.get_mut(&id).unwrap();
            if member.raft.is_none() && member.down_until <= self.now {
                self.restart(id);
            }
        }
        // Split the network or heal it.
        if self.rng.chance(6) {
            self.side.clear();
            if self.rng.chance(700) {
                for id in POOL {
                    let side = self.rng.below(2) as u8;
                    self.side.insert(id, side);
                }
            }
        }
        // Change the membership.
        if self.rng.chance(3) {
            let leader = self
                .members
                .iter_mut()
                .find(|(_, m)| m.raft.as_ref().is_some_and(|r| r.role() == Role::Leader));
            if let Some((&id, member)) = leader {
                let mut voters: Vec<NodeId> = POOL.to_vec();
                while voters.len() > 3 && self.rng.chance(500) {
                    let at = self.rng.below(voters.len() as u64) as usize;
                    voters.remove(at);
                }
                let _ = member.raft.as_mut().unwrap().change_membership(voters);
                self.process(id);
            }
        }
        // Compact a member's log at what it applied.
        if self.rng.chance(15) {
            let id = POOL[self.rng.below(5) as usize];
            let member = self.members.get_mut(&id).unwrap();
            if let Some(raft) = member.raft.as_mut() {
                let index = raft.applied();
                if index > raft.snapshot_meta().index && raft.compact(index).is_ok() {
                    member.disk.snapshot = raft.snapshot_meta().clone();
                    member.disk.snapshot_state = member.disk.state.clone();
                    member.disk.log.retain(|entry| entry.index > index);
                }
            }
        }
    }

    fn restart(&mut self, id: NodeId) {
        let member = self.members.get_mut(&id).unwrap();
        let disk = &member.disk;
        member.raft = Some(Raft::restore(
            id,
            self.options,
            disk.hard,
            disk.snapshot.clone(),
            disk.log.clone(),
            disk.applied,
        ));
    }

    fn tick(&mut self) {
        self.now += 1;
        for id in POOL {
            if let Some(raft) = self.members.get_mut(&id).unwrap().raft.as_mut() {
                raft.tick();
                self.process(id);
            }
        }
        self.deliver();
    }

    /// Each member's view, for failure messages.
    fn describe(&self) -> String {
        self.members
            .iter()
            .map(|(id, m)| match &m.raft {
                Some(r) => format!(
                    "
  {id}: {:?} term {} leader {:?} commit {} applied {} last {} snapshot {} config {:?} state {} tail {:?}",
                    r.role(),
                    r.term(),
                    r.leader(),
                    r.commit(),
                    r.applied(),
                    r.last_index(),
                    r.snapshot_meta().index,
                    r.config(),
                    m.disk.state.len(),
                    m.disk.state.iter().rev().take(3).map(|(i, v)| (*i, String::from_utf8_lossy(v).into_owned())).collect::<Vec<_>>()
                ),
                None => format!("
  {id}: down"),
            })
            .collect()
    }

    /// The members in the leader's configuration, if there is a leader.
    fn final_members(&self) -> Option<BTreeSet<NodeId>> {
        self.members
            .values()
            .filter_map(|m| m.raft.as_ref())
            .find(|r| r.role() == Role::Leader)
            .map(|leader| leader.config().members())
    }

    fn run(mut self, ticks: u64) {
        for _ in 0..ticks {
            if self.rng.chance(200) {
                self.propose();
            }
            self.chaos();
            self.tick();
        }
        // Heal everything and let the cluster settle.
        self.faults = false;
        self.side.clear();
        for id in POOL {
            if self.members[&id].raft.is_none() {
                self.restart(id);
            }
        }
        let mut leader = None;
        for _ in 0..400 {
            self.tick();
            leader = self
                .members
                .values()
                .filter_map(|m| m.raft.as_ref())
                .find(|r| r.role() == Role::Leader)
                .map(|r| r.id());
            if leader.is_some() {
                break;
            }
        }
        leader.expect("a leader once the network heals");
        // A last write commits everywhere in the configuration. Like a
        // client, it is proposed again when a leader loses it (Raft drops
        // uncommitted entries of a deposed leader), and a membership change
        // under way finishes first.
        let mut attempts: Vec<Vec<u8>> = Vec::new();
        let mut members = BTreeSet::new();
        let applied_attempt = |sim: &Sim, id: &NodeId, attempts: &[Vec<u8>]| {
            sim.members[id]
                .disk
                .state
                .iter()
                .any(|(_, value)| attempts.contains(value))
        };
        for round in 0..1_200 {
            if round % 150 == 0 {
                let leader = self
                    .members
                    .iter_mut()
                    .find(|(_, m)| m.raft.as_ref().is_some_and(|r| r.role() == Role::Leader));
                if let Some((&id, member)) = leader {
                    self.next_value += 1;
                    let value = format!("v{}", self.next_value).into_bytes();
                    if let Ok(index) = member.raft.as_mut().unwrap().propose(value.clone()) {
                        self.proposed.insert(value.clone(), (id, index));
                        attempts.push(value);
                        self.process(id);
                    }
                }
            }
            self.tick();
            let Some(now) = self.final_members() else {
                continue;
            };
            members = now;
            let settled = self
                .members
                .values()
                .filter_map(|m| m.raft.as_ref())
                .find(|r| r.role() == Role::Leader)
                .is_some_and(|r| !r.config().is_joint());
            if settled
                && members
                    .iter()
                    .all(|id| applied_attempt(&self, id, &attempts))
            {
                break;
            }
        }
        assert!(
            !members.is_empty(),
            "a leader at the end; members: {}",
            self.describe()
        );
        for id in &members {
            assert!(
                applied_attempt(&self, id, &attempts),
                "member {id} did not apply the last write; members: {}",
                self.describe()
            );
        }
        for id in &members {
            let state = &self.members[id].disk.state;
            let values: BTreeSet<&Vec<u8>> = state.iter().map(|(_, value)| value).collect();
            for acked in &self.acked {
                assert!(
                    values.contains(acked),
                    "member {id} lost an acknowledged write"
                );
            }
        }
        ACKED.fetch_add(self.acked.len() as u64, Ordering::Relaxed);
        // Members' applied sequences agree.
        let reference = &self.members[members.iter().next().unwrap()].disk.state;
        for id in &members {
            let state = &self.members[id].disk.state;
            let shared = state.len().min(reference.len());
            assert_eq!(
                &state[..shared],
                &reference[..shared],
                "member {id} diverged"
            );
        }
    }
}

#[test]
fn raft_survives_random_schedules() {
    let seeds: u64 = std::env::var("RAFT_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(300);
    let first = std::env::var("RAFT_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let seeds = if first > 1 { first } else { seeds };
    for seed in first..=seeds {
        let result = std::panic::catch_unwind(|| Sim::new(seed).run(1_500));
        if let Err(error) = result {
            let message = error
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| error.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            panic!("seed {seed}: {message}");
        }
    }
    let count = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let reads = count(&READS);
    let (leaders, snapshots, configs, acked, crashes) = (
        count(&LEADERS),
        count(&SNAPSHOTS),
        count(&CONFIGS),
        count(&ACKED),
        count(&CRASHES),
    );
    println!(
        "{} schedules: {leaders} terms with a leader, {snapshots} snapshots installed, {configs} configurations committed, {acked} acknowledged writes, {reads} linearizable reads, {crashes} crashes",
        seeds - first + 1
    );
    // The schedules exercise what they claim to.
    assert!(
        leaders > seeds - first + 1
            && snapshots > 0
            && configs > 0
            && acked > 0
            && crashes > 0
            && reads > 0
    );
}

#[test]
fn a_single_member_commits_alone() {
    let mut raft = Raft::new(1, [1], Options::default());
    for _ in 0..30 {
        raft.tick();
    }
    let _ = raft.ready();
    assert_eq!(raft.role(), Role::Leader);
    let index = raft.propose(b"x".to_vec()).unwrap();
    let ready = raft.ready();
    assert!(ready.committed.iter().any(|entry| entry.index == index));
}

#[test]
fn messages_round_trip_as_json() {
    let message = Message::Append {
        term: 3,
        prev_index: 7,
        prev_term: 2,
        entries: vec![Entry {
            term: 3,
            index: 8,
            payload: Payload::Data(b"abc".to_vec()),
        }],
        commit: 7,
        read: 0,
    };
    let text = serde_json::to_string(&message).unwrap();
    assert_eq!(serde_json::from_str::<Message>(&text).unwrap(), message);
}

/// Found by an outside review: an `Append` whose entry sat at the
/// snapshot's index (0 on a new member) made the follower cut its log at
/// `usize::MAX`, and the panic stopped its Raft thread. Malformed appends,
/// and appends that would replace committed entries, are now dropped.
#[test]
fn malformed_appends_change_nothing() {
    let append = |prev_index, prev_term, entries: Vec<(u64, u64)>, commit| Envelope {
        from: 99,
        to: 1,
        message: Message::Append {
            term: 50,
            prev_index,
            prev_term,
            entries: entries
                .into_iter()
                .map(|(index, term)| Entry {
                    term,
                    index,
                    payload: Payload::Data(b"x".to_vec()),
                })
                .collect(),
            commit,
            read: 0,
        },
    };
    let mut raft = Raft::new(1, [1, 2, 3], Options::default());
    // The message of the review: an entry at index 0, after index 0.
    raft.step(append(0, 0, vec![(0, 50)], 1));
    assert_eq!(raft.last_index(), 0);
    assert_eq!(raft.commit(), 0);
    // Entries out of order, with a gap, or that go back.
    raft.step(append(0, 0, vec![(2, 50)], 0));
    raft.step(append(0, 0, vec![(1, 50), (3, 50)], 0));
    raft.step(append(0, 0, vec![(1, 50), (1, 50)], 0));
    raft.step(append(u64::MAX, 0, vec![(0, 50)], 0));
    assert_eq!(raft.last_index(), 0);

    // A real append is taken, and committed entries are never replaced.
    raft.step(append(0, 0, vec![(1, 50), (2, 50)], 2));
    assert_eq!(raft.last_index(), 2);
    assert_eq!(raft.commit(), 2);
    let _ = raft.ready();
    raft.step(append(1, 50, vec![(2, 51)], 2));
    assert_eq!(raft.last_index(), 2);
    let ready = raft.ready();
    assert!(ready.committed.is_empty());
    // Past the commit, a conflict replaces what follows, as Raft says.
    raft.step(append(2, 50, vec![(3, 50), (4, 50)], 2));
    raft.step(append(2, 50, vec![(3, 52)], 2));
    assert_eq!(raft.last_index(), 3);
}
