# Mental Palace: roadmap

**Status:** done. Every stage below is built and tested; what it became is described in [mental-palace.md](mental-palace.md). What is left for later versions is at the end.

A Mental Palace is a way for agents to arrange the memories Grafeno
already holds into spaces: a palace, its rooms, the places inside each
room, and the memories anchored at those places. An agent enters a room at
the start of a session and gets back what is there, walks from a topic
through the graph to the memories around it, and finds everything again
after a restart.

It is a layer over the graph and the memory library, not another store: a
palace is nodes and edges in a namespace, written through the same
transactions as everything else.

This roadmap covers the whole implementation, in stages small enough to
verify one at a time. What is left for later versions is listed apart, at
the end.

## Contents

- [Scope](#scope)
- [What exists today](#what-exists-today)
- [Model](#model)
- [Tools](#tools)
- [Stages](#stages)
- [Tests](#tests)
- [Compatibility](#compatibility)
- [Open decisions](#open-decisions)
- [Risks](#risks)
- [Later versions](#later-versions)

## Scope

In the first version:

- Logical spatial organization: palaces, rooms and loci.
- Anchoring existing memories to one or several loci, without copying
  them and without touching their own edges.
- Reading the structure back (a map).
- Walks: from a topic to the memories around it, over graph relations.
- Rebuilding context on entering a room, with the room's memories and the
  task checkpoints that already exist.
- Persistence across restarts, and isolation between namespaces.
- Six MCP tools.

Not in the first version: embeddings or models of its own, a new storage
format, automatic placement or reorganization of memories, inference,
additional agents, and any new screen in Grafeno Studio.

## What exists today

Checked in the repository before writing this plan.

| Piece | Where | What the palace takes from it |
| --- | --- | --- |
| Nodes, edges, namespaces, transactions | `crates/grafeno` | Everything is stored as ordinary nodes and edges; every call takes a namespace. |
| Traversals: neighbors, k hops, shortest path, subgraph | `crates/grafeno/src/traverse.rs` | The map and the walks. |
| Names and name lookup (`normalize_name`, `find_by_name`) | `crates/grafeno/src/memory.rs` | Finding a palace, room or locus by its name. |
| Hybrid recall and `context_for` | `crates/grafeno-memory/src/recall.rs`, `context.rs` | Finding the starting points of a walk; packing what a room holds into a token budget. |
| Checkpoints (`save_checkpoint`, `resume`, the `about` edge) | `crates/grafeno-memory/src/checkpoint.rs` | A checkpoint can already point at any node, so it can point at a room. |
| Communities, documents, maintenance | `crates/grafeno-memory/src` | Not reused, but they scan the namespace, so palace nodes must be kept out of them (stage 2). |
| MCP server: 27 tools, one dispatcher, `WRITING_TOOLS`, `McpAccess` | `crates/grafeno-server/src/mcp.rs` | The pattern for the new tools: a `namespace` argument defaulting to the server's, the allow-list checked once in `call_tool`, writing tools hidden on a read-only server. |
| MCP tests over an in-memory database | `crates/grafeno-server/tests/mcp.rs` | The `server`, `call` and `structured` helpers. |
| Grafeno Studio | A separate product since phase 15; it reads engines through the HTTP API (`/v1/{ns}/subgraph` among others) | Nothing to change in the engine: palaces are plain nodes and edges and arrive like any others. |

Edge labels already taken by the memory library, which the palace must not
reuse: `follows`, `about`, `member_of`, `part_of`, `has_part`, `next`,
`mentions`, `distinct_from`.

## Model

```
(Palace) -[has_room]-> (Room) -[has_locus]-> (Locus) <-[anchored_at]- (any memory node)
```

| Concept | Stored as | Properties |
| --- | --- | --- |
| Palace | A node labeled `Palace` | `name`, `description`; text = name and description |
| Room | A node labeled `Room`, one `has_room` edge from its palace | `name`, `description`, `topic`; text = name, topic and description |
| Locus | A node labeled `Locus`, one `has_locus` edge from its room | `name`, `position` (its order in the room); text = name |
| Memory | The node that already exists | Unchanged |
| Anchor | An `anchored_at` edge, memory → locus | `note` (why it is there); who recorded it is the edge's own `recorded_by` |

Rules:

- A palace's name is unique in its namespace; a room's in its palace; a
  locus's in its room. Names are compared with `normalize_name`.
- Creating is idempotent: creating a palace, room or locus that exists
  returns the existing one.
- Every room has a locus named `entrance`, created with the room, so a
  memory can be anchored to a room without choosing a place.
- A memory may be anchored at any number of loci. Anchoring the same
  memory at the same locus twice keeps one edge.
- Anchoring never copies, relabels or rewrites the memory. Removing an
  anchor removes the edge only.
- A palace and everything under it live in one namespace. Edges are
  created inside a namespace, so an anchor cannot cross to another one.

## Tools

All take the optional `namespace` argument the other tools take. Names may
be given in place of ids for palaces, rooms and loci.

| Tool | Writes | In | Out |
| --- | --- | --- | --- |
| `palace_create` | yes | `name`, `description?` | The palace's id, and whether it was created or already there |
| `palace_create_room` | yes | `palace`, `name`, `topic?`, `description?`, `loci?` (names) | The room's id, its loci |
| `palace_anchor` | yes | `memory` (id), `room`, `locus?` (default `entrance`, created when missing), `palace?`, `note?`, `remove?` | The anchor, and the places the memory is now anchored at |
| `palace_map` | no | `palace?`, `room?`, `memories?` (include them, off by default), `limit?` | With no arguments, the namespace's palaces; with a palace, its rooms, loci and counts; with a room, its loci and their memories |
| `palace_walk` | no | `topic` or `from` (id), `palace?`, `hops?` (default 2, at most 3), `limit?` | The rooms and loci the topic leads to, and the memories reached, each with the path that led to it |
| `palace_enter` | no | `room`, `palace?`, `budget?` (tokens) | The room's description and loci, its memories packed into the budget, and the latest checkpoint about the room when there is one |

`palace_create`, `palace_create_room` and `palace_anchor` join
`WRITING_TOOLS`, so a read-only server leaves them out.

What is deliberately not a new tool:

- Saving where the work in a room stands is `checkpoint` with `about` set
  to the room. `palace_enter` reads it back.
- Renaming or describing a palace, room or locus is `update`.
- Deleting one is `forget`.
- Searching memories without a palace remains `recall` and `context`.

## Stages

Each stage ends with its own tests passing and the existing ones still
green.

### Stage 0: confirm the assumptions (done)

Read-only. What the code says:

- **Names.** A node's `name`, `title` and `label` properties and its
  `aliases` are kept normalized in a table of their own, and
  `find_by_name` is a lookup in it, not a scan
  (`crates/grafeno/src/memory.rs`). A palace is found by that lookup and
  then by its label; rooms and loci are found among their parent's
  children. The lookup also matches titles and aliases, so the palace
  code compares the `name` property again.
- **Recall over a set of nodes.** `RecallQuery` filters by label only.
  `palace_enter` has to collect the room's memories itself; stage 5
  decides how they are packed into the budget.
- **Deleting.** `forget` on a node calls `delete_node`, which removes the
  node and its edges and nothing else. Deleting a memory removes its
  anchors (tested). Deleting a room leaves its loci without a room, and
  deleting a palace leaves its rooms without a palace: `anchors_of`
  reports such places with no room or palace, and stage 6 decides whether
  a palace tool deletes downwards.
- **Uniqueness.** "Unique under a parent" is about an edge and a
  property together, which a property constraint does not express. The
  check stays in the write transaction, which is safe because there is
  one writer.
- **Found on the way: named entities.** `remember` reuses the node that
  already has a name. A room named "Rust" has that name, so a memory
  about Rust could be attached to the room as if it were the entity.
  Added to stage 2.

### Stage 1: the model in `grafeno-memory` (done)

A new module, `crates/grafeno-memory/src/palace.rs`, exported from
`lib.rs`.

- Constants: `PALACE`, `ROOM`, `LOCUS`, `HAS_ROOM`, `HAS_LOCUS`,
  `ANCHORED_AT`, `ENTRANCE`.
- `create_palace`, `create_room`, `create_locus` on a `WriteTx`, idempotent.
- `find_palace`, `find_room`, `find_locus` on a `ReadTx`, by name or id.
- `anchor` and `unanchor` on a `WriteTx`; `anchors_of` (the places of a
  memory) on a `ReadTx`.

Done when: unit tests cover creation, idempotence, name uniqueness per
parent, anchoring one memory at several loci, and a memory's node being
byte-for-byte the same before and after anchoring.

Built as planned, in `crates/grafeno-memory/src/palace.rs`, with ten
tests in `crates/grafeno-memory/tests/palace.rs` that also cover
namespaces and reopening a database file. Beyond the plan: creating a
room that exists adds the loci it lacks; a palace, room or locus cannot
itself be anchored; `is_structure` and `STRUCTURE_LABELS` tell palace
nodes from memories, for stage 2.

### Stage 2: keep palace nodes out of what scans the namespace (done)

Palace, room and locus nodes carry text so they can be found, which also
puts them in the way of code that treats every node as a memory.

- `recall`: skip them when the query names no labels, as it already does
  for `Community` (`recall.rs`).
- Community detection: leave them out of the graph it clusters
  (`communities.rs`), or a palace becomes the hub of every community.
- Maintenance: duplicates, weak memories and consolidation must not
  propose merging, archiving or summarizing them (`maintenance.rs`).
- Extraction: they are not sources of mentions (`SOURCE_LABELS`).
- Named entities: `remember`, entity resolution and `find_matches` must
  not take a palace, room or locus for the entity of the same name.

Done when: a test builds a namespace with and without a palace over the
same memories and gets the same recall results, communities and
maintenance proposals from both.

What was changed, each with `is_structure` or the set of a namespace's
palace nodes (`palace::structure`):

| Where | What it does now |
| --- | --- |
| `recall` | Unless the query asks for `Palace`, `Room` or `Locus` by label, palace nodes are not returned, are not seeds, are not walked through, and the edges to them do not count when a node is damped as a hub. |
| `context_for` | Follows from recall; and an `anchored_at` edge is not listed as a fact. |
| Communities | Palace nodes are not in the graph that is clustered. |
| `find_duplicates`, `weak_memories` | Palace nodes are never proposed for merging or archiving. |
| `consolidation_groups` | A locus is not an entity that its anchored memories are "about". |
| Extraction | A palace node's name is not in the gazetteer, its text is not read, and a phrase is not linked to it. |
| `remember` (MCP) | A named memory does not reuse a palace, room or locus of that name. |

Left as they were, on purpose: `find_matches` and the entity resolution
that goes by label, because they only look at nodes of the label asked
for, and no memory has a palace label.

Tests: `crates/grafeno-memory/tests/palace_isolation.rs` compares recall
(seven queries, with and without labels, one to three hops), context,
duplicates, weak memories and consolidation groups before and after a
palace is built over the same memories, communities before and after, and
extraction over two namespaces with the same texts. With the exclusions
switched off, three of its four tests fail. One more in
`crates/grafeno-server/tests/mcp.rs` covers `remember`.

Two things a palace still changes, and the tests do not hide:

- **Text scores.** Palace nodes have text, and full-text scoring counts
  every text in the namespace (how rare a word is, how long texts are).
  The order of the results was the same in every query tested; the
  scores are not identical to the last digit.
- **Candidates.** Recall takes the best 50 text matches before filtering.
  Palace nodes that match take some of those places, so a query that
  matches more than 50 texts could lose a memory at the tail.

And one cost: in a namespace that has a palace, recall counts a node's
edges by reading them (to leave the anchors out) where it otherwise asks
for a count capped at 1 024. Namespaces without a palace take the path
they always took.

### Stage 3: map (done)

- `map` in `palace.rs`: the palaces of a namespace; a palace's rooms and
  loci with counts; a room's loci with their memories, in `position`
  order, paged by `limit`.

Done when: tests read back a palace with several rooms, an empty room and
a memory that appears in two rooms.

Built as `palaces`, `map_palace` and `map_room`, with `MapOptions`.
Palaces and rooms come by name, loci in the order they were made, and a
locus's memories newest first. A map gives counts; the memories at each
locus are listed only when asked for, up to a limit per locus that cuts
the list and not the count.

### Stage 4: walk (done)

- `walk` in `palace.rs`. Starting points: the node given in `from`, or the
  rooms and loci whose text matches `topic` (recall restricted to the
  `Room` and `Locus` labels), plus the memories that match and are
  anchored. From there: the anchored memories of those places, then their
  graph neighbors up to `hops`, with the path kept for each result.
- Order: places first by match score, memories by distance and then by
  the importance and recency recall already computes.
- Inside a palace when one is given; across the namespace's palaces
  otherwise.

Done when: tests cover a topic that names a room, a topic that only a
memory matches, a memory reached through a relation and not through an
anchor, the hop limit, and a topic that matches nothing (an empty result,
not an error).

Built as `walk`, with `WalkQuery`. How it turned out:

- A walk answers with the places the topic led to (each with its path,
  as `Work / Storage / Decisions`) and the memories reached. Each memory
  has its distance, the memory in the palace the walk came from, the
  relations followed from it (edge label, direction, node) and the places
  it is anchored at.
- Distance 0 is what was found in the palace: the memories anchored at
  the places that matched, place by place, then the anchored memories
  that matched by themselves. From there the walk follows every relation
  of the graph, in both directions, and never goes through a palace node:
  two memories in the same room are not one hop apart because of the room.
- Within a distance, memories are ordered by strength (importance decayed
  since the last use, the measure maintenance uses), not by the relevance
  recall computes: the topic chose where to start, not what is near.
- A memory that matches the topic but has no place in a palace is not
  returned. That is `recall`'s answer, and the tool's description has to
  say so.
- Archived memories and community summaries are left out, and what was
  only reachable through one of them with them. A node with more than 50
  relations is returned but not walked through.
- Hops default to 2 and are capped at 3. The walk stops as soon as it has
  `limit` memories, so a limit keeps the nearest.

### Stage 5: enter (done)

- `enter` in `palace.rs`: the room, its loci, its anchored memories packed
  into the token budget, and `resume` with `about` set to the room.
- No checkpoint is not an error: the result says there is none.

Done when: a test saves a checkpoint about a room, reopens the database
and gets the room's memories and that checkpoint from `enter`.

Built as `enter`, with `EnterOptions`. It returns the room's map, the
palace it is in, a context and the checkpoint.

- The context is the same thing `context_for` returns, with the same
  rendering and the same budget rules, over the room's memories instead
  of a recall's, the strongest first. For that, the packing in
  `context.rs` was split from the recall that fed it: `context_of` packs
  any list of nodes. `context_for` behaves as before.
- The checkpoint is the latest one whose `about` is the room, from any
  task and any agent. A checkpoint about the palace, or about a memory in
  the room, is not looked for.
- Archived memories are left out. An empty room is entered and gives an
  empty context.

Tests for stages 3 to 5: nine in
`crates/grafeno-memory/tests/palace_walk.rs`, one of them over a database
file closed and opened again.

### Stage 6: MCP tools (done)

In `crates/grafeno-server/src/mcp.rs`:

- The six handlers, the six entries in `tools()`, the three names in
  `WRITING_TOOLS`, and the dispatcher arms.
- Arguments parsed with `parse_args`; ids with `parse_id`; errors through
  `describe`, as a tool error (`isError: true`) and never a protocol error.
- Text content shaped for a model (names, snippets, dates) and the same
  data in `structuredContent`.
- One sentence in `INSTRUCTIONS`: enter the room of the work at the start
  of a session, anchor what should be found there again.

Done when: `crates/grafeno-server/tests/mcp.rs` has the tests listed
under [Tests](#tests), and `handshake_and_tool_list` counts the new tools.

Built as planned: six handlers, their schemas, three of them among the
writing tools, and two sentences in the server's instructions. Decided on
the way:

- A palace or a room is given by its name or its id. A room's name alone
  is enough when one palace has a room of that name; when several do, the
  error names them and asks for `palace`.
- `palace_anchor` makes a locus that is missing, and never a room or a
  palace.
- `palace_map` lists a room's memories by default, and only counts for a
  palace or for all of them.
- `palace_enter` returns the context as text and the room, the memories
  packed, how many were left out, and the checkpoint; with no checkpoint,
  a hint says how to save one. It records an access on the memories it
  returns, as `recall` does, and not on a read-only server.
- `palace_walk` with nothing found says that `recall` searches every
  memory.
- No tool to delete a room downwards was added: `forget` leaves a room's
  loci without a room, which is listed among the known limits and the
  later versions.

Four tests were added to `crates/grafeno-server/tests/mcp.rs`, and the
tools were run once more over a real `grafeno mcp --memory` process, whose
results are the example in the documentation.

### Stage 7: Studio check (done, by the API)

No code in the engine. With a database that holds a palace:

- Ask `/v1/{ns}/subgraph` from a palace and confirm rooms, loci, anchors
  and memories come back as ordinary nodes and edges.
- Open the same database in Grafeno Studio and confirm they draw, and that
  the `Palace`, `Room` and `Locus` labels can be told apart.

Done when: the result is written under "Known limits" in the document of
stage 8, whatever it is.

The first point is a test now
(`a_palace_is_read_as_ordinary_nodes_and_edges` in
`crates/grafeno-server/tests/http.rs`): the subgraph route from a palace
returns its rooms, loci, anchors and memories, and a query walks from the
palace to the memory. The second was not done: Grafeno Studio is a
separate product in its own repository and was not opened. The
documentation says so.

### Stage 8: documentation (done)

- `docs/mental-palace.md`: purpose and concepts, design decisions, the
  node and edge model, the tools with a worked example from an empty
  namespace, how it fits with recall, context and checkpoints, known
  limits, how to run and test it, and what is planned later, apart from
  what exists.
- `crates/grafeno-server/README.md` and `README.es.md`: the tools in the
  MCP section.
- `CHANGELOG.md`.
- `docs/phases/README.md` and a phase file, if this is taken as a phase.
- This roadmap: each stage marked done as it lands.

Written: `docs/mental-palace.md`, the tools in both server READMEs, the
CHANGELOG, a link in the README and a paragraph in the memory crate's
documentation. The phases index was left as it is: whether this is a
phase is the maintainer's to say.

## Tests

| What | Where | Stage |
| --- | --- | --- |
| Palaces, rooms and loci are created and found again; creating twice gives one | `crates/grafeno-memory/tests/palace.rs` | 1 |
| A memory anchored at several loci is one node, unchanged, with its own edges intact | same | 1 |
| Recall, communities and maintenance give the same results with and without a palace | same | 2 |
| Map of a palace and of a room | same | 3 |
| Walks by topic, from a node, through relations, with a hop limit | same | 4 |
| Enter returns memories and the checkpoint about the room | same | 5 |
| Everything survives closing and reopening a database file | same, with `Grafeno::open` on a temporary file | 1, 5 |
| A palace in namespace `a` is invisible from `b`; the same palace name in both is two palaces; a memory id from `b` cannot be anchored in `a` | same, and `tests/mcp.rs` with `McpAccess.allow` | 1, 6 |
| The six tools end to end | `crates/grafeno-server/tests/mcp.rs` | 6 |
| A read-only server lists and answers `palace_map`, `palace_walk` and `palace_enter` only | same | 6 |
| Bad input (unknown room, missing name, malformed id) is a tool error with a readable message | same | 6 |
| The existing tools behave as before | The existing suites, unchanged | every stage |

To run:

```bash
cargo test -p grafeno-memory
```

```bash
cargo test -p grafeno-server --test mcp
```

```bash
cargo test --workspace
```

## Compatibility

- No change to the storage layout, so no migration. A database written
  before the palace opens as it is, with no palaces in it.
- No existing tool changes its arguments or its result. The one visible
  change is in stage 2: palace nodes do not appear in an unlabeled
  `recall`.
- `Palace`, `Room` and `Locus` become labels with a meaning, and
  `has_room`, `has_locus` and `anchored_at` edge labels with a meaning. A
  database that already uses those names for something else would be read
  as holding palaces; the documentation says so.
- No new dependency.
- Nothing in `crates/grafeno` changes unless stage 0 finds a lookup that
  only the core can do efficiently.

## Open decisions

| Decision | Leaning | Settled in |
| --- | --- | --- |
| Loci as nodes, or anchors straight to rooms with a `locus` property | **Settled:** nodes | Stage 1 |
| An `entrance` locus in every room | **Settled:** yes | Stage 1 |
| Whether `palace_anchor` creates a missing locus | **Settled:** yes for loci, no for rooms and palaces; `anchor` in the library takes a locus that exists | Stage 6 |
| Name uniqueness by a constraint or by a check in the transaction | **Settled:** a check in the transaction | Stage 0 |
| Hiding palace nodes from an unlabeled `recall` | **Settled:** hidden unless asked for by label | Stage 2 |
| Order of loci inside a room: a `position` property or edges between loci | **Settled:** a property | Stage 3 |
| Whether deleting a room deletes its loci | **Left open:** `forget` leaves them without a room; deleting downwards is in the later versions | — |
| Taken as a phase in `docs/phases` | To be decided by the maintainer | Stage 8 |

## Risks

- **Structure nodes leaking into memory features.** A palace touches
  every memory under it through two hops, which distorts communities,
  spreading in recall and consolidation. Closed in stage 2 for the
  features that exist; a feature added later that reads every node of a
  namespace has to leave palace nodes out too.
- **Hubs.** A locus with thousands of anchors makes maps and walks slow
  and long. A map lists a limited number of memories per locus, but it
  still reads every anchor to count them, and a walk reads every memory
  anchored at the places it starts from before it applies its limit.
  Entering a room reads all its memories to order them by strength.
- **Finding a room reads its palace's rooms.** A palace is found through
  the name table, but a room is found by reading every room of its palace,
  and a locus every locus of its room. Fine for tens or hundreds; a palace
  with thousands of rooms would want an index.
- **Tool count.** The server has 27 tools and its header says "few of
  them". Six more is why renaming, deleting and checkpointing reuse the
  tools that exist.
- **Flaky CI.** One existing test in the cluster groups suite fails now
  and then; a red run there is not evidence about this work, and is
  reported as such, not passed over.

## Later versions

None of this is part of the stages above.

- Routes: ordered paths between loci and between rooms, and walking one.
- Doors: links between rooms of different palaces.
- Suggested placement: proposing a room for a new memory, for the agent
  to accept or not.
- Rooms proposed from community summaries.
- Reorganization: splitting a crowded room, merging two.
- Palace tools over the HTTP API and in the client libraries.
- A palace view in Grafeno Studio.
- Palaces shared between namespaces, with the rights of phase 14.
- Moving a palace with export and import as one unit.
