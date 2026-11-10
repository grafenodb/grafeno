//! Grafeno servers kept in sync by Raft (phase 13.2).
//!
//! A [`Node`] is one member: a Grafeno database, its Raft state machine
//! ([`grafeno_raft::Raft`]) persisted in a file of its own, and a thread
//! that drives it: it ticks the clock, persists what Raft asks for, sends
//! messages through a [`Transport`], and applies committed entries to the
//! database in order.
//!
//! Writes go through the leader ([`Node::write`]). It runs the write once
//! in a transaction it discards ([`Grafeno::dry_run`]), which yields the
//! write's effects as a replication log entry (13.1); it proposes the entry,
//! and when a majority has it, every member, the leader included, applies
//! it the same way ([`grafeno::WriteTx::apply_entry`]). Only then does the
//! write return, with the value it computed. One way of applying for all,
//! so a member that crashed at any point recovers by applying the log from
//! the position its database recorded. Writes are serialized on the leader:
//! the next one runs on the state the previous one left.
//!
//! Raft's own entries (each new leader's no-op, configurations) are applied
//! as empty entries, so the database's position and Raft's index stay the
//! same number. A snapshot is a backup of the database at an applied index;
//! a member too far behind receives it and replaces its data with it
//! ([`Grafeno::restore_backup`]).

mod storage;

pub mod check;
pub mod memory;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use grafeno::{Grafeno, ReadTx, Timestamp, WriteTx};
use grafeno_raft::{Envelope, Index, Message, NodeId, Payload, Raft, Role, SnapshotMeta, Term};
use serde::{Deserialize, Serialize};

pub use grafeno_raft::{Config, Options as RaftOptions};
pub use storage::Storage;

/// What can go wrong for a cluster write or request.
#[derive(Clone, Debug)]
pub enum Error {
    /// This member is not the leader; the leader it knows of, if any.
    NotLeader(Option<NodeId>),
    /// The write was proposed but a new leader replaced it: it did not
    /// happen, and may be tried again.
    Lost,
    /// No answer in time (no quorum, or a slow cluster): the write may or
    /// may not have happened.
    Timeout,
    /// A membership change is already under way.
    ChangeInProgress,
    /// The member is stopped.
    Stopped,
    /// The member no longer holds a snapshot at this position (it has
    /// moved on for longer than snapshots are kept).
    NotHeld(Index),
    Storage(String),
    Engine(grafeno::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotLeader(Some(leader)) => write!(f, "not the leader; member {leader} is"),
            Error::NotLeader(None) => f.write_str("not the leader, and no leader is known"),
            Error::Lost => f.write_str("the write was lost to a change of leader; try again"),
            Error::Timeout => f.write_str("the cluster did not answer in time"),
            Error::ChangeInProgress => f.write_str("a membership change is under way"),
            Error::Stopped => f.write_str("the member is stopped"),
            Error::NotHeld(index) => {
                write!(
                    f,
                    "this member no longer holds a snapshot at position {index}"
                )
            }
            Error::Storage(message) => write!(f, "raft storage: {message}"),
            Error::Engine(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<grafeno::Error> for Error {
    fn from(error: grafeno::Error) -> Self {
        Error::Engine(error)
    }
}

/// How messages reach other members. `snapshot` carries the bytes of a
/// snapshot's file along with a [`Message::Snapshot`].
pub trait Transport: Send + Sync + 'static {
    fn send(&self, envelope: Envelope, snapshot: Option<Arc<Vec<u8>>>);
}

#[derive(Clone, Debug)]
pub struct NodeOptions {
    pub id: NodeId,
    /// One tick of Raft's clock.
    pub tick: Duration,
    pub raft: RaftOptions,
    /// Where snapshot files are written.
    pub snapshot_dir: PathBuf,
    /// How long a write waits for its commit.
    pub write_timeout: Duration,
}

impl NodeOptions {
    pub fn new(id: NodeId, snapshot_dir: impl Into<PathBuf>) -> Self {
        Self {
            id,
            tick: Duration::from_millis(50),
            raft: RaftOptions {
                seed: id,
                ..RaftOptions::default()
            },
            snapshot_dir: snapshot_dir.into(),
            write_timeout: Duration::from_secs(10),
        }
    }
}

/// A member's view of the cluster.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub id: NodeId,
    pub role: String,
    pub term: Term,
    pub leader: Option<NodeId>,
    pub commit: Index,
    /// The last entry applied to the database.
    pub applied: Index,
    pub last_index: Index,
    pub snapshot_index: Index,
    pub voters: Vec<NodeId>,
    /// The previous voters, during a membership change.
    pub outgoing: Vec<NodeId>,
    /// Elections this member started, and leader changes it saw, since it
    /// started.
    pub elections: u64,
    pub leader_changes: u64,
    /// Snapshots this member sent to others, and installed from a leader.
    pub snapshots_sent: u64,
    pub snapshots_installed: u64,
    /// On the leader: how far each other member has come, and how many
    /// entries it lacks.
    pub peers: Vec<PeerProgress>,
    /// The entry format the cluster writes: the newest every voter reads.
    pub format: u32,
    /// What each member said it runs (from its messages).
    pub members: BTreeMap<NodeId, MemberInfo>,
}

/// How far another member has come, as the leader knows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PeerProgress {
    pub id: NodeId,
    pub matched: Index,
    pub lag: Index,
}

/// What a member runs, as it says with its messages: members of two
/// versions coexist in a rolling upgrade, and the leader writes no entry
/// format a voter cannot read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub version: String,
    /// The newest replication entry format it reads.
    pub format: u32,
    /// Its database layout.
    pub layout: u64,
}

impl MemberInfo {
    /// This process.
    pub fn here() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            format: grafeno::ENTRY_FORMAT,
            layout: grafeno::SCHEMA_VERSION,
        }
    }
}

type Reply<T> = SyncSender<Result<T, Error>>;

enum Event {
    Message(Envelope, Option<Arc<Vec<u8>>>),
    /// Data to propose; answered once applied (or lost).
    Propose(Vec<u8>, Reply<()>),
    Membership(Vec<NodeId>, Reply<Index>),
    Compact(Reply<Index>),
    /// A linearizable read: the index to wait for.
    ReadIndex(Reply<Index>),
    Stop,
}

struct Inner {
    id: NodeId,
    db: Arc<Grafeno>,
    events: Mutex<Option<Sender<Event>>>,
    status: Mutex<Status>,
    changed: Condvar,
    /// Writes run one at a time; an explicit transaction keeps the gate
    /// from its `BEGIN` to its end (16.4).
    gate: Mutex<bool>,
    gate_freed: Condvar,
    options: NodeOptions,
    /// A snapshot after each of the last [`MAX_SNAPSHOTS`] applied entries,
    /// kept for [`SNAPSHOTS_KEPT`] at most, so a query split across members
    /// reads every part at one position.
    snapshots: Mutex<BTreeMap<Index, (Instant, Arc<ReadTx>)>>,
    /// What the other members said they run.
    members: Mutex<BTreeMap<NodeId, MemberInfo>>,
    counters: Mutex<Counters>,
}

/// What the status counts, with what it last saw.
#[derive(Default)]
struct Counters {
    leader: Option<NodeId>,
    leader_changes: u64,
    snapshots_sent: u64,
    snapshots_installed: u64,
}

/// How long snapshots of past positions are kept at most.
pub const SNAPSHOTS_KEPT: Duration = Duration::from_secs(2);
/// Snapshots kept at most, however recent. Every open snapshot makes redb
/// keep the pages it sees: with hundreds of them (one per write for 5 s,
/// as first built), concurrent writes slowed from 120 a second to 3 within
/// seconds; with 64 they keep their pace. A member that has moved further
/// past a split read's position has its part run by the member that
/// splits it.
pub const MAX_SNAPSHOTS: usize = 64;

/// The right to write, held by one write or one explicit transaction at a
/// time; given back when dropped.
pub struct WriteGate {
    inner: Arc<Inner>,
}

impl Drop for WriteGate {
    fn drop(&mut self) {
        *lock(&self.inner.gate) = false;
        self.inner.gate_freed.notify_one();
    }
}

impl Inner {
    /// Waits for the write gate (for `wait` at most; forever with `None`).
    fn acquire(self: &Arc<Self>, wait: Option<Duration>) -> Option<WriteGate> {
        let deadline = wait.map(|wait| Instant::now() + wait);
        let mut held = lock(&self.gate);
        while *held {
            match deadline {
                None => {
                    held = self
                        .gate_freed
                        .wait(held)
                        .unwrap_or_else(|e| e.into_inner());
                }
                Some(deadline) => {
                    let left = deadline.checked_duration_since(Instant::now())?;
                    held = self
                        .gate_freed
                        .wait_timeout(held, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            }
        }
        *held = true;
        Some(WriteGate {
            inner: self.clone(),
        })
    }

    /// Keeps `read`, a snapshot at `index`, and lets old ones go (the
    /// latest always stays).
    fn pin(&self, index: Index, read: ReadTx) {
        let mut snapshots = lock(&self.snapshots);
        let now = Instant::now();
        snapshots
            .entry(index)
            .or_insert_with(|| (now, Arc::new(read)));
        let latest = snapshots.keys().next_back().copied().unwrap_or(index);
        snapshots
            .retain(|at, (when, _)| *at == latest || now.duration_since(*when) < SNAPSHOTS_KEPT);
        while snapshots.len() > MAX_SNAPSHOTS {
            snapshots.pop_first();
        }
    }
}

/// An explicit transaction on the leader (16.4): its statements run in
/// [`Transaction::tx`], which captures what they do; it holds the write
/// gate until it is committed ([`Node::commit_transaction`]) or dropped
/// (rolled back).
pub struct Transaction {
    tx: WriteTx,
    _gate: WriteGate,
}

impl Transaction {
    pub fn tx(&mut self) -> &mut WriteTx {
        &mut self.tx
    }

    /// Discards everything the transaction did.
    pub fn rollback(self) -> Result<(), Error> {
        self.tx.abort().map_err(Error::Engine)
    }
}

/// One member of a cluster.
pub struct Node {
    inner: Arc<Inner>,
    driver: Mutex<Option<JoinHandle<()>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Node {
    /// Starts a member on `db` with the Raft state in `storage`. A member
    /// that never ran starts with `voters` as the first configuration
    /// (every founding member passes the same set), or, with an empty set,
    /// waits to be added by the leader (joining).
    pub fn start(
        db: Arc<Grafeno>,
        storage: Storage,
        transport: Arc<dyn Transport>,
        voters: &[NodeId],
        options: NodeOptions,
    ) -> Result<Self, Error> {
        std::fs::create_dir_all(&options.snapshot_dir)
            .map_err(|e| Error::Storage(e.to_string()))?;
        let loaded = storage.load()?;
        let applied = db.begin_read()?.log_position()?.applied;
        let id = options.id;
        let mut snapshot_file = loaded.snapshot_file.clone();
        let raft = if loaded.is_new() && applied > 0 {
            // A database restored from a backup (a new cluster, or a member
            // joining from one): Raft starts after what it holds, from a
            // snapshot of it (of unknown term: entries up to it are
            // committed wherever it came from).
            let meta = SnapshotMeta {
                index: applied,
                term: 0,
                config: Config::new(voters.iter().copied()),
            };
            let path = options
                .snapshot_dir
                .join(format!("snapshot-{applied}.redb"));
            let _ = std::fs::remove_file(&path);
            db.begin_read()?.backup(&path)?;
            storage.persist(None, None, &[], Some((&meta, &path)))?;
            snapshot_file = Some(path);
            Raft::restore(
                id,
                options.raft,
                grafeno_raft::HardState::default(),
                meta,
                Vec::new(),
                applied,
            )
        } else if loaded.is_new() {
            Raft::new(id, voters.iter().copied(), options.raft)
        } else {
            let snapshot = loaded.snapshot.clone().unwrap_or_else(|| SnapshotMeta {
                index: 0,
                term: 0,
                config: Config::new(voters.iter().copied()),
            });
            Raft::restore(id, options.raft, loaded.hard, snapshot, loaded.log, applied)
        };
        let (sender, receiver) = mpsc::channel();
        let inner = Arc::new(Inner {
            id,
            db,
            events: Mutex::new(Some(sender)),
            status: Mutex::new(Status::default()),
            changed: Condvar::new(),
            gate: Mutex::new(false),
            gate_freed: Condvar::new(),
            options,
            snapshots: Mutex::new(BTreeMap::new()),
            members: Mutex::new(BTreeMap::new()),
            counters: Mutex::new(Counters::default()),
        });
        let mut driver = Driver {
            inner: inner.clone(),
            raft,
            storage,
            transport,
            snapshot_file,
            incoming: HashMap::new(),
            waiters: HashMap::new(),
            reads: HashMap::new(),
            next_read: 0,
        };
        driver.publish();
        let handle = std::thread::Builder::new()
            .name(format!("grafeno-raft-{id}"))
            .spawn(move || driver.run(receiver))
            .map_err(|e| Error::Storage(e.to_string()))?;
        Ok(Self {
            inner,
            driver: Mutex::new(Some(handle)),
        })
    }

    pub fn id(&self) -> NodeId {
        self.inner.id
    }

    pub fn db(&self) -> &Arc<Grafeno> {
        &self.inner.db
    }

    pub fn status(&self) -> Status {
        lock(&self.inner.status).clone()
    }

    fn send(&self, event: Event) -> Result<(), Error> {
        lock(&self.inner.events)
            .as_ref()
            .ok_or(Error::Stopped)?
            .send(event)
            .map_err(|_| Error::Stopped)
    }

    /// Hands a message from another member (with a snapshot's bytes).
    pub fn deliver(&self, envelope: Envelope, snapshot: Option<Arc<Vec<u8>>>) {
        let _ = self.send(Event::Message(envelope, snapshot));
    }

    /// Waits until `done` holds for the status, or `timeout` passes.
    pub fn wait_for(&self, timeout: Duration, done: impl Fn(&Status) -> bool) -> Option<Status> {
        let deadline = Instant::now() + timeout;
        let mut status = lock(&self.inner.status);
        loop {
            if done(&status) {
                return Some(status.clone());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            status = self
                .inner
                .changed
                .wait_timeout(status, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Runs a write on the cluster: on the leader only; returns once a
    /// majority has it and this member applied it.
    pub fn write<T>(&self, f: impl FnOnce(&mut WriteTx) -> grafeno::Result<T>) -> Result<T, Error> {
        let _gate = self.inner.acquire(None).expect("waits without a limit");
        self.ready_to_write()?;
        let (value, entry) = self.inner.db.dry_run(f)?;
        let Some(entry) = entry else {
            return Ok(value);
        };
        self.propose_entry(entry)?;
        Ok(value)
    }

    /// Checks this member leads, and waits until its database holds every
    /// entry before (the new term's no-op too).
    fn ready_to_write(&self) -> Result<(), Error> {
        let timeout = self.inner.options.write_timeout;
        let status = self.status();
        if status.role != "leader" {
            return Err(Error::NotLeader(status.leader));
        }
        let ready = self
            .wait_for(timeout, |s| s.role != "leader" || s.applied >= s.last_index)
            .ok_or(Error::Timeout)?;
        if ready.role != "leader" {
            return Err(Error::NotLeader(ready.leader));
        }
        Ok(())
    }

    /// Opens an explicit transaction (16.4): takes the write gate (waiting
    /// up to `wait` for the write or transaction that holds it), on the
    /// leader only, and gives a transaction whose changes are captured. No
    /// other write runs until [`Node::commit_transaction`] or a drop of the
    /// returned [`Transaction`].
    pub fn begin_transaction(&self, wait: Duration) -> Result<Transaction, Error> {
        let gate = self.inner.acquire(Some(wait)).ok_or(Error::Timeout)?;
        self.ready_to_write()?;
        let tx = self.inner.db.begin_captured()?;
        Ok(Transaction { tx, _gate: gate })
    }

    /// Replicates what `transaction` did as one entry, and returns once a
    /// majority has it and this member applied it. If this member no longer
    /// leads, nothing happened.
    pub fn commit_transaction(&self, transaction: Transaction) -> Result<(), Error> {
        let Transaction { tx, _gate } = transaction;
        let entry = tx.into_entry()?;
        let Some(entry) = entry else {
            return Ok(());
        };
        let status = self.status();
        if status.role != "leader" {
            return Err(Error::NotLeader(status.leader));
        }
        self.propose_entry(entry)
    }

    /// Proposes an entry and waits for it to be applied here.
    fn propose_entry(&self, entry: grafeno::Entry) -> Result<(), Error> {
        let timeout = self.inner.options.write_timeout;
        let format = self.cluster_format();
        if entry.format > format {
            return Err(Error::Engine(grafeno::Error::Invalid(format!(
                "this write needs replication entry format {}, and a member reads only up to {format}: upgrade every member first",
                entry.format
            ))));
        }
        let bytes = serde_json::to_vec(&entry).map_err(|e| Error::Storage(e.to_string()))?;
        let (reply, answer) = mpsc::sync_channel(1);
        self.send(Event::Propose(bytes, reply))?;
        match answer.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Timeout),
        }
    }

    /// The index a linearizable read must wait for (leader only): every
    /// write acknowledged before this call is at or below it.
    pub fn read_index(&self) -> Result<Index, Error> {
        let deadline = Instant::now() + self.inner.options.write_timeout;
        loop {
            let (reply, answer) = mpsc::sync_channel(1);
            self.send(Event::ReadIndex(reply))?;
            let left = deadline.saturating_duration_since(Instant::now());
            match answer.recv_timeout(left) {
                Ok(Ok(index)) => return Ok(index),
                // A new leader has not committed in its term yet.
                Ok(Err(Error::Lost)) if Instant::now() < deadline => {
                    std::thread::sleep(self.inner.options.tick);
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(Error::Timeout),
            }
        }
    }

    /// Waits until this member can serve a linearizable read: the leader
    /// confirms its index, and this member has applied it.
    pub fn read_barrier(&self) -> Result<Index, Error> {
        let index = self.read_index()?;
        self.wait_for(self.inner.options.write_timeout, |s| s.applied >= index)
            .map(|_| index)
            .ok_or(Error::Timeout)
    }

    /// Notes what another member runs (from its messages).
    pub fn note_member(&self, id: NodeId, info: MemberInfo) {
        if id != self.inner.id {
            lock(&self.inner.members).insert(id, info);
        }
    }

    /// The newest entry format every voter reads: a member not heard from
    /// yet, or that does not say, is taken to read only the first.
    pub fn cluster_format(&self) -> u32 {
        let status = self.status();
        let members = lock(&self.inner.members);
        status
            .voters
            .iter()
            .chain(&status.outgoing)
            .filter(|id| **id != self.inner.id)
            .map(|id| members.get(id).map_or(1, |info| info.format))
            .fold(grafeno::ENTRY_FORMAT, u32::min)
    }

    /// The latest snapshot this member holds, and its position.
    pub fn snapshot(&self) -> Result<(Index, Arc<ReadTx>), Error> {
        lock(&self.inner.snapshots)
            .iter()
            .next_back()
            .map(|(index, (_, read))| (*index, read.clone()))
            .ok_or(Error::Stopped)
    }

    /// A snapshot of the database at exactly `index`: waits up to
    /// `timeout` for the member to apply it, and fails if the member has
    /// moved past it by more than [`MAX_SNAPSHOTS`] entries or for longer
    /// than [`SNAPSHOTS_KEPT`].
    pub fn snapshot_at(&self, index: Index, timeout: Duration) -> Result<Arc<ReadTx>, Error> {
        self.wait_for(timeout, |s| s.applied >= index)
            .ok_or(Error::Timeout)?;
        lock(&self.inner.snapshots)
            .get(&index)
            .map(|(_, read)| read.clone())
            .ok_or(Error::NotHeld(index))
    }

    /// Changes the voting members (leader only): the old and new sets
    /// decide together, then the new set alone.
    pub fn change_membership(&self, voters: &[NodeId]) -> Result<Index, Error> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.send(Event::Membership(voters.to_vec(), reply))?;
        answer
            .recv_timeout(self.inner.options.write_timeout)
            .map_err(|_| Error::Timeout)?
    }

    /// Snapshots the database at the applied index and drops the log up to
    /// it; returns that index.
    pub fn compact(&self) -> Result<Index, Error> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.send(Event::Compact(reply))?;
        answer
            .recv_timeout(self.inner.options.write_timeout)
            .map_err(|_| Error::Timeout)?
    }

    /// Stops the member's thread (its persisted state stays).
    pub fn stop(&self) {
        if let Some(sender) = lock(&self.inner.events).take() {
            let _ = sender.send(Event::Stop);
        }
        if let Some(handle) = lock(&self.driver).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Driver {
    inner: Arc<Inner>,
    raft: Raft,
    storage: Storage,
    transport: Arc<dyn Transport>,
    snapshot_file: Option<PathBuf>,
    /// Snapshot bytes that arrived with a message, by index.
    incoming: HashMap<Index, Arc<Vec<u8>>>,
    /// Writes proposed here, by index: the term they were proposed in.
    waiters: HashMap<Index, (Term, Reply<()>)>,
    /// Linearizable reads waiting for the leader's confirmation.
    reads: HashMap<u64, Reply<Index>>,
    next_read: u64,
}

impl Driver {
    fn run(&mut self, events: Receiver<Event>) {
        let tick = self.inner.options.tick;
        let mut next_tick = Instant::now() + tick;
        loop {
            let wait = next_tick.saturating_duration_since(Instant::now());
            match events.recv_timeout(wait) {
                Ok(Event::Message(envelope, snapshot)) => {
                    if let (Message::Snapshot { meta, .. }, Some(bytes)) =
                        (&envelope.message, snapshot)
                    {
                        self.incoming.insert(meta.index, bytes);
                    }
                    self.raft.step(envelope);
                }
                Ok(Event::Propose(data, reply)) => match self.raft.propose(data) {
                    Ok(index) => {
                        self.waiters.insert(index, (self.raft.term(), reply));
                    }
                    Err(grafeno_raft::Error::NotLeader(leader)) => {
                        let _ = reply.send(Err(Error::NotLeader(leader)));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::Lost));
                    }
                },
                Ok(Event::Membership(voters, reply)) => {
                    let outcome =
                        self.raft
                            .change_membership(voters)
                            .map_err(|error| match error {
                                grafeno_raft::Error::NotLeader(leader) => Error::NotLeader(leader),
                                _ => Error::ChangeInProgress,
                            });
                    let _ = reply.send(outcome);
                }
                Ok(Event::ReadIndex(reply)) => {
                    self.next_read += 1;
                    let context = self.next_read;
                    match self.raft.read_index(context) {
                        Ok(()) => {
                            self.reads.insert(context, reply);
                        }
                        Err(grafeno_raft::Error::NotLeader(leader)) => {
                            let _ = reply.send(Err(Error::NotLeader(leader)));
                        }
                        Err(_) => {
                            let _ = reply.send(Err(Error::Lost));
                        }
                    }
                }
                Ok(Event::Compact(reply)) => {
                    let outcome = self.compact();
                    let _ = reply.send(outcome);
                }
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            while Instant::now() >= next_tick {
                self.raft.tick();
                next_tick += tick;
            }
            if let Err(error) = self.handle_ready() {
                // The state machine cannot go on without this entry: stop
                // rather than diverge.
                eprintln!(
                    "grafeno cluster member {}: {error}; stopping",
                    self.inner.id
                );
                break;
            }
            // A leader can step down without a message to send.
            self.publish();
        }
        for (_, (_, reply)) in self.waiters.drain() {
            let _ = reply.send(Err(Error::Stopped));
        }
        for (_, reply) in self.reads.drain() {
            let _ = reply.send(Err(Error::Stopped));
        }
        lock(&self.inner.status).role = "stopped".into();
        self.inner.changed.notify_all();
    }

    fn snapshot_path(&self, index: Index) -> PathBuf {
        self.inner
            .options
            .snapshot_dir
            .join(format!("snapshot-{index}.redb"))
    }

    fn handle_ready(&mut self) -> Result<(), Error> {
        let ready = self.raft.ready();
        if ready.is_empty() {
            return Ok(());
        }
        // A snapshot from the leader: its data replaces this database's.
        let mut installed = None;
        if let Some(meta) = &ready.snapshot {
            let bytes = self
                .incoming
                .remove(&meta.index)
                .ok_or_else(|| Error::Storage("a snapshot arrived without its data".into()))?;
            let path = self.snapshot_path(meta.index);
            std::fs::write(&path, bytes.as_slice()).map_err(|e| Error::Storage(e.to_string()))?;
            self.inner.db.restore_backup(&path)?;
            lock(&self.inner.counters).snapshots_installed += 1;
            installed = Some((meta.clone(), path));
            // Writes proposed here before cannot be told apart any more.
            for (_, (_, reply)) in self.waiters.drain() {
                let _ = reply.send(Err(Error::Lost));
            }
        }
        self.storage.persist(
            ready.hard_state.as_ref(),
            ready.truncate_from,
            &ready.entries,
            installed
                .as_ref()
                .map(|(meta, path)| (meta, path.as_path())),
        )?;
        if let Some((_, path)) = installed {
            self.replace_snapshot_file(path);
        }
        for (context, index) in ready.reads {
            if let Some(reply) = self.reads.remove(&context) {
                let _ = reply.send(index.ok_or(Error::NotLeader(self.raft.leader())));
            }
        }
        for envelope in ready.messages {
            let snapshot = match envelope.message {
                Message::Snapshot { .. } => {
                    lock(&self.inner.counters).snapshots_sent += 1;
                    self.snapshot_file
                        .as_ref()
                        .and_then(|path| std::fs::read(path).ok())
                        .map(Arc::new)
                }
                _ => None,
            };
            self.transport.send(envelope, snapshot);
        }
        for entry in ready.committed {
            let applied = match &entry.payload {
                Payload::Data(bytes) => {
                    let record: grafeno::Entry =
                        serde_json::from_slice(bytes).map_err(|e| Error::Storage(e.to_string()))?;
                    self.inner
                        .db
                        .write(|tx| tx.apply_entry(entry.index, &record))
                }
                Payload::Noop | Payload::Config(_) => {
                    let empty = grafeno::Entry {
                        format: grafeno::ENTRY_FORMAT,
                        layout: grafeno::SCHEMA_VERSION,
                        at: Timestamp::from_micros(0),
                        ops: Vec::new(),
                        changes: Vec::new(),
                    };
                    self.inner
                        .db
                        .write(|tx| tx.apply_entry(entry.index, &empty))
                }
            };
            applied?;
            // The status shows the entry before its write returns.
            self.publish();
            if let Some((term, reply)) = self.waiters.remove(&entry.index) {
                let _ = reply.send(if term == entry.term {
                    Ok(())
                } else {
                    Err(Error::Lost)
                });
            }
        }
        Ok(())
    }

    fn replace_snapshot_file(&mut self, path: PathBuf) {
        if let Some(old) = self.snapshot_file.replace(path.clone())
            && old != path
        {
            let _ = std::fs::remove_file(old);
        }
    }

    fn compact(&mut self) -> Result<Index, Error> {
        let index = self.raft.applied();
        if index <= self.raft.snapshot_meta().index {
            return Ok(self.raft.snapshot_meta().index);
        }
        let path = self.snapshot_path(index);
        let _ = std::fs::remove_file(&path);
        self.inner.db.begin_read()?.backup(&path)?;
        self.raft
            .compact(index)
            .map_err(|_| Error::Storage("cannot compact".into()))?;
        let meta = self.raft.snapshot_meta().clone();
        self.storage
            .persist(None, None, &[], Some((&meta, &path)))?;
        self.replace_snapshot_file(path);
        self.publish();
        Ok(index)
    }

    /// Shares the current view with callers.
    fn publish(&self) {
        let read = self.inner.db.begin_read().ok();
        let applied = read
            .as_ref()
            .and_then(|read| read.log_position().ok())
            .map_or(0, |position| position.applied);
        // Held before the status says it is applied.
        if let Some(read) = read {
            self.inner.pin(applied, read);
        }
        let config = self.raft.config();
        let status = Status {
            id: self.inner.id,
            role: match self.raft.role() {
                Role::Leader => "leader",
                Role::Candidate => "candidate",
                Role::PreCandidate => "pre-candidate",
                Role::Follower => "follower",
            }
            .into(),
            term: self.raft.term(),
            leader: self.raft.leader(),
            commit: self.raft.commit(),
            applied,
            last_index: self.raft.last_index(),
            snapshot_index: self.raft.snapshot_meta().index,
            voters: config.voters.iter().copied().collect(),
            outgoing: config.outgoing.iter().copied().collect(),
            elections: self.raft.elections(),
            ..Status::default()
        };
        let mut status = status;
        {
            let mut counters = lock(&self.inner.counters);
            let leader = self.raft.leader();
            if leader.is_some() && leader != counters.leader {
                counters.leader_changes += 1;
                counters.leader = leader;
            }
            status.leader_changes = counters.leader_changes;
            status.snapshots_sent = counters.snapshots_sent;
            status.snapshots_installed = counters.snapshots_installed;
        }
        let last = status.last_index;
        status.peers = self
            .raft
            .progress()
            .into_iter()
            .map(|(id, matched)| PeerProgress {
                id,
                matched,
                lag: last.saturating_sub(matched),
            })
            .collect();
        {
            let members = lock(&self.inner.members);
            status.members = members.clone();
            status.members.insert(self.inner.id, MemberInfo::here());
            status.format = status
                .voters
                .iter()
                .chain(&status.outgoing)
                .filter(|id| **id != self.inner.id)
                .map(|id| members.get(id).map_or(1, |info| info.format))
                .fold(grafeno::ENTRY_FORMAT, u32::min);
        }
        let mut current = lock(&self.inner.status);
        if *current != status {
            *current = status;
            self.inner.changed.notify_all();
        }
    }
}
