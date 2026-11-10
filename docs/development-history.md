# Historial de desarrollo / Development history

[English](../README.md) · [Español](../README.es.md)

Every phase built so far, in detail; each new step adds its line here. The
linked READMEs give the introduction and the first steps.

Cada fase construida hasta ahora, en detalle (en inglés). Los README enlazados
presentan el proyecto y los primeros pasos.

- **Phase 1, core:** storage on [redb](https://github.com/cberner/redb),
  transactions, neighbors, k hops, shortest path, subgraph.
- **Phase 2, retrieval:** BM25 full-text index, HNSW vector index, hybrid
  recall (RRF + graph expansion) and `context_for`.
- **Phase 3, server:** the `grafeno` binary with an HTTP API (`grafeno serve`)
  and an MCP server for agents (`grafeno mcp`), a TypeScript client and a
  Docker image. See [crates/grafeno-server](../crates/grafeno-server/README.md).
- **Phase 4, foundations and validation:** namespaces and statistics, paged
  listings, integrity check and index rebuild, export/import, and property,
  durability and concurrency tests plus benchmarks
  ([docs/benchmarks.md](benchmarks.md)).
- **Phase 5, shell and query language:** `grafeno shell`, a console for
  a file or a server, and [GQ](query-language.md), a declarative graph
  query language with time travel (`VALID AT`, `KNOWN AT`).
- **Phase 6, Studio and installers:** Grafeno Studio, the web interface
  (explorer, graph view with time travel, query editor, search and context
  lab, administration) served by `grafeno serve` at `/studio/`; a Windows
  installer with a Windows service; `.deb` and `.rpm` packages with a
  systemd service; `install.sh` for any Linux. See
  [docs/install.md](install.md).
- **Phase 7, memory:** entities found by normalized names and aliases (MCP
  `remember` updates them instead of duplicating), importance, accesses and
  decay in recall ranking, duplicate detection and merging, archiving, and
  consolidation of old memories into summaries; a Memory view in the Studio.
- **Studio redesign:** a new look with light and dark themes; nodes, labels,
  properties, text, importance and facts are created and edited from the
  interface (explorer, graph and dialogs); a script workspace with tabs,
  GQ highlighting and completion, run-at-cursor or run-all, and results as a
  table, JSON or graph; a command palette (Ctrl+K). The logo is a piece of
  graphene: three fused carbon rings.
- **Phase 8.1, complete GQ:** `OPTIONAL MATCH`, `WITH`, `UNWIND`, `UNION`,
  `MERGE`, `FOREACH`, `CALL` procedures, `CASE`, list comprehensions,
  quantifiers, `reduce`, pattern predicates in `WHERE`, `EXPLAIN` and
  `PROFILE` ([docs/query-language.md](query-language.md)).
- **Phase 8.2, statistics and time:** `stdev`, `variance`, `median`,
  percentiles and `mode`; durations with calendar arithmetic,
  `duration.between`, date parts and `date.truncate` for grouping by period;
  `temporal.series` with per-row `VALID AT` to count what was true at each
  point; bar and line charts in the Studio's script results.
- **Phase 8.3, indexes and safe queries:** `CREATE INDEX`, unique
  constraints, a planner that starts from indexes or the smallest label and
  stops early for `LIMIT`; per-query limits (time, records read, rows held)
  and cancellation, so one query cannot take the service down; an indexes
  panel in the Studio.
- **Phase 8.4, concurrent and bulk work:** group commit for concurrent
  writes (11 times the writes per second on disk), eventual durability,
  bulk imports in chunks, and 503 with `Retry-After` when the server is
  full.
- **Phase 8.5, graph algorithms:** `CALL algo.*` for weighted paths
  (Dijkstra, A*, Yen, all shortest paths), centrality (degree, PageRank,
  betweenness, closeness), communities (components, label propagation,
  Louvain, triangles), similarity, link prediction and FastRP embeddings,
  on a cached in-memory projection; over HTTP too, and in the Studio's
  graph (color by community, size by centrality).
- **Phase 9.1, namespaces you create:** namespaces are records of their
  own (layout version 6): empty ones stay listed, creating an existing one
  is a clear conflict, and dropping one deletes everything in it; over
  HTTP, the shell (`\ns create`, `\ns drop`), GQ (`SHOW NAMESPACES`) and
  the Studio.
- **Phase 9.2, graph lab design:** a new design for the Studio, dark first,
  with a floating rail and command bar over the canvas, Space Grotesk and
  JetBrains Mono bundled for offline use, and glowing nodes.
- **Phase 9.3, a graph you can handle:** the Studio's graph is a live
  physics simulation on its own canvas: drag and pin nodes, lasso and
  move groups, connect by dragging, context menus, a minimap, and
  communities that collapse into one node.
- **Phase 9.4, desktop application:** `apps/desktop`, a Tauri app that
  opens database files with the engine inside the app (no service, no
  port) or connects to a server, with native menus, dialogs and recent
  databases (now in [Grafeno Studio](https://github.com/grafenodb/grafeno-studio/blob/main/desktop/README.md)).
- **Phase 9.5, installers and icons:** Grafeno Desktop installers for
  Windows (`.exe`, `.msi`) and Linux (`.deb`, `.rpm`, `.AppImage`) with
  `.redb` files opening in it, every icon rendered from one source, and a
  Grafeno Studio menu entry with its icon in the server's Linux packages.
- **Phase 10.1, isolation and authorization:** API keys with grants per
  namespace (read, write, admin, prefixes), owners, and keys limited to
  their own records; every write records who made it (`@recorded_by`);
  every route checked by one policy and tested against leaks; `grafeno
  keys`, MCP fences (`--allow`, `--read-only`, `--as`) and a keys panel in
  the Studio.
- **Phase 10.2, integrity under failure:** crash tests during imports,
  rebuilds, drops and upgrades; databases written by every past layout
  version open and upgrade; damaged files are refused by a checksum
  check (`grafeno check`) instead of crashing; online backups (`grafeno
  backup`, `GET /v1/backup`, the Studio).
- **Phase 10.3, vector search under change:** recall@10 measured after
  deletions, reinserts and churn; four repair bugs fixed (a reinserted
  vector could become its own only neighbor), so recall stays at 0.99 or
  more; `grafeno compact` and a recall estimate (`grafeno check --recall`).
- **Phase 10.4, costly queries:** 32 hostile queries (products, long paths,
  huge `UNWIND`s and reduces, deep nesting) all stop within their limits; a
  memory limit for values and held rows, time checked inside computing
  loops, a nesting limit, cost estimates in `EXPLAIN` with an optional
  `max_cost`, and a fuzz test that the parser and planner never panic.
- **Phase 10.5, correct merges:** merges refuse nodes that disagree (two
  emails, a person and a company, two birth dates) unless forced; duplicate
  detection follows the same identity rules and catches typos; every merge
  is recorded (who, when, both nodes as they were) and can be undone.
- **Phase 10.6, memory under large graphs:** measured with 10 M edges and
  1 M vectors in 2 GiB and 512 MiB containers; caches sized by bytes
  (`[memory]`), projections at about 12 bytes per edge, a streaming
  `check` ten times faster, and GQ algorithms bounded by `max_memory` and
  `top`.
- **Phase 10.7, one writer:** saturation measured (about 2 ms a write up
  to 4 000 a second with immediate durability, 11 000 at most); imports
  go through a bulk lane, so an agent writing during a large import waits
  88 ms at p99 instead of the whole import; `GET /v1/metrics` for
  Prometheus. Phase 10 (hardening) is complete.
- **Phase 11.0, entity resolution:** text similarity in GQ (Levenshtein,
  Damerau, Jaro-Winkler, n-grams, Soundex and a Spanish phonetic key),
  fuzzy trigram indexes (`CREATE FUZZY INDEX`, about 23 times faster than
  comparing with every node), and match rules per label with strong
  keys, weighted fields, neighbors as evidence and explained decisions
  (`db.match`, `POST /v1/{ns}/match`, the `find_matches` MCP tool).
- **Phase 11.1, extraction:** memories are read for what they mention
  (known names through a gazetteer, emails, URLs, handles, hashtags,
  dates, amounts, identifiers, recurring new names) and linked by
  `mentions` edges; namesakes are settled by context or queued for
  review; runs read only what changed (`grafeno extract`, the `extract`
  MCP tool); agents can send their own mentions.
- **Phase 11.2, contradicting facts:** edge constraints (`REQUIRE ONE PER
  SOURCE`, `TARGET` or `PAIR`) end the old fact in valid time when a new
  one contradicts it, as a new version so `KNOWN AT` keeps what was
  believed; or flag, or reject; functional properties keep their
  earlier values (`history(n, 'city')`).
- **Phase 11.3, community summaries:** Louvain levels kept as a
  hierarchy, each community summarized without a model (central members,
  distinctive terms, label mix, key facts, time span) as a `Community`
  node; refreshes rewrite only what changed and keep an agent's own text;
  `overview` answers broad questions and drills down.
- **Phase 11.4, following the writes:** a change log numbered per
  namespace (layout 12), streamed as server-sent events
  (`GET /v1/{ns}/changes`), pushed to signed webhooks, polled by the MCP
  `changes` tool; NDJSON ingestion with idempotency keys; extraction and
  community summaries can follow the log in the background
  (`[background]`).
- **Phase 12.1, a wider query language and tools:** path variables, `shortestPath`,
  `CALL { … }`, `COUNT { … }`, pattern comprehensions, map projections,
  `=~` and more functions; import and export in CSV and GraphML; 146
  hand-written conformance scenarios (no external conformance suite is run,
  and no binary wire protocol is built).
- **Phase 12.2, query optimization and concurrency:** planner statistics
  (common values and buckets per index, edge degrees by direction) kept in
  memory and rebuilt as data drifts; starts and pattern order by estimated
  cost; parsed statements cached; large read scans split across threads.
- **Phase 12.3, batched execution:** a match feeds its aggregates in
  batches turned into columns, folding into one accumulator per group
  instead of holding rows (2.3 to 2.9 times faster grouped; millions of
  matches counted within the limits).
- **Phase 12.4, batched and parallel traversal:** patterns grow from batches
  of starts one step at a time, reading each node, edge list and record
  once for every branch, with large frontiers spread over the cores (two
  hops from every company 9.5 → 2.9 s).
- **Phase 12.5, compact storage:** layout 13 stores adjacency entries
  compactly (19% smaller) and every vector also as int8; searches walk the
  int8 copies and rescore exactly, at the same recall (1 M vectors in 512
  MiB: about 5 ms to 3.5 to 4.2 ms a search).
- **Phase 13.1, replication log:** with the log on, every committed write
  transaction is kept as the effects it had (records' final values, the
  change log entries); another database applies the entries in order and
  holds the same data, catching up from a backup and the entries after it.
- **Phase 13.2, Raft:** servers form a cluster (`[cluster]` in
  `grafeno.toml`): a leader elected with Raft (written here, tested by a
  deterministic simulation of thousands of failure schedules) orders the
  writes, a majority must hold each before it returns, and a new leader
  takes over in under a second when one fails.
- **Phase 13.3, failover:** any member takes any request (followers
  forward writes to the leader), reads choose their consistency (local,
  bounded, linearizable), clients that know several members move on when
  one dies and read their own writes, and keyed writes run once. Fault
  injection (members cut off, restarted and killed) checked by a
  linearizability checker found no anomaly; writes resume about half a
  second after the leader dies.
- **Phase 13.4, parallel queries:** a costly read is split across the
  members, each taking a share of its start at the same log position,
  and merged into exactly the rows one server returns (sums no longer
  depend on row order); aggregates over 200 000 people run 2.9 times
  faster on five members of three threads than on one.
- **Phase 13.5, several groups:** a cluster runs more Raft groups, each
  with its own leader and database, and places namespaces on them (write
  throughput 109, 153 and 181 a second with one, two and three groups on
  one machine); a namespace moves between groups while it is written,
  pausing writes for tens of milliseconds and losing none.
- **Phase 13.6, operating a cluster:** status and `/metrics` per Raft
  group (elections, leader changes, snapshots, each member's lag), a
  Cluster view in Grafeno Studio, rolling upgrades where the leader writes
  no format an older member cannot read, backups from followers, new
  clusters and members from backups, a three-member compose and runbooks
  (a member lost, a majority lost, a disk replaced, an upgrade), each run
  under load with a linearizability checker watching.

- **Phase 14.1, accounts and sign-in:** users sign in with a password
  (Argon2id, a password policy, lockouts) and get sessions that renew
  and can be ended; service accounts hold API keys, which can expire, be
  rotated and be scoped to some namespaces or to reading; the Studio, the
  shell and the TypeScript client sign in. Existing keys became service
  accounts on their own.

- **Phase 14.2, roles and privileges:** a catalog of privileges on the
  server, namespaces, labels and properties; roles that hold privileges,
  the built-in levels and other roles, granted to accounts; a denial
  wins over any grant, so "everything in `team-*` but `team-secret`" is
  one grant and one denial. Rights are computed once per change and a
  check costs about 100 ns with thousands of roles.

- **Phase 14.3, managing access:** `CREATE USER`, `ALTER USER` (password,
  status, expiry), `CREATE ROLE`, `GRANT`, `DENY`, `REVOKE`, `GRANT ROLE`
  and the `SHOW` statements in GQ, run by accounts that may manage
  accounts or roles; `EXPLAIN` names the privilege a statement lacks; the
  same through `/v1/admin`, `grafeno roles`, `grafeno grants`, the
  TypeScript client and an Access view in the Studio.

- **Phase 14.4, enforcement down to records:** what a caller may not read
  is absent everywhere (queries and their counts, paths and procedures,
  searches, recall, algorithms, exports, the change log, statistics), as
  if it did not exist, and hidden properties are absent from records;
  every change is checked against labels, properties and operations;
  record policies limit a role to the records that match a condition
  (`@recorded_by = current_user()`, `team = $account.team`).

- **Phase 14.5, audit:** an audit log of security events (sign-ins and
  their failures, lockouts, sessions, keys, every change to accounts and
  roles, refused requests) with who, how, from where and what, each once,
  replicated and backed up; read with the `audit` privilege as pages, a
  stream, JSON Lines or `db.audit()` in GQ.

- **Phase 15.1, the engine alone:** the engine has no interface; Grafeno
  Studio and the desktop app moved to their own repository (Grafeno Studio), to
  be installed on their own as a separate product. The engine's
  packages install the engine only.

- **Phase 15.2, the engine installed like a database server:** a
  one-liner that adds signed APT and DNF repositories and installs the
  package (x86_64 and aarch64), or the static binary with a system or user
  service, checking checksums and signatures; `grafeno setup` (several
  instances on one machine, `grafeno@<name>.service`), `grafeno instances`
  and `grafeno doctor`; the engine copies a database before upgrading its
  layout, and `grafeno instances rollback` goes back; AppArmor and SELinux
  policies; a guided Windows setup with a virtual service account, the
  first administrator, the Event Log and winget manifests; all of it
  installed, upgraded and removed on clean machines.

- **Phase 15.3, the API as the only contract:** `GET /v1/about` (version,
  API version, capabilities) and `x-grafeno-api` on every response,
  `GET /v1/settings`, `grafeno restore`; a test that every route is
  documented; the TypeScript client 0.2.0 checks engines (`connect()`,
  `supports()`, older engines told from `/health`) and is run against the
  real engine binaries of each supported version.

- **Phase 15.4, the desktop app without an engine:** Grafeno Desktop (in
  the Grafeno Studio repository) links no engine code; it opens files by starting the installed
  engine (`grafeno serve --stop-with-stdin` on a loopback port with a
  token of its own) and talks to every engine over HTTP, with saved
  servers, the local service, clusters and several windows; measured
  against Electron and kept on Tauri.

- **Phase 15.5, two products, two releases:** the engine (`v…`
  tags, its CHANGELOG.md) and Grafeno Studio with Grafeno Desktop
  (`desktop-v…` tags, its own change log with the engines it works with)
  release apart, and install side by side without touching each other's
  files or data, as tests on Debian and Fedora show.

- **Phase 16.1, a free license and documentation about Grafeno only:** MIT
  or Apache-2.0 everywhere a package carries metadata; the documentation
  and sources name no other product (a test checks it); GQ is named on its
  own; the compatibility aliases of the text functions are gone.
- **Phase 16.2, null as a stored value:** setting a property to `null`
  keeps the key; `REMOVE` and `remove_props` delete it; `hasKey`; indexes
  hold no nulls, and unique indexes take several.

Next, the rest of phase 16 (data model, search, analysis and events), then
phase 17: integrations (client libraries for more languages). The plan is in
[docs/phases](phases/README.md), one file per phase.
