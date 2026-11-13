# Grafeno Cognitive Memory: implementation plan

**Status:** every phase, 0 to 6, is done. What is left of each is said
under it.

What comes after, phases 7 to 13, starts from an audit of what these
phases built: [SPECIALIZATION_AUDIT.md](SPECIALIZATION_AUDIT.md),
[ARCHITECTURE.md](ARCHITECTURE.md) and [ROADMAP.md](ROADMAP.md).

The aim is for Grafeno to be the persistent memory and knowledge of any
agent, through six phases of the Mental Palace, and to stay independent
of any agent, model or provider. An agent that uses it as its memory is a
separate program, outside this repository, and comes after the phases.

This plan is the result of reading the repository as it is at commit
`907ecca`. For each phase it says what already exists, what is partly
there and what is missing, and proposes the smallest change that closes
the gap. Most of what the six phases ask for exists; the plan is short
where it does.

- [Principles](#principles)
- [What Grafeno is today](#what-grafeno-is-today)
- [Baseline](#baseline)
- [The six phases at a glance](#the-six-phases-at-a-glance)
- [Phase 1: structural palace](#phase-1-structural-palace-done)
- [Phase 2: cognitive memory](#phase-2-cognitive-memory)
- [Phase 3: retrieval](#phase-3-retrieval)
- [Phase 4: consolidation and evolution](#phase-4-consolidation-and-evolution)
- [Phase 5: memory for several agents](#phase-5-memory-for-several-agents)
- [Phase 6: metacognition](#phase-6-metacognition)
- [Order of work](#order-of-work)
- [Risks](#risks)
- [Decisions to take](#decisions-to-take)
- [After the phases: an agent](#after-the-phases-an-agent)

## Principles

- Reuse what exists before adding anything.
- No model inside the engine. What needs judgement (reading a text for
  its relations, classifying a memory) is done by the agent and handed to
  Grafeno through a structured tool.
- No change to the file layout unless a phase cannot be done without it.
  Every proposal below is nodes, edges and properties.
- No existing tool, route or result changes its meaning.
- Small stages, each with tests, and a phase is not called done until
  its tests ran.
- Nothing is deleted on its own, and what rewrites knowledge can be
  undone.

## What Grafeno is today

| Piece | Where | What it is |
| --- | --- | --- |
| Model and errors | `crates/grafeno-core` | Nodes (labels, properties, `sources`, `recorded_by`), edges with valid time and record time. |
| Engine | `crates/grafeno` | One file, one writer and many readers on snapshots; namespaces; full-text and vector indexes; traversals; constraints with contradictions; merges that can be undone; names and entity resolution; rights down to records; audit; change log; backup. |
| Query language | `crates/grafeno-query` | GQ, with time travel. |
| Memory | `crates/grafeno-memory` | Recall, context, extraction and reconciliation, communities, maintenance, checkpoints, documents, timelines, palaces. |
| Server | `crates/grafeno-server` | `grafeno serve` (HTTP API with accounts, keys, roles and audit), `grafeno mcp` (MCP over stdio), `grafeno shell`. |
| Cluster | `crates/grafeno-cluster`, `crates/grafeno-raft` | Several servers on the same data. |
| Grafeno Studio | Its own repository | An interface over the HTTP API. Not in this repository. |

The MCP server has 33 tools:

| For | Tools |
| --- | --- |
| Storing and connecting | `remember`, `relate`, `update`, `forget` |
| Finding and reading | `recall`, `context`, `get_node`, `query`, `overview` |
| Long texts | `ingest` |
| Sessions | `checkpoint`, `resume` |
| Time | `timeline`, `contradictions`, `changes` |
| Upkeep | `maintain`, `feedback`, `merge`, `undo_merge`, `consolidate`, `find_matches` |
| Mentions | `extract`, `add_mentions`, `review`, `resolve_review` |
| Summaries | `summarize_communities`, `set_summary` |
| Palaces | `palace_create`, `palace_create_room`, `palace_anchor`, `palace_map`, `palace_walk`, `palace_enter` |

Two facts about the servers shape phase 5:

- `grafeno mcp` opens the database file itself. It has no option to
  reach a running server, so an MCP agent and `grafeno serve`, or two MCP
  agents, cannot use one file.
- `grafeno serve` owns its file and serves many clients, with accounts,
  keys, roles and an audit log. It has routes for most of the memory
  (recall, context, extract, review, merges, duplicates, weak memories,
  consolidation, communities, contradictions, ingest, changes) and none
  for checkpoints, timelines, feedback or palaces.

## Baseline

Run on Windows on the tree that became `907ecca`:

| Check | Result |
| --- | --- |
| `cargo test --workspace --no-fail-fast` | 473 passed, 0 failed |
| `cargo clippy --workspace --all-targets` | no warnings |
| `cargo fmt --all --check` | clean |
| `cargo doc -p grafeno-memory` with warnings as errors | clean |

Two things seen while running them, neither a defect of the code:

- On this machine the operating system's application control now and
  then refuses to start a freshly built test program (`os error 4551`).
  The program passes when run again.
- One test of the cluster's groups has failed now and then in CI before
  this work.

A phase's tests are added to this baseline; a phase is done when the
whole of it is green.

## The six phases at a glance

| Phase | Exists | Partly | Missing |
| --- | --- | --- | --- |
| 1. Structural palace | Everything | — | — |
| 2. Cognitive memory | Everything, since phase 2 | — | — |
| 3. Retrieval | Everything, since phase 3 | — | — |
| 4. Consolidation | Everything, since phase 4 | — | — |
| 5. Several agents | Everything but the TypeScript client and clusters, since phase 5 | — | The tools route on a cluster; the TypeScript client |
| 6. Metacognition | Everything, since phase 6 | — | — |

## Phase 1: structural palace (done)

Built and tested before this plan: palaces, rooms, loci and anchors as
ordinary nodes and edges; the map, walks and entering a room; six MCP
tools. See [../mental-palace.md](../mental-palace.md) for what it is and
[../mental-palace-roadmap.md](../mental-palace-roadmap.md) for how it was
built.

Acceptance, as asked: make a palace, store and anchor memories, restart
and get the same structure back. Covered by
`a_palace_survives_closing_and_reopening_the_file`,
`entering_a_room_after_a_restart_gives_its_memories_and_its_checkpoint`
and `a_read_only_server_reads_palaces_and_writes_none` (a second server
on a file the first one wrote).

Left open from it, and placed in a later phase here:

| Open | Goes to |
| --- | --- |
| Moving between rooms (doors, routes) | Phase 3 |
| Deleting a room with what is under it | Phase 4 |
| Palaces over HTTP | Phase 5 |

## Phase 2: cognitive memory (done)

Built as proposed below, with the recommended decisions: two properties,
four kinds, `add_mentions` extended and `feedback` extended, no new tool.
What it does and its limits: [cognitive-memory.md](cognitive-memory.md).
Tests: `crates/grafeno-memory/tests/knowledge.rs` (six) and
`a_technical_description_becomes_entities_relations_and_places` in
`crates/grafeno-server/tests/mcp.rs`, the acceptance below.

Two things turned out differently from the proposal:

- `palace_verify` was not needed: `feedback` takes `verified` with a
  `source` and a `certainty`, and keeps what the record was before in
  `verified_from`.
- An agent's relation may say it was `stated` by the text; `inferred` is
  only the default.


What the phase asks for, against what is there:

| Asked | Today |
| --- | --- |
| Entities from memories and episodes | **Exists.** `extract` reads texts with no model (names, emails, URLs, handles, dates, amounts, recurring phrases); `add_mentions` takes an agent's own reading; ambiguous ones wait in `review`. |
| Relations between entities | **Partly.** `relate` records one fact, with dates and sources. There is no call that takes everything an agent read in a text at once. |
| Kinds of memory: facts, decisions, experiences, tasks | **Partly.** A memory has labels and `remember` takes any; `Memory`, `Note`, `Episode`, `Summary` and `Fact` are used by convention. Nothing names the four kinds or filters by them. |
| Similar memories | **Exists.** `find_duplicates` by name and by meaning, text similarity, `find_matches`. |
| Entities and memories in rooms and loci | **Exists.** `palace_anchor` takes any node that is not part of a palace. |
| Provenance | **Exists.** `sources` on nodes and edges, `recorded_by`, and a mention says whether extraction or an agent made it. |
| Certainty; inferred apart from verified | **Missing.** No node or edge says how sure it is or how it came to be known. |

Proposed, smallest first:

1. **Certainty and origin on facts and memories**, as two properties
   with fixed meanings, written by `remember` and `relate` when given and
   shown by `get_node`, `recall` and `context`:
   - `certainty`: a number from 0 to 1. Absent means not stated.
   - `origin`: `stated` (someone said it), `observed` (the agent saw it:
     a file, an output), `inferred` (the agent concluded it) or
     `verified` (checked against a source, which `sources` names).

   No layout change: they are properties. Existing records have neither
   and read as "not stated".
2. **Kinds of memory** as four labels with a meaning: `Fact`, `Decision`,
   `Experience` and `Task`, beside the ones in use. `remember` documents
   them; `recall` already filters by label. No new tool.
3. **One call for an agent's reading**: extend `add_mentions` so each
   mention may carry relations to other mentions of the same call
   (`label`, `to`, `certainty`), recorded as edges with `origin`
   `inferred` and the memory as their source. The agent does the
   reading; Grafeno resolves the entities with the rules it has and
   stores the result. This is the structured contract, with no model in
   the engine.
4. **`palace_verify`** (phase 6 names it; it belongs with the data):
   marks a fact or a memory `verified` with the source that confirms it,
   or `refuted`, keeping the earlier value in its history. To check
   first: `feedback` already takes a verdict (`useful`, `obsolete`,
   `wrong`). If a fourth verdict `verified` with a `source` says the
   same, extend `feedback` and add no tool.

Not proposed: a model in the engine, automatic classification of a
memory's kind, relations guessed from sentences by rules.

Acceptance:

- A technical description is ingested; the agent's reading goes in with
  one call; the entities, the relations between them, the memory they
  came from and their certainty are read back with `get_node`, `query`
  and `recall`.
- An inferred relation and a verified one are told apart in every tool
  that shows a fact.
- Records written before this phase read as before.
- The entities and the memory are anchored in a room and found by
  `palace_walk`.

## Phase 3: retrieval (done)

Built as proposed below; what it does is in
[../mental-palace.md](../mental-palace.md#doors-relevance-and-contexts),
and the measures in [../benchmarks.md](../benchmarks.md). Tests:
`crates/grafeno-memory/tests/palace_retrieval.rs` and
`walks_join_memories_follow_doors_and_leave_out_repeats` in
`crates/grafeno-server/tests/mcp.rs`. Decided on the way:

- Repeats are told by the share of words two texts have in common (85 %,
  normalized), not by the text similarity measures, which compare
  characters and cost more on long chunks. Off by default in the library,
  on in the tools.
- Relevance is the full-text score of the topic over what the walk
  reached, not recall's fused score: a walk has no embedding. Words are
  compared whole, so `invoice` does not match `invoices`.
- A door is followed one step, from the rooms the walk found, not from
  the rooms it reached through a door.


| Asked | Today |
| --- | --- |
| By the graph's relations | **Exists.** Recall's expansion, with hubs damped; `palace_walk`; GQ. |
| Semantic or hybrid search | **Exists.** Text and vectors fused by rank or by weighted scores; the embedding is the caller's. |
| Associative walks with bounded depth | **Exists.** `palace_walk`, up to 3 hops, 50 relations per node. |
| Ranking by relevance | **Exists** in recall. **Partly** in walks, which order by distance and then by strength. |
| No redundancy in results | **Missing.** Two chunks that say the same both come back. |
| Context in a token budget | **Exists.** `context`, `palace_enter`. |
| The routes and sources used | **Exists.** Recall's `why`; a walk's `via`, `origin` and `places`; a context item's `sources`. |

Proposed:

1. **Redundancy**: when packing a context, skip an item whose text is
   nearly the same as one already packed, by the text similarity the
   engine has (no model). An option of `context_for` and `context_of`,
   on by default in the tools, reporting how many were skipped.
2. **`palace_walk` by relevance**: with a topic, order what is reached by
   the recall score of the topic over those memories first and by
   strength second; add an optional `budget` that returns the walk as a
   packed context too. Both are additions; without them the tool answers
   as today.
3. **Doors**: a `leads_to` edge between two rooms, made with `relate`
   (no new tool), that `palace_walk` follows from a room to the rooms it
   leads to. Left from phase 1.
4. **Measured**: an example in `grafeno-memory` that builds a palace of
   a few thousand memories and reports latency and tokens of recall,
   walk and enter, beside the benchmarks there are.

Acceptance:

- Questions that need two or three memories joined are answered by a
  walk or a context that contains them, with the path that joined them.
- A context with duplicated chunks holds one of each and says how many
  it left out.
- The example prints latency and tokens; the numbers go in
  `docs/benchmarks.md` with how they were taken.

## Phase 4: consolidation and evolution (done)

Built as proposed below; what it does is in
[../mental-palace.md](../mental-palace.md#review-removal-and-undoing).
Tests: `crates/grafeno-memory/tests/palace_evolution.rs` (six, the
acceptance among them) and
`palaces_are_reviewed_rooms_removed_and_consolidations_undone` in
`crates/grafeno-server/tests/mcp.rs`. Decided on the way:

- Removing downwards is `forget` on a palace, a room or a locus, with
  `dry_run`, and no tool of its own. Before, `forget` on a room left its
  loci behind; now it takes them.
- A summary records which memories it archived (`archived_sources`), so
  that `consolidate` with `undo` brings back those and not one archived
  before it. Summaries written before this phase cannot be undone this
  way.
- A merge was already recorded and undone (`undo_merge`); a
  consolidation's summary appears in its sources' timelines as a
  `summarizes` fact, with who recorded it.
- Superseded memories are told by shared entities and shared words, with
  no model: a newer memory that only says the opposite in other words is
  not found.


| Asked | Today |
| --- | --- |
| Duplicates | **Exists.** `find_duplicates`, `maintain`, match rules. |
| Possible contradictions | **Exists** under a constraint (one employer at a time): the earlier fact ends, the new one is refused, or both are flagged. **Missing** without a constraint. |
| Validity in time | **Exists.** Valid time and record time on every fact; time travel in GQ. |
| Updating without losing history | **Exists.** Invalidate against retract; property history; timelines. |
| Consolidating redundant memories | **Exists.** `consolidate` writes a summary and archives; `merge` folds a duplicate, with `undo_merge`. |
| Obsolete memories | **Partly.** Weak memories (unimportant and unused) and `feedback` `obsolete`. Nothing looks at age against a newer memory on the same subject. |
| Reorganizing the palace | **Missing.** |

Proposed:

1. **`palace_review`** (reads only): for a palace or a room, proposals
   with their reasons, none applied: rooms with too many memories for
   one context; loci never used; memories anchored in a room whose
   community lies mostly in another; memories of a community with no
   room; two rooms whose memories overlap. Built on the community
   summaries and the maps there are.
2. **Superseded memories** in `maintain`: pairs of memories about the
   same entities where the newer says something the older does not,
   listed for the agent to judge; nothing is archived by it.
3. **Deleting a room or a palace downwards**: one call that removes a
   room's loci and anchors with it and never a memory, with a dry run
   that says what would go. Left from phase 1. To check first whether
   `forget` should grow a mode for it.
4. **A record of what consolidation did**: `consolidate` and `merge`
   already keep what they replaced. Check that each leaves who did it
   and when in a way `timeline` shows, and add it where it does not.

Not proposed: contradictions found by reading sentences, which needs a
model; that is the agent's, through `feedback` and `relate`.

Acceptance:

- Two statements in succession about the same thing (a person's
  employer in 2021 and in 2024) keep both, each with its dates and who
  recorded it, and `timeline` tells the order.
- A consolidation and a merge are each undone, and the memory is as it
  was.
- `palace_review` on a palace made badly on purpose proposes the fixes
  expected, and changes nothing.
- No memory is deleted or archived by a tool that was not asked to.

## Phase 5: memory for several agents (done)

Built as proposed below, with the recommended decision: one generic
route. What it does is in
[../mental-palace.md](../mental-palace.md#several-agents-one-memory).
Test: `crates/grafeno-server/tests/agents.rs`, the acceptance below with
real processes. Decided on the way:

- `POST /v1/{ns}/tools/{name}` runs a memory tool as the caller, and
  `GET /v1/{ns}/tools` lists those the caller may use. The route needs the
  right to read the namespace; a writing tool needs a right to write
  there, or it answers as on a read-only MCP server. The tool's
  namespace is the path's.
- The memory tools check rights by namespace. A caller whose rights reach
  labels, properties or records in that namespace is refused there and
  uses the API's own routes, which check each record.
- GQ does not run through the tools route: `grafeno mcp --url` sends the
  `query` tool to `POST /v1/{ns}/query`, which checks each statement.
- The tools route is refused on a cluster for now: its writes would not
  go through the leader.
- Step 4, the TypeScript client, belongs to its own repository and is
  not done.


| Asked | Today |
| --- | --- |
| An agent's identity apart from sessions, palaces and namespaces | **Exists** on the server: accounts and keys. Over MCP it is a name given at start (`--as`). |
| Private and shared palaces | **Partly.** A palace lives in a namespace, and a role grants reading or writing on a namespace, so a private namespace and a shared one already give private and shared palaces. Nothing documents or tests it as such. |
| Reading and writing rights | **Exists.** Roles and privileges, down to labels, properties and records. |
| Who made or changed each memory | **Exists.** `recorded_by` on nodes and edges; the change log; the audit log. |
| Concurrent access | **Exists** in one process: one writer, readers on snapshots. **Missing** across MCP processes. |
| MCP and HTTP | **Partly.** Both exist; MCP cannot reach a server, and HTTP lacks palaces, checkpoints, timelines and feedback. |
| Audit | **Exists.** `GET /v1/audit`. |

The file has one owner. That is how the engine works and it stays so:
the answer to several agents is that one `grafeno serve` owns the file
and every agent is its client.

Proposed:

1. **`grafeno mcp --url`**: the MCP server as a client of a running
   `grafeno serve`, signed in with a key, instead of opening a file.
   Every tool call becomes a request to the server, so rights, audit and
   `recorded_by` are the server's, and many agents share one file safely.
   `--db` stays as it is for one agent on one file. With `--db`, starting
   a second process on the same file already fails ("Database already
   open. Cannot acquire lock."); make that failure say to use `--url`.
2. **The routes the tools need**: checkpoints, timelines, feedback and
   palaces over HTTP, each calling the same library function the MCP
   tool calls. The simplest shape to evaluate first is one route that
   runs a memory tool by name with the caller's rights, so MCP over
   `--url` and the tools stay one list; against it, a route per feature
   in the style of the API there is. This is the largest piece of the
   plan.
3. **Shared palaces, stated and tested**: a guide and tests for two
   agents with their own keys, one namespace each and one shared, where
   one may write the shared palace and the other only read it.
4. **Client library**: palaces in the TypeScript client once the routes
   exist. In its own repository.

Acceptance:

- Two agents, each its own MCP process with its own key, on one server:
  both read a shared palace, the one with the right anchors a memory in
  it, the other is refused, and neither sees the other's private
  namespace.
- Each write shows who made it, in the record and in the audit log.
- Many writes at once from both lose none, and the file passes
  `grafeno check` afterwards.
- A second process on a file that is open is refused with a message that
  says what to do.

## Phase 6: metacognition (done)

Built as proposed below: one tool, `palace_reflect`, and questions as a
kind of memory. What it does is in
[../mental-palace.md](../mental-palace.md#reflection). Tests:
`crates/grafeno-memory/tests/reflect.rs`, whose first test is the
acceptance below, and `reflection_separates_what_is_known_from_what_is_not`
in `crates/grafeno-server/tests/mcp.rs`. Decided on the way:

- A question is closed by a relation `answers` from the memory that
  answers it, made with `relate`; no tool of its own.
- "Mentioned and never described" is a node only `mentions` reach, with
  no text and nothing but a name.
- The steps a checkpoint left are gaps of the room or node it is about;
  a topic has no checkpoint of its own.
- `palace_knowledge_gaps` and `palace_inspect` were not needed: the gaps
  are a section of the reflection, and inspecting a record is `get_node`
  and `timeline`.


What exists answers part of it: `maintain` reports health, duplicates,
weak memories and contradictions on record; `contradictions` lists what
clashed; `timeline` and `get_node` show a node's history and sources;
`overview` says what a namespace is about. None says what is thin,
unsupported or old about one topic.

Every answer of this phase is computed from what is stored and says how:
counts, dates and ids, not judgements. It depends on phase 2, since
certainty and origin are what it reports.

Proposed, as one tool before four:

1. **`palace_reflect`** (reads only): for a topic, a room or a node,
   - what is known: the memories and facts found, counted by kind and by
     origin (stated, observed, inferred, verified, not stated);
   - what is uncertain: facts with low certainty, inferred facts with no
     source, memories with no source;
   - what conflicts: contradictions on record that involve them, and
     facts flagged and not settled;
   - what may be stale: facts whose valid time ended, memories not
     updated or used for long, memories archived or marked obsolete;
   - what is missing: entities mentioned and never described, loci with
     nothing in them, questions an agent left (see below), checkpoints
     whose next steps were never followed by another;
   - what to check next, each item pointing at the ids above;
   - the limits of the answer: how many candidates were looked at, what
     the search could not see (no embeddings, a namespace not readable).
2. **Open questions** as a kind of memory: the label `Question`, stored
   with `remember`, anchored like any memory and closed by a relation to
   what answers it. It is how a gap an agent noticed is kept, with no
   new tool.
3. **`palace_verify`**: from phase 2.
4. **`palace_knowledge_gaps` and `palace_inspect`** are not proposed as
   tools of their own yet: the first is a part of `palace_reflect`, and
   the second is `get_node`, `palace_map` and `timeline`. If using
   `palace_reflect` shows its answer is too large, the gaps become their
   own tool then.

Not proposed: an engine that decides something is false, rewrites
knowledge or looks things up by itself. It lists; the agent acts, with
tools that leave a record.

Acceptance:

- A topic with partial information: the answer separates known facts,
  inferred ones, contradictions not settled, stale items and gaps, each
  with the ids it is about.
- Every item can be checked by reading the node or fact it names.
- A topic with nothing stored says so, and does not answer with the
  nearest unrelated memory.
- Nothing is written by the tool.

## Order of work

Each step is a stage of its own, with its tests, in this order:

| Step | What | Why here |
| --- | --- | --- |
| 2.1 | `certainty` and `origin` on facts and memories | Phases 3, 4 and 6 report them. |
| 2.2 | The four kinds of memory | Small; phase 6 counts by them. |
| 2.3 | An agent's reading in one call | The contract for a model outside the engine. |
| 2.4 | Verifying | Closes phase 2. |
| 3.1 | Redundancy in contexts | Independent. |
| 3.2 | Walks by relevance, with a budget | Builds on 3.1. |
| 3.3 | Doors between rooms | Independent. |
| 3.4 | Latency and tokens measured | After 3.1 to 3.3. |
| 4.1 | `palace_review` | Reads only. |
| 4.2 | Superseded memories | Reads only. |
| 4.3 | Deleting downwards | The one destructive call of the plan; dry run first. |
| 5.1 | The routes over HTTP | Before 5.2. |
| 5.2 | `grafeno mcp --url` | The piece that lets agents share a file. |
| 5.3 | Shared palaces tested with two agents | Acceptance of phase 5. |
| 6.1 | Open questions | Small. |
| 6.2 | `palace_reflect` | Last: it reports on everything before it. |

Phase 5 does not depend on phases 2 to 4 and may go first if several
agents are needed sooner.

## Risks

| Risk | What contains it |
| --- | --- |
| Too many tools. The server's own header asks for few, and there are 33. | The plan adds at most four (`palace_review`, `palace_reflect`, and `palace_verify` and a delete only if no existing tool can take them), and extends `add_mentions`, `palace_walk`, `feedback` and `maintain` instead. |
| New properties with a meaning (`certainty`, `origin`) clash with a database that used those names for something else. | They are read only where the tools write them; documented as reserved, as the palace labels are. |
| The HTTP surface for phase 5 is large and public. | One generic route evaluated before many; versioned with the API there is; covered by the contract tests. |
| An MCP process over `--url` behaves differently from one over `--db` (rights, errors, latency). | The same test suite runs against both. |
| Features that read every node of a namespace forget to leave palace nodes out. | `palace::is_structure` and the test that compares a namespace with and without a palace, extended with each feature. |
| Metacognition read as truth. | Every item names the records it was computed from; the tool's description says it counts and lists. |
| Persistence. | No layout change in any phase; every phase has a test over a file closed and opened again. |

## Decisions to take

These are the maintainer's. Each has a recommendation, which the work
follows unless the maintainer says otherwise:

| Decision | Step | Recommended |
| --- | --- | --- |
| The four origins and a certainty from 0 to 1 | 2.1 | **Taken:** yes; a record with neither is shown as "not stated". |
| Extending `add_mentions` or a new tool for an agent's reading | 2.3 | **Taken:** extended. |
| Extending `feedback` or a new `palace_verify` | 2.4 | **Taken:** extended. |
| One generic HTTP route for memory tools, or one route per feature | 5.1 | **Taken:** one generic route. |
| Phase 5 before phases 2 to 4 | — | No, unless several agents are needed sooner: phase 2 is what phases 3, 4 and 6 report on. |
| Numbered phases in `docs/phases` | — | The maintainer's alone. |

A second MCP process on a file another one holds open is already
refused, with `storage error: Database already open. Cannot acquire
lock.` (checked while writing this plan); step 5.2 makes that message
point at `--url`.

## After the phases: an agent

An agent or orchestrator that uses Grafeno as its memory is a separate
program in its own repository. It talks to Grafeno over MCP or HTTP like
any other client, and Grafeno knows nothing of it. Nothing in this plan
builds it, and nothing in it is needed by the phases. What it needs from
Grafeno is what the phases give: a room to enter at the start of a
session, a checkpoint to leave at the end, one call to hand over what it
read, a walk and a context to answer from, and a reflection to know what
it does not know.
