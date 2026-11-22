# Changes to the Grafeno engine

The engine (`grafeno`: the server, the shell, the administration commands
and its installers) has its own versions and releases, tagged
`v<version>`; Grafeno Studio and Grafeno Desktop have theirs, in their own
repository. A version's section here is its release's notes;
the release workflow refuses a tag whose version has no section.

What the engine speaks to clients is versioned apart from it: the HTTP
API version (`GET /v1/about`, now 1) changes only when a change would
break a client of the previous one, and the database files' layout
version (now 19) is upgraded in place, with a copy kept first.

## Unreleased

Hyperedges, first stage: facts that relate two or more nodes at once, each
in a role, stored as records of their own with an incidence index, in the
Rust API of the `grafeno` crate (docs/hyperedges.md), in GQ, and in the
replication log, so clusters and replicas hold them, and in HTTP routes
and MCP tools of their own; the change log and event subscriptions
follow them, and JSON lines exports carry them. Recall does not know them:
an experiment found event nodes answer as well (docs/benchmarks.md).

- **Fixed**: a memory a newer one `supersedes` read as current. A
  context (`context`, `palace_enter`, walks with a budget) now says
  `superseded by …` in its heading and lists `superseded_by` (a new
  field of `ContextItem`, left out of JSON when empty); `palace_enter`
  puts such memories after the current ones; `palace_reflect` lists them
  under `stale` as "replaced by …", with the reason, instead of finding
  only memories overtaken by their words. A `supersedes` fact that no
  longer holds marks nothing. `knowledge::replaced_by` gives what
  replaces a memory at a time.
- **Added**: not null, type and functional rules on hyperedges (`CREATE
  CONSTRAINT FOR HYPEREDGE h:label REQUIRE h.key IS NOT NULL | IS :: type
  | IS FUNCTIONAL`, `hyperedges` on `Constraint::Exists`, `Typed` and
  `Functional`), checked like those on nodes (at the end of a GQ
  statement); `hyperedge_property_history`, GQ `history(h, key)` and
  `GET /v1/{ns}/hyperedges/{id}/history/{key}`. `Constraint::kind` gives
  the record kind; a replication entry declaring a rule on hyperedges has
  format 8. Code that builds these variants adds the field.
- **Added**: CSV and GraphML carry hyperedges: a third CSV file
  (`export_hyperedges_csv`, `import_hyperedges_csv`,
  `/v1/{ns}/export/hyperedges.csv` and `/import/hyperedges.csv`,
  `grafeno import --hyperedges`, written by `grafeno export --format csv`
  when there are hyperedges), and GraphML's own `<hyperedge>` elements,
  roles in a `roles` key; `export_graphml_all` counts them, and
  `InterchangeStats.hyperedges` (left out when 0).
- **Added**: `POST /v1/{ns}/batch` takes `create_hyperedge` (with a `ref`
  of its own, and participants that may be `$name`s), `update_hyperedge`
  (properties and participants), `delete_hyperedge`,
  `invalidate_hyperedge` and `retract_hyperedge`, and `delete_node` takes
  `hyperedges` (`restrict`, `cascade` or `detach`). Hyperedge names come
  back in `hyperedge_refs`, left out when there are none; a name stands for
  one record, and a node's name where a hyperedge goes fails.
- **Added**: `HyperEdge`, `Participant`, `NewHyperEdge` and `HyperEdgeId`;
  on `WriteTx`, `create_hyperedge`, `set_hyperedge_props`,
  `remove_hyperedge_props`, `set_hyperedge_participants`,
  `invalidate_hyperedge`, `retract_hyperedge`, `delete_hyperedge` and
  `delete_node_with` (`OnHyperEdges::Restrict`, `Cascade` or `Detach`); on
  `ReadTx`, `hyperedge`, `node_hyperedges`, `hyperedges_with_label` (in
  pages), `hyperedges_by_participants` (`ParticipantMatch::Contains` or
  `Exact`) and `hyperedges_page`, through the snapshot's view;
  `Grafeno::set_max_participants` (256 by default). `check` and
  `rebuild_indexes` cover the hyperedge indexes, and backups copy them.
- **Changed**: the database layout is 19 (three new tables, empty until
  used). Files of earlier layouts are upgraded when opened, with a copy
  kept first; Grafeno 0.4 refuses a file of layout 19. `delete_node`
  refuses a node that takes part in a hyperedge (`Error::Conflict`), and so
  do the routes and GQ `DELETE`. `Error` has a new
  variant, `HyperEdgeNotFound`; `CheckReport` and `RebuildReport` have
  hyperedge counts.
- **Added**: GQ on hyperedges: `[OPTIONAL] MATCH HYPEREDGE h:label {…}
  [(a) AS role, (b)] [EXACT]` with `VALID AT`, `KNOWN AT` and `WHERE`,
  starting from a bound participant's incidence list when there is one;
  `CREATE HYPEREDGE`; `SET`, `REMOVE`, `DELETE` and `RETRACT` of a
  hyperedge; `participants(h)`; `id`, `type`, `keys`, `properties` and the
  pseudo-properties of a hyperedge. Write statistics count hyperedges.
  The engine also reads hyperedge ids by node, label and page
  (`hyperedge_ids_of`, `hyperedge_ids_with_label`, `hyperedge_ids`).
- **Added**: GQ `MERGE HYPEREDGE h:label {…} [(a) AS role, (b)] [ON CREATE
  SET …] [ON MATCH SET …]`: the hyperedges holding now with that label,
  those properties and exactly those (node, role) pairs in any order, or a
  new one, looked up and written in the statement's write transaction, so
  concurrent merges make one.
- **Added**: property indexes and unique or key constraints on
  hyperedges: `CREATE INDEX FOR HYPEREDGE h:label ON (h.key, …)`, `CREATE
  CONSTRAINT FOR HYPEREDGE h:label REQUIRE h.key IS UNIQUE | IS KEY`,
  `IndexDef::on_hyperedges` (`hyperedges: true`), `IndexDef::kind` and
  `RecordKind`, and `index_seek_hyperedges` on `ReadTx` and `WriteTx`.
  Every hyperedge write keeps them; `check`, `rebuild_indexes`, exports
  and replication cover them; `MATCH HYPEREDGE` starts from one when its
  pattern or `WHERE` fix the first properties. A replication entry that
  declares one has format 8 and an export that holds one version 3.
- **Added**: two reproducible measurements, `hyperedge_index_bench`
  (lookups by property on 100 000 hyperedges, with and without an index)
  and `hyperedge_recall_eval` (recall strategies with and without
  hyperedges, against event nodes, on 92 questions in English and
  Spanish), with their numbers in docs/benchmarks.md and docs/data.
- **Changed**: GQ `DETACH DELETE` takes a node out of its hyperedges
  (deleting those left with one node); `DELETE` of a node in a hyperedge
  fails. Query values have a new variant, `Value::HyperEdge`.
- **Added**: the replication log carries hyperedges (`Op::HyperEdge`):
  members apply them and derive their indexes, and a namespace moving
  between cluster groups takes them along. Log entries that carry one have
  format 8; members of 0.4 refuse them, and a cluster with such a member
  refuses hyperedge writes until every member is upgraded.
- **Added**: HTTP routes `/v1/{ns}/hyperedges` (create, list, find by
  participants, read, patch, invalidate, retract, delete) and
  `/v1/{ns}/nodes/{id}/hyperedges`; the `hyperedges` capability; MCP tools
  `relate_group`, `find_group` and `forget_group`.
- **Changed**: `DELETE /v1/{ns}/nodes/{id}` refuses a node in a hyperedge
  with 409 unless `?hyperedges=cascade` or `detach` says what to do.
- **Added**: the change log records hyperedges (`hyperedge`, its label and
  its `nodes` in each entry), through the reader's view; event
  subscriptions follow them with `ON HYPEREDGES` (`hyperedges`), with
  their properties before and after.
- **Added**: JSON lines exports carry hyperedges (`"type":"hyperedge"`,
  after each namespace's edges) and imports restore them
  (`WriteTx::restore_hyperedge`); `DumpStats` counts them. An export with
  a hyperedge has version 3; one without keeps version 2, which 0.4
  reads.
- **Added**: merging nodes moves the merged node's hyperedges to the kept
  one (deleting one only the two were in), and `undo_merge` restores them;
  `MergeReport` counts `hyperedges_moved` and `hyperedges_dropped`, and the
  merge record keeps the hyperedges as they were.

## 0.4.0 (2026-11-20)

Memory that holds up over time and across languages, and an
administrator's token that changes itself. A namespace can say its
language (Spanish or English) so recall finds other forms of the words;
replaced memories rank below what replaced them; hybrid search, graph
expansion and recall by label no longer lose the answer they found; and
the context a model reads cannot be forged by what a memory says. Each
was found by new measurements on agent questions and thousands of
conversations, kept as tests. The database layout (18), the HTTP API
version (1) and the log entry format are those of 0.3: files open as
they are and clients of 0.3 keep working. In a cluster, set a
namespace's `language` once every member runs 0.4: a member of 0.3
copies the setting without indexing its texts again.

- **Added**: a namespace's `language` (`en` or `es`, on POST and PATCH
  `/v1/namespaces`): node texts and questions are reduced to their stem
  in it, verbs too, so "decidimos" finds "decidir"; setting it indexes
  the namespace's texts again, and replicas do the same.
- **Fixed**: graph expansion in recall took the first place from the
  best match (61 % to 36 % on the new agent questions): a node the graph
  reaches now rises up to just below the memory that led to it.
- **Fixed**: recall with `labels` filtered the best 50 of everything, so
  a decision a crowd of conversations outscored was not found; it now
  searches within the labels too (an old decision in the context 95 % of
  the time among 3 000 or 15 000 conversations, against 72 % and 55 %).
- **Fixed**: a stored memory written to look like the context's own
  structure (an item, a section, a fact) could forge it in what a model
  reads; such lines are now escaped and control characters dropped. The
  shell shows control characters in cells instead of letting them drive
  the terminal.
- **Added**: retrieval fixtures (agent questions in Spanish and English,
  thousands of conversations) with their embeddings, measurements of
  retrieval quality, long-term memory and many writers, recovery drills
  on real processes, and a check of every namespace route against
  another namespace's key; see [benchmarks](docs/benchmarks.md).
- **Added**: the static token changes itself. `POST /v1/auth/token`,
  called with the token and given the current one again, replaces it at
  once and writes the new one to the settings file it was read from;
  `grafeno config token --rotate` asks the running server. The token
  stays the administrator's credential, with no account; a token given by
  `--token` or `GRAFENO_TOKEN` is changed there.
- **Changed**: `recall` and `context` rank a memory that a `supersedes`
  fact true now replaced below what replaced it: its score is multiplied
  by `superseded_weight` (0.2 by default, 1 to leave it as it was),
  whoever recorded either, and a `superseded` signal says so.
- **Fixed**: hybrid search lost strong matches only one list found (a
  memory in another language than the question, say), which fell below
  every weak match of the text list. A node one list lacks now counts
  there as if it ranked four times lower (rank fusion), or with half its
  scaled score (weighted).
- **Fixed**: `maintain` did not find two memories with the same text when
  there were no embeddings; the same text (case, accents and punctuation
  aside, three words at least) is now a duplicate.
- **Changed**: compacting a namespace's vector graph
  (`POST /v1/{ns}/compact`) needs `update` on the namespace instead of
  the schema right, so whoever writes the namespace keeps it in shape.

## 0.3.1 (2026-11-20)

Security fixes from a review of 0.3.0, on access, the network and
packaging; see [docs/security.md](docs/security.md#review-of-version-030).
The database layout (18), the HTTP API version (1) and the log entry
format are those of 0.3.0. A compose cluster now needs
`GRAFENO_CLUSTER_SECRET`, and a member refuses the example settings'
secret.

- **Fixed**: on a cluster member with several groups and no static token,
  a request to a group whose file had no account made the server open
  again, and every caller its owner. Each group keeps its own keys;
  whether the server is open is decided by the first group.
- **Fixed**: a group import by an account with the `cluster` privilege
  could write accounts, keys, sessions and roles; only a superuser's
  carries them now.
- **Fixed**: `POST /v1/{ns}/batch` retracted and invalidated facts without
  the `retract` right, and recorded no author; it needs the right and
  writes as its caller.
- **Fixed**: a GQ idempotency key returned the result of another caller's
  statement; a key now stands for its caller and statement.
- **Fixed**: a webhook subscription delivered events its creator could not
  read; it needs read access to the whole namespace.
- **Fixed**: GraphML and CSV imports ignored the caller's write
  restrictions and wrote as nobody; they write as their caller.
- **Fixed**: a refresh with a wrong secret ended the session it named;
  only a reused refresh token ends it now.
- **Fixed**: `recall` and `context` with `touch` recorded accesses for a
  caller that may only read; `touch` needs the `update` right.
- **Fixed**: an account named `owner` could use the static token's
  explicit transactions.
- **Fixed**: a failed request task sent its message to the client; it
  gets a reference, and the message goes to the log.
- **Fixed**: secrets are compared in a time that tells nothing of their
  length.
- **Fixed**: a webhook delivery checked that its name had only public
  addresses, then let the HTTP client, or a proxy of the environment, look
  it up again, and could reach an address nobody checked. It connects to
  the addresses checked, through no proxy; with `[background]
  webhooks_to_private = true` deliveries go as before.
- **Fixed**: IPv6 addresses that carry an IPv4 one (NAT64, 6to4, the
  compatible and translated forms) passed the webhook check whatever that
  address was; they are as public as it is. Teredo and local-use NAT64
  are refused.
- **Fixed**: with the cluster secret, `POST /raft` read a body of any size
  and kept each snapshot Raft refused until the process ended. A message
  is refused past 1 GiB before it is read, only a snapshot carries data
  after its message, and a snapshot Raft does not install is dropped.
- **Changed**: a cluster member refuses to start with the members' secret
  of the example settings (`change-this-shared-secret`), which it only
  warned about. `GRAFENO_CLUSTER_SECRET` gives `[cluster] secret` from the
  environment; the compose cluster requires it, beside `GRAFENO_TOKEN`.
- **Fixed**: `install.sh --service` without root wrote a user unit with no
  confinement; it now writes only its data folder, with the system and
  the home read-only.
- **Changed**: releases are built with pinned actions (by commit), Rust
  1.99.1, images by digest, packaging tools by version and the zig wheel
  by hash; only the jobs that publish can write to the repository, and
  the Windows signing certificate reaches a tag's build only.
- **Fixed**: while an explicit write transaction was open, a refused
  request (a wrong credential, a missing right, a failed sign-in) was not
  answered until the transaction ended, since its audit event waited for
  the writer. The answer waits for the event 200 ms at most; the event is
  written once the writer is free, with 64 waiting at most and the rest
  counted as left out.

## 0.3.0 (2026-11-13)

Agent memory as a place, and knowledge that says how it is known: mental
palaces, provenance and kinds of knowledge, explainable retrieval,
knowledge over time and a reflection on what is and is not known, all
reachable from MCP and, through the new `tools` capability, over HTTP.
The database layout (18), the HTTP API version (1) and the log entry
format are those of 0.2: files open as they are, clients of 0.2 keep
working, and 0.2 and 0.3 members can share a cluster while it is upgraded.
Security fixes from two outside reviews are included.

- **Added**: the `tools` capability in `GET /v1/about`, so clients know
  the memory tools run over HTTP (`GET /v1/{ns}/tools`,
  `POST /v1/{ns}/tools/{name}`); they did since the cognitive memory's
  phase 5, unannounced.
- **Checked**: the graph engine on its own (phase 13). `grafeno-core`,
  `grafeno` and `grafeno-query` depend on nothing of the cognitive layer,
  and their tests pass alone: 290 passed, none failed.
- **Added**: the quality of knowledge (phase 12). `palace_reflect` names
  the sources behind each item it lists, counts records by the evidence
  behind them (conflicting, obsolete, verified, supported, and with no
  source by origin: counts, never probabilities), lists what was stated
  with no source apart from what is uncertain, and finds what a kind's
  expected shape asks for and a memory lacks: shapes declared once as a
  memory labeled `Shape`, or given in the call (`expected`).
- **Added**: the palace as a view (phase 11). `palace_route` follows given
  relations in order from a node (forwards or backwards, once or repeated,
  towards a goal, at a past time): a decision's versions, a chain of
  dependencies, who maintains what an incident hit. `get_node` with
  `reconstruct` rebuilds a node's context in one call: its places in every
  palace, current facts, timeline, versions, justification and
  reflection. `palace_review` also proposes retracting facts recorded
  twice, and points at memories related to nothing else of their room and
  rooms whose memories are not related at all; nothing is applied.
- **Added**: knowledge over time (phase 10). `timeline` answers the
  `history` of a memory replaced through `supersedes` (a decision changed
  several times): every version, the current one last, with the reason
  (`relate`'s new `reason`) and evidence of each change. With `compare`,
  it lists the facts that ended, began or changed between two times.
  `palace_reflect` tells a change of state (a newer fact took an older
  one's place under a constraint) from a contradiction (both stand,
  flagged or recorded with `contradicts`, which it now lists too).
- **Fixed**: `palace_reflect` looked for overtaken memories over the
  whole namespace; it looks among the scope's own.
- **Added**: explainable retrieval (phase 9). `palace_walk` follows only
  some relations (`relations`) or the facts true at a time (`valid_at`),
  and each memory says how it was found (`found_by`) and, apart, what
  supports it or puts it in doubt (`justification`: sources, how it is
  known, supported by, contradicted by, superseded by); `recall` answers
  the justification too. `recall`, `context` and `palace_walk` take `skip`
  for the next part; contexts pack related memories together and say their
  token count is an estimate (`tokens_estimated`). A measure of questions
  that join four memories is in `docs/benchmarks.md`.
- **Changed**: the MCP tools `recall` and `context` scale each search's
  scores to 0..1 before fusing them (`fusion: "rank"` keeps the old way):
  under rank fusion what the best match is related to never outranked
  weak matches. A walk by relevance gives what relations reach half its
  origin's relevance per hop.
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
