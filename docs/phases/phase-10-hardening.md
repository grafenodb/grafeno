# Phase 10: Hardening

[All phases](README.md) · [← Phase 9: Desktop application](phase-09-desktop-application.md) · [Phase 11: Memory that keeps itself →](phase-11-memory-that-keeps-itself.md)

**Status:** done, in seven steps.

The priority technical risks, measured and closed before more features build on them.

Steps:

- [10.1 Isolation and authorization (done)](#101-isolation-and-authorization-done)
- [10.2 Data integrity under failure (done)](#102-data-integrity-under-failure-done)
- [10.3 HNSW quality after deletions (done)](#103-hnsw-quality-after-deletions-done)
- [10.4 Costly GQ (done)](#104-costly-gq-done)
- [10.5 Correct memory merges (done)](#105-correct-memory-merges-done)
- [10.6 RAM under large graphs (done)](#106-ram-under-large-graphs-done)
- [10.7 One writer (done)](#107-one-writer-done)

The guidelines (2026-11-04): agent-agnostic and independent of any LLM,
local, and efficient without a GPU. Nothing here calls a model: extraction,
reconciliation, contradiction checks and summaries are rules, statistics
and graph algorithms on the CPU. An agent that has an LLM can still bring
its own results through the API (extracted entities, a summary), and they
are stored with their provenance like any other write.

The risks come first: isolation, integrity and memory correctness are what
the later features (automatic merging, automatic invalidation, continuous
ingestion) would otherwise multiply. Each step ends with tests, a
benchmark where it measures something, documentation and its own commit.


## 10.1 Isolation and authorization (done)

One static token gave full
access to every namespace.

- Principals: named API keys (stored hashed), each with grants per
  namespace (or a namespace prefix such as `team-*`): `read`, `write`,
  `admin` (indexes, drop, import), plus a global `owner`. The static token
  of `grafeno.toml` stays as the owner.
- Identity in provenance: every write records the principal that made it
  (`recorded_by` on nodes and edges), queryable as `@recorded_by`.
- Provenance rules: a principal may retract, invalidate or merge only
  what it may write; an optional rule restricts that to records it made
  (agents that must not rewrite each other's memory).
- Every surface checks the same grants: HTTP, MCP, shell, GQ (a query
  only sees its namespace; procedures and algorithms too), export and
  import, the desktop app's bridge.
- Tests: a leak matrix, every endpoint and procedure called with keys of
  the wrong namespace and the wrong role, plus property tests that random
  queries never return another namespace's ids.
- Studio and `grafeno keys create | list | revoke`.

As built: principals live in a `principals` table (layout version 7) with
the SHA-256 of their secret (`gfk_` and 40 hex digits); the server keeps
them in memory by hash and reloads them when one changes. One middleware
maps every `/v1` route to what it needs (any caller, owner, or a role on
the namespace in its path) and serves the request with the caller in a
task-local, so handlers ask for more where the route alone cannot tell:
writing queries need write and index commands admin (from the parsed
statement), creating a namespace needs admin on its name, and listings,
`SHOW NAMESPACES` (through a filter in the query limits) and exports keep
to what the caller may read. Writes run as an `Actor` set on the write
transaction for each job of a group commit: the engine stamps
`recorded_by` and, for own-only actors, refuses changes to others' nodes
and edges (deleting a node checks its edges too; merging checks both nodes
and the moved edges) with `Error::Forbidden`, which GQ and HTTP report as
403. The owner token records nothing. A server without a token stays open
until its first key, which must be an owner; the Studio switches to it.
The desktop app gives each opened file a random owner token of its own.
MCP opens the file itself, so its fences (`--allow`, `--read-only`,
`--as`, `--own-only`) guard against the agent, not the user. Route names
under `/v1` are reserved as namespace names. Tests: a leak matrix over
every namespace route for a key of another namespace and a reader, owner
routes, revoked keys, own-only rules over HTTP and GQ, prefix grants.

## 10.2 Data integrity under failure (done)



- Crash tests extended to bulk imports, migrations, index rebuilds and
  namespace drops (a child process killed at random moments; every
  reported commit survives, nothing is half-done, the check passes).
- Migration fixtures: a committed database file for every layout version,
  opened and upgraded in tests, compared with a fresh build of the same
  data.
- Corruption: flipped bytes and truncated files are detected (open fails
  with a clear error, or the check names what is wrong), never a panic or
  silent wrong answers; `grafeno check --all` for every namespace.
- Online backup: `grafeno backup` and `GET /v1/backup` write a consistent
  copy from a read snapshot while writes go on; restore is opening it.

As built:
- `tests/crash.rs` kills a child process at random moments during a bulk
  import, a one-transaction import, index rebuilds, namespace drops (and
  refills) and a layout upgrade: every namespace checks clean afterwards,
  a one-transaction import is all or nothing, a dropped namespace is gone
  or whole, an interrupted upgrade upgrades on the next open. A bulk
  import commits its chunks with eventual durability, so a crash loses
  the chunks since the last flush, consistently (an *error* keeps them).
- `tests/fixtures/layout-v1..6.redb.gz` were written by the code of each
  past layout version (`tests/fixtures/make.py` builds those commits in
  git worktrees and runs a generator); `tests/upgrade.rs` opens every one
  with the current code and checks counts, text and vector search, the
  names index, valid time, traversals, importance, indexes and empty
  namespaces, then writes and reopens. Every new layout version adds its
  fixture.
- Damage: redb trusts its pages, and reading a damaged one can panic
  inside redb. `Grafeno::verify_file` runs redb's integrity check (every
  page against its checksum, repairing what it can) before a file is
  trusted; `grafeno check` runs it first. `tests/corruption.rs` damages a
  file 150 ways (flipped bytes, zeroed pages, truncation): after
  verification each file is refused or reads back exactly as before;
  nothing crashes and no answer silently differs. The server catches
  panics per request (500, pointing at `grafeno check`).
- Online backup: `ReadTx::backup` copies every table from the snapshot
  into a new file (written beside it and renamed when complete; it
  refuses to run if a table it does not know exists, so a future table
  cannot be forgotten). `grafeno backup OUT` (from a file, or from a
  running server with `--config`), `GET /v1/backup` (owners; the response
  is the file), and a button in the Studio's administration (in the
  desktop app it writes the file directly). Keys are included.

## 10.3 HNSW quality after deletions (done)

Deleting a vector
reconnected its neighbors, but quality after many changes was not
measured.

- A benchmark of recall@10 (against exact search) after deleting and
  reinserting 10%, 30% and 50% of the vectors, and after a churn of
  updates; results in `docs/benchmarks.md`.
- What it shows decides the fix: better repair on delete (reconnecting
  through the deleted node's two-hop neighborhood), and a `compact`
  command that rebuilds a namespace's graph when quality drops below a
  threshold (the check can report an estimated recall from a sample).

As built: the measurement found that inserting a vector back under the
same id (and every update, which removes and inserts) could leave a node
linked only to itself, through links left pointing at it; and that
repairs could close neighborhoods into islands and missed nodes that
linked to the removed one without being its neighbors. Inserts now never
choose the node itself; a removal repairs both its neighbors (with a
search from each, `REPAIR_EF` 16) and the nodes two hops away that
pointed at it, gives a neighbor that only the removed node reached a link
from its closest neighbor, and skips ids already removed. Recall@10 with
`ef` 64 stays at 0.99 or more through every state of the benchmark
(numbers in `docs/benchmarks.md`); deletions cost about five times more.
Vectors no search can reach are counted (`unreachable_vectors` in the
check, no longer a problem: a few appear in any changing HNSW), and
`compact_vectors` rebuilds a namespace's graph (`grafeno compact`,
`POST /v1/{ns}/compact`). `vector_recall` estimates recall from a sample
of the namespace's own vectors (`grafeno check --recall N`,
`GET /v1/{ns}/vectors/recall`). `tests/vector_churn.rs` deletes a third,
inserts it back, churns three times and compacts, checking recall and
that every reinserted vector is found.

## 10.4 Costly GQ (done)



- Adversarial tests: cartesian products, long variable-length paths,
  large UNWINDs, deep nesting, huge literals and parameters, sorts and
  collects of millions of rows; each must stop within its limits with a
  `query_limit` error and bounded memory.
- A memory budget for intermediate results (sorts, `collect`, `DISTINCT`,
  aggregates), not only a row count.
- A cost estimate in `EXPLAIN` from the planner's statistics, and an
  optional limit that refuses plans estimated above it before running.
- Fuzzing the parser and planner (random queries from the grammar):
  errors, never panics.

As built: `tests/adversarial.rs` runs 32 hostile queries on a graph of
2 000 nodes and 6 000 edges, each on a thread with a server worker's 2 MiB
stack, and requires each to end within twice its 2 s timeout. The first
run found three ways out. Loops that only compute (`reduce`, list
comprehensions, quantifiers, `UNWIND`) never looked at the clock, so a
`reduce` over a million items ran for minutes: they now count work
(`Budget::work`), checking time and cancellation every thousand items and
every 256 KB copied. Values had no size: a string doubled forty times, a
`replace` that multiplies its input or a list of lists could take every
byte of memory, so `max_memory` (512 MiB) bounds each value built (checked
before a concatenation allocates, and after list and map literals,
comprehensions and functions), and an estimate of the rows held at once,
from a sample of 32, is checked between clauses and inside `UNWIND`. And
deep nesting overflowed the stack (5 000 parentheses, 50 000 `AND`s, 20 000
`NOT`s): the parser counts the depth of the tree it builds, chains of
operators included, and stops at 200, while values stop at 128 lists or
maps deep. Each hostile query now ends with `query_limit` or an error in
at most 3 s, most in well under one.

`EXPLAIN` gains `rows` and `cost` columns, estimated from the counts the
namespace keeps (nodes and edges per label): an anchor's rows come from its
source (one for a bound node or an `@id`, a tenth of the label for an
index lookup, the label's count for a scan), each expansion multiplies by
the average degree for its edge labels (summed over the hops of a
variable-length step), a node label keeps its share of the nodes, and
conditions keep half. Loops in expressions cost their list's length when it
can be told (literals, `range` of literals) and ten otherwise; algorithms
cost what they visit (betweenness, a search from every node). The estimate
ranks an index lookup (hundreds) below a scan (thousands) below a
three-way product or ten-hop paths (tens of billions). `max_cost` refuses
a statement whose total is above it before it reads anything; it is off by
default, as the estimate is only as good as the counts.

`tests/fuzz.rs` (proptest) feeds random token sequences, valid queries with
random edits and arbitrary text to the lexer, parser, planner, `EXPLAIN`
and executor: errors, never panics. Parameters need no limit of their own:
the HTTP body limit bounds them, `serde_json` refuses nesting deeper than
128, and their use is checked like any value's.

## 10.5 Correct memory merges (done)

Merging joined two nodes when
asked.

- Identity rules: strong keys (declared unique properties such as email
  or an external id) decide; names and aliases only suggest; conflicting
  strong keys or labels block a merge.
- Contradiction rules: merging two nodes with conflicting functional
  facts (two different `born_on`) reports the conflict instead of keeping
  both silently.
- Provenance: a merge keeps where every fact came from and records itself
  (`merged_from`, by whom, when), and can be undone.
- Tests with tricky cases: namesakes, people and companies with the same
  name, nicknames, typos, translations.

As built (`crates/grafeno/src/merge.rs`): before merging, the engine lists
where the two nodes disagree, as `Conflict`s of four kinds.

- *Strong keys*: `email` and `external_id` by default (`MergeOptions.
  strong_keys`), and the key of every unique constraint on either node's
  labels. Values compare trimmed and lowercased.
- *Labels*: each node has a label the other lacks (`Person` and `Company`
  sharing `Entity`); one label more (`Person` and `Person:Engineer`) is
  fine. `Archived` is left out.
- *Properties*: the same property with different values (two `born_on`),
  names and aliases aside, as those are how duplicates differ.
- *Functional edges*: for labels declared one-at-a-time
  (`MergeOptions.functional`, such as `born_in`), different current
  targets. Ended or retracted facts do not count.

Any conflict refuses the merge with `Error::Conflict` (409 over HTTP, with
the list in `error.conflicts`); `force` merges anyway with the kept node's
values, and the report lists what it overrode. Names never decide alone: in
`find_duplicates`, strong-key and label conflicts take a pair out (two Ana
Garcías with two emails are namesakes), the same strong key proposes one
whatever the names (score 1), and each conflicting fact lowers a pair's
score by 15% and is listed in its reasons and `conflicts`. Typos are found
by filing every name word under itself and its one-letter deletions (two
words one edit apart share a key) and confirmed with an edit distance (one
edit, two from eight letters on). A changed or added last letter is not a
typo but another name (Mario and Maria, Daniel and Daniela). Nicknames and
translations are not guessed: they match through `aliases` or embeddings.

Every merge writes a `MergeRecord` to a `merges` table (layout version 8):
id, kept and merged ids, when, by whom (the transaction's actor), the
conflicts forced over, both nodes as they were (record, text, embedding,
memory state), and the edges moved and dropped. That is the provenance of
every fact the kept node gained. `merges(ns, node)` lists them (`GET
/v1/{ns}/merges?node=`), and `undo_merge` puts everything back: the merged
node with its id, text, embedding and memory, the kept node as it was, the
moved edges on the merged node, the dropped ones restored; edges added to
the kept node since stay. It refuses if the kept node changed after the
merge (those changes would be lost) unless forced, and a merge undoes once.
The record stays, marked undone. Records are not exported (they hold
copies of nodes that an import would duplicate) and go with their
namespace.

`tests/merges.rs` (engine) and `duplicates_follow_identity_rules_in_tricky_cases`
(memory) cover namesakes, the same email written differently, a person and
a company with one name, two birth dates, functional edges, typos in one
word and in a full name, gendered and suffixed names that are not typos,
a nickname and a translation through aliases, and a merge undone exactly
(names index, text index, vectors, counters and the check all agree).

## 10.6 RAM under large graphs (done)



- Benchmarks with 10 million edges and 1 million vectors, in containers
  with 512 MB and 2 GB of memory: peak and steady memory per operation.
- Caps by bytes, not entries, for the projection cache and the vector
  cache; cheaper projections (u32 positions, no edge ids unless asked).
- Measures decide the next steps (vector quantization lands in 12.3).

As built: `crates/grafeno-query/examples/ram_bench.rs` builds 1 M nodes and
10 M edges, and 1 M vectors of dimension 128, and measures each common
operation's resident and peak memory in containers of 2 GiB and 512 MiB
(numbers in `docs/benchmarks.md`). What it found and what changed:

- `Grafeno::open_with(path, &OpenOptions)` sizes the caches: redb's page
  cache (256 MiB by default, it was redb's 1 GiB), the projection cache
  (256 MiB, by bytes instead of four entries) and the vector cache of a
  write transaction (128 MiB, by bytes instead of 262 144 entries).
  `OpenOptions::with_memory(bytes)` splits one budget. The server reads
  `[memory]` in `grafeno.toml`.
- Projections are built in one pass straight into CSR arrays of `u32`
  positions (no intermediate list of edges, no hash map from ids:
  positions are found by binary search in the sorted ids), reserved up
  front. The in-edges point at the out-edges instead of copying them, and
  edge ids are kept only with `ProjectionSpec::edge_ids` (path algorithms
  ask for them). About 12 bytes per edge and 24 per node; `Step::edge` is
  now the edge's number, and `Projection::edge_id` its id.
- `check` streams: one ordered pass over each record table and each
  index, compared by fingerprints, with point lookups only to explain a
  mismatch. Memory: the node ids and the values of unique indexes.
  `rebuild_indexes` reads nodes and edges a page at a time (texts and
  vectors are still held, as they are reindexed from scratch).
- GQ: rows a procedure builds count against `max_memory` while it builds
  them, node and edge values are estimated with their properties, and the
  scoring algorithms take `top: n`.

Measured (`docs/benchmarks.md`): with 10 M edges every operation fits in
512 MiB with small caches, and stays near 300 MiB with the defaults
outside graph algorithms; `check` went from killed at 2 GiB to 249 s; a
million vectors are searched in about 5 ms with 140 MiB.

Left for later, from the numbers: aggregates that stream (counting 10 M
edges in GQ times out; 12.3); vector inserts slow down as the index
grows (each 100 000 took 96 s at first and 809 s at the million) and a
cache that keeps the most used vectors instead of starting over would
help, as would quantization (12.3); projections are rebuilt per query
when the cache is too small for them; the edge-label filter of
projections reads every adjacency entry; and duplicate detection
(`find_duplicates`) still loads every node of the namespace.

## 10.7 One writer (done)



- Measure saturation: write latency (p50, p99, p999) against offered
  load, queue depth and group sizes, with immediate and eventual
  durability, on disk.
- Metrics at `GET /metrics` (Prometheus text): queue depth, commit time,
  group size, rejected writes, query times and limits hit.
- Tuning from the numbers (group size, flush interval, separate queues for
  bulk and interactive writes so an import cannot starve an agent).

As built: the writer has two lanes. Interactive writes are grouped as
before and always served first; bulk writes run one per transaction
between groups, with eventual durability, and a bulk writer waits for
room instead of being refused. `Writer::import` reads an export in
chunks and writes each in the bulk lane, and the server imports through
it (a rebuild and a compaction go through it too): before, an import
opened its own transactions, so the writer, and every agent, waited for
the whole import, and a chunked import switched the whole database to
eventual durability while it ran. Measured (`examples/saturation.rs`,
numbers in `docs/benchmarks.md`): an agent writing 200 times a second
waited up to 12 s during a 300 000-node import, and now 88 ms at p99 with
chunks of 2 000 (the server's default, `[writer] import_chunk`).

Saturation, on a laptop SSD: with immediate durability writes stay near
2 ms up to about 4 000 a second; then groups reach `max_batch`, which
bounds throughput, so it went from 128 to 512 (from about 8 000 to about
11 000 writes a second). With eventual durability writes take about 0.15
ms up to about 8 000 a second and saturate near 16 000; the flush every
500 ms holds the writer for 10 to 25 ms, which is the p99.

The writer keeps counters and histograms (`Writer::metrics`): queue
depth per lane, writes, failures, rejections, retries, groups, writes per
group, commit time and latency per lane. `GET /v1/metrics` serves them
in the Prometheus text format with the server's own: requests by status
class and their time, 503s, GQ queries and their time, and the limits
they hit (timeout, reads, rows, memory, cost, cancelled), and the
projection cache's memory. It needs a secret like the rest of `/v1`. The
writer's options are `[writer]` in `grafeno.toml`.
