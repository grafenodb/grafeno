# Phase 16: Data model, search, analysis and events

[All phases](README.md) · [← Phase 15: Engine and interface apart](phase-15-engine-and-interface-apart.md) · [Phase 17: Integrations →](phase-17-integrations.md)

**Status:** planned, in ten steps.

What the engine still lacks as a database people build on: a free
license, null as a value, namespaces made from scripts, transactions that
span several statements, richer indexes and constraints, full-text search
beyond one ranked text per node, more capable vector indexes, a wider
catalog of graph algorithms (for analysis and for simulations), a way to
record how a query walks the graph, and change events with their values,
which stay off until an administrator turns them on. Nothing that works
today changes its behavior unless a step says so, and every such change is
listed in the change log of the release that brings it.

Steps:

- [16.1 A free license, and documentation about Grafeno only (planned)](#161-a-free-license-and-documentation-about-grafeno-only-planned)
- [16.2 Null as a stored value (planned)](#162-null-as-a-stored-value-planned)
- [16.3 Namespaces from scripts (planned)](#163-namespaces-from-scripts-planned)
- [16.4 Explicit transactions (planned)](#164-explicit-transactions-planned)
- [16.5 Advanced indexes and constraints (planned)](#165-advanced-indexes-and-constraints-planned)
- [16.6 Full-text search (planned)](#166-full-text-search-planned)
- [16.7 Better vector indexes (planned)](#167-better-vector-indexes-planned)
- [16.8 More graph algorithms, for analysis and simulation (planned)](#168-more-graph-algorithms-for-analysis-and-simulation-planned)
- [16.9 Recording how a query walks the graph (planned)](#169-recording-how-a-query-walks-the-graph-planned)
- [16.10 Change events with values, off by default (planned)](#1610-change-events-with-values-off-by-default-planned)

The owner's decisions (2026-11-10): the license is MIT or Apache-2.0, at
the user's choice; null is stored as a value; transactions become explicit
(`BEGIN`, `COMMIT`, `ROLLBACK`); the change log of 11.4 stays as it is and
the new events are off by default. The documentation names no other
company, product or game.

## 16.1 A free license, and documentation about Grafeno only (planned)

**Where it starts from.** The repository has no license file; the Linux
packages say `license = "Unspecified"` and the crates say nothing. The
documentation names other products in comparisons and in the formats it
reads and writes, and one client application by name (in the plan of the
integrations, in the history and in the READMEs).

- `LICENSE-MIT` and `LICENSE-APACHE` at the root; `license = "MIT OR
  Apache-2.0"` in the workspace and every crate, the Linux packages, the
  Windows installer's license page and the release archives; a "License"
  section in both READMEs and in `CONTRIBUTING` (contributions are under
  the same terms).
- The documentation, the phase files, the READMEs, `DESIGN.md`, the
  change log, the API's notes, help texts, error messages and code
  comments describe Grafeno on its own: formats by what they are (CSV
  with typed headers, GraphML), the installation as "a database server's",
  and no client application by name. A test fails the build when a word
  from a short deny list appears in the documentation or the sources.
- The integrations phase becomes generic (client libraries for more
  languages), as [phase 17](phase-17-integrations.md).

Done when: the license files and fields are everywhere a package carries
metadata, the deny-list test passes, and nothing else changes.

## 16.2 Null as a stored value (planned)

**Where it starts from.** `SET n.x = null`, a `null` in a merge
(`SET n += {x: null}`, `PATCH` over HTTP) and `null` in a new node's
properties all remove the property, so a property set to null and one
never set cannot be told apart.

- A property holds `null` like any other value: `SET n.x = null` keeps the
  key, `keys(n)` and `properties(n)` list it, exports carry it. `REMOVE
  n.x` (and a new `remove: ["x"]` in HTTP merges) deletes the key. Inside
  lists and maps, nulls are kept too.
- Reading a missing key still gives `null`; a new function tells them
  apart: `hasKey(n, 'x')`. Comparisons keep their three-valued logic.
- Indexes do not hold nulls (an index lookup for `= null` matches
  nothing, as the comparison does); unique indexes and constraints ignore
  them.
- The HTTP API announces the change as a capability (`null_values`), and
  a client that wants the old behavior sends the key in `remove`. The
  change log of the release says so, as a change in behavior.
- No layout change: properties are JSON, which already holds null; what
  changes is that writes stop dropping it. Replication and the cluster's
  entry format carry it as they carry any value.

Done when: GQ, HTTP, MCP, the shell, CSV and GraphML keep and show nulls;
indexes and constraints behave as above; the conformance scenarios cover
it.

## 16.3 Namespaces from scripts (planned)

**Where it starts from.** A namespace is created with `POST
/v1/namespaces` or implicitly by its first write; the command line can
only list them (`grafeno namespaces`), and GQ has `SHOW NAMESPACES` only.

- GQ: `CREATE NAMESPACE name [IF NOT EXISTS] [COMMENT 'text']`, `ALTER
  NAMESPACE name SET COMMENT 'text'`, `DROP NAMESPACE name [IF EXISTS]`,
  under the rights of 14.2 (creating needs the server's right, dropping the
  namespace's).
- Command line: `grafeno namespaces create|drop|describe`, and `grafeno
  shell -f script.gq` (and `--file` on `grafeno run`) runs a file of
  statements in order, stopping at the first error unless asked to go on,
  so a namespace with its indexes, constraints and first data can be set
  up from one script kept with an application's code.
- An example script in the documentation that creates a namespace, its
  indexes and constraints, and loads a few nodes.

Done when: the statements work in the shell, over HTTP and in scripts,
are audited (14.5) and replicated (13.x).

## 16.4 Explicit transactions (planned)

**Where it starts from.** Every statement, batch and request is one ACID
transaction: atomic (redb commits all or nothing), consistent (constraints
and indexes are checked inside it), isolated (one writer, readers see a
snapshot) and durable (immediate durability by default). But a
transaction never spans more than one request.

- `BEGIN`, `COMMIT` and `ROLLBACK` in GQ: in the shell and in scripts
  (16.3), the statements between them run in one transaction.
- Over HTTP: `POST /v1/{ns}/transactions` opens one and gives its id;
  `POST /v1/{ns}/transactions/{id}/query` runs statements in it;
  `.../commit` and `.../rollback` end it. A transaction that is idle for
  longer than its timeout (default 30 s, `[transactions]` in
  `grafeno.toml`) is rolled back. Reading transactions see one snapshot
  for their whole life; writing ones hold the writer, so the server limits
  how many wait and how long, and the metrics show them.
- In a cluster, a writing transaction runs on the leader and replicates as
  one entry at commit; a change of leader rolls it back, and the client is
  told so.
- The guarantees written down (isolation is serializable, since writes are
  one at a time) and tested: a crash before `COMMIT` leaves nothing; a
  reader never sees part of a transaction; a timeout or a dropped
  connection rolls back.

Done when: the shell, HTTP, the TypeScript client and MCP can run
multi-statement transactions, and the tests above pass, alone and in the
cluster tests.

## 16.5 Advanced indexes and constraints (planned)

**Where it starts from.** Property indexes are on one node property of
one label (value, unique or fuzzy); constraints are uniqueness (through a
unique index), one fact per source, target or pair (11.2) and functional
properties (11.2).

- Indexes on several properties (`ON (p.last, p.first)`, usable by a
  prefix of the keys), on edge properties (`FOR ()-[r:rated]-() ON
  (r.stars)`), and over several labels.
- Constraints: a property must exist (`IS NOT NULL`, which with 16.2 also
  refuses a stored null), must have a type (`IS :: INTEGER`, `STRING`,
  `FLOAT`, `BOOLEAN`, `LIST`, `MAP`, `DATETIME`), a key made of several
  properties (exists and unique together), and the same on edges.
- Declaring a constraint checks the existing data and reports what breaks
  it, refusing (or, as 11.2 does, only flagging when asked).
- `SHOW INDEXES` and `SHOW CONSTRAINTS` list every kind; `EXPLAIN` shows
  which one a plan uses; the planner's statistics (12.2) cover them.
- A layout change if the index entries need a new table; with it, a
  fixture and the upgrade test.

Done when: each kind is declared, enforced on every write path (GQ,
HTTP, batches, imports, replication), listed, used by plans, and measured
on large graphs.

## 16.6 Full-text search (planned)

**Where it starts from.** Each node has at most one indexed text, ranked
with BM25 (`db.search`, `GET /search`); recall fuses it with vector search
(RRF) and graph expansion.

- Full-text indexes over chosen properties of a label (`CREATE FULLTEXT
  INDEX notes FOR (n:Note) ON EACH [n.title, n.body]`), alongside the node
  text, with a weight per field.
- Queries with phrases (`"exact words"`), required and excluded terms
  (`+`, `-`), `AND` and `OR`, prefixes (`graph*`), terms within a distance
  of each other, and fuzzy terms (`graf~1`); analyzers per index (accents,
  stop words and stemming for English and Spanish).
- Results with their score and the fragments that matched.
- Hybrid search where the caller says how: `mode: "text" | "vector" |
  "hybrid"`, and for hybrid either rank fusion with its constant or
  weighted scores; in GQ (`db.search(…, {mode: …})`), HTTP, MCP and
  recall.
- Measured: quality on a labelled set (precision at 10) and speed against
  today's search.

## 16.7 Better vector indexes (planned)

**Where it starts from.** One HNSW graph per namespace over the node
embedding, with one dimension and cosine similarity; int8 copies with
exact rescoring (12.5).

- Named vector indexes, several per namespace, on a label and a property
  (`CREATE VECTOR INDEX docs FOR (d:Doc) ON (d.embedding) OPTIONS
  {dimension: 768, similarity: 'cosine' | 'dot' | 'euclidean'}`), each
  with its own HNSW parameters (`m`, `ef_construction`, `ef_search`).
- Filtered search: a predicate on labels and properties applied during
  the graph walk, not after it, so a selective filter still returns `k`
  results.
- Search by several vectors at once, and by a node's own vector
  (`db.similar(node)`).
- Building an index in the background over existing nodes, with
  progress, while writes continue.
- Measured: recall at 10 and latency per similarity and filter
  selectivity, against 10.3's and 12.5's numbers.

## 16.8 More graph algorithms, for analysis and simulation (planned)

**Where it starts from.** `CALL algo.*` offers paths (shortest, A*, k
shortest, all shortest), centrality (degree, PageRank, betweenness,
closeness), communities (WCC, SCC, label propagation, Louvain, triangles),
similarity, link prediction and FastRP. None of these changes.

- Analysis: k-core decomposition, Leiden, HITS, eigenvector and harmonic
  centrality, articulation points and bridges, minimum spanning tree,
  maximum flow and minimum cut, bipartite matching, topological order and
  cycles, random walks, local clustering coefficient, conductance.
- Simulation on graphs (worlds made of places, agents and resources):
  - distance fields from many sources at once, with weights, for agents
    that move toward the nearest goal;
  - influence maps: values spreading from sources with decay and limits;
  - flood fill of the region that satisfies a condition;
  - diffusion of a quantity along edges over steps (heat, rumor,
    resources), with capacities;
  - step-by-step rules applied to every node from its neighbors' state;
  - assignment of agents to tasks at least total cost, and the flow of
    resources along a network at least cost;
  - weighted random walks for agents, reproducible with a seed.
- Each algorithm in modes that stream rows, report statistics, or write
  results back to properties (`{write: 'property'}`), under the query
  budget and the rights of 14.4, with an `EXPLAIN` cost.
- Tests against known results on small graphs; time and memory on the
  10.6 graphs.

## 16.9 Recording how a query walks the graph (planned)

**Where it starts from.** `PROFILE` reports rows, reads and time per step
of a plan, but not which nodes and edges were visited, nor in what order.

- `TRACE <statement>` (and `trace: true` over HTTP, `\trace` in the
  shell) runs the statement and records its walk: the anchors found, each
  edge followed with its two ends, nodes kept and dropped by each filter
  (and which condition dropped them), rows produced, in order, with the
  step of the plan each event belongs to.
- Bounded: a maximum of events (and bytes); past it the trace says it
  was cut, and the query still finishes.
- Returned with the result as a list of events that a client can replay
  step by step on a drawing of the graph; the documented event format is
  part of the API.
- Off unless asked: no cost for queries that are not traced (measured).

## 16.10 Change events with values, off by default (planned)

**Where it starts from.** The change log (11.4) records which records
changed, not their values; it is on by default and feeds streams,
webhooks and background work.

- Event subscriptions per namespace (`CREATE EVENT SUBSCRIPTION name ON
  NODES | EDGES | PROPERTIES | TEXT | VECTORS [WHERE …] DELIVER TO
  STREAM | WEBHOOK 'url'`), whose events carry the values before and after
  (for vectors, the new vector or only its identity, as chosen), and which
  properties changed.
- Off by default: nothing is recorded until `[events] enabled = true` in
  `grafeno.toml` and an administrator creates a subscription (a privilege
  of its own, 14.2); then only what subscriptions ask for is kept.
- Delivery with the change log's guarantees (order per namespace,
  retries, signatures, a window, a gap when a consumer falls behind), from
  the leader in a cluster.
- Measured: write cost with events off (none), on with one subscription,
  and on with values of large vectors.

Done when: events are off on a new install, a subscription delivers the
values it asks for over streams and webhooks, and the change log still
works as before.
