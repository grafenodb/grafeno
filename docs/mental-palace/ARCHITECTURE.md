# Architecture of the cognitive memory

How Grafeno's memory is built, from the file up to the agent. What each
piece does is in [../mental-palace.md](../mental-palace.md) and
[cognitive-memory.md](cognitive-memory.md); what is planned is in
[ROADMAP.md](ROADMAP.md).

- [Layers](#layers)
- [The data model](#the-data-model)
- [Reserved names](#reserved-names)
- [How a request flows](#how-a-request-flows)
- [Several agents](#several-agents)
- [Rules the layers keep](#rules-the-layers-keep)
- [Where to extend](#where-to-extend)

## Layers

```
 agent ── MCP (stdio) ──┐                    Grafeno Studio, scripts
                        │                              │
          grafeno mcp ──┼── --url ──► grafeno serve ◄──┘  HTTP /v1
          (tools)       │             (routes, rights, audit, tools route)
                        ▼                     │
              grafeno-memory  ◄───────────────┘
   recall · context · walk · palace · knowledge · reflect · maintenance
   extraction · communities · checkpoints · documents · timelines
                        │
              grafeno-query (GQ, time travel, planner, limits)
                        │
              grafeno (engine): one file, one writer, snapshots,
              namespaces, indexes, traversals, algorithms, constraints,
              names, merges, rights, audit, change log, backup
                        │
              grafeno-core (nodes, edges, ids, times)
```

- **`grafeno-core`** is the model and nothing else.
- **`grafeno`** is a graph database that knows nothing of agents: it would
  serve any application that keeps a graph.
- **`grafeno-query`** is GQ over the engine.
- **`grafeno-memory`** is the cognitive layer: functions over a read
  snapshot or inside a write transaction, which take a namespace and
  return plain data. It has no I/O, no threads and no network.
- **`grafeno-server`** turns those functions into tools (MCP) and routes
  (HTTP), with rights, audit and the processes around them.

## The data model

Everything is nodes and edges in a namespace.

| Piece | Stored as |
| --- | --- |
| A memory (note, fact, decision, experience, task, question, document, chunk) | A node with a kind label, its text, properties, `sources` (what it came from), its creation and update times, who recorded it. |
| An entity (a person, a service) | A node with a name, found again by its name and aliases. |
| A fact | An edge with a label, valid time (`valid_from`, `valid_to`), record time (recorded, expired), properties, `sources`, who recorded it. |
| How something is known | Properties `origin`, `certainty`, `verified_by`, `verified_at`, `verified_from`. |
| Importance and use | The engine's memory state per node (importance, accesses, last access). |
| A contradiction | A record in the namespace's state, written by a constraint, naming both facts and what was done. |
| A palace | Nodes `Palace`, `Room`, `Locus`; edges `has_room`, `has_locus`, `anchored_at`, `leads_to`. |
| A summary | A node `Summary` with `summarizes` edges and the list of memories it archived. |
| A checkpoint | A node `Checkpoint`, with `follows` and `about` edges. |
| A community | A node `Community`, with `member_of` and `part_of` edges. |

A memory exists once. A palace, a summary, a community and a checkpoint
point at it; none copies it.

## Reserved names

Labels with a meaning: `Palace`, `Room`, `Locus`, `Archived`, `Community`,
`Summary`, `Checkpoint`, `Question`, `Fact`, `Decision`, `Experience`,
`Task`, `Document`, `Chunk`, `Conversation`, `Code`.

Edge labels with a meaning: `has_room`, `has_locus`, `anchored_at`,
`leads_to` (between rooms), `mentions`, `summarizes`, `member_of`,
`part_of` (between communities), `has_part` and `next` (in documents),
`follows` and `about` (checkpoints), `answers`, `distinct_from`.

Properties with a meaning: `name`, `title`, `aliases`, `origin`,
`certainty`, `verified_by`, `verified_at`, `verified_from`, `feedback`,
`archived_sources`, `task`, `topic`, `position`.

A database that used one of them for something else would be read as
meaning this. Nothing checks it on opening; each is documented where it
is introduced.

## How a request flows

**Storing** (`remember`, `relate`, `add_mentions`, `ingest`): one write
transaction. Names are resolved first (an entity of that name is reused);
constraints run as the fact is written and may end, refuse or flag;
origin and certainty are checked and stored as given; the change log
records it; the agent's name is recorded on it.

**Recalling** (`recall`, `context`): one read snapshot. Full text and
vectors give candidates, fused by rank; the graph spreads part of the
best scores to neighbors, damped at hubs; importance and recency weigh
the result; palace nodes and archived memories are left out. A context
packs the results and the facts between them into a token budget,
leaving out repeats.

**Walking** (`palace_walk`): places that match the topic, the memories
anchored there, doors to other rooms, then relations from those memories
up to three hops; ordered by distance or relevance; optionally packed.

**Reflecting** (`palace_reflect`): the memories in scope, the facts
around them, then each section computed from records: origin and
certainty, contradiction records, valid times, memory state, archives,
questions and their answers, mentions.

## Several agents

```
 agent A ─ grafeno mcp --url ─┐
                              ├─► grafeno serve ── one file
 agent B ─ grafeno mcp --url ─┘     rights by key, audit, one writer
 Studio, scripts ─ HTTP ──────┘
```

One process owns a database file. A server owns it for many agents; each
agent's MCP process sends its tools to `POST /v1/{ns}/tools/{name}` with
its own key, and GQ to `POST /v1/{ns}/query`. A single agent may instead
own a file (`grafeno mcp --db`) and serve the HTTP API beside it
(`--listen`) for an interface to watch.

## Rules the layers keep

1. **No model in the engine or the memory layer.** Judgement comes from
   the agent, through structured calls.
2. **No layout change for a cognitive feature.** Labels, edges and
   properties carry it.
3. **Nothing is deleted by a tool that was not asked to delete.** Archive,
   invalidate, propose; delete only on request, with a dry run where it
   reaches far.
4. **Every derived answer names its records.** A reflection, a review, a
   walk and a context point at ids.
5. **The engine stays a general graph database.** Its tests run without
   palaces, memories or agents.

## Where to extend

| To add | Put it |
| --- | --- |
| A kind of memory | A label with a meaning in `knowledge.rs` (`KINDS`) and the docs. |
| A relation with a meaning | A constant and its rule in the memory crate, used by the tools that care. |
| A way to find memories | A function in `grafeno-memory` over a `ReadTx`, then a tool in `mcp.rs`. |
| A rule that decides on write | A constraint in the engine, if it is general; a function in the memory crate, if not. |
| A tool | `mcp.rs`: handler, schema, `WRITING_TOOLS` if it writes, a test, the README. |
