# Phase 12: Compatibility and speed

[All phases](README.md) · [← Phase 11: Memory that keeps itself](phase-11-memory-that-keeps-itself.md) · [Phase 13: Distributed operation →](phase-13-distributed-operation.md)

**Status:** done, in five steps.

A wider query language and tool compatibility, query optimization under concurrency, batched execution, batched and parallel traversal, compact storage and vector quantization.

Phases 10 to 12 were planned together: their guidelines are in [Phase 10](phase-10-hardening.md).

Steps:

- [12.1 A wider query language and tool compatibility (done, except the two items named below)](#121-a-wider-query-language-and-tool-compatibility-done-except-the-two-items-named-below)
- [12.2 Query optimization and performance under concurrency (done)](#122-query-optimization-and-performance-under-concurrency-done)
- [12.3 Batched execution (done)](#123-batched-execution-done)
- [12.4 Batched and parallel traversal (done)](#124-batched-and-parallel-traversal-done)
- [12.5 Compact storage and vector quantization (done, without the compressed postings and adjacency blocks; see below)](#125-compact-storage-and-vector-quantization-done-without-the-compressed-postings-and-adjacency-blocks-see-below)

## 12.1 A wider query language and tool compatibility (done, except the two items named below)



- The rest of a common graph query language: path variables (`p = (…)`,
  `nodes(p)`, `relationships(p)`, `shortestPath`), `CALL { … }`
  subqueries, `EXISTS { … }` and `COUNT { … }`, pattern comprehensions,
  map projections, `=~`, more list, string and math functions; a
  conformance suite run in CI.
- Tools: import and export in CSV and GraphML; a binary wire protocol for
  drivers of other languages, if the conformance suite is far enough
  along.

As built:

- Paths are a value (`Value::Path`, nodes and edges in written order):
  `p = (…)`, `nodes(p)`, `relationships(p)`, `length(p)`. A match keeps the
  edges of each step, oriented left to right whichever end it started
  from, only used when the pattern names a path. `shortestPath` and
  `allShortestPaths` take one edge pattern and use the breadth-first search
  variable-length edges already use, so `allShortestPaths` gives one path
  per end.
- `CALL { … }` runs once per row, which it sees whole (no need for `WITH x`,
  which also works); what its RETURN gives multiplies the row, and without
  RETURN (writes) the row stays. A name it returns may not exist outside.
  It is one transaction: `IN TRANSACTIONS` and `UNION` inside are refused.
- `COUNT { … }` and pattern comprehensions share `pattern_rows` with
  `EXISTS { … }`; map projections are `n {.a, .*, k: v, var}`; `=~` matches
  the whole string with the `regex` crate, whose time is linear in the text,
  with compiled size bounded and expressions cached per thread. New
  functions are in `morefns.rs`: `reverse`, `tail`, `isEmpty`, `toBoolean`,
  `to…OrNull`, `to…List`, `ltrim`, `rtrim`, `left`, `right`, `char_length`,
  `nullIf`, `asin`, `acos`, `atan`, `atan2`, `cot`, `degrees`, `radians`,
  `haversin`, `isNaN`, `rand`, `randomUUID`, `elementId`.
- Nested `CALL { }` and `FOREACH` count against the parser's depth limit;
  all the new forms run under the query budget
  (`tests/adversarial.rs`).
- `crates/grafeno/src/interchange.rs`: CSV with typed headers (`id:ID`,
  `:LABEL`, `name:type`, `:TEXT`; `:START_ID`, `:END_ID`, `:TYPE`,
  `valid_from`, `valid_to`) and GraphML (node labels in a `labels` key,
  edge labels in `label`).
  Exports hold current knowledge: retracted facts stay out. Imports keep
  Grafeno ids, so a re-import updates; other ids map to new nodes,
  remembered per namespace (state kind `import.id`), so a later edges file
  finds them. Bad rows are skipped and reported. `grafeno export` and
  `grafeno import` take `--format csv|graphml`; the server has
  `/v1/{ns}/export.graphml`, `/export/nodes.csv`, `/export/edges.csv` and
  the same under `/import/` (admin role).
- Conformance: `tests/conformance/scenarios.txt` holds 146 scenarios
  (a graph, a query, the expected result), run by `cargo test`. They are
  written by hand from the language's semantics, so they guard against
  regressions rather than measure conformance to an outside standard;
  five scenarios pin choices GQ makes on purpose.
- **Not built: a binary wire protocol.** It depended on the conformance
  suite being far enough along, and a hand-written suite gives no number
  to judge that by.
- Measured (`examples/interchange_bench.rs`, 200 000 records): CSV exports
  at 397 000 records a second and imports at 27 600; GraphML at 341 000 and
  25 900.

## 12.2 Query optimization and performance under concurrency (done)



- Statistics for the planner (value histograms per index, label and edge
  label counts by direction) and join ordering for patterns with several
  parts; plan caching for repeated queries with parameters.
- Parallel scans for large read queries, and read throughput measured
  with many concurrent readers and a busy writer.

As built:

- `crates/grafeno/src/planstats.rs`: `PlannerStats` per namespace. Per
  value index: entries, distinct values, the 64 most common values with
  their counts, and 64 equal-depth buckets (encoded bounds, so a range is
  estimated by the buckets it covers, half for one it crosses); an
  equality with a value outside the common list gets the remaining entries
  over the remaining distinct values, and nothing when the list holds
  every value. Per edge label: edges, distinct sources and distinct
  targets, so the average degree out and in (one employer per person, 200
  people per company). Two passes over each index and one over each
  adjacency table; 0.3 s for 200 000 people, 4 indexes and 201 000 edges.
- They live in memory, in the `Shared` state every transaction of a
  database holds, and are not stored (no layout change). `ReadTx::
  planner_stats` builds them from the snapshot the first time and again
  when the namespace drifts (nodes or edges off by a tenth and at least
  100, an index created or dropped); while one reader rebuilds, the others
  plan with the previous ones. Statements that write use only what is in
  memory (`cached_planner_stats`), so a build never runs under the write
  lock. `ReadTx::analyze` and `CALL db.analyze()` rebuild them at once.
  An index's estimate is scaled by how much its label grew since.
- `plan.rs`: a pattern starts from a bound node or an `@id` as before;
  otherwise every node's best source (index, label, scan) is estimated in
  rows, with the values the query gives (literals and parameters while
  planning; at run time, the row's values too), and the whole pattern is
  costed from each node: candidates read, then rows × degree at each step
  (by direction, from the statistics) × the share of targets that fit. The
  cheapest wins. `EXPLAIN` shows the estimate (`~2 080 nodes`), and
  `cost.rs` uses the same sources and degrees.
- Join ordering: `order_patterns` runs the patterns of one `MATCH`
  greedily, a pattern sharing a bound variable first, otherwise the
  cheapest. Rows are the same (edge uniqueness was already per pattern);
  their order without `ORDER BY` can change.
- Plan caching: `cache.rs` keeps 1 024 parsed statements by text (least
  recently used out, an eighth at a time; texts over 16 KiB are not kept;
  syntax errors are not kept). Plans are not cached: anchors are chosen
  per run from current statistics, so a cached statement never plans with
  a dropped index or stale counts. `statement_cache_stats()` reports hits.
- Parallel scans: `parallel.rs`. A read-only match whose start is a label
  or an index lookup estimated at 4 096 nodes or more, with no row cap from
  a `LIMIT`, splits the candidate ids into chunks (eight per thread, at
  least 512 ids) that threads take in turn; each chunk's rows return in
  chunk order, so rows and their order equal one thread's. Threads share
  the snapshot (`ReadTx` is `Sync`) and the `Budget`, whose counters are
  now atomic, so reads, time and cancellation count across them. At most
  one thread per CPU works at once over the whole process
  (`set_parallelism`, `scan_threads` in `[query]`); a query that gets none
  runs alone, and work inside a worker never splits again.
- Tests: `grafeno-query/tests/planner.rs` (statistics against known
  distributions and degrees, `db.analyze`, starts by value distribution
  and parameters, a range against a small label, join order, statistics
  rebuilt after drift and new or dropped indexes, the cache with
  parameters, parallel rows equal to serial and limits across threads).
- Measured (`examples/planner_bench.rs`, 200 000 people, against a build of
  12.1 in the same session, alternating): two equalities with the rare
  one written second 354.7 ms → 11.1 ms; a range holding every person
  against a label of 1 000 companies 1 770 ms → 2.7 ms; the big pattern
  written first 2 593 ms → 1.8 ms; 50 000 point queries with a parameter
  17 682 → 18 616 a second; a scan of 200 000 people 743 ms → 161 ms on 16
  threads (706 ms on one). Reads with many readers did not change, as the
  query measured (a point lookup and two hops, about 200 rows) already
  started from its index: best of three runs, 1 reader 1 259 queries a
  second, 16 readers 9 168 (8.5 ms p99 at most), and 7 960 with a busy
  writer committing 10 500 nodes a second meanwhile.

## 12.3 Batched execution (done)



- Execution in batches of rows with column vectors instead of one row at
  a time, for scans, filters and aggregates.

(Vector quantization and compressed postings and adjacency lists, first
planned here, moved to 12.5: the adjacency format is better designed with
12.4's batched traversal, which is what reads it.)

As built:

- `crates/grafeno-query/src/aggregate.rs`. A `MATCH` followed by a
  (non-optional) `RETURN` or `WITH` with aggregates no longer hands its
  rows on: `match_pattern_into` emits them in batches of up to 1 024 to a
  consumer, and the runner folds each batch into the groups. A batch is
  first turned into columns (each row's group key, and one vector per
  aggregate argument) by `Spec::columns`, which reads only the rows, so a
  parallel scan computes them on its worker threads; `Groups::fold` then
  folds the columns in row order. Every other aggregation (after `UNWIND`,
  `WITH`, a procedure) folds its held rows the same way, so there is one
  code path; `Scope::Group` is gone, replaced by `Scope::Folded` (a group's
  first row and its folded aggregates).
- Accumulators: a counter for `count` (with a set of keys for `DISTINCT`),
  running totals for `sum` and `avg` that keep the row-at-a-time results
  exactly (integers checked as they add, so an overflow on the way is still
  an error; floats added in row order; durations apart), the best value for
  `min` and `max` (the first of equal minimums and the last of equal
  maximums, as `min_by` and `max_by` gave), and the values themselves only
  for `collect`, `median`, percentiles, `stdev`, `variance` and `mode`, which
  then reduce as before (`eval::reduce`). Groups keep the order they first
  appeared in.
- `parallel::map_chunks` hands each chunk's result on in chunk order as
  soon as the chunks before it are done (an ordered delivery under a lock)
  instead of joining everything at the end, so a parallel scan feeds the
  fold while it runs, and the results (floating-point sums included) equal
  one thread's. A fold holds at most a batch; `max_rows` still counts every
  row a match returns whole (the collecting path checks the running total).
- Filters still run row by row, inside the match; scans and filters gain
  from building no row maps for the aggregation and from not holding them.
  Expansion is still one anchor at a time: that is 12.4.
- Tests: `grafeno-query/tests/batch.rs` (every aggregate against values
  worked out by hand, `DISTINCT`, nested aggregates, ties of `min` and
  `max`, empty groups, overflow and type errors, durations, `WITH … WHERE`,
  `*`, `ORDER BY` an aggregate, PROFILE rows, 90 000 rows counted under a
  `max_rows` of 2 000, parallel folds equal to serial ones).
- Measured (`examples/batch_bench.rs`, 200 000 people, against 12.2 in the
  same session, best of two runs each): counting a label 291 → 204 ms;
  grouping by a property 609 → 233 ms; a filter and four aggregates 479 ->
  162 ms; one hop grouped by the far end 989 → 650 ms; `collect` per group
  54 → 23 ms; two hops counting 1 990 000 rows 6 309 → 4 825 ms, and with
  the standard limits it now completes instead of failing on `max_rows`.

## 12.4 Batched and parallel traversal (done)



As built:

- `crates/grafeno-query/src/traverse.rs`. A pattern grows from batches of
  64 anchors (one at a time under a row cap, so a `LIMIT` reads no more
  than it needs), step by step: the partial matches of a batch are a
  frontier, and `step` reads the distinct nodes it stands on once, sorted
  by id, then extends every partial match in order. What a step reads is
  kept for the rest of the traversal from that input row: each node's
  edges for that step (already filtered by label, direction and time),
  each target node decoded once, each edge record once, each node's reach
  for a variable-length step (a breadth-first search per source, run once
  however many partial matches stand there).
- The caches are shared by every thread working on the traversal: maps in
  32 shards behind read-write locks, bounded (2^20 edges listed per step,
  2^18 nodes and edge records) and emptied past the bound; a read that
  misses after that reads again. At first each chunk of a parallel scan had
  caches of its own, and nothing was shared: 512 consecutive uids work at
  512 different companies. Shared, the reads of the two-hop count fell from
  4.03 M to 1.12 M.
- Parallel traversal: a frontier over 4 096 partial matches with steps
  left goes on in slices of that size; in a read-only query without a row
  cap, `parallel::map_chunks` (now generic over what it splits) spreads the
  slices over the shared threads and joins their rows in slice order. This
  is what lets one company, or a thousand, with hundreds of employees each
  use every core, where 12.2 only split large sets of anchors.
- Rows and their order equal growing one anchor at a time: each partial
  match's branches stay together and in order, and slices are joined in
  order. `tests/traversal.rs` checks this against the row-capped path
  (which grows one anchor at a time) on eight pattern shapes, and parallel
  against one thread.
- Found on the way, in 12.2's planner: a node whose inline properties read
  another variable of the pattern (`(q {team: p.team})`) could be chosen to
  start it, and failed with an unknown variable. Such nodes no longer start
  a pattern while another node can.
- Not built from the plan: partial matches are still row maps, not
  columns (rows built only at the end), and variable-length steps search
  per source rather than 64 sources per machine word. Rows returned whole
  share nothing between branches and gain nothing (1.5 s either way for
  199 000 rows); the gains are where branches meet.
- Measured (`examples/traversal_bench.rs`, 200 000 people, 1 000 companies,
  5 acquaintances each, against 12.3 in the same session, best of several
  runs): colleagues of 10 000 people counted 5.6 → 2.7 s (reads 4.03 M →
  1.12 M); two hops from every company grouped 9.5 → 2.9 s; reach within 3
  hops of 2 000 people 2.3 → 2.0 s; friends of friends of 20 000 people
  unchanged at 1.1 to 1.2 s, though with 42% fewer reads.

The research that led here: today a match grows from one anchor at a
time: each candidate expands step by step (its partial matches breadth
first, but alone), so 200 000 people who work at 1 000 companies read
those companies' adjacency 200 000 times, decode each company again for
every person, and jump around the adjacency B-tree in id order of the
people rather than of the nodes read. The plan, in two parts:

- **Batched traversal with columnar frontiers.** Take the candidates of a
  start in batches (a thousand or so) and expand the whole batch one step
  at a time, level by level, as a breadth-first search from many sources
  does. The partial matches of a batch are columns (the node at each
  position, the edges of each step, the input row they came from), not
  one row map each. At each step: collect the distinct nodes the frontier
  stands on, sort them by id (adjacency keys are `(ns, node, label,
  edge)`, so reads become a forward walk through the B-tree), read each
  one's adjacency once, filter edges by label, direction and time once per
  node, and join the result back to the partial matches that stand there
  (a hash join from node id to its edges). Targets are decoded once per
  batch, and their labels and properties checked once, however many
  partial matches reach them. Rows are built only at the end, or never:
  12.3's folding consumes the columns directly. Variable-length steps and
  `shortestPath` become level-synchronous searches from a batch of sources
  with a visited bitmap per source (64 sources per machine word, as
  multi-source BFS does), instead of one search per start.
- **Parallel traversal.** When a level's frontier is large enough (the
  same kind of threshold as 12.2's parallel scans, measured), split its
  distinct nodes across the shared threads; each thread reads and filters
  its share and produces its part of the next frontier, and the parts are
  concatenated in frontier order, so the rows and their order stay those
  of one thread, as in 12.2. Searches from many sources split by source
  batch. All threads share the snapshot and the query's budget.
- Measures: one and two hops grouped by the far end, friends of friends,
  variable-length reach and shortest paths from many sources, against
  12.3; results equal to the row-at-a-time matcher (kept for patterns the
  batched one does not cover, such as path variables at first).

## 12.5 Compact storage and vector quantization (done, without the compressed postings and adjacency blocks; see below)



- Vector quantization (int8, with exact rescoring of the best
  candidates), compressed postings and adjacency lists (delta-encoded ids
  in blocks per node, read whole by 12.4's traversal); memory and speed
  compared with 10.6's numbers. A new layout version, with its upgrade and
  fixture.

As built (layout version 13):

- `vectors_q8`: every vector also kept as int8, its largest absolute value
  (f32) then one signed byte per dimension, that value scaled to 127
  (`vector::quantize`): 4 + d bytes instead of 4·d. Searches walk the HNSW
  graph with these (`dot_q8`, computed from the stored bytes), then rescore
  the `ef` candidates found with the exact vectors and return the best
  `limit`, so the scores returned are exact. Inserts build the graph with
  the exact vectors, as before. The copies follow every write: insert,
  remove, compaction, rebuild, namespace drop and backup.
- `adjacency_out` and `adjacency_in` keep their keys; their values are a
  compact encoding (`store::Adj`, a `redb::Value` whose in-memory type is
  still the tuple, so no reader changed): a byte of flags for which of
  `expired_at`, `valid_from` and `valid_to` are set, the other end (16
  bytes), then `recorded_at` and the set bounds as zigzag varints. A plain
  fact's value takes 25 bytes instead of 48.
- Upgrade: a file of layouts 3 to 12 has each adjacency table renamed,
  copied entry by entry into the new type under its name, and dropped
  (one transaction, streaming, nothing held in memory); every vector gets
  its int8 copy. `tests/fixtures/layout-v12.redb.gz` is new (layout 12 had
  no fixture), so every past layout is upgraded by `upgrade.rs`.
- Measured (`examples/storage_bench.rs`, a file with 100 000 nodes, 300 000
  edges, a third with valid-time ranges, and 50 000 clustered vectors of
  128 dimensions; against 12.4 in the same session): the adjacency tables
  store 26.3 → 21.3 MiB each (19% less; 14% with B-tree metadata); the
  int8 copies add 7.8 MiB next to the 26.2 MiB of exact vectors; the file
  stays at 514 MiB (redb grows it in large steps); recall@10 at `ef` 64 is
  0.996 both ways and the median search 1.35 against 1.40 ms; reading every
  node's edges is no faster (1.25 to 1.40 s before, 1.42 to 1.47 s after:
  decoding varints costs about what the smaller entries save in memory).
- What this buys: with everything in memory, little. A search touches a
  quarter of the vector bytes (plus `ef` exact ones), which pays when the
  vectors do not fit in the page cache. Measured in E2 on 10.6's file of
  1 M vectors of 128 dimensions (2.06 GB), copied twice and opened by each
  build (`ram_bench search`, 1 000 searches per round, two rounds per run,
  runs alternating): in 512 MiB with a 128 MiB page cache, 4.9 to 5.2 ms a
  search before and 3.5 to 4.2 ms after; in 2 GiB with 256 MiB, 5.0 to 6.0
  ms before and 3.2 to 3.7 ms after; resident memory the same (140 and 272
  MiB). Upgrading that file to layout 13 took 4.5 s (the int8 copies of 1 M
  vectors, about 126 MB, fitting in its free pages: the file kept its
  size).
- Not built: compressed postings and adjacency blocks (delta-encoded ids
  per node or per term, read whole). Both replace the one-entry-per-edge
  and one-entry-per-posting tables with blocks rewritten on every change,
  which redesigns the text index, the writes, `check` and `rebuild`; with
  the value encoding above the adjacency is 14% smaller without it.
