# Specialization audit

The state of Grafeno after phases 0 to 6 of the
[implementation plan](IMPLEMENTATION_PLAN.md), read from the repository at
commit `e98b2a1`, and what it would take to set it apart as the memory of
agents. Phases 8 to 13 are planned from it in [ROADMAP.md](ROADMAP.md); how
the pieces fit is in [ARCHITECTURE.md](ARCHITECTURE.md).

Each capability is classified:

| Class | Meaning |
| --- | --- |
| **EXISTENTE** | Built, tested, documented. |
| **PARCIAL** | Some of it is there; what is missing is said. |
| **PENDIENTE** | Not there; worth building, in the roadmap. |
| **DESCARTADA** | Not to be built, with the reason. |

- [The repository](#the-repository)
- [The palace's entities](#the-palaces-entities)
- [What phases 3 to 6 built](#what-phases-3-to-6-built)
- [Persistence and its evolution](#persistence-and-its-evolution)
- [Tests](#tests)
- [Duplications](#duplications)
- [Bottlenecks and limits](#bottlenecks-and-limits)
- [Engine and cognitive layer](#engine-and-cognitive-layer)
- [Documentation and public contracts](#documentation-and-public-contracts)
- [Comparison with other kinds of system](#comparison-with-other-kinds-of-system)
- [Capabilities, classified](#capabilities-classified)
- [What could set Grafeno apart](#what-could-set-grafeno-apart)
- [What not to build](#what-not-to-build)

## The repository

Seven crates, about 76 000 lines of Rust in `src` and 32 000 in tests:

| Crate | Lines (src / tests) | What it is |
| --- | ---: | --- |
| `grafeno-core` | 610 / — | The model: nodes, edges, ids, times, errors. |
| `grafeno` | 25 738 / 8 038 | The engine: one file, one writer and readers on snapshots, namespaces, full-text and vector indexes, traversals, about fifty graph algorithms, constraints and contradictions, names and entity resolution, merges with undo, rights down to records, audit, change log, backup and restore, export and import, integrity checks, replication log. |
| `grafeno-query` | 18 052 / 6 398 | GQ, the declarative query language, with time travel (`VALID AT`, `KNOWN AT`), a planner, limits, split reads. |
| `grafeno-memory` | 8 172 / 4 198 | The cognitive layer: recall, context, extraction and reconciliation, communities, maintenance, checkpoints, documents, timelines, knowledge (origin and certainty), palaces, reflection. |
| `grafeno-raft`, `grafeno-cluster` | 1 232 + 1 398 / 1 796 | Raft and members kept in sync. |
| `grafeno-server` | 21 106 / 11 394 | `grafeno serve` (an HTTP API of about 100 routes, accounts, keys, roles, audit), `grafeno mcp` (35 tools, over stdio, beside an HTTP API, or as a client of a server), `grafeno shell`, administration. |

The graphical interface, Grafeno Studio, is a separate product in its own
repository, and reads engines over the HTTP API only.

## The palace's entities

| Concept | What it is in the code |
| --- | --- |
| Palace, Room, Locus | Nodes labeled `Palace`, `Room`, `Locus`, joined by `has_room` and `has_locus` (`crates/grafeno-memory/src/palace.rs`). |
| Anchor | An `anchored_at` edge, memory → locus, with an optional `note`. |
| Door | A `leads_to` edge, room → room. |
| Memory | Any node that is not part of a palace: a note, a fact, a person, a chunk, a checkpoint. Nothing is copied into a palace. |

They live in the same namespace and file as everything else, written
through the same transactions; a palace has no store of its own.

## What phases 3 to 6 built

| Area | Where | Checked by |
| --- | --- | --- |
| Retrieval: walks by distance or relevance, doors, a packed context, repeats left out | `palace.rs` (`walk`), `context.rs` | `tests/palace_retrieval.rs`, `tests/palace_walk.rs`, `examples/palace_bench.rs` |
| Consolidation: reviews, removal downwards, superseded memories, undoing a summary | `palace.rs` (`review`, `remove`), `maintenance.rs` | `tests/palace_evolution.rs` |
| Several agents: tools over HTTP, `grafeno mcp --url` | `grafeno-server/src/http.rs`, `mcp.rs` | `grafeno-server/tests/agents.rs` (real processes) |
| Metacognition: reflection, questions | `reflect.rs` | `tests/reflect.rs` |

## Persistence and its evolution

- One file per database (one per Raft group on a cluster). Its layout
  version is 18; a newer engine upgrades an older file in place, after a
  copy of it (`copy_before_upgrade`), and refuses one newer than itself.
- **Every phase of the plan added no layout change.** Palaces, origin,
  certainty, questions and anchors are nodes, edges and properties; a file
  written before them opens unchanged and reads as holding none.
- Backup (`GET /v1/backup`, `grafeno backup`), restore, export and import
  (NDJSON, CSV, GraphML) and `grafeno check` cover the whole file, palaces
  included.
- What evolves without a layout change has a cost: reserved labels
  (`Palace`, `Room`, `Locus`, `Question`, `Archived`, `Community`,
  `Checkpoint`) and properties (`origin`, `certainty`, `verified_by`,
  `archived_sources`) have a meaning a database may already give them
  otherwise. Each is documented; none is checked when a database is
  opened.

## Tests

The workspace's tests were run for this audit: **513 passed, 0 failed**,
`cargo clippy` and `cargo fmt` clean. Two things of the environment, not
of the code, were seen and are recorded in the history of the work: the
operating system's application control refuses now and then to start a
freshly built test program (it passes rebuilt), and once a doctest
program was refused the same way.

## Duplications

Found while reading the code; none is a defect, each is a cost when it
changes.

| What | Where | Proposed |
| --- | --- | --- |
| Words of a text as a set, and their overlap | `context.rs` (`words`, `overlap`), `maintenance.rs` (`words_of` and the overlap in `superseded`) | One helper in the memory crate. |
| A node's name or the start of its text | `context.rs` (`readable_name`), `palace.rs` (`title`), `reflect.rs` (`describe`), `mcp.rs` (`name_of`, `snippet`) | One helper; the tools use it. |
| A fact as a sentence | `context.rs` (facts), `reflect.rs` (`sentence`), `timeline.rs` | The same helper. |
| Reading contradictions on record | MCP `contradictions`, `maintain`, `reflect` | One function in the memory crate that filters them by nodes. |
| Text matching of a topic | `recall` (fused), `walk` (recall of places, then full text for relevance), `reflect` (full text) | Kept apart on purpose: each needs a different breadth. Documented. |

## Bottlenecks and limits

Measured ones first (`docs/benchmarks.md`, 20 000 notes, release build):
recall 4.2 ms, a walk by distance 8.2 ms, by relevance with a context 17
ms, entering a room of 1 000 notes 7 ms.

| Limit | Cause | Size of it |
| --- | --- | --- |
| A walk reads every memory anchored in the rooms it starts from | It gathers before it ranks | Linear in the rooms' size; measured above. |
| `reflect` runs `superseded` over the whole namespace | It looks for overtaken memories among all, then keeps the scope's | Not measured; grows with all memories about shared entities. **To fix first.** |
| `reflect` and `contradictions` read every contradiction on record | The record is a list, not indexed by node | Grows with the namespace's history. |
| A room is found among its palace's rooms, a locus among its room's | No index on a name under a parent | Fine to hundreds; not measured past that. |
| Text matching compares whole words | The full-text index has no stemming | "invoice" does not match "invoices". |
| The tools route checks rights by namespace only, and not on a cluster | Its writes do not go through the leader; its reads do not apply per-record views | Refused in both cases, with a message. |
| Certainty is what agents state | No model in the engine | By design. |

## Engine and cognitive layer

The boundary holds: the engine (`grafeno`, `grafeno-query`) knows nothing
of palaces, kinds of memory, origin or reflection. What the cognitive
layer needs from it is general: names and entity resolution, a memory
state per node (importance, accesses), constraints that record
contradictions, bitemporal edges, change logs, full-text and vector
search, traversals.

Two places blur it a little:

- `ARCHIVED` lives in the memory crate, but the engine's statistics and
  the server's routes (`/archive`, `/weak`) know it by name.
- The MCP tools live in the server crate, and the memory crate's
  functions have no HTTP routes of their own beyond the generic tools
  route of phase 5.

Neither is worth moving now.

## Documentation and public contracts

- `crates/grafeno-server/README.md` (and its Spanish twin) lists every
  route and every MCP tool; a test (`tests/contract.rs`) fails when a
  route is not documented or a documented one does not exist.
- `docs/mental-palace.md` and the three documents of this folder describe
  the palace, the cognitive model and the plan.
- MCP tools describe themselves to agents; there is no test that every
  tool is in the README (there is for routes). **Pending.**
- The HTTP API is versioned (`/v1`, `GET /v1/about`); the MCP tools are
  not, and their answers have grown fields across phases without
  removing any.

## Comparison with other kinds of system

Read from public sources in November 2026: project documentation,
repositories, and papers where there were any, and second-hand write-ups
where there were not, which is said. **No measurement was run against
any of them**, so nothing here says Grafeno does better or worse; it says
what each kind of system does, and how Grafeno's way differs.

| Kind of system | What it does, as published | How Grafeno's way differs |
| --- | --- | --- |
| **A general graph database** with a declarative query language, and, beside it, an experimental package of agent memory with an MCP server | Short-term, long-term and "reasoning" memory layers in one graph, the long-term one a fixed schema of people, objects, locations, events and organizations; the database itself is a separate server. | Grafeno is the database and the memory in one program and one file; its memory layer is not a fixed schema but labels and edges an agent chooses. |
| **A memory layer for applications**, hosted and open source | A model extracts facts from conversations and decides, by more model calls, to add, update or delete; a graph variant existed and, by second-hand accounts, its open-source graph and conflict resolution were later removed. | Grafeno puts no model inside: extraction is by rules or by the agent's own reading, and a contradiction is decided by a constraint the user declared, keeping both facts' history. |
| **A temporal knowledge graph library** for agents, with a hosted product on top | Episodes (messages, texts) become entities and facts; facts carry valid and record times, and a model call per new fact decides whether it invalidates older ones, which are marked, not deleted; communities summarize the graph. | The same bitemporal idea is in Grafeno's engine itself (every edge, `VALID AT`/`KNOWN AT` in queries), and invalidation is by constraints, not by a model. Episodes exist (`ingest`), but there is no model that reads them. |
| **An agent runtime** that manages its own memory | The model edits labeled blocks pinned in its context, and searches a conversation log and an archival store through tools; older messages are summarized when the context fills. | Grafeno is not an agent and does not decide what to remember; it gives an agent a place to keep and find things. A runtime of that kind could use Grafeno as its archival and recall store. |
| **A local memory organized as a palace** | Verbatim storage of conversations, organized in wings, rooms, halls, tunnels and drawers. Its published benchmark comes from verbatim retrieval; a reproduction reported room-based retrieval scoring lower than plain retrieval. | Grafeno's palace is a view over memories that live once in the graph, with relations between them; a walk follows the graph, not only the room. The finding of that reproduction is a warning: arranging memories by place should be measured, not assumed to help retrieval. |

What this suggests for Grafeno:

- **Keep the model out of the engine.** Every system above that decides
  with a model pays for a model call per write, and its decisions are not
  reproducible; Grafeno's are, and the agent brings the model.
- **Measure retrieval quality**, not only latency. Grafeno has latency
  numbers and no measure of whether what it returns is what was needed.
  A small labeled set of questions over a fixed graph would give one
  (phase 9).
- **Episodes deserve a clearer place**: the temporal library's split of
  raw episodes from the facts drawn out of them is close to `ingest` and
  `add_mentions`, but Grafeno does not name the episode a fact came from
  as a first-class link (phase 8).

## Capabilities, classified

| Capability | Class | Evidence or gap |
| --- | --- | --- |
| Memories as nodes, facts as edges, any labels | EXISTENTE | Engine model. |
| Bitemporal facts (valid and record time), time travel in queries | EXISTENTE | `VALID AT`, `KNOWN AT`; constraints end facts instead of deleting them. |
| Contradictions under declared constraints | EXISTENTE | Invalidate, reject or flag, with a record. |
| Contradictions with no constraint | PENDIENTE | An explicit `contradicts` relation an agent records (phase 8, 10). |
| Kinds of memory | PARCIAL | Fact, Decision, Experience, Task, Question; not Concept, Hypothesis, Evidence, Goal, Constraint, Episode by name (phase 8). |
| Origin and certainty, verification | EXISTENTE | Phase 2. |
| Extraction method recorded | PARCIAL | A mention says extracted or agent; a fact does not (phase 8). |
| Entity resolution and merges with undo | EXISTENTE | Match rules, names, `merge`, `undo_merge`. |
| Duplicate relations | PENDIENTE | Two identical current facts are not found (phase 8). |
| Hybrid recall (text, vectors, graph, recency, importance) | EXISTENTE | `recall`. |
| Walks with relation and time filters | PARCIAL | Walks follow every relation, at any time (phase 9). |
| Explanation of why a result came back | PARCIAL | `why` in recall, `via` in walks; evidence and uncertainty per result missing (phase 9). |
| Exact token counts | PARCIAL | A tokenizer can be passed in the library; the tools always estimate and do not say so (phase 9). |
| History of a decision and the reasons it changed | PARCIAL | Facts keep history; a `supersedes` relation between decisions is not named (phase 10). |
| Comparing two states of the graph | PENDIENTE | Time travel answers one time; no difference of two (phase 10). |
| Palaces, rooms, loci, anchors, doors, maps, walks, entering | EXISTENTE | Phases 1, 3. |
| One memory in several palaces | EXISTENTE | Anchors. |
| Routes with a goal (decisions, dependencies, history) | PENDIENTE | Phase 11. |
| Reconstructing the context of a decision, event or entity | PARCIAL | `palace_enter`, `timeline`, `reflect` each do part (phase 11). |
| Reviews and reorganization proposals | EXISTENTE | `palace_review`. |
| Poorly connected areas, redundant relations | PENDIENTE | Phase 11. |
| Reflection: known, uncertain, conflicts, stale, gaps | EXISTENTE | Phase 6. |
| Sources behind what is known, and claims with no evidence | PARCIAL | Verified items are named; stated claims with no source are not a category (phase 12). |
| Gaps against an expected shape (a schema of a kind) | PENDIENTE | Phase 12. |
| Graph algorithms: paths, components, cycles, order, centrality, communities | EXISTENTE | About fifty `algo.*` procedures. |
| Depth-first order as a procedure | DESCARTADA | Breadth-first distances, paths and orders cover the uses seen; no need shown. |
| Backup, restore, import, export, checks, upgrades | EXISTENTE | Phases 4, 10, 15. |
| Several agents on one memory | EXISTENTE | Phase 5, rights by namespace. |
| Memory tools on a cluster | PENDIENTE | Phase 5's limit. |
| A model inside the engine (extraction, conflict decisions, summaries) | DESCARTADA | See below. |
| Embeddings computed by the engine | DESCARTADA | The caller brings them; the engine indexes them. |

## What could set Grafeno apart

Differences, not claims of being better; each should be measured before
it is said in public:

1. **One program and one file** hold the graph engine, the query language,
   the memory, the palace and the MCP server, with backups and checks that
   cover all of it.
2. **Reproducible memory**: no model inside; every decision (a
   contradiction, an invalidation, a reorganization proposal) comes from
   rules and declared constraints and can be explained from records.
3. **Bitemporal facts in the engine**, not in a layer above it, queried in
   the same language as everything else.
4. **Knowledge that says how it is known**: origin, certainty, sources,
   verification, and a reflection that separates the known from the
   uncertain, the clashing, the stale and the missing.
5. **Rights down to records and an audit log** under a memory that several
   agents share.

## What not to build

| Not to build | Why |
| --- | --- |
| A model inside the engine | It makes writes cost a model call, decisions unreproducible, and ties the engine to a provider. Agents have models. |
| Computing embeddings | Same reasons; the engine already indexes those it is given. |
| A fixed ontology of entity types | Agents' domains differ; labels with documented meanings serve them better. |
| Automatic reorganization of palaces | Phase 4 proposes; an agent or a person decides. |
| Searching the web, or any outside source, from the engine | It is an agent's act, with its own permissions. |
| Calibrated probabilities from heuristic scores | Nothing calibrates them; scores are called scores. |

Sources read for the comparison (outside this repository):
the documentation of each project, its repository, the arXiv paper of the
temporal knowledge graph library (2501.13956), and second-hand reports
where noted above.
