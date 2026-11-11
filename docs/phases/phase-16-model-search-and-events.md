# Phase 16: Data model, search, analysis and events

[All phases](README.md) · [← Phase 15: Engine and interface apart](phase-15-engine-and-interface-apart.md) · [Phase 17: Integrations →](phase-17-integrations.md)

**Status:** in progress: 16.1 to 16.8 done.

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

- [16.1 A free license, and documentation about Grafeno only (done)](#161-a-free-license-and-documentation-about-grafeno-only-done)
- [16.2 Null as a stored value (done)](#162-null-as-a-stored-value-done)
- [16.3 Namespaces from scripts (done)](#163-namespaces-from-scripts-done)
- [16.4 Explicit transactions (done)](#164-explicit-transactions-done)
- [16.5 Advanced indexes and constraints (done)](#165-advanced-indexes-and-constraints-done)
- [16.6 Full-text search (done)](#166-full-text-search-done)
- [16.7 Better vector indexes (done)](#167-better-vector-indexes-done)
- [16.8 More graph algorithms, for analysis and simulation (done)](#168-more-graph-algorithms-for-analysis-and-simulation-done)
- [16.9 Recording how a query walks the graph (planned)](#169-recording-how-a-query-walks-the-graph-planned)
- [16.10 Change events with values, off by default (planned)](#1610-change-events-with-values-off-by-default-planned)

The owner's decisions (2026-11-10): the license is MIT or Apache-2.0, at
the user's choice; null is stored as a value; transactions become explicit
(`BEGIN`, `COMMIT`, `ROLLBACK`); the change log of 11.4 stays as it is and
the new events are off by default. The documentation names no other
company, product or game.

## 16.1 A free license, and documentation about Grafeno only (done)

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

As built:

- `LICENSE-MIT` (copyright "The Grafeno authors") and `LICENSE-APACHE`;
  `license = "MIT OR Apache-2.0"` and `repository` in `[workspace.package]`,
  inherited by every crate; the `.deb` installs both texts in
  `/usr/share/doc/grafeno`, the `.rpm` in `/usr/share/licenses/grafeno`;
  the `.tar.gz` and the Windows zip carry them; the Windows setup shows a
  license page (`packaging/windows/license.txt`) and installs both texts;
  the winget manifest and the TypeScript client say the same. "License"
  sections in both READMEs and both CONTRIBUTING files.
- The owner's choices on naming (2026-11-10): the query language is
  called GQ only, described as Grafeno's declarative graph query
  language, and the documentation's "Differences" section became "Choices
  to know", stating GQ's own semantics; the compatibility aliases of the
  text-similarity functions (five names outside the `text.` prefix) were
  removed (a change listed in `CHANGELOG.md`). Two test files were renamed
  `language.rs` and `language_more.rs`; the conformance scenarios' choices
  are named `choice: …`.
- About 140 mentions rewritten in the READMEs, `DESIGN.md`, the change
  log, the guides, the phase files, the server's README (the MCP section
  shows a client configuration without naming a client), help texts, the
  MCP tool descriptions, comments and test names. Kept: operating systems
  and distributions (the installation guide needs them, "Oracle Linux"
  included), standards (SQL, GraphML, CSV, MCP, Raft) and Grafeno's own
  dependencies.
- `crates/grafeno/tests/names.rs` walks every text file of the repository
  (not `target`, `node_modules`, `dist` or `.git`) and fails on a short
  deny list of names, matched without case at word boundaries, with the
  platform exception above; a second test checks the matching itself.
- Not covered by the test: the separate documentation site, whose same
  pass is left uncommitted for its owner.

## 16.2 Null as a stored value (done)

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

As built:

- The engine's `merge_props` stores every value it is given, nulls
  included; `set_node_props` and `set_edge_props` say so, and the new
  `remove_node_props` and `remove_edge_props` remove keys by name (under
  the same rights checks, 14.4, as setting them; nothing is written when
  none of the keys exists). A new node or edge already kept the nulls it
  was created with in the library; GQ's `CREATE` (and `MERGE`) dropped
  them and now keeps them.
- GQ: `SET` stores nulls, `REMOVE n.key` removes, and `SET n = {…}`
  removes the keys the map lacks before merging it (it used to set every
  key to null first). `hasKey(x, 'key')` (in `morefns.rs`) answers for
  nodes, edges and maps.
- HTTP `PATCH /v1/{ns}/nodes/{id}` and batch updates take `remove_props`;
  the MCP `update` tool too; the TypeScript client's `NodePatch` has the
  field. `GET /v1/about` lists the capability `null_values`; the API
  version stays 1, as reading a property gives `null` either way.
- Extraction (11.1) cleared `mentioned_dates` and `mentioned_amounts` by
  setting them to null; it now removes them.
- Indexes: `encode_value` already had no encoding for null, so a null is
  never indexed and an `= null` lookup finds nothing. The integrity check
  compared every value under a unique index, so several nulls showed as a
  clash; it now compares only values the index holds (a bug the new tests
  found).
- Exports: JSON Lines keeps nulls (the props are JSON). CSV and GraphML
  cannot tell a null from an empty value, so a null property is exported
  as an absent one, as the query language guide says.
- No layout change, and replication carries nulls as it carries any
  value (entries hold the records' after-images).
- Tests: `grafeno/tests/nulls.rs` (kept and removed by name on nodes and
  edges, indexes and unique indexes with nulls, the check, the export),
  seven conformance scenarios (keys, `REMOVE`, nulls at creation, `+=`
  and `=`, stored null against a missing key, `hasKey`, nulls in lists and
  maps), and an HTTP test of `PATCH` with `remove_props` and the
  capability.

## 16.3 Namespaces from scripts (done)

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

As built:

- GQ `CREATE NAMESPACE`, `ALTER NAMESPACE … SET COMMENT | REMOVE COMMENT`
  and `DROP NAMESPACE`, with `IF NOT EXISTS` and `IF EXISTS`
  (`Schema::Namespace(NamespaceOp)`; a name is a word or a quoted string).
  They return the namespace's row, as `SHOW NAMESPACES` does, and count
  `namespaces_created` and `namespaces_dropped` in the write stats.
- `requirements()` gives them a kind of their own,
  `StatementKind::Namespace`, with the namespace they name and the action
  they take (`CreateNamespace`, `Schema`, `Drop`), so the server checks
  rights there, as the namespace routes do, not on the namespace the
  statement was sent through (which needs no right for it). With several
  groups (13.5) the statement is refused unless sent through the group
  the named namespace belongs to, since a new namespace lives in group 0
  unless the directory says otherwise.
- Replicated as any write: they run in the write transaction, whose entry
  carries the namespace record (`Touch::Namespace`). Not audited, as the
  `/v1/namespaces` routes are not: the audit log (14.5) records security
  events (sign-ins, refusals, changes to accounts and rights), and a
  refused namespace statement is recorded as any 403 is.
- The command line: `grafeno namespaces create | describe | drop` on a
  database file (`drop` wants `--yes`); `grafeno namespaces` alone lists
  them, as before.
- Scripts: `grafeno shell -f` (and `-c`, and the shell's `\ns create` and
  `\ns drop`) existed; scripts keep running after a failure, as phase 5
  documented and tested, so instead of changing that, `--stop-on-error`
  stops at the first failure. `grafeno run --file` was not added: the
  shell is the one way to run scripts. The query language guide has an
  example `setup.gq`.
- Tests: eight conformance scenarios (create with a comment, quoted
  names, duplicates, `IF NOT EXISTS`, `ALTER`, `DROP` and `IF EXISTS`), an
  access test (the owner, an admin of `team-*` who may create `team-b` but
  not `other`, a reader who may not drop), and a shell test (a setup
  script, `--stop-on-error`, the command line's create, describe and
  drop).

## 16.4 Explicit transactions (done)

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

As built:

- The engine already had what a transaction across statements needs: a
  `WriteTx` lives until `commit` or `abort` and moves between threads, as
  does a `ReadTx`. New: `Grafeno::begin_captured`, a write transaction
  whose changes are captured as a replication entry, and
  `WriteTx::into_entry`, which ends it uncommitted and gives the entry.
- The cluster (`grafeno-cluster`): writes were serialized by a mutex held
  for one call; it became a gate whose guard can outlive a call.
  `Node::begin_transaction(wait)` takes it (on the leader, once its
  database holds every entry before) and gives a captured transaction;
  `commit_transaction` proposes its entry, so a transaction's statements
  replicate as one entry and no other write runs between its first
  statement and its commit; dropping or rolling it back gives the gate
  back. Plain writes use the same gate.
- The server (`transactions.rs`): sessions with an owner (the caller's
  name), a group, a timeout and the work itself, read (a snapshot) or
  write (a `WriteTx` and the single permit of writing transactions on a
  single server; a cluster `Transaction` on a leader). Routes under
  `/v1/{ns}/transactions`; beginning or ending one needs read rights
  there, and each statement is checked as `/query` checks it
  (`check_statement`, taken out of the query route), except that access
  statements are refused inside transactions. A statement that fails in a
  writing transaction marks it failed. Sessions past their idle time or
  lifetime are rolled back by a task every second and when touched.
  `[transactions]` in `grafeno.toml`, shown by `GET /v1/settings`;
  `grafeno_transactions_open{kind}` in the metrics; capability
  `transactions`.
- On a single server a writing transaction holds redb's write lock, so
  the group-commit writer waits for it; the idle timeout (30 s by
  default) bounds how long.
- The shell: `BEGIN [READ ONLY]`, `COMMIT`, `ROLLBACK` on a file (the
  engine's transactions) or a server (the HTTP routes); `*` in the
  prompt; a script or session ending inside one rolls it back. GQ refuses
  the three words outside a session, with where to use them.
- The TypeScript client: `Namespace.begin()`, a `Transaction` with
  `query`, `status`, `commit`, `rollback`, and `transaction(work)`.
- Not done: the MCP tools stay one call, one transaction (an agent's
  session would hold the writer between its turns); and savepoints, as
  redb has none once tables are open.
- Tests: HTTP (`tests/transactions.rs`: isolation, commit, rollback, a
  failed statement, a reading snapshot, the busy writer, idle timeout, a
  plain write waiting, and a server killed with a transaction open), the
  shell on a file and on a real server, the cluster
  (`explicit_transactions_commit_as_one_entry`: one entry for two
  statements, plain writes waiting, rollback, followers refusing), and the
  TypeScript client against the real binary.

## 16.5 Advanced indexes and constraints (done)

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

As built:

- `IndexDef` gained `more_keys` (a composite index), `more_labels`,
  `edges` and `required` (with `unique`, a key), all defaulted, so stored
  definitions read as before and no layout changed. A composite entry
  concatenates its values, each self-delimiting (strings escape their
  zero bytes and end with two zero bytes), so byte order stays value
  order property by property; single-property entries keep their first
  encoding. Lookups: `index_seek_nodes` and `index_seek_edges` (equal
  values for a prefix, then a range on the next property).
- Writes: `put_node` and `put_edge` (every path ends there: the API, GQ,
  batches, imports, replicated entries, rebuilds) read the namespace's
  index definitions once and maintain node and edge entries; unique
  checks compare the stored values of every property, and a retracted
  fact no longer holds its values (the new version that ends an
  invalidated fact has the same ones). `check` and `rebuild` cover edge
  entries.
- Constraints: `Constraint::Exists` (`IS NOT NULL`) and
  `Constraint::Typed` (`IS :: TYPE`: boolean, integer, float, string,
  list, map, datetime), on nodes or edges; keys are unique indexes with
  `required`. Declaring one checks the data and names the first three
  records that break it. The checks run after each write, or, after
  `WriteTx::defer_checks`, at `check_deferred`: a GQ statement defers to
  its end, so `MERGE … ON CREATE SET` can set a required property. Not
  done: declaring a rule that only flags what breaks it (it refuses).
- GQ: `ON (n.a, n.b)`, `FOR ()-[r:label]-()`, `FOR (n:A|B)`, and
  `REQUIRE … IS UNIQUE | KEY | NOT NULL | :: TYPE | TYPED TYPE`. `SHOW
  INDEXES` adds `on` and `key`, `SHOW CONSTRAINTS` adds `on` and lists
  every kind. The planner reads a composite index with equal values for
  its first properties and any seek on the next, and starts a pattern at
  the ends of edges an edge index finds (the side of the anchor, or both
  for an undirected step); its statistics cover every non-fuzzy index
  (composite ones by whole entries, edge ones against edge label counts).
- Measured (`examples/schema_bench.rs`, 200 000 people and 400 000 edges,
  release build, laptop): loading took 14.0 s with no indexes and 19.7 s
  with a key on three properties, an edge index, a not-null rule on edges
  and a type rule on nodes. A whole key took 1.5 ms instead of 118 ms by
  scan, its first property 0.6 ms instead of 72 ms, a prefix and a range
  0.2 ms instead of 84 ms, and a range on an edge property 0.7 ms instead
  of 916 ms. Loading without indexes or rules was unchanged (an A/B of
  the previous build and this one, alternately: 3.8 to 4.2 s each).
- Tests: `grafeno/tests/advanced_indexes.rs` (the API, deferred checks,
  lookups, replicated entries applied by a follower, an export imported
  elsewhere) and `grafeno-query/tests/advanced_indexes.rs` (plans and
  results of each kind, reads against scans, keys with `MERGE`, labels
  added later, rules on edges, rebuild and `check`, syntax errors).

## 16.6 Full-text search (done)

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

As built:

- A full-text index is an `IndexDef` with `fulltext: {analyzer,
  weights}` (`grafeno/src/fulltext.rs`), so it is declared, replicated
  (`Op::Index`), exported, rebuilt, checked and dropped as any index. Its
  entries share `index_entries`: one per term, field and node, the key
  holding the term, the field, its length, the frequency and the first 64
  positions. Documents and terms per field are in a new table,
  `fulltext_stats`, kept as the entries are (so every member of a cluster
  derives them). Layout 18 adds it, and marks files that may hold the
  indexes of 16.5 or these, which a layout 17 Grafeno would misread
  (fixture `layout-v17`, from 16.5's commit, and the upgrade test builds a
  full-text index on every upgraded fixture).
- Analyzers: words split on what is not a letter or a digit, lowercased,
  accents removed, with their offsets in characters; `standard` drops
  English and Spanish stop words (as the node text does), `english` and
  `spanish` drop their own and stem with a light suffix stripper of
  ours, `simple` keeps everything.
- Queries: a lexer and a recursive parser (`OR` over `AND` over clauses
  of `+`, `-`/`NOT` and plain terms); prefixes and fuzzy terms expand to
  at most 64 index terms, skipping from term to term in the index;
  scores are BM25 per field, weighted and summed; phrases check
  positions, nearness a window.
- `ReadTx::fulltext_search` (hits with fragments), `ReadTx::search` with
  `SearchRequest {text, index, embedding, mode, fusion, limit,
  candidates, fragments}` and `fuse` (rank or weighted). GQ: `CREATE
  FULLTEXT INDEX … ON EACH [...] OPTIONS {analyzer, weights}`,
  `db.searchIndex`, and `db.search(text, {…})`; `SHOW INDEXES` gained
  `kind`. HTTP: `POST /v1/{ns}/search`, `index` and `fragments` on
  `/search/text`, `index`, `mode`, `fusion`, `k`, `text_weight` on recall
  and context; MCP `recall` takes `index` and `mode`; capabilities
  `search.fulltext` and `search.hybrid`; the TypeScript client `search()`
  and `searchText(query, {index, fragments})`.
- Measured (`grafeno/examples/fulltext_bench.rs`, release build, laptop):
  a synthetic labelled set of 20 000 documents in 400 topics, each of
  eight made-up words from a shared pool of 800, words in random forms
  (`x`, `xs`, `xing`), queries of two plain words of a topic. Precision at
  10: the node text 0.36, the full-text index (English analyzer, title
  weighing 3) with the plain words 0.43, with both words required (`+a
  +b`) 0.83. Time per query: 0.06 ms for the node text, 0.33 ms for the
  index (0.27 with both required, 0.48 with fragments). The set is
  synthetic: it shows what stems, weights and required terms change, not
  how a real corpus ranks.
- Not done: analyzers for other languages, synonyms, and full-text
  indexes on edges.

## 16.7 Better vector indexes (done)

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

As built:

- The HNSW code (`vector.rs`) works on a *space*: the node embeddings of a
  namespace (its name, as before) or a vector index (`ns\0index`), in the
  same tables, so no table was added. A space's metadata holds its
  similarity, `m` and `ef_construction` (defaulted for existing ones:
  cosine, 16, 100). Distances: `1 − dot` (cosine, normalized), `−dot`
  (dot product), squared L2 (Euclidean), each also from the int8 copies
  and from the stored bytes; scores are the cosine, the product, and
  `1 / (1 + d)`. A node's level and the link counts follow its space's
  `m`.
- A vector index is an `IndexDef` with `vector: {dimension, similarity,
  m, ef_construction, ef_search, building}` (`vindex.rs`). Writes go
  through the index entry path, so every member derives the graph from
  the nodes; a rebuild resets the spaces and fills them again; `check`
  checks each graph and its count against the nodes; a namespace drop
  clears its spaces.
- Filtered search: the layer-0 walk keeps two heaps, candidates (every
  node reached) and results (only those the filter keeps), and goes on
  while fewer than `ef` are kept or a candidate is closer than the
  farthest kept. Labels alone are checked in the label index; properties
  (and a reader's view) read the node. When at most 4 096 nodes have the
  filter's labels, they are scored exactly instead.
- `ReadTx::vector_search(ns, query, &VectorSearch {index, limit, ef,
  filter, exclude})` and `ReadTx::similar`; GQ `CREATE VECTOR INDEX`,
  `db.searchVector(e, {…})`, `db.searchVectors`, `db.similar`; HTTP
  `/search/vector` with `index`, `filter` and `embeddings`, and
  `/similar`; `vector_index` in hybrid search and recall; `SHOW INDEXES`
  gained `state`; capability `search.vector.indexes`; the TypeScript
  client.
- Background builds: `background: true` stores the index with
  `building`; `WriteTx::build_index_step` indexes the next chunk of nodes
  (in id order) and is logged as itself (`Bulk::BuildIndex`), so each
  member does the same step; a server runs one step per pending index
  every half second on the leader; `Grafeno::build_pending_indexes`
  finishes them (the local shell calls it after each statement).
- Replication entry format 7: entries holding an index or constraint of
  16.5 to 16.7 or a build step (16.5 and 16.6 had added such indexes
  without a new format; an older member would have misread them).
- Measured (`grafeno/examples/vector_index_bench.rs`, release build,
  laptop): 20 000 vectors of dimension 128 in 64 clusters, 200 queries,
  `ef` 64, as 10.3 measured. Building each index took 7 s. Recall@10 was
  1.000 for cosine, dot and Euclidean (10.3: 1.000 for cosine), p50 0.6
  to 0.8 ms (12.5's int8 searches: 0.51 ms on the measuring machine of
  then, 1.35 ms in 14.4's run). With a filter during the walk, recall@10
  stayed at 1.000 (0.998 and 1.000 for dot) whether the filter kept a
  half (p50 about 1.0 ms), a tenth (about 2.9 ms, scored exactly) or a
  hundredth (about 0.3 ms, scored exactly); filtering the plain results
  afterwards kept 0.51, 0.10 and 0.01 of them. Before labels were checked
  in the label index and small sets scored exactly, a hundredth took 88
  ms and a tenth 12 ms.
- Not done: filters on ranges of values (they are equalities), and
  vector indexes on edges.

## 16.8 More graph algorithms, for analysis and simulation (done)

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

As built:

- Engine (`grafeno/src/algo/analysis.rs`, `simulation.rs`), over the
  projection of 9.x: k-core by peeling, HITS and eigenvector centrality by
  power iteration (the eigenvector one adds each node's own score, so a
  bipartite graph does not oscillate), articulation points and bridges by
  one iterative depth-first search, Kruskal's spanning forest, Dinic's
  maximum flow with its minimum cut, Hopcroft-Karp matching, Kahn's
  topological order (and a cycle when it fails), weighted random walks
  from a seed, conductance per group, and Leiden's guarantee on top of
  Louvain (each community split into its connected parts; not Leiden's
  full refinement). Simulation: multi-source Dijkstra (distance fields),
  influence as `value × decay^cost`, breadth-first flood fill through a
  condition, diffusion that keeps the total (shares split by weight, a
  full node sends back what does not fit), synchronous rules (life,
  threshold, majority), the Hungarian method for assignment, and
  successive cheapest paths for flows at least cost. Harmonic centrality
  and the clustering coefficient reuse closeness and triangles.
- GQ (`algorithms.rs`): 22 procedures with the usual projection settings;
  edge results give the edges (their ids are kept in the projection only
  for these). Node properties (amounts, states, supplies, conditions) are
  read through the caller's view. Minimum-cost flows build two
  projections (capacity and cost) and check they hold the same edges.
- Modes, for every algorithm: `mode: 'stats'` (a row per numeric
  column) and `write` (a property, or a map of columns to properties),
  carried out where the rows go (`exec.rs`); they must be in a literal
  settings map, so that a writing call makes the statement a write
  before it runs (rights, write transaction). Other settings in that map
  may still be parameters (`{mode: 'stats', labels: $labels}`); `mode` or
  `write` in a parameter is refused with an error (at first it was
  silently ignored, which the HTTP route's `$settings` hit). Over HTTP,
  `/v1/{ns}/algo/{name}` serves every algorithm, nodes and edges in their
  compact form, with `stats: true` (the server writes the settings map
  into the query, each value a parameter, keys checked as names) or
  `write`. Capabilities `algorithms.analysis` and `algorithms.simulation`.
  The TypeScript client's `algorithm()` takes `stats`, tested against the
  real server with an analysis and a simulation algorithm and both modes.
- Measured (`grafeno/examples/algo_bench.rs`, the 10.6 graph of 100 000
  nodes and 500 000 edges in communities, release build, laptop; memory
  is what each step allocated beyond what was held, counted by the
  allocator): k-core 64 ms (11 MiB), Leiden 771 ms (43 MiB, the same 384
  communities as Louvain here), HITS 20 rounds 86 ms, eigenvector 100
  rounds 343 ms, cut points 55 ms (16 MiB), spanning forest 36 ms (19
  MiB), maximum flow 356 ms (46 MiB), matching 78 ms, random walks (10 000
  of 20 steps) 29 ms, distance field from 100 sources 63 ms, influence of
  10 sources 256 ms, flood fill 12 ms, diffusion 20 steps 393 ms (27
  MiB), life rule 20 steps 75 ms, assignment 300 × 300 1 ms, minimum-cost
  flow of 50 units 615 ms (55 MiB). The algorithms of before took what
  they took in 9.x's table, within this laptop's variation.
- Not done: Leiden's refinement phase itself, rules with more than two
  states, and diffusion with losses.

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
