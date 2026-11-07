# Phase 8: Performance, query language and analysis

[All phases](README.md) · [← Phase 7: Memory](phase-07-memory.md) · [Phase 9: Desktop application →](phase-09-desktop-application.md)

**Status:** done, in five steps.

.

Steps:

- [8.1 GQ completeness (done)](#81-gq-completeness-done)
- [8.2 Statistics and time (done)](#82-statistics-and-time-done)
- [8.3 Property indexes and safe queries (done)](#83-property-indexes-and-safe-queries-done)
- [8.4 Concurrent and bulk work (done, except the shared read caches)](#84-concurrent-and-bulk-work-done-except-the-shared-read-caches)
- [8.5 Graph algorithms (done)](#85-graph-algorithms-done)

Each step ends with tests, benchmarks where they apply, documentation and
its own commit.

## 8.1 GQ completeness (done)

The executor became a pipeline of clauses
(rows flow from one clause to the next), which the remaining Cypher clauses
need. Reads in a writing statement go through its write transaction, so
later clauses (and `MERGE` on later rows) see earlier writes.

- `OPTIONAL MATCH` (missing parts are `null`), `WITH` (projection and
  aggregation between steps; `WITH … WHERE` filters aggregates, like SQL's
  `HAVING`), `UNWIND`, `UNION` and `UNION ALL`.
- `CASE` (simple and searched), list comprehensions
  (`[x IN list WHERE … | …]`), pattern predicates in `WHERE`
  (`WHERE (a)-[:knows]->(b)`, `WHERE NOT (a)--()`, `EXISTS { … }`).
- `MERGE` (match or create, with `ON CREATE SET` and `ON MATCH SET`) and
  `FOREACH` (writes per list item).
- `CALL procedure(…) YIELD …`: built-in procedures (`db.labels()`,
  `db.relationshipTypes()`, `db.propertyKeys()`, later the algorithms).
- `EXPLAIN` and `PROFILE`: the plan, and rows and time per step.
- Also: `all`, `any`, `none`, `single`, `reduce`, list slices, `^`,
  and `db.search` / `db.searchVector` procedures. Left for later:
  `CALL { … }` subqueries, pattern comprehensions, map projections.

## 8.2 Statistics and time (done)

Durations live in `grafeno-query`'s
`temporal` module, on the engine's own calendar helpers (no date library).

- Aggregates: `stdev`, `stdevp`, `variance`, `median`, `percentileCont`,
  `percentileDisc`, `mode`, `count(DISTINCT …)` for all of them.
- A duration type (ISO 8601, such as `P1Y2M10D`, `PT4H`): date and time
  arithmetic (`date + duration`, `b - a`), `duration.between(a, b)`,
  `duration.inDays(a, b)`, components (`d.days`, `d.hours`).
- Date parts and truncation: `t.year`, `t.month`, `t.week`, `t.dayOfWeek`,
  `date.truncate('month', t)`, for grouping by period.
- Valid time in numbers: how long a fact held
  (`duration.between(r.@valid_from, coalesce(r.@valid_to, now()))`), and
  `CALL temporal.series(start, end, step)`, to count what was valid at each
  point of a period in one query.
- Studio: bar and line charts over script results, one shared Y axis,
  categorical colors in a fixed, colorblind-validated order (no pie charts:
  they compare close values badly).
- Also: `VALID AT` / `KNOWN AT` may use row variables, `round(x, digits)`,
  `log`, `exp`, `sign`, `pi()`, `timestamp()`.

## 8.3 Property indexes and safe queries (done)

Measured in
[docs/benchmarks.md](../benchmarks.md): a 2% range over 100,000 nodes goes
from 233 ms to 11 ms. Values are encoded so that byte order is value order
(numbers as `f64` with flipped bits, strings as UTF-8), and lookups read
inclusive ranges that the query's own conditions then narrow. Queries read
candidates in pages, and limits are checked on every read.

- Declared indexes, as in Postgres and Neo4j:
  `CREATE INDEX FOR (n:Person) ON (n.age)`, `DROP INDEX`, `SHOW INDEXES`,
  also from the Studio. An index table keyed by `(ns, index, sortable
  value, node)` (layout version 5) serves equality, ranges,
  `STARTS WITH` and `IN`; it is kept in the write transaction and checked
  and rebuilt like the other indexes. Uniqueness constraints build on it.
- A planner: starts a pattern from its most selective part (an index, an
  id, the smallest label by its counter), pushes `WHERE` filters into the
  scan, and stops scanning as soon as `LIMIT` is met.
- Guards, so one query cannot take the service down: a time limit, a
  limit on rows read and on rows held in memory (sorts, aggregates,
  `collect`), and cancellation when the client goes away. Defaults in
  `grafeno.toml`; a query over its limits fails with a clear error and
  leaves the server serving others.

## 8.4 Concurrent and bulk work (done, except the shared read caches)


redb has one writer and any number of readers (MVCC); readers already run
in parallel. Numbers are in [docs/benchmarks.md](../benchmarks.md).

- **Group commit** (`grafeno::Writer`): a thread takes the writes waiting
  in its queue (up to 128), runs them in arrival order in one transaction
  and commits once. redb allows a savepoint only before a transaction
  touches a table, so a write cannot be undone alone: when one fails, the
  transaction is discarded, that write gets its error, and the others run
  again. Writes are therefore `Fn` (safe to run twice; only the last run
  counts). The server sends every HTTP write through it, GQ writes
  included (`grafeno_query::execute_in`). 16 threads of single-node
  writes on a file: 740 writes/s alone, 8,100 grouped.
- **Durability** (`durability = "eventual"` in `grafeno.toml`,
  `Grafeno::set_durability`): commits skip the disk sync; the writer
  flushes twice a second and on shutdown. A crash loses at most the last
  half second, never part of a commit.
- **Bulk loading** (`grafeno import --bulk`, `POST /v1/import?bulk=true`,
  `Grafeno::import_with`): chunks of 20,000 records with eventual
  durability and one flush, texts indexed per chunk with sorted postings
  (`WriteTx::set_texts`). About 1.3 times faster than one transaction; an
  error keeps the chunks before it. The text postings stay the main cost.
- **Backpressure**: at most `max_requests` (512) requests under `/v1` at
  once, and 4,096 writes waiting; beyond them the server answers 503
  `busy` with `Retry-After: 1`.
- **Not done: shared read caches.** Keeping decoded vectors across
  transactions needs to know, for each snapshot, whether a cached vector
  was written after it; redb does not expose snapshot versions, and the
  measured cost (about 1 ms per vector search at 10k vectors) did not
  justify a second, hand-kept version scheme. It stays in the remaining
  limits.

## 8.5 Graph algorithms (done)

On disk, adjacency is already index-free: a
node's edges by label are one contiguous key range, and each entry holds
the neighbor's id and the edge's times. Algorithms that touch the whole
graph work on an in-memory projection instead: a compact adjacency array
(CSR, each node holding the positions of its neighbors) of a namespace or
part of it (labels, edge labels, `VALID AT`), built on demand and reused
while the data does not change.

- Pathfinding: shortest paths weighted by a property (Dijkstra), A*, the
  k shortest paths (Yen), all shortest paths.
- Centrality: degree, PageRank, betweenness, closeness.
- Communities: connected components (weak and strong), label propagation,
  Louvain.
- Similarity: node similarity by shared neighbors (Jaccard, overlap,
  cosine) and k nearest neighbors by embedding.
- Link prediction: common neighbors, Adamic-Adar, resource allocation,
  preferential attachment.
- Node embeddings: FastRP, whose vectors can be written back as node
  embeddings and searched with the vector index.
- Each runs as `CALL algo.<name>(…) YIELD …` in GQ, through HTTP, and from
  the Studio (color by community, size by centrality). Modes: stream the
  results, return statistics, or write them to a property.

As built: `grafeno::algo` holds the projection and the algorithms, which
work on node positions and know nothing of storage. A projection is built
with one range scan of the outgoing adjacency of the namespace (edge
records are read only for a weight property) and keeps both directions as
CSR arrays. The projection cache lives in the database: every commit bumps
a counter twice (odd while committing), and a read transaction records the
counter it saw if no commit ran while it opened, so it can reuse a
projection built for the same snapshot; they are kept up to a budget in
bytes (256 MiB by default, see 10.6). Write
transactions project their own state and never cache. In GQ, statistics
and writing need no special modes: rows are aggregated with `RETURN` or
stored with `SET`, and `POST /v1/{ns}/algo/{name}` with `write` builds that
query. Betweenness, closeness and FastRP spread over the cores; the
others run on one. Every algorithm checks the query's time limit and
cancellation between rounds.

A performance pass (layout version 3) followed phase 4: traversals 3-4×
faster, BM25 3× faster, vector inserts 2.5× faster in batches. Numbers and
the remaining limits are in [docs/benchmarks.md](../benchmarks.md).
