# Phase 13: Distributed operation

[All phases](README.md) · [← Phase 12: Compatibility and speed](phase-12-compatibility-and-speed.md) · [Phase 14: Users, roles and permissions (RBAC) →](phase-14-users-roles-and-permissions.md)

**Status:** done, in six steps.

Several servers holding the same data, kept in sync by a replicated log with Raft; a leader elected among them coordinates writes, the others take over when it fails, and reads and large queries spread over all of them.

Steps:

- [13.1 A replicated log of deterministic write groups (done)](#131-a-replicated-log-of-deterministic-write-groups-done)
- [13.2 Raft: leader election and log replication (done)](#132-raft-leader-election-and-log-replication-done)
- [13.3 Continuing when an instance fails (done)](#133-continuing-when-an-instance-fails-done)
- [13.4 Queries in parallel across instances (done)](#134-queries-in-parallel-across-instances-done)
- [13.5 More than one group: namespaces placed on different servers (done)](#135-more-than-one-group-namespaces-placed-on-different-servers-done)
- [13.6 Operating a cluster (done)](#136-operating-a-cluster-done)

The goal: run Grafeno on several servers that hold the same data, so that
the service keeps working when one of them fails, reads scale with the
number of servers, and large queries run on several of them at once,
without giving up what one server guarantees today (a write is
all-or-nothing, a read sees whole transactions, an acknowledged write is
never lost).

The guidelines of phases 10 to 12 hold (local, agent-agnostic, CPU only),
plus: no external coordinator (no separate consensus service, no shared disk), one
binary, and a single server keeps working exactly as now, with the cluster
off. Each step ends with tests (fault injection where it applies), a
measurement, documentation and its own commit.

**Where it starts from.** One process owns the file and is its only
writer; concurrent writes are grouped into one redb transaction (group
commit, 8.4); every write already leaves a change log of what it changed,
record by record, numbered per namespace (11.4); a backup is a consistent
snapshot taken without stopping writers (4); the file has a layout version
and upgrades on open; and 12.3's aggregates fold into accumulators, the
form a distributed query needs to merge partial results.

**The model chosen.** One leader per group of servers (three or five),
elected with Raft, orders every write into a replicated log; a write is
acknowledged once a majority has the entry, and every member applies the
log in the same order to its own redb file. The alternatives were weighed:

- *Shipping redb pages or files* ("physical" in the storage sense): redb
  has no write-ahead log to ship, its page layout differs from file to file,
  and copying whole files only works for catching up, not for each write.
  Backups (snapshots) are used exactly for that: a new or lagging member.
- *Shipping the requests* (statement replication): a GQ write or a `remember`
  call is not deterministic (it draws ids, reads the clock, reads the state
  it runs on), so members could diverge.
- *Shipping the effects* (record-level replication), chosen: the leader runs
  each write group once, and the log entry holds what it did to the records
  (nodes and edges written or deleted, with their final ids and times,
  texts, embeddings, index and schema changes), which is what 11.4's change
  log already captures. Members apply effects, never run statements again,
  and rebuild their derived entries (adjacency, labels, postings, HNSW
  links, int8 copies, counters) from them, so a member's file is the same
  data, verifiable with `check` and by comparing exports.

## 13.1 A replicated log of deterministic write groups (done)



- Every write group (what the writer thread commits today) is turned, on the
  leader, into a log entry of resolved effects: final ids and timestamps,
  GQ statements reduced to the records they wrote, embeddings and texts as
  values, schema changes (indexes, constraints, principals) as values. An
  entry carries the layout and entry format versions it was written with.
- An `apply` path that takes an entry and performs it in one redb write
  transaction, deriving every index from the records, and stores the log
  position it reached (`applied_index`) in the same transaction, so a crash
  between applying and recording is impossible and replaying never applies
  twice.
- A log table (entries by index and term), its truncation after a snapshot,
  and snapshots built from backups plus the position they hold. A new
  layout version, with its upgrade and fixture.
- Done when: replaying a database's log onto an empty file gives the same
  data (exports equal, `check` clean) as the file that wrote it, for every
  kind of write, including writes that fail halfway (nothing in the log)
  and group commits that mix failing and succeeding writes; property tests
  against the 4.x model.

As built (`crates/grafeno/src/replication.rs`, layout version 14):

- Capture: a write transaction notes the key of every primary record it
  writes (`WriteTx::note`, called where records are written: `put_node`,
  `remove_node_record`, `put_edge`, `delete_edge`, texts, embeddings,
  memory states, index and constraint definitions, principals, namespaces,
  match rules, state entries, merge records); dropping a namespace,
  rebuilding its indexes and compacting its vectors are noted as that work
  (`WriteTx::bulk`) and not record by record. At commit, before the change
  log is written, each noted key's final value is read and the
  `replication::Entry` (format, layout, the commit clock, the operations in
  the order first touched, plus the namespaces registered on the way and
  the change log entries of the commit) is appended to `replication_log`
  at the next index, in the same redb transaction. Aborted transactions and
  writes of a failed group commit leave nothing: the group is retried in a
  new transaction.
- Apply (`WriteTx::apply_entry`, `Grafeno::apply_entries`): an entry at
  index `applied + 1` is performed record by record with the derivations of
  the normal write path (`put_node` and `put_edge` keep labels, names,
  property indexes, adjacency and counters; text and embedding writes keep
  postings, the HNSW graph and the int8 copies; index definitions are
  created and filled), while rules that decide (constraints, property
  history) do not run (`applying`), and the change log receives the
  leader's entries with the leader's clock, so sequence numbers and times
  are the same on every member. The entry is kept in the member's own log
  and `replication_applied` advances in the same transaction: an entry
  already applied is skipped, one with a gap before it is refused, and a
  crash cannot leave one half applied.
- The log is off until `Grafeno::enable_replication_log` (kept in `meta`):
  a single server writes nothing more. `ReadTx::log_position` and
  `log_entries`, `WriteTx::truncate_log` (after a snapshot); a snapshot is a
  backup, which now copies the log and the position, and a member restored
  from one goes on from the entry after it.
- Data equality, not byte equality: the HNSW graph of a member is built in
  the order its entries apply, which can differ from the leader's when a
  transaction set a vector twice; exports, counters, the change log,
  constraint events and query results are the same.
- Tests: `grafeno/tests/replication.rs` (every kind of write over several
  transactions, replayed: exports, statistics, constraints, match rules,
  merges, indexes, state, change log and principals equal, `check` clean,
  traversals and searches answer the same; idempotence and gaps; a member
  from a backup after the log was truncated; group commits with failing
  writes; the log off), `grafeno-query/tests/replication.rs` (GQ writes:
  `MERGE`, `SET`, `DETACH DELETE`, `RETRACT`, `FOREACH`, indexes and an edge
  constraint invalidating facts), the 4.x model property test now replays
  each random sequence's log and compares, and `upgrade.rs` with the new
  `layout-v13` fixture.
- Measured (`examples/replication_bench.rs`, 100 000 nodes with text and
  99 999 edges in transactions of 1 000, best of three alternating runs, in
  memory): writes at 18 384 records a second with the log on against 22 925
  off (80%); the log takes 568 bytes per record (JSON, with each text and
  each change log entry); a replica applies it at 22 298 records a second.
  A binary encoding of entries is left for 13.2, where they cross the
  network.

## 13.2 Raft: leader election and log replication (done)



- Consensus among three or five members: terms, elections with randomized
  timeouts, log replication with quorum commit, a pre-vote phase so that a
  member that was cut off does not depose a working leader on return, and
  membership changes (joint consensus) to add, remove or replace a member
  without stopping. To evaluate first: an existing async Rust library
  against writing the protocol, which is small but subtle; the choice is
  recorded here with its reasons.
- The leader's writer proposes each group and acknowledges its writes once
  the entry is committed (a majority has it durably); followers apply
  committed entries in order. Durability settings (immediate, eventual)
  become per-entry fsync policies on every member.
- Snapshot install for a member too far behind (a backup streamed with its
  log position, then the log after it), and log compaction.
- A cluster transport between members (its own port, authenticated with a
  cluster secret and TLS), and `[cluster]` in `grafeno.toml`: the member's
  id, its peers, timeouts. `grafeno cluster init`, `join`, `leave`,
  `status`.
- Done when: a deterministic in-process simulation (members, a network
  that drops, delays, duplicates and reorders messages, crashes and
  restarts, all from a seed) runs thousands of schedules and never loses an
  acknowledged write, never applies two different entries at one index, and
  always elects a leader once a majority can talk.

As built (`crates/grafeno-raft`, `crates/grafeno-cluster`,
`crates/grafeno-server/src/cluster.rs`):

- The choice: Raft written here, not an existing library. As a deterministic state
  machine (`Raft::tick`, `step`, `propose`, `change_membership`, `compact`,
  and a `Ready` with the hard state and entries to persist, the messages to
  send after persisting, and the committed entries to apply), the protocol
  can be simulated from a seed, which an async library on tokio makes hard;
  it adds no dependency and fits 13.1's log and the server's threads. It has
  terms and randomized timeouts, pre-vote, leader stickiness and
  check-quorum, replication with conflict hints, commitment of the leader's
  own term (a no-op per term), membership by joint consensus in two
  automatic phases (a removed leader steps down once the new configuration
  commits), compaction, and snapshots whose data the caller carries. A
  snapshot that gets no answer is sent again at the next check-quorum.
- The simulation (`grafeno-raft/tests/simulation.rs`): five members, three
  voting at first; a network that drops, delays, duplicates and reorders
  messages and splits in two; crashes and restarts from persisted state;
  membership changes and compaction; client writes retried as a client
  would. Each schedule checks one leader per term, one entry per committed
  index, that no acknowledged write is lost, and that once healed a leader
  is elected and every member converges. 300 schedules run with `cargo
  test` (3 000 passed while it was written; `RAFT_SEEDS` runs more); in
  300 schedules there were 2 076 terms with a leader, 1 109 snapshots
  installed, 1 614 configurations committed, 35 638 acknowledged writes
  and 1 785 crashes. Writing it found its own bugs (a snapshot lost in the
  network was never sent again) and the test's (a write proposed to a
  leader that is then deposed may be dropped; members leave).
- A member (`grafeno_cluster::Node`): a thread owns the member's `Raft`,
  ticks it, persists in a redb file of its own (`<db>.raft`: the data file
  has one writer), sends through a `Transport`, and applies committed
  entries to the database in order. Raft's own entries (no-ops,
  configurations) apply as empty entries, so the database's position and
  Raft's index are the same number.
- Writes on the leader (`Node::write`): `Grafeno::dry_run` runs the write in
  a transaction it discards and returns its value and 13.1's entry; the
  entry is proposed, and once a majority has it every member, the leader
  included, applies it, and the write returns. One way of applying for all,
  so a member that crashed anywhere recovers from its recorded position.
  Writes are serialized on the leader (each runs on the state the previous
  left); grouping several in one entry is left for later. A follower
  refuses with the leader's id; a write whose entry a new leader replaced
  fails as `Lost` (it did not happen); one with no answer in time fails as
  `Timeout` (it may have).
- Snapshots are backups at the applied index (`Node::compact`); a member
  too far behind receives the file's bytes beside the message and replaces
  its data in one transaction (`Grafeno::restore_backup`).
- The server: `[cluster]` in `grafeno.toml` (id, the address members talk
  on, a shared secret, the first voters, the peers' addresses, the tick).
  Members talk on their own port, `POST /raft` with one message (a length,
  the message as JSON, then a snapshot's bytes), authenticated by the
  secret; a thread and a bounded queue per peer, so sending never blocks
  Raft. Every write handler goes through `Writes`, which on a member calls
  the leader's path; a follower answers 503 naming the leader. `GET
  /v1/cluster`, `POST /v1/cluster/voters`, `POST /v1/cluster/compact` (owner)
  and `grafeno cluster status|voters|compact`.
- Not built in this step: TLS between members (the traffic is
  authenticated, not encrypted: keep members on a private network),
  imports into a cluster (refused; import into one server and start the
  cluster from it), peers learned at run time (addresses come from the
  configuration), and work that must run on the leader alone
  (extraction, summaries, webhooks run on every member and fail on
  followers): 13.3.
- Tests: the simulation; `grafeno-cluster/tests/cluster.rs` (members in one
  process: replication with equal exports, change logs and positions; a
  leader cut off while the others elect another, its lone write never
  happening; a restart from persisted state; members joining from the log
  and from a snapshot, and leaving); `grafeno-server/tests/cluster.rs`
  (three `grafeno serve` processes: writes through the leader's API, a
  follower naming the leader, the leader's process killed and writes going
  on).
- Measured (`grafeno-cluster/examples/cluster_bench.rs`, files on one NVMe
  disk, immediate durability, sequential writes of one node): one server
  618 writes a second (p50 1.58 ms), a three-member cluster in one process
  137 (p50 7.22 ms, p99 9.94 ms): each write is a dry run, an fsync of the
  Raft log on the leader and on each follower, then an apply with its own
  fsync on each member, all on one disk. A leader cut off is replaced and a
  write committed in 510 to 831 ms (median 595) with 50 ms ticks.

## 13.3 Continuing when an instance fails (done)



- Failover: when the leader stops answering, a new one is elected within a
  bounded time and writes resume; the old leader, if it comes back, steps
  down (terms fence it). Writes in flight during the change fail with a
  retryable error or succeed exactly once (idempotency keys, as NDJSON
  ingestion already has).
- Clients find the leader: followers forward writes to it (or answer with
  its address); the TypeScript client, the shell, MCP (`--url`) and the
  Desktop app accept several server addresses and move to another when one
  fails.
- Reads by consistency level: `linearizable` (through the leader, with a
  lease or a read index), `bounded` (any member at most N ms or N entries
  behind), and read-your-writes (a write returns its log position; a read
  that carries it waits until the member has applied it).
- Work that must run once (extraction, community summaries, webhooks, the
  change log's retention, 11.x) runs on the leader only, with its progress
  in the replicated state, so a new leader resumes where the old one
  stopped; server-sent event streams continue their numbering across a
  failover.
- Done when: fault injection on real processes (a docker compose of three
  and five members, killing, pausing and partitioning them with
  `iptables`/`tc`) under a workload of reads and writes, checked by a
  linearizability checker (registers and counters), finds
  no anomaly; failover time and lost availability measured.

As built (`crates/grafeno-raft`, `crates/grafeno-cluster`,
`crates/grafeno-server/src/cluster.rs`, the shell and the TypeScript
client):

- Linearizable reads by ReadIndex (`Raft::read_index`): the leader notes
  its commit index, confirms it is still the leader with a round of
  heartbeats that a majority answers (a read round carried by appends and
  echoed in the replies), and hands the index back in `Ready::reads`; a
  leader that has not yet committed an entry of its term answers "try
  again", and one that steps down fails its pending reads. No leases: they
  depend on clocks, and a round costs one heartbeat. `Node::read_barrier`
  waits until the member has applied the index. The simulation checks every
  read against the writes acknowledged before it was asked (34 291 reads in
  1 000 schedules while it was written).
- Routing on every member (`cluster::route`, a layer on `/v1`): a follower
  forwards to the leader what only it can serve, with the original path,
  the body and an allowlist of headers (authorization, content type,
  idempotency key, `Last-Event-ID`, `x-grafeno-*`), marked so it is never
  forwarded twice. Peers name the leader's API address (`api` in `peers`).
  A write refused on a follower (421 `not_leader`) is forwarded; during an
  election the follower waits up to 3 s for a leader first. Connections
  between members time out after 500 ms, so a dead leader is noticed
  quickly (Windows takes 2 s to refuse a connection on its own).
- Consistency, per request (`x-grafeno-consistency`): `local` (the
  default: what the member has applied), `bounded` (to the leader when the
  member is more than 1 000 entries behind or knows no leader), and
  `linearizable` (a read barrier on the leader; a follower forwards it).
  Read-your-writes: every response carries the member's applied position
  (`x-grafeno-position`); a request with `x-grafeno-after: N` waits up to
  5 s for the member to apply N (503 otherwise). Positions are Raft
  indexes, the same on every member.
- Exactly once: a GQ write with an idempotency key (`idempotency_key` in
  the body or an `Idempotency-Key` header) stores its result in the same
  replicated transaction, and the same key again returns that result
  without running it, on any member and after a failover.
- Clients: the TypeScript client takes several URLs (`url: [...]`) and the
  shell several in `--url` (comma-separated); both use one member, move to
  the next when it does not answer, and send the last position they saw,
  so they read their own writes wherever they land. The TypeScript client
  also takes a consistency (per client or per statement) and idempotency
  keys, and shows `url`, `position` and `cluster()`.
- Work that runs once: webhook delivery and background tasks (extraction,
  community summaries) run on the leader alone; their cursors live in the
  replicated state, so a new leader resumes where the old one stopped. The
  change log is replicated as the leader wrote it (13.1), so sequence
  numbers are the same on every member and a server-sent event stream
  resumes on another member with `Last-Event-ID`.
- The checker (`grafeno_cluster::check`): linearizability of a register,
  by Wing and Gong's search with Lowe's memo of visited (linearized set,
  value) pairs. Writes whose outcome is unknown (a timeout, a lost
  connection) may take effect at any time after they were sent, or never;
  reads with no answer are left out.
- Fault injection: `grafeno-cluster/tests/linearizable.rs` runs five
  members in one process while clients write distinct values to one
  register and read it linearizably, and a nemesis cuts members off (one,
  the leader, or two at once), restarts them (the leader too) and compacts
  their logs, one fault every 0.5 to 1.3 s; `grafeno-server/tests/faults.rs`
  runs three `grafeno serve` processes, with clients that know every member,
  killing a process (half the time the leader's) every 1.5 to 3.5 s and
  starting it again. Both check the history and print the failover time
  and availability; `FAULT_SEED` replays a run. The control
  (`FAULT_UNSAFE_READS=1`, reads of a member's own state without ReadIndex)
  is rejected by the checker in every run, so the test can see the anomaly
  it looks for.
- Measured (E1-disk, 20 ms ticks, so election timeouts of 200 to 400 ms):
  in one process, three runs of 30 s with 130 faults: 23 645 operations,
  all linearizable; after a fault that hit the leader, the first write sent
  after it succeeded within 370 to 384 ms (median of each run), 1 015 ms at
  most; writes succeeded in 68 to 75% of 100 ms windows under that rate of
  faults. With real processes, two runs of 40 s with 32 processes killed
  (19 of them leaders): 8 114 operations, all linearizable; writes resumed
  529 to 531 ms (median) after the leader was killed, 697 ms at most, and
  succeeded in 73 to 78% of windows. Before the connection timeout the
  median was 1 545 ms (Windows's 2 s refusal); with a 1 s timeout, 1 023 ms.
- Not built in this step: partitions and pauses of real processes
  (`iptables`, `tc`, docker compose; partitions are exercised in one
  process, kills on real processes), a counter workload (registers only),
  read leases, and the Desktop app and MCP with several addresses (MCP
  opens a database file and has no remote mode yet).
- Tests: `grafeno-raft/tests/simulation.rs` (reads added),
  `grafeno-cluster/tests/linearizable.rs`, `grafeno-cluster/src/check.rs`
  (the checker on hand-made histories), `grafeno-server/tests/cluster.rs`
  (a write forwarded by a follower with its position, a read on a third
  member waiting for it, a linearizable read on a follower, a keyed write
  sent three times, failover), `grafeno-server/tests/faults.rs`, and
  `clients/typescript/test/cluster.test.ts` (the client across three
  processes, the member it uses killed).

## 13.4 Queries in parallel across instances (done)



- Reads spread over members: a server (or the client) balances reads over
  the members that meet the requested consistency.
- Large read queries split across members: every member holds all the data,
  so a query whose start is large (12.2's parallel scans, 12.4's frontiers)
  is cut into ranges of anchor ids; each member runs its ranges at the same
  log position (it waits until it has applied it) and returns partial
  results, 12.3's accumulators made mergeable (counts, integer sums, min,
  max, `collect` in range order; floating-point sums and averages with a
  summation that does not depend on the order, decided in this step so that
  results equal one server's), and the coordinator merges them in range
  order. Queries that return rows stream them back in range order.
- Graph algorithms that already parallelize (PageRank, components) stay on
  one member; distributing them is a separate decision.
- Done when: every query of the conformance and planner suites returns the
  same rows on one server and split across three; speed-up measured for
  scans, two-hop traversals and aggregates with 1, 3 and 5 members.

As built (`crates/grafeno-query/src/split.rs`, `grafeno-cluster`,
`grafeno-server/src/cluster.rs`, the TypeScript client):

- What splits: a read whose first clause is a `MATCH`, whose clauses up
  to the final `RETURN` work row by row (`MATCH`, `OPTIONAL MATCH`,
  `UNWIND`, a `WITH` that neither aggregates, sorts, drops duplicates nor
  limits), with no `UNION`, procedure or volatile function (`rand()`,
  `randomUUID()`, the clock). Anything else runs on one member.
- Parts (`execute_part`): part `k` of `n` takes the `k`th share of the
  first pattern's candidates, by position in the order one server visits
  them (a label's ids, an index lookup's, or a scan's pages), so the
  parts' rows end to end are one server's rows; a part large enough
  still scans on several threads (12.2). Every part plans with the
  coordinator's plan information, statistics included
  (`PlannerStats::to_wire` carries their encoded buckets), so all make the
  same plan: statistics in memory can differ from member to member. With
  aggregates a part returns its groups unfinished; without, its projected
  rows (distinct with `DISTINCT`, sorted by their `ORDER BY` keys, which
  it sends along, cut to `SKIP + LIMIT` when that is enough). Values
  travel exactly (`Value::to_wire`: floats by their bits, nodes, edges
  and paths whole).
- Merging (`merge_parts`): groups merge in part order (a group seen
  before merges its accumulators; a new one goes after), then finish as
  on one server; rows go end to end, lose duplicates, are sorted stably
  (ties keep the order one server found them in), then `SKIP` and
  `LIMIT`. The decision this step was to make: sums do not depend on the
  order of the rows, on one server too. Integers add in 128 bits (only a
  total that does not fit 64 is an overflow: `max + 1 - 5` is no longer
  an error), floats exactly (Shewchuk's partials, as Python's
  `math.fsum`: the exact sum rounded once, so `0.1 + 0.2 + 0.3` sums to
  0.6). `count`, `min`, `max` merge directly; `DISTINCT` aggregates carry
  their distinct values; `collect`, percentiles, `stdev`, `variance` and
  `mode` carry their values in row order.
- One position: a member keeps a snapshot after each of the last 64
  applied entries, for 2 s at most (`Node::snapshot_at`; first built as
  one per entry for 5 s, which slowed concurrent writes, see 13.5). The
  member that splits a read takes its latest position; the others wait
  until they reach it (up to 5 s) and read their snapshot at exactly that
  position; one that has moved further on has its part run by the member
  that splits.
- The server: a read on a cluster member splits when the plan's
  estimated cost (as `EXPLAIN` shows it) reaches `split_cost` in
  `[cluster]` (200 000 by default), or always with `x-grafeno-split: on`
  (`off` never). The member runs its own part and asks each other member
  for one (`POST /v1/{ns}/query/part`, with the caller's credentials, so
  the same grants apply); a member that does not answer has its part run
  here. The answer says `split: {parts, here}`.
- Reads spread: the TypeScript client with `spread: true` starts each
  request on the next member (writes are forwarded, and read-your-writes
  holds through positions). No server-side balancing.
- Equality: the conformance, planner, language, query, traversal and batch
  suites check every read that can be split, whole against 2, 3 and 5
  parts sent through JSON (70 of the 146 conformance scenarios split; the
  others write, or start without a `MATCH`); a control that merges the
  parts in the wrong order fails at once. `grafeno-server/tests/split.rs`
  compares eight reads split across three processes, from a follower and
  from the leader, with `here` equal to 1, and again with a member down
  (`here` 2).
- Measured (`split_bench`, 200 000 people, 3 scan threads per member to
  stand for a machine of that size; every member on this laptop's 8
  cores): a scan 233 → 129 → 112 ms with 1, 3 and 5 members (2.1 times),
  an aggregate by city 412 → 176 → 141 ms (2.9), the top 20 by score 745 →
  337 → 250 ms (3.0), two hops 2.54 → 1.83 → 1.77 s (1.4). For context,
  one server with 9 threads on the same machine: 115, 255, 546 and
  1 364 ms. Two hops land on random nodes, so every member reads the whole
  graph and on one machine each process needs its own cache of it; on
  separate machines that is not shared, and the measure here is a floor.
- Not built in this step: server-side balancing of reads, splitting a
  query whose aggregating `WITH` comes before more clauses (it runs on one
  member), streaming rows back as parts finish (parts answer whole), and
  measuring on separate machines.

## 13.5 More than one group: namespaces placed on different servers (done)



- Write throughput is still one leader's. To go beyond it, a cluster can run
  several Raft groups, each owning whole namespaces (multi-Raft), with a
  small replicated directory saying which group owns which namespace; any
  member routes a request to the right group.
- Moving a namespace between groups without stopping it: snapshot, catch up
  with its change log, a short write pause, switch in the directory.
- Queries that span namespaces (only those that ask, as today) run on each
  owner and merge as in 13.4.
- Done when: write throughput measured with one, two and three groups, and a
  namespace moved under load without losing or duplicating a write.

As built (`grafeno/src/replication.rs`, `grafeno-server/src/groups.rs`,
`grafeno-server/src/cluster.rs`):

- Groups: `groups = [{ id = 1, voters = [1, 2, 3] }, …]` in `[cluster]`.
  Group 0 is the cluster of 13.2, held by every member, with the file
  `db`; each other group has its own voters, database file
  (`<db>.group-<id>.redb`), Raft state, snapshots and leader, and its
  members talk on the same port (`/raft/{id}`). A member runs a full
  server state per group it holds (writer, change streams, webhooks,
  background work on that group's leader), sharing keys, limits and
  metrics.
- The directory lives in group 0 (state `cluster`/`placement`, replicated
  as any write); a namespace it does not name is in group 0. Any member
  takes any request: a layer in front of every route looks the
  namespace up (`/v1/{ns}/…`, `/v1/namespaces/{ns}`) and hands the request
  to that group's router, whose own layers forward to that group's
  leader as in 13.3, or, when this member does not hold the group, to a
  member that does (once: `x-grafeno-group-hop`). `GET /v1/namespaces`
  merges the groups this member holds; `GET /v1/cluster/groups` lists the
  groups with this member's view of each; `GET /v1/cluster/placement` the
  directory.
- Copying a namespace, in the engine: `ReadTx::namespace_ops` gives every
  primary record of a namespace as 13.1's effects, its change log
  included (`Op::Change`: entries with their sequence numbers and clocks);
  `WriteTx::import_ops` writes effects copied from another database as
  this one's own writes (noted for its log, so a group's leader
  replicates them), running no rule and making no change log entry but
  the copied ones; `Entry::ops_of` filters a log entry to a namespace. The
  engine test copies a namespace, catches up with later entries and finds
  the same namespace, sequence numbers included; the copy's own log
  replays it onto a third database.
- Moving (`POST /v1/cluster/placement {namespace, group}`, on group 0's
  leader, which must hold the source group): the directory notes the
  move; the target drops what an earlier attempt left; the namespace is
  copied from a snapshot of the source in writes of 2 000 effects; then
  the source's log entries after that snapshot, filtered to the
  namespace, and its change log entries after those copied, in batches,
  round after round while writes go on, until a round finds fewer than 32
  entries. The pause: a fence written in the source's own log
  (`fence_namespace`) makes every later write to the namespace there fail
  as busy; writes are serialized on a leader, so every write before the
  fence is in the entries before it, and those are copied; the directory
  switches; 5 s later the source drops what it held and keeps the fence,
  so a late write is refused, never lost. Clients retry the busy writes
  of the pause (with idempotency keys, which move with the namespace).
  Every write of a move is idempotent, so the mover sends again one that
  meets a change of leader. A move that fails before the switch lifts the
  fence and restores the directory.
- Measured (`groups_bench`, three members on one machine and one disk,
  50 ms ticks, 24 clients writing one node per request to three
  namespaces): 109, 153 and 181 writes a second with one, two and three
  groups (p50 218, 116 and 135 ms): each group commits on its own, but the
  members share this machine's disk and cores. A namespace of 20 000
  nodes (41 557 effects) moved in 11 s while a client kept writing to it:
  2 catch-up rounds, a pause of 39 ms with 3 entries copied in it, 2 busy
  answers retried, and the 264 writes made during the move all there,
  once. In the end-to-end test, 290 to 340 writes from three clients
  during a move, on all three members, each exactly once, and the
  namespace's change log numbered without a gap.
- Found on the way: the snapshots 13.4 kept (one per applied entry, for
  5 s) slowed concurrent writes on a leader from 120 a second to 3 within
  seconds, as redb keeps the pages every open snapshot sees; members now
  keep the last 64 positions, for 2 s at most.
- Not built in this step: exports and `SHOW NAMESPACES` across groups
  (they cover the group the request reaches; listing merges), backups of
  more than group 0's file (back up each group's file), groups whose
  members change at run time (voters come from the configuration), moves
  run by a member that does not hold the source group, and splitting
  queries over namespaces of several groups (no GQ query spans
  namespaces; recall and context read one).
- Tests: `grafeno/tests/replication.rs` (copy and catch-up, the fence),
  `grafeno-server/tests/groups.rs` (three processes, two more groups:
  placement through any member, writes and reads routed, listings merged,
  a move under three writers checked on every member).

## 13.6 Operating a cluster (done)



- Status and metrics per member (role, term, commit and applied positions,
  lag, elections, snapshot transfers) in `/metrics`, `grafeno cluster
  status` and a cluster view in the Studio and the Desktop app.
- Rolling upgrades: members of two consecutive versions coexist; entries
  and snapshots carry their format version, and a leader never writes a
  format a follower cannot read; layout upgrades happen member by member.
- Backups from a follower, restore into a new cluster, and adding a member
  from a backup.
- Installation: a three-member docker compose, systemd units and the
  Windows service with `[cluster]`, and runbooks for losing one member,
  losing a majority, and replacing a disk.
- Done when: a rolling upgrade and every runbook have been run on the
  compose under load, with the linearizability checker watching.

As built (`grafeno-raft`, `grafeno-cluster`, `grafeno-server`, the
Studio's Cluster view, `packaging/compose/`, `docs/cluster-runbooks.md`):

- Status: every member's status (per group) counts the elections it
  started, the leader changes it saw and the snapshots it sent and
  installed; on the leader it lists each other member's last known index
  and lag; it says the entry format the group writes and what each member
  runs. `/v1/metrics` has `grafeno_cluster_*` series per group (leader,
  term, commit, applied, last and snapshot indexes, entry format,
  elections, leader changes, snapshots sent and installed, and
  `peer_lag{member}` on the leader); `grafeno cluster status|voters|compact
  --group N` and `/v1/cluster/groups/{id}/status|voters|compact` reach any
  group. Grafeno Studio has a Cluster view (the desktop app bundles the
  Studio): the groups as the member it talks to sees them, the lag of the
  others where it leads, the members' versions and formats, where
  namespaces live, and moves.
- Rolling upgrades: members say what they run (version, newest entry
  format, layout) with every message (`x-grafeno-member`). An entry carries
  the oldest format that can hold it (2 only with change log entries a
  namespace move copies, 1 otherwise), followers decide by the format,
  not by the layout of the database that wrote it (entries are records,
  whatever layout stores them), and a leader refuses a write whose format
  a voter cannot read (members before 13.6 do not say, and are taken to
  read format 1): "upgrade every member first". Followers hear
  only from the leader, so the leader's status is the one that knows
  every member's format. Layout upgrades happen as each member opens its
  file.
- Backups and members from backups: backups can be taken on a follower
  (`/v1/backup`, `?group=N` for another group's file). A database restored
  from a backup starts Raft after the position it holds (a snapshot of it
  at that index, of unknown term, persisted with a snapshot file), so a
  new cluster starts from copies of one backup, and a member joins from an
  older backup and catches up from the log after it, without a snapshot
  when the leader still holds those entries. A group's settings take
  `join = true` for a member rejoining it.
- Installation: `packaging/compose/` (three members, two Raft groups,
  each member's image and settings file chosen on its own, rejoin
  settings); the systemd unit and the Windows service need only the
  `[cluster]` section in their `grafeno.toml`; the image build touches the
  workspace's sources first (a cached target holding another version's
  build made Cargo keep stale crates). `docs/cluster-runbooks.md` covers
  installing, watching, backups and restores, losing one member, losing a
  majority, replacing a disk, and rolling upgrades.
- The drill: `drill` (an example of `grafeno-server`) loads a cluster with
  clients writing distinct values to a register and reading it
  linearizably on random members, then checks the history and reports
  availability; `packaging/compose/drill.sh` runs every runbook on the
  compose cluster under it, from an image of 13.5 (commit 275a196) to one
  of 13.6.
- Run on the compose with three clients under `drill`: a rolling upgrade
  from 13.5 to 13.6 with the leader first (11 218 operations; the move
  refused while a 13.5 member remained, format 2 and the move after),
  losing the leader for 10 s (1 754), losing a majority for 10 s (1 220;
  writes stop and nothing is lost), and replacing a disk from a
  follower's backup (1 779; caught up from the log, no snapshot): every
  history linearizable, every member holding the same registers at the
  end. Found on the way: a member whose volume survived an earlier run
  joined with that cluster's data and Raft state and served an old value
  (members with the same ids and secret cannot tell two clusters apart:
  the runbooks say never to reuse a data directory across clusters); the
  image build kept stale crates from a cached build of another version;
  and the drill script itself raced a previous run.
- Not built in this step: a cluster identity in messages (see above),
  TLS between members, a cluster view that asks
  every member (the Studio shows the member it talks to), automatic
  removal of a dead member, five-member and multi-machine drills, and
  network partitions on the compose (members are stopped, not cut off;
  partitions were exercised in one process in 13.3).
- Tests: `grafeno-cluster/tests/operations.rs` (a new cluster from a
  follower's backup, a member joining from an older backup without a
  snapshot, a restart of a member started that way, the status counters
  and lag, format negotiation), and the drill.

**Measures for the phase** (in `docs/benchmarks.md` and `docs/performance.md`,
against one server): write latency and throughput with three and five
members on one machine and over a real network; failover time; reads per
second with one to five members; speed-up of split queries; time to add a
member from a snapshot of 1 M nodes and 10 M edges; memory per member.
