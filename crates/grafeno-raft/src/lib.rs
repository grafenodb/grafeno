//! Raft consensus (Ongaro and Ousterhout, 2014) as a deterministic state
//! machine, for Grafeno's clusters (phase 13.2).
//!
//! [`Raft`] does no input or output and reads no clock: the caller feeds it
//! ticks ([`Raft::tick`]), messages from other members ([`Raft::step`]) and
//! proposals ([`Raft::propose`]), and collects what to do in a [`Ready`]:
//! the hard state and log entries to persist, the messages to send (only
//! after persisting), and the committed entries to apply, in order. The same
//! inputs always give the same outputs, so a whole cluster, its network and
//! its crashes can be simulated from a seed (see `tests/simulation.rs`).
//!
//! What it implements: terms and randomized election timeouts; pre-vote, so
//! that a member cut off from the others does not depose a working leader
//! when it comes back; leader stickiness and check-quorum (a leader that
//! cannot reach a majority steps down; members that hear from a leader
//! refuse to vote for others); log replication with conflict hints;
//! commitment by quorum of entries of the leader's term (with a no-op entry
//! at the start of each term); membership changes by joint consensus, run
//! in two automatic phases (old and new together, then new alone); and
//! snapshots: the log is compacted up to an applied index, and a member too
//! far behind receives the snapshot's metadata (its data travels beside the
//! message, handled by the caller).
//!
//! Linearizable reads use ReadIndex ([`Raft::read_index`]): the leader
//! notes its commit index, confirms it is still the leader by a round of
//! heartbeats a majority answers, and only then hands the index back; a
//! read served once that index is applied sees every write acknowledged
//! before it began.
//!
//! Not here: read leases, learners, leadership transfer.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub type NodeId = u64;
pub type Term = u64;
pub type Index = u64;

/// The voting members, and during a change the previous ones as well
/// (joint consensus: a decision needs a majority of each).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub voters: BTreeSet<NodeId>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub outgoing: BTreeSet<NodeId>,
}

impl Config {
    pub fn new(voters: impl IntoIterator<Item = NodeId>) -> Self {
        Self {
            voters: voters.into_iter().collect(),
            outgoing: BTreeSet::new(),
        }
    }

    pub fn is_joint(&self) -> bool {
        !self.outgoing.is_empty()
    }

    /// Every member that votes in this configuration.
    pub fn members(&self) -> BTreeSet<NodeId> {
        self.voters.union(&self.outgoing).copied().collect()
    }

    fn majority(set: &BTreeSet<NodeId>, ok: &impl Fn(NodeId) -> bool) -> bool {
        set.iter().filter(|id| ok(**id)).count() * 2 > set.len()
    }

    /// Whether the members for which `ok` holds decide.
    pub fn quorum(&self, ok: impl Fn(NodeId) -> bool) -> bool {
        Self::majority(&self.voters, &ok)
            && (self.outgoing.is_empty() || Self::majority(&self.outgoing, &ok))
    }

    /// The highest index a quorum has, given each member's.
    fn agreed(&self, matched: impl Fn(NodeId) -> Index) -> Index {
        let of = |set: &BTreeSet<NodeId>| {
            let mut values: Vec<Index> = set.iter().map(|id| matched(*id)).collect();
            values.sort_unstable_by(|a, b| b.cmp(a));
            values.get(set.len() / 2).copied().unwrap_or(0)
        };
        let agreed = of(&self.voters);
        if self.outgoing.is_empty() {
            agreed
        } else {
            agreed.min(of(&self.outgoing))
        }
    }
}

/// What an entry carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Payload {
    /// Written by each new leader, to commit what earlier terms left.
    Noop,
    /// The application's.
    Data(Vec<u8>),
    /// A new configuration, in effect from when it is appended.
    Config(Config),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub term: Term,
    pub index: Index,
    pub payload: Payload,
}

/// What must survive a restart besides the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardState {
    pub term: Term,
    pub vote: Option<NodeId>,
}

/// The last entry a snapshot covers and the configuration then.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub index: Index,
    pub term: Term,
    pub config: Config,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    Vote {
        term: Term,
        last_index: Index,
        last_term: Term,
        pre_vote: bool,
    },
    VoteReply {
        term: Term,
        granted: bool,
        pre_vote: bool,
    },
    Append {
        term: Term,
        prev_index: Index,
        prev_term: Term,
        entries: Vec<Entry>,
        commit: Index,
        /// The leader's latest read round (ReadIndex), echoed back.
        #[serde(default)]
        read: u64,
    },
    AppendReply {
        term: Term,
        success: bool,
        /// On success, the last index the follower now matches.
        matched: Index,
        /// On failure, where the leader should try next.
        hint: Index,
        /// The read round of the append this answers.
        #[serde(default)]
        read: u64,
    },
    /// The leader's snapshot: its data travels with the message, handled by
    /// the caller ([`Ready::snapshot`] says when to install it).
    Snapshot {
        term: Term,
        meta: SnapshotMeta,
    },
    SnapshotReply {
        term: Term,
        index: Index,
    },
}

impl Message {
    pub fn term(&self) -> Term {
        match self {
            Message::Vote { term, .. }
            | Message::VoteReply { term, .. }
            | Message::Append { term, .. }
            | Message::AppendReply { term, .. }
            | Message::Snapshot { term, .. }
            | Message::SnapshotReply { term, .. } => *term,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub from: NodeId,
    pub to: NodeId,
    pub message: Message,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Ticks without a leader before campaigning: each timeout is drawn
    /// between this and twice this.
    pub election_ticks: u64,
    /// Ticks between a leader's heartbeats (well under `election_ticks`).
    pub heartbeat_ticks: u64,
    /// Entries per append message at most.
    pub max_entries: usize,
    pub pre_vote: bool,
    pub check_quorum: bool,
    /// Seeds the election timeouts (with the member's id).
    pub seed: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            election_ticks: 10,
            heartbeat_ticks: 2,
            max_entries: 64,
            pre_vote: true,
            check_quorum: true,
            seed: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    PreCandidate,
    Candidate,
    Leader,
}

/// What to do after a call: persist `hard_state`, then the log changes
/// (remove persisted entries from `truncate_from` on, append `entries`),
/// install `snapshot` if set; only then send `messages`; then apply
/// `committed`, in order.
#[derive(Debug, Default)]
pub struct Ready {
    pub hard_state: Option<HardState>,
    pub truncate_from: Option<Index>,
    pub entries: Vec<Entry>,
    /// A snapshot received from the leader and accepted: the caller installs
    /// its data and replaces its persisted log with the entries after it.
    pub snapshot: Option<SnapshotMeta>,
    pub messages: Vec<Envelope>,
    pub committed: Vec<Entry>,
    /// Reads asked with [`Raft::read_index`] that are settled: the index to
    /// wait for, or `None` when leadership was lost before confirming.
    pub reads: Vec<(u64, Option<Index>)>,
}

impl Ready {
    pub fn is_empty(&self) -> bool {
        self.hard_state.is_none()
            && self.truncate_from.is_none()
            && self.entries.is_empty()
            && self.snapshot.is_none()
            && self.messages.is_empty()
            && self.committed.is_empty()
            && self.reads.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Only the leader takes proposals; the leader known, if any.
    NotLeader(Option<NodeId>),
    /// A membership change is already under way.
    ChangeInProgress,
    /// The index asked for is not applied yet, or was compacted.
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
struct Progress {
    next: Index,
    matched: Index,
    /// Heard from since the last check-quorum.
    active: bool,
    /// A snapshot sent and not yet acknowledged.
    snapshot: Option<Index>,
}

/// One member's Raft state machine.
pub struct Raft {
    id: NodeId,
    options: Options,
    rng: u64,
    term: Term,
    vote: Option<NodeId>,
    role: Role,
    leader: Option<NodeId>,
    /// Entries after the snapshot.
    log: Vec<Entry>,
    snapshot: SnapshotMeta,
    commit: Index,
    applied: Index,
    config: Config,
    config_index: Index,
    elapsed: u64,
    timeout: u64,
    heartbeat: u64,
    votes: BTreeMap<NodeId, bool>,
    progress: BTreeMap<NodeId, Progress>,
    messages: Vec<Envelope>,
    persist_from: Option<Index>,
    hard_dirty: bool,
    installed: Option<SnapshotMeta>,
    /// Elections this member started (pre-votes aside).
    elections: u64,
    /// ReadIndex: the last read round sent, the reads waiting for a round a
    /// majority answered `(context, round, index)`, the highest round each
    /// member answered in this term, and the settled reads.
    read_round: u64,
    pending_reads: Vec<(u64, u64, Index)>,
    read_acks: BTreeMap<NodeId, u64>,
    settled_reads: Vec<(u64, Option<Index>)>,
    /// The read round of the append being answered.
    reply_read: u64,
}

impl Raft {
    /// A member of a new cluster whose first configuration is `voters`
    /// (every founding member starts the same way).
    pub fn new(id: NodeId, voters: impl IntoIterator<Item = NodeId>, options: Options) -> Self {
        let snapshot = SnapshotMeta {
            index: 0,
            term: 0,
            config: Config::new(voters),
        };
        Self::restore(id, options, HardState::default(), snapshot, Vec::new(), 0)
    }

    /// A member restarted from what it persisted: its hard state, its last
    /// snapshot, the log entries after it, and the last index it applied.
    pub fn restore(
        id: NodeId,
        options: Options,
        hard: HardState,
        snapshot: SnapshotMeta,
        log: Vec<Entry>,
        applied: Index,
    ) -> Self {
        let applied = applied.max(snapshot.index);
        let mut raft = Self {
            id,
            options,
            rng: (options.seed ^ id.wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1,
            term: hard.term,
            vote: hard.vote,
            role: Role::Follower,
            leader: None,
            log,
            config: snapshot.config.clone(),
            config_index: snapshot.index,
            snapshot,
            commit: applied,
            applied,
            elapsed: 0,
            timeout: 0,
            heartbeat: 0,
            votes: BTreeMap::new(),
            progress: BTreeMap::new(),
            messages: Vec::new(),
            persist_from: None,
            hard_dirty: false,
            installed: None,
            elections: 0,
            read_round: 0,
            pending_reads: Vec::new(),
            read_acks: BTreeMap::new(),
            settled_reads: Vec::new(),
            reply_read: 0,
        };
        raft.reload_config();
        raft.reset_timeout();
        raft
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    pub fn term(&self) -> Term {
        self.term
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn leader(&self) -> Option<NodeId> {
        self.leader
    }

    pub fn commit(&self) -> Index {
        self.commit
    }

    pub fn applied(&self) -> Index {
        self.applied
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn last_index(&self) -> Index {
        self.log
            .last()
            .map_or(self.snapshot.index, |entry| entry.index)
    }

    pub fn snapshot_meta(&self) -> &SnapshotMeta {
        &self.snapshot
    }

    /// Elections this member started since it began (pre-votes aside).
    pub fn elections(&self) -> u64 {
        self.elections
    }

    /// On the leader, the last index each other member is known to hold;
    /// empty elsewhere.
    pub fn progress(&self) -> Vec<(NodeId, Index)> {
        if self.role != Role::Leader {
            return Vec::new();
        }
        self.progress
            .iter()
            .filter(|(id, _)| **id != self.id)
            .map(|(id, progress)| (*id, progress.matched))
            .collect()
    }

    fn last_term(&self) -> Term {
        self.log
            .last()
            .map_or(self.snapshot.term, |entry| entry.term)
    }

    fn term_at(&self, index: Index) -> Option<Term> {
        if index == self.snapshot.index {
            return Some(self.snapshot.term);
        }
        if index < self.snapshot.index {
            return None;
        }
        self.entry(index).map(|entry| entry.term)
    }

    fn entry(&self, index: Index) -> Option<&Entry> {
        let offset = index.checked_sub(self.snapshot.index + 1)?;
        self.log.get(usize::try_from(offset).ok()?)
    }

    fn random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn reset_timeout(&mut self) {
        let base = self.options.election_ticks.max(1);
        self.elapsed = 0;
        self.timeout = base + self.random() % base;
    }

    /// The configuration in effect: the last one in the log, or the
    /// snapshot's.
    fn reload_config(&mut self) {
        let found = self
            .log
            .iter()
            .rev()
            .find_map(|entry| match &entry.payload {
                Payload::Config(config) => Some((config.clone(), entry.index)),
                _ => None,
            });
        let (config, index) =
            found.unwrap_or_else(|| (self.snapshot.config.clone(), self.snapshot.index));
        self.config = config;
        self.config_index = index;
        if self.role == Role::Leader {
            let last = self.last_index();
            let members = self.config.members();
            self.progress.retain(|id, _| members.contains(id));
            for id in members {
                if id != self.id {
                    self.progress.entry(id).or_insert(Progress {
                        next: last + 1,
                        matched: 0,
                        active: true,
                        snapshot: None,
                    });
                }
            }
        }
    }

    fn send(&mut self, to: NodeId, message: Message) {
        self.messages.push(Envelope {
            from: self.id,
            to,
            message,
        });
    }

    fn mark_persist(&mut self, from: Index) {
        self.persist_from = Some(self.persist_from.map_or(from, |current| current.min(from)));
    }

    fn set_hard(&mut self, term: Term, vote: Option<NodeId>) {
        if self.term != term || self.vote != vote {
            self.term = term;
            self.vote = vote;
            self.hard_dirty = true;
        }
    }

    fn become_follower(&mut self, term: Term, leader: Option<NodeId>) {
        for (context, _, _) in self.pending_reads.drain(..) {
            self.settled_reads.push((context, None));
        }
        self.read_acks.clear();
        let vote = if term > self.term { None } else { self.vote };
        self.set_hard(term, vote);
        self.role = Role::Follower;
        self.leader = leader;
        self.votes.clear();
        self.progress.clear();
        self.reset_timeout();
    }

    /// Whether this member heard from a leader recently (and so refuses to
    /// help depose it).
    fn in_lease(&self) -> bool {
        self.options.check_quorum
            && self.leader.is_some()
            && self.elapsed < self.options.election_ticks
    }

    /// Advances logical time by one tick.
    pub fn tick(&mut self) {
        self.elapsed += 1;
        match self.role {
            Role::Leader => {
                self.heartbeat += 1;
                if self.heartbeat >= self.options.heartbeat_ticks {
                    self.heartbeat = 0;
                    self.broadcast();
                }
                if self.options.check_quorum && self.elapsed >= self.options.election_ticks {
                    self.elapsed = 0;
                    let id = self.id;
                    let active = self.config.quorum(|member| {
                        member == id || self.progress.get(&member).is_some_and(|p| p.active)
                    });
                    for progress in self.progress.values_mut() {
                        progress.active = false;
                        // A snapshot that got no answer is sent again.
                        progress.snapshot = None;
                    }
                    if !active {
                        let term = self.term;
                        self.become_follower(term, None);
                    }
                }
            }
            _ => {
                if self.elapsed >= self.timeout && self.config.members().contains(&self.id) {
                    self.campaign();
                }
            }
        }
    }

    fn campaign(&mut self) {
        if self.options.pre_vote && self.role != Role::PreCandidate {
            self.role = Role::PreCandidate;
            self.leader = None;
            self.votes = BTreeMap::from([(self.id, true)]);
            self.reset_timeout();
            if self.config.quorum(|member| member == self.id) {
                return self.start_election();
            }
            let (last_index, last_term, term) =
                (self.last_index(), self.last_term(), self.term + 1);
            for member in self.config.members() {
                if member != self.id {
                    self.send(
                        member,
                        Message::Vote {
                            term,
                            last_index,
                            last_term,
                            pre_vote: true,
                        },
                    );
                }
            }
        } else {
            self.start_election();
        }
    }

    fn start_election(&mut self) {
        self.elections += 1;
        let term = self.term + 1;
        self.set_hard(term, Some(self.id));
        self.role = Role::Candidate;
        self.leader = None;
        self.votes = BTreeMap::from([(self.id, true)]);
        self.reset_timeout();
        if self.config.quorum(|member| member == self.id) {
            return self.become_leader();
        }
        let (last_index, last_term) = (self.last_index(), self.last_term());
        for member in self.config.members() {
            if member != self.id {
                self.send(
                    member,
                    Message::Vote {
                        term,
                        last_index,
                        last_term,
                        pre_vote: false,
                    },
                );
            }
        }
    }

    fn become_leader(&mut self) {
        self.read_acks.clear();
        self.role = Role::Leader;
        self.leader = Some(self.id);
        self.votes.clear();
        self.heartbeat = 0;
        self.elapsed = 0;
        self.progress.clear();
        let last = self.last_index();
        for member in self.config.members() {
            if member != self.id {
                self.progress.insert(
                    member,
                    Progress {
                        next: last + 1,
                        matched: 0,
                        active: true,
                        snapshot: None,
                    },
                );
            }
        }
        self.append(Payload::Noop);
        self.broadcast();
    }

    /// Appends to the leader's log.
    fn append(&mut self, payload: Payload) -> Index {
        let index = self.last_index() + 1;
        let is_config = matches!(payload, Payload::Config(_));
        self.log.push(Entry {
            term: self.term,
            index,
            payload,
        });
        self.mark_persist(index);
        if is_config {
            self.reload_config();
        }
        self.maybe_commit();
        index
    }

    /// Proposes data; returns the index it will have if committed.
    pub fn propose(&mut self, data: Vec<u8>) -> Result<Index, Error> {
        if self.role != Role::Leader {
            return Err(Error::NotLeader(self.leader));
        }
        let index = self.append(Payload::Data(data));
        self.broadcast();
        Ok(index)
    }

    /// Changes the voting members to `voters`: the old and new sets decide
    /// together first, then the new set alone (proposed automatically once
    /// the joint configuration commits).
    pub fn change_membership(
        &mut self,
        voters: impl IntoIterator<Item = NodeId>,
    ) -> Result<Index, Error> {
        if self.role != Role::Leader {
            return Err(Error::NotLeader(self.leader));
        }
        if self.config.is_joint() || self.config_index > self.commit {
            return Err(Error::ChangeInProgress);
        }
        let voters: BTreeSet<NodeId> = voters.into_iter().collect();
        if voters == self.config.voters || voters.is_empty() {
            return Ok(self.config_index);
        }
        let joint = Config {
            voters,
            outgoing: self.config.voters.clone(),
        };
        let index = self.append(Payload::Config(joint));
        self.broadcast();
        Ok(index)
    }

    fn broadcast(&mut self) {
        let peers: Vec<NodeId> = self.progress.keys().copied().collect();
        for peer in peers {
            self.send_append(peer);
        }
    }

    fn send_append(&mut self, peer: NodeId) {
        let Some(progress) = self.progress.get(&peer).copied() else {
            return;
        };
        if progress.next <= self.snapshot.index {
            if progress.snapshot != Some(self.snapshot.index) {
                let meta = self.snapshot.clone();
                if let Some(entry) = self.progress.get_mut(&peer) {
                    entry.snapshot = Some(meta.index);
                }
                let term = self.term;
                self.send(peer, Message::Snapshot { term, meta });
            }
            return;
        }
        let prev_index = progress.next - 1;
        let prev_term = self.term_at(prev_index).unwrap_or(0);
        let last = self.last_index();
        let end = last.min(prev_index + self.options.max_entries as u64);
        let entries: Vec<Entry> = (progress.next..=end)
            .filter_map(|index| self.entry(index).cloned())
            .collect();
        let (term, commit, read) = (self.term, self.commit, self.read_round);
        self.send(
            peer,
            Message::Append {
                term,
                prev_index,
                prev_term,
                entries,
                commit,
                read,
            },
        );
    }

    /// Asks for a linearizable read: once a majority confirms this member
    /// is still the leader, [`Ready::reads`] gives `context` the index to
    /// wait for before reading. Fails if not the leader, or if the leader
    /// has not committed an entry of its term yet (try again shortly).
    pub fn read_index(&mut self, context: u64) -> Result<(), Error> {
        if self.role != Role::Leader {
            return Err(Error::NotLeader(self.leader));
        }
        if self.term_at(self.commit) != Some(self.term) {
            return Err(Error::Unavailable);
        }
        self.read_round += 1;
        let (round, index) = (self.read_round, self.commit);
        self.pending_reads.push((context, round, index));
        self.settle_reads();
        self.broadcast();
        Ok(())
    }

    /// Reads whose round a majority (with the leader) answered.
    fn settle_reads(&mut self) {
        let id = self.id;
        let mut settled = Vec::new();
        self.pending_reads.retain(|(context, round, index)| {
            let confirmed = self.config.quorum(|member| {
                member == id || self.read_acks.get(&member).is_some_and(|r| r >= round)
            });
            if confirmed {
                settled.push((*context, Some(*index)));
            }
            !confirmed
        });
        self.settled_reads.extend(settled);
    }

    fn maybe_commit(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        let (id, last) = (self.id, self.last_index());
        let agreed = self.config.agreed(|member| {
            if member == id {
                last
            } else {
                self.progress.get(&member).map_or(0, |p| p.matched)
            }
        });
        if agreed > self.commit && self.term_at(agreed) == Some(self.term) {
            self.commit = agreed;
            self.on_commit();
        }
    }

    /// After the commit index moves on the leader: finish a membership
    /// change, step down if removed, and tell followers.
    fn on_commit(&mut self) {
        if self.config_index <= self.commit {
            if self.config.is_joint() {
                let config = Config::new(self.config.voters.iter().copied());
                self.append(Payload::Config(config));
            } else if !self.config.voters.contains(&self.id) {
                self.broadcast();
                let term = self.term;
                self.become_follower(term, None);
                return;
            }
        }
        self.broadcast();
    }

    /// Handles a message from another member.
    pub fn step(&mut self, envelope: Envelope) {
        let Envelope { from, message, .. } = envelope;
        let term = message.term();
        if term > self.term {
            match &message {
                Message::Vote { pre_vote: true, .. } => {}
                Message::VoteReply {
                    pre_vote: true,
                    granted: true,
                    ..
                } => {}
                Message::Vote {
                    pre_vote: false, ..
                } if self.in_lease() => return,
                Message::Append { .. } | Message::Snapshot { .. } => {
                    self.become_follower(term, Some(from))
                }
                _ => self.become_follower(term, None),
            }
        } else if term < self.term {
            match message {
                Message::Append { .. } | Message::Snapshot { .. } => {
                    let reply_term = self.term;
                    self.send(
                        from,
                        Message::AppendReply {
                            term: reply_term,
                            success: false,
                            matched: 0,
                            hint: 0,
                            read: 0,
                        },
                    );
                }
                Message::Vote { pre_vote, .. } => {
                    let reply_term = self.term;
                    self.send(
                        from,
                        Message::VoteReply {
                            term: reply_term,
                            granted: false,
                            pre_vote,
                        },
                    );
                }
                _ => {}
            }
            return;
        }
        match message {
            Message::Vote {
                term,
                last_index,
                last_term,
                pre_vote,
            } => self.on_vote(from, term, last_index, last_term, pre_vote),
            Message::VoteReply {
                term,
                granted,
                pre_vote,
            } => self.on_vote_reply(from, term, granted, pre_vote),
            Message::Append {
                prev_index,
                prev_term,
                entries,
                commit,
                read,
                ..
            } => {
                self.reply_read = read;
                self.on_append(from, prev_index, prev_term, entries, commit)
            }
            Message::AppendReply {
                success,
                matched,
                hint,
                read,
                ..
            } => {
                if self.role == Role::Leader {
                    let ack = self.read_acks.entry(from).or_insert(0);
                    *ack = (*ack).max(read);
                    self.settle_reads();
                }
                self.on_append_reply(from, success, matched, hint)
            }
            Message::Snapshot { meta, .. } => self.on_snapshot(from, meta),
            Message::SnapshotReply { index, .. } => self.on_snapshot_reply(from, index),
        }
    }

    fn on_vote(
        &mut self,
        from: NodeId,
        term: Term,
        last_index: Index,
        last_term: Term,
        pre_vote: bool,
    ) {
        let log_ok = last_term > self.last_term()
            || (last_term == self.last_term() && last_index >= self.last_index());
        if pre_vote {
            let granted = log_ok && term > self.term && !self.in_lease();
            let reply_term = if granted { term } else { self.term };
            self.send(
                from,
                Message::VoteReply {
                    term: reply_term,
                    granted,
                    pre_vote: true,
                },
            );
            return;
        }
        let granted = log_ok && self.vote.is_none_or(|vote| vote == from);
        if granted {
            self.set_hard(self.term, Some(from));
            self.reset_timeout();
        }
        let reply_term = self.term;
        self.send(
            from,
            Message::VoteReply {
                term: reply_term,
                granted,
                pre_vote: false,
            },
        );
    }

    fn on_vote_reply(&mut self, from: NodeId, term: Term, granted: bool, pre_vote: bool) {
        let expected = match (self.role, pre_vote) {
            (Role::PreCandidate, true) => term == self.term + 1 || !granted,
            (Role::Candidate, false) => term == self.term,
            _ => false,
        };
        if !expected {
            return;
        }
        self.votes.insert(from, granted);
        let won = self
            .config
            .quorum(|member| self.votes.get(&member) == Some(&true));
        let lost = self
            .config
            .quorum(|member| self.votes.get(&member) == Some(&false));
        if won {
            if pre_vote {
                self.start_election();
            } else {
                self.become_leader();
            }
        } else if lost {
            let term = self.term;
            self.become_follower(term, None);
        }
    }

    fn on_append(
        &mut self,
        from: NodeId,
        prev_index: Index,
        prev_term: Term,
        entries: Vec<Entry>,
        commit: Index,
    ) {
        if self.role != Role::Follower {
            let term = self.term;
            self.become_follower(term, Some(from));
        }
        // A leader's entries follow `prev_index` one by one. Any other
        // message is not one a leader sends, and is dropped before it
        // changes anything (it once took the log apart from the wrong
        // place and stopped this member's Raft thread).
        if entries
            .iter()
            .enumerate()
            .any(|(i, entry)| Some(entry.index) != prev_index.checked_add(1 + i as u64))
        {
            return;
        }
        self.leader = Some(from);
        self.elapsed = 0;
        let (term, read) = (self.term, self.reply_read);
        // What the snapshot already holds is matched.
        if prev_index < self.snapshot.index {
            let entries: Vec<Entry> = entries
                .into_iter()
                .filter(|e| e.index > self.snapshot.index)
                .collect();
            let matched = self.snapshot.index;
            if entries.first().is_some_and(|e| e.index == matched + 1) || entries.is_empty() {
                let prev_term = self.snapshot.term;
                return self.on_append(from, matched, prev_term, entries, commit);
            }
            self.send(
                from,
                Message::AppendReply {
                    term,
                    success: true,
                    matched,
                    hint: 0,
                    read,
                },
            );
            return;
        }
        if self.term_at(prev_index) != Some(prev_term) {
            let hint = if prev_index > self.last_index() {
                self.last_index() + 1
            } else {
                // Skip the whole conflicting term at once.
                let conflict = self.term_at(prev_index);
                let mut index = prev_index;
                while index > self.commit.max(self.snapshot.index) + 1
                    && self.term_at(index - 1) == conflict
                {
                    index -= 1;
                }
                index.max(self.commit + 1)
            };
            self.send(
                from,
                Message::AppendReply {
                    term,
                    success: false,
                    matched: 0,
                    hint,
                    read,
                },
            );
            return;
        }
        let last_new = prev_index + entries.len() as u64;
        let mut changed_config = false;
        for entry in entries {
            match self.term_at(entry.index) {
                Some(existing) if existing == entry.term => continue,
                Some(_) => {
                    // A conflict: what follows is replaced. A committed
                    // entry never is: a message that says otherwise is not
                    // a leader's, and is dropped.
                    if entry.index <= self.commit.max(self.snapshot.index) {
                        return;
                    }
                    let keep = (entry.index - self.snapshot.index - 1) as usize;
                    changed_config |= self.log[keep..]
                        .iter()
                        .any(|e| matches!(e.payload, Payload::Config(_)));
                    self.log.truncate(keep);
                }
                None => {}
            }
            changed_config |= matches!(entry.payload, Payload::Config(_));
            self.mark_persist(entry.index);
            self.log.push(entry);
        }
        if changed_config {
            self.reload_config();
        }
        if commit > self.commit {
            self.commit = commit.min(last_new).max(self.commit);
        }
        self.send(
            from,
            Message::AppendReply {
                term,
                success: true,
                matched: last_new,
                hint: 0,
                read,
            },
        );
    }

    fn on_append_reply(&mut self, from: NodeId, success: bool, matched: Index, hint: Index) {
        if self.role != Role::Leader {
            return;
        }
        let last = self.last_index();
        let Some(progress) = self.progress.get_mut(&from) else {
            return;
        };
        progress.active = true;
        if success {
            if matched > progress.matched {
                progress.matched = matched;
            }
            progress.next = progress.next.max(matched + 1);
            let behind = progress.next <= last;
            self.maybe_commit();
            if behind {
                self.send_append(from);
            }
        } else if hint > 0 {
            progress.next = hint
                .min(progress.next.saturating_sub(1))
                .max(progress.matched + 1)
                .max(1);
            self.send_append(from);
        }
    }

    fn on_snapshot(&mut self, from: NodeId, meta: SnapshotMeta) {
        if self.role != Role::Follower {
            let term = self.term;
            self.become_follower(term, Some(from));
        }
        self.leader = Some(from);
        self.elapsed = 0;
        let term = self.term;
        if meta.index <= self.commit {
            let index = self.commit;
            self.send(from, Message::SnapshotReply { term, index });
            return;
        }
        if self.term_at(meta.index) == Some(meta.term) {
            // The log already has it: keep what follows.
            let keep_from = (meta.index - self.snapshot.index) as usize;
            self.log.drain(..keep_from);
        } else {
            self.log.clear();
            // Every persisted entry after it goes too.
            self.mark_persist(meta.index + 1);
        }
        self.snapshot = meta.clone();
        self.commit = meta.index;
        self.applied = meta.index;
        self.reload_config();
        self.installed = Some(meta.clone());
        let index = meta.index;
        self.send(from, Message::SnapshotReply { term, index });
    }

    fn on_snapshot_reply(&mut self, from: NodeId, index: Index) {
        if self.role != Role::Leader {
            return;
        }
        let Some(progress) = self.progress.get_mut(&from) else {
            return;
        };
        progress.active = true;
        progress.snapshot = None;
        progress.matched = progress.matched.max(index);
        progress.next = progress.next.max(index + 1);
        self.maybe_commit();
        self.send_append(from);
    }

    /// Drops the log up to `index` (applied, and saved by the caller as a
    /// snapshot): members that need those entries get the snapshot.
    pub fn compact(&mut self, index: Index) -> Result<(), Error> {
        if index > self.applied {
            return Err(Error::Unavailable);
        }
        if index <= self.snapshot.index {
            return Ok(());
        }
        let term = self.term_at(index).ok_or(Error::Unavailable)?;
        let config = self
            .log
            .iter()
            .take_while(|entry| entry.index <= index)
            .filter_map(|entry| match &entry.payload {
                Payload::Config(config) => Some(config.clone()),
                _ => None,
            })
            .last()
            .unwrap_or_else(|| self.snapshot.config.clone());
        let drop = (index - self.snapshot.index) as usize;
        self.log.drain(..drop);
        self.snapshot = SnapshotMeta {
            index,
            term,
            config,
        };
        Ok(())
    }

    /// What to persist, send and apply since the last call.
    pub fn ready(&mut self) -> Ready {
        let mut ready = Ready {
            messages: std::mem::take(&mut self.messages),
            snapshot: self.installed.take(),
            ..Ready::default()
        };
        if self.hard_dirty {
            self.hard_dirty = false;
            ready.hard_state = Some(HardState {
                term: self.term,
                vote: self.vote,
            });
        }
        if let Some(from) = self.persist_from.take() {
            let from = from.max(self.snapshot.index + 1);
            ready.truncate_from = Some(from);
            ready.entries = (from..=self.last_index())
                .filter_map(|index| self.entry(index).cloned())
                .collect();
        }
        ready.reads = std::mem::take(&mut self.settled_reads);
        if self.commit > self.applied {
            let from = self.applied.max(self.snapshot.index) + 1;
            ready.committed = (from..=self.commit)
                .filter_map(|index| self.entry(index).cloned())
                .collect();
            self.applied = self.commit;
        }
        ready
    }
}
