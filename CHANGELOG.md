# Changes to the Grafeno engine

The engine (`grafeno`: the server, the shell, the administration commands
and its installers) has its own versions and releases, tagged
`v<version>`; Grafeno Studio and Grafeno Desktop have theirs, in their own
repository. A version's section here is its release's notes;
the release workflow refuses a tag whose version has no section.

What the engine speaks to clients is versioned apart from it: the HTTP
API version (`GET /v1/about`, now 1) changes only when a change would
break a client of the previous one, and the database files' layout
version (now 18) is upgraded in place, with a copy kept first.

## Unreleased

- **License**: Grafeno is free software under the MIT license or the
  Apache License 2.0, at the user's choice (`LICENSE-MIT`,
  `LICENSE-APACHE`). The packages, the Windows setup (a license page),
  the archives and the winget manifest carry it.
- **Added**: `TRACE <statement>` (and `trace: true` on
  `POST /v1/{ns}/query`, `\trace` in the shell, `trace` in the
  TypeScript client's `query()`): the walk of a statement as events
  (anchors, edges followed, what patterns and conditions kept and
  dropped, rows), bounded to 10 000 events and 4 MiB (capability
  `query.trace`).
- **Added**: graph algorithms for analysis (`algo.kCore`, `leiden`,
  `hits`, `eigenvector`, `harmonic`, `articulationPoints`, `bridges`,
  `spanningTree`, `maxFlow`, `bipartiteMatching`, `topologicalOrder`,
  `cycle`, `randomWalks`, `clustering`, `conductance`) and for simulation
  (`algo.distanceField`, `influence`, `floodFill`, `diffusion`,
  `automaton`, `assignment`, `minCostFlow`); every algorithm takes
  `mode: 'stats'` and `write: 'property'` in its settings map, and over
  HTTP `stats: true` beside the settings; a mode passed in a parameter is
  refused, not ignored (capabilities `algorithms.analysis`,
  `algorithms.simulation`). The TypeScript client's `algorithm()` takes
  `stats`.
- **Added**: named vector indexes, several per namespace, on a property
  of one or more labels: `CREATE VECTOR INDEX … ON (d.embedding) OPTIONS
  {dimension, similarity: 'cosine' | 'dot' | 'euclidean', m,
  ef_construction, ef_search, background}`; searches filtered by labels
  and properties during the walk; several vectors at once; a node's own
  vector (`db.similar`, `POST /v1/{ns}/similar`); background builds with
  their state in `SHOW INDEXES` (capability `search.vector.indexes`). The
  TypeScript client has `searchVector(embedding, {index, filter})`,
  `searchVectors` and `similar`.
- **Changed**: replication entries move to format 7 when they hold an
  index or constraint of 16.5 to 16.7 or a background build, so a member
  of the version before refuses them instead of misreading them.
- **Added**: full-text indexes over chosen properties of one or more
  labels, with a weight per property and an analyzer (`simple`,
  `standard`, `english`, `spanish`: accents, stop words, stemming):
  `CREATE FULLTEXT INDEX … ON EACH [n.title, n.body] OPTIONS {…}`, and
  `db.searchIndex(index, query)` with phrases, `+required`, `-excluded`,
  `AND`, `OR`, `prefix*`, `fuzzy~1`, `"near words"~3`, `field:word`,
  scores and fragments.
- **Added**: search by text, vector or both as the caller says:
  `db.search(text, {mode, embedding, index, fusion, k, text_weight})`,
  `POST /v1/{ns}/search`, `index` and `fragments` on
  `POST /v1/{ns}/search/text`, and `index`, `mode` and `fusion` on recall
  and MCP `recall` (capabilities `search.fulltext`, `search.hybrid`). The
  TypeScript client has `search()` and `searchText(query, {index,
  fragments})`.
- **Changed**: database files move to layout 18 (indexes of 16.5 and
  full-text indexes, which an older Grafeno would misread, and
  `fulltext_stats`); `SHOW INDEXES` has a `kind` column.
- **Added**: indexes on several properties (`ON (p.last, p.first)`,
  used by a prefix of them), on edge properties (`FOR ()-[r:rated]-()`)
  and over several labels (`(n:A|B)`); constraints `IS NOT NULL`,
  `IS :: TYPE`, `IS UNIQUE` on several properties and `IS KEY`, on nodes
  and edges, checked against the data when declared and at the end of
  each query statement (capability `indexes.advanced`).
- **Changed**: `SHOW INDEXES` has two more columns (`on`, `key`) and
  `SHOW CONSTRAINTS` one (`on`).
- **Added**: explicit transactions. `BEGIN [READ ONLY]`, `COMMIT` and
  `ROLLBACK` in the shell and scripts (on a file or a server), and
  `POST /v1/{ns}/transactions` with `…/{id}/query`, `…/commit` and
  `…/rollback` over HTTP (capability `transactions`), with idle and
  lifetime limits (`[transactions]`); on a cluster a transaction commits
  as one replicated entry. The TypeScript client has `begin()` and
  `transaction(work)`.
- **Added**: `CREATE NAMESPACE name [IF NOT EXISTS] [COMMENT '…']`,
  `ALTER NAMESPACE name SET COMMENT '…' | REMOVE COMMENT` and `DROP
  NAMESPACE name [IF EXISTS]` in GQ, checked on the namespace they name;
  `grafeno namespaces create | describe | drop` on a database file; and
  `grafeno shell --stop-on-error` for scripts that must stop at the first
  failure.
- **Changed**: a `null` property is a value. `SET n.x = null`, a `null` in
  `SET n += {…}`, in a new node's properties (GQ `CREATE`, HTTP, MCP) and
  in an HTTP `PATCH` or the MCP `update` tool store `null` and keep the
  key, where before they removed it. To remove a property: `REMOVE n.x`
  in GQ, `remove_props: ["x"]` in a `PATCH` or in `update`, or
  `remove_node_props` / `remove_edge_props` in the library. `SET n = {…}`
  still removes the keys the map lacks. New: `hasKey(x, 'key')`; the
  capability `null_values` says an engine works this way. Indexes hold no
  nulls, and nulls never clash under a unique index.
- **Changed**: the text-similarity functions answer only to their `text.*`
  names; the five compatibility aliases outside that prefix are gone, and a
  query that uses one fails with "unknown function". Their replacements:
  the Levenshtein distance is `text.levenshtein`, the Jaro-Winkler
  similarity `text.jaroWinkler`, the Sørensen-Dice similarity
  `text.dice(a, b, 2)`, the phonetic key `text.soundex`, and the
  normalized Levenshtein similarity is closest to
  `text.similarity(a, b, 'levenshtein')`.

## 0.1.0 (2026-11-07)

The first release: everything built in phases 1 to 15 (the story, step by
step, is in [docs/development-history.md](https://github.com/grafenodb/grafeno/blob/main/docs/development-history.md)).

- **The graph**: namespaces of nodes and edges with properties, facts with
  valid and record time, full-text (BM25) and vector (HNSW, int8 copies)
  search, hybrid recall and context for prompts, memory upkeep (importance,
  decay, consolidation, summaries).
- **Queries**: GQ, a declarative graph query language with `EXPLAIN`, batches,
  traversals, graph algorithms, a cost-based planner, batched and parallel
  execution.
- **Knowledge upkeep**: entity resolution and record linkage, extraction of
  mentions, cardinality constraints, community summaries, a change log with
  streams, webhooks and NDJSON ingestion.
- **Access**: accounts with passwords and sessions, API keys, roles with
  grants and denials down to labels, properties and records, an audit log.
- **Clusters**: Raft replication, failover, linearizable reads, queries
  split across members, namespaces on several Raft groups.
- **Interfaces**: an HTTP API (version 1, `GET /v1/about` with 42
  capabilities), an MCP server, an interactive shell, a TypeScript client.
- **Installation**: a guided Windows setup (a virtual service account, the
  first administrator, the Event Log, winget manifests), `.deb` and `.rpm`
  packages for x86_64 and aarch64 with AppArmor and SELinux policies,
  signed APT and DNF repositories, a `curl` one-liner, `grafeno setup` for
  instances, `grafeno doctor`, a copy of the database before a layout
  upgrade and `grafeno instances rollback`.
- **For Grafeno Desktop**: `grafeno serve --stop-with-stdin` and `grafeno
  restore`, so the desktop app opens local files with the installed engine.

Database files: layout 17 (files of every earlier layout open and are
upgraded). HTTP API: version 1.
