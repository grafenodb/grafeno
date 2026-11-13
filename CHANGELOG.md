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

- **Added**: a model of knowledge (phase 8). Kinds of memory with a
  meaning: Episode, Concept, Hypothesis, Evidence, Constraint and Goal
  beside Fact, Decision, Experience, Task and Question. Relations with a
  meaning: related_to, depends_on, part_of, derived_from, supported_by,
  contradicts, supersedes and caused_by, which `relate` refuses without
  `sources`. `remember` and `relate` take `method`; `get_node` answers a
  provenance block. `relate` and an agent's reading record the same
  current fact once; `maintain` lists facts stored twice. A merge keeps
  what the merged node was and how it was known in `merged_from`.
- **Added**: metacognition (phase 6 of the Mental Palace plan).
  `palace_reflect` says what is known about a topic, a room or a node and
  what is not, each item with its record: what is known (by kind and by
  origin, the verified named), what is uncertain (inferred with no source,
  low certainty), conflicts recorded under constraints, what may be out of
  date, gaps (open questions, things mentioned and never described, empty
  places, steps left in a checkpoint), what to check next, and the limits
  of the look. A topic with nothing stored says so. It writes nothing.
  `Question` is a kind of memory, open until a memory `answers` it. In
  `grafeno-memory`: `reflect`, `QUESTION`, `ANSWERS`.
- **Added**: several agents share one memory (phase 5 of the Mental
  Palace plan). `grafeno mcp --url URL --token KEY` runs the agent's
  tools on a Grafeno server instead of opening a file: the server owns the
  file, checks each agent's rights by its key and records its writes, so
  any number of agents work on it at once. `GET /v1/{ns}/tools` and
  `POST /v1/{ns}/tools/{name}` run the memory tools over HTTP as the
  caller: reading tools need the right to read the namespace, writing
  ones a right to write. Rights by namespace only; not on a cluster yet.
  The message of a file held by another process names `--url`.
- **Added**: phase 4 of the Mental Palace plan. `palace_review` proposes
  how to arrange a palace (split a crowded room, merge rooms that hold the
  same, move a memory to the room its community lives in, remove empty
  places, a room for a community with none) and applies nothing.
  `maintain` lists older memories a newer one about the same entity may
  overtake. `consolidate` with `undo` deletes a summary and brings back
  the memories it archived. In `grafeno-memory`: `review`, `remove`,
  `superseded`, `undo_summary`.
- **Changed**: `forget` on a palace, a room or a locus deletes it with
  what is under it (rooms, loci, anchors), never a memory; before, it left
  the loci of a room behind. `forget` takes `dry_run`.
- **Added**: retrieval over palaces (phase 3 of the Mental Palace plan).
  `palace_walk` takes `order` (`distance`, the default, or `relevance`:
  the full-text match of the topic, then distance) and `budget` (the
  memories reached and the facts joining them as text, for a prompt). A
  `leads_to` fact from a room to another is a door: `palace_map` lists a
  room's doors, and walks go through them. `context`, `palace_enter` and
  a walk's context leave out a memory nearly the same as one already
  packed, and say how many. In `grafeno-memory`:
  `ContextOptions::redundancy`, `WalkOrder`, `WalkQuery::budget`, `DOOR`.
  Measured in `examples/palace_bench.rs` (docs/benchmarks.md).
- **Added**: `grafeno mcp --listen ADDR` also serves the HTTP API on the
  agent's database, with `--token` and `--cors-origins`: Grafeno Studio and
  `grafeno shell --url` connect to it and see what the agent writes as it
  writes it, though only one process can open the file. A second process
  on a file that is open is refused with a message that says how.
- **Security**: fixes from two outside reviews, each with a test; see
  [docs/security.md](docs/security.md). A key made with a scoped key
  takes its scope. On Windows, settings and data are no longer readable
  by every user (`config init`, the instance commands, the installer;
  `doctor` reports it). Cluster members prove themselves with the secret
  on every hop, an empty secret is refused, `/status` on the members' port
  needs it, and a malformed `Append` no longer stops a member's Raft
  thread. Sign-in attempts are counted when they begin, with bounded
  memory and a few password checks at a time. A writing transaction needs
  a right to write; exporting everything needs the export right.
  Webhooks reach only public addresses (`[background] webhooks_to_private`).
  Refusals are sampled in the audit log
  (`grafeno_audit_refusals_left_out_total`). A panic answers with a
  reference. Limits: an ingested line (4 MiB), an MCP message (16 MiB),
  edit distances, dates up to year 200 000. `COUNT { … }` and other
  patterns in expressions follow their MATCH's `VALID AT`.
  `docker compose` needs `GRAFENO_TOKEN`.
- **Added**: how a memory or a fact is known. `remember` and `relate`
  take `origin` (`stated`, `observed`, `inferred`, `verified`) and
  `certainty` (0 to 1), stored as properties; `get_node`, `recall` and
  `context` show them, and a record with neither reads as not stated.
  `add_mentions` takes `relations` between the entities it mentions,
  stored as facts with the memory as source and `inferred` by default, and
  answers with the entity each mention became. `feedback` takes the
  verdict `verified`, with the `source` that supports it, keeping what the
  record was before. `Fact`, `Decision`, `Experience` and `Task` are the
  kinds of memory with a meaning. In `grafeno-memory`: the `knowledge`
  module, `ingest_reading` and `verify`. No layout change. See
  [docs/mental-palace/cognitive-memory.md](docs/mental-palace/cognitive-memory.md).
- **Added**: mental palaces. Memories that already exist are arranged
  into places and found again by where they are: a palace has rooms, a
  room has loci, and a memory is anchored at any number of loci without
  being copied or changed. Six MCP tools: `palace_create`,
  `palace_create_room`, `palace_anchor`, `palace_map`, `palace_walk` (from
  a topic or a node to the memories around it, over the graph's
  relations) and `palace_enter` (a room's memories and facts within a
  token budget, and the latest checkpoint about the room). In
  `grafeno-memory`: the `palace` module, and `context_of`, which packs any
  list of nodes as `context_for` packs a recall's. A palace is ordinary
  nodes (`Palace`, `Room`, `Locus`) and edges (`has_room`, `has_locus`,
  `anchored_at`): the file layout stays 18 and the HTTP API is unchanged.
  See [docs/mental-palace.md](docs/mental-palace.md).
- **Changed**: recall, context, community summaries, duplicates, weak
  memories, consolidation and extraction leave out the nodes a palace is
  made of, and MCP's `remember` does not reuse one for a memory of the
  same name. A recall that asks for `Palace`, `Room` or `Locus` by label
  returns them. Databases without those labels behave as before.
- **Changed**: recall's graph expansion damps well-connected nodes. The
  share a seed's score passes along an edge is divided by
  `sqrt((1 + ln d₁)(1 + ln d₂))`, d₁ and d₂ being the edges of its ends,
  so a node linked to everything (the user, the current project, a common
  tag) no longer collects score from every match and ranks as relevant to
  everything. Each node passes to 100 neighbors at most, those the search
  found first. `expand` takes `hubs` (1; 0 for the earlier behavior) and
  `fanout` (100; 0 for all); capability `recall.hubs`. Results with a
  graph signal score lower than before, and hubs and their neighbors rank
  lower (#11).
- **Added**: an example and a test of an answer only the graph finds
  (`cargo run -p grafeno-memory --example multi_hop`), in the README (#14).
- **Changed**: without a tokenizer, `context_for` (and `/context`, MCP's
  `context`) counts tokens by kind of character instead of four
  characters a token, which ran short on code, structured data and text
  outside English; contexts hold a little less and stay within budgets
  (#16).

- **Added**: vector graph health. Each vector graph (a namespace's own,
  and each vector index) counts the vectors removed or replaced since it
  was built and the link lists those removals repaired:
  `GET /v1/{ns}/vectors/health`, `ReadTx::vector_health`, and in
  `grafeno check`; capability `vectors.health`. The counts live in the graph's metadata, so the file
  layout stays 18 (graphs start counting at 0 after the upgrade) (#12).
- **Added**: the server compacts a vector graph on its own once its
  removals reach half its vectors (`compact_vectors_at` in
  `[background]`, 0 to turn it off), for graphs of 1 000 to 50 000
  vectors (`compact_vectors_min`, `compact_vectors_max`), and optionally
  when the sampled recall@10 falls below `compact_vectors_below_recall`.
  A compaction holds the writer about as long as inserting every vector
  (6 s for 20 000 of 128 dimensions); searches use the old graph until it
  commits. On a cluster the leader compacts and members replay it (#12).
- **Added**: `POST /v1/{ns}/compact?index=` and
  `WriteTx::compact_vector_index` compact a vector index;
  `grafeno compact` compacts a namespace's indexes too. Compacting keeps
  the index's similarity and `m`. In a cluster, upgrade every member
  before vector indexes are compacted: older members rebuild them with
  the default similarity when they replay it.

- **Changed**: new vectors join their graph when the transaction
  commits. When a commit brings several to a space (a group commit, a
  batch, a background index build), their neighbors are searched on a
  read snapshot on several threads, and the writer only writes the link
  lists, working out again those that changed meanwhile. With 100 000
  vectors of 128 dimensions the writer's ceiling for node + text + vector
  writes went from about 210 to 375 a second and the median latency at
  100 a second from 189 to 44 ms; recall is unchanged. Members of a
  cluster that apply the same entry build the same graph. A write alone
  in its transaction costs what it did (#18).

- **Added**: six MCP tools. `checkpoint` and `resume` save where a task
  stands and take it up again in a later session (with what was written
  since). `ingest` stores a document, a conversation or code as a whole
  and its chunks, cut at headings, turns or definitions, and extracts what
  they mention; the same source again replaces it. `contradictions` lists
  facts that contradicted earlier ones; `timeline` tells how a node
  changed; `feedback` marks a memory or fact useful, obsolete or wrong.
  `maintain` also reports the namespace's size, its vector graphs' wear
  and contradictions, and an integrity check with `check: true`. In the
  library: `grafeno_memory::{save_checkpoint, resume, ingest_document,
  cut, timeline}`.

## 0.2.1 (2026-11-11)

The same database layout (18), HTTP API (version 1) and log entry format
as 0.2.0, so 0.2.0 and 0.2.1 members can share a cluster.

- **Fixed**: in a cluster, a follower that fell behind while clients wrote
  a lot could stay seconds behind, and further behind the more it lagged:
  every message of the leader to it waited in one queue, each repeating
  the entries it lacked. Of the messages waiting for a member, only the
  newest append is now sent (it holds the leader's whole view of that
  member), so a follower catches up at once. Seen as a namespace moved
  under load whose last writes were missing on one member.
- **Fixed**: in the library, a caller whose view reads nothing of a
  namespace (rights to write there, none to read) got the stored counts
  from `db.labels()`, `db.edgeLabels()`, `db.propertyKeys()` and
  `db.stats()`; they now count what it sees: nothing. The server refused
  such a caller's queries already.
- **Changed**: the TypeScript client left this repository. It is now the
  TypeScript SDK in [grafenodb/grafeno-sdk](https://github.com/grafenodb/grafeno-sdk),
  published to npm as `@grafenodb/client`, with its own versions; the
  engine's CI still runs its tests against every engine change.

## 0.2.0 (2026-11-11)

Phase 16: the data model, search, analysis and events (the story, step
by step, is in [docs/development-history.md](https://github.com/grafenodb/grafeno/blob/main/docs/development-history.md)).

**Upgrading.** The database files move to layout 18, upgraded in place
when 0.2.0 first opens them, with a copy of the old file kept first. In a
cluster, upgrade every member before writing with the new features: their
log entries (format 7) are refused by members still on 0.1.0. Behavior
that changes for existing data: `SET n.key = null` now keeps the key with
a `null` value (`REMOVE` deletes it), and the text-similarity functions
answer only to their `text.*` names (see below).

- **Fixed**: an algorithm's `write: '@embedding'` in GQ stored a property
  named `@embedding` instead of each node's vector; it now sets the vector,
  as `SET node.@embedding` does (another `@` name is refused). HITS gave
  `-0.0` for a node no hub reaches. The shell's `TRACE` table cut the
  index an anchor came from and the condition that dropped a row to their
  last eight characters, as it does ids. A type constraint's error says
  "not of type INTEGER" (it said "not a INTEGER").
- **Added**: change events with values, off by default. With
  `[events] enabled = true` in `grafeno.toml`, `CREATE EVENT SUBSCRIPTION
  name ON NODES | EDGES | PROPERTIES | TEXT | VECTORS [FOR :Label] [KEYS
  …] [WHERE key = value …] [WITH VECTORS] [DELIVER TO STREAM | WEBHOOK
  'url' [SECRET 's']]` (with `DROP EVENT SUBSCRIPTION` and `SHOW EVENT
  SUBSCRIPTIONS`) records, on the change log's entries, the values before
  and after, the keys that changed, texts and vectors; read at
  `GET /v1/{ns}/events/{name}` (a page or server-sent events) or posted to
  a signed webhook in order (capability `events.values`). A new namespace
  privilege, `events`, which administrators have. `GET /v1/{ns}/changes`
  is unchanged. The TypeScript client's `events()`.

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
