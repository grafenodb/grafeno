# Roadmap: phases 8 to 13

Planned from the [specialization audit](SPECIALIZATION_AUDIT.md) of phase
7. Every capability asked for is classified as **EXISTENTE** (built and
tested), **PARCIAL** (part of it; the rest planned here), **PENDIENTE**
(to build here) or **DESCARTADA** (not to build, and why). Only PARCIAL and
PENDIENTE items get work; each phase ends with its acceptance test, the
workspace's tests and its documentation, as phases 0 to 6 did.

The rules of [ARCHITECTURE.md](ARCHITECTURE.md#rules-the-layers-keep)
hold throughout: no model in the engine, no layout change for a cognitive
feature, nothing deleted unasked, every answer naming its records, the
engine a general graph database.

- [Phase 8: a model of knowledge](#phase-8-a-model-of-knowledge)
- [Phase 9: explainable retrieval](#phase-9-explainable-retrieval)
- [Phase 10: knowledge over time](#phase-10-knowledge-over-time)
- [Phase 11: the palace as a view](#phase-11-the-palace-as-a-view)
- [Phase 12: the quality of knowledge](#phase-12-the-quality-of-knowledge)
- [Phase 13: the graph engine on its own](#phase-13-the-graph-engine-on-its-own)
- [Order](#order)

## Phase 8: a model of knowledge (done)

Built as planned below; what it does is in
[cognitive-memory.md](cognitive-memory.md#relations-with-a-meaning).
Tests: `knowledge_keeps_identity_relations_and_evidence` in
`crates/grafeno-server/tests/mcp.rs` (the acceptance) and
`facts_stored_twice_are_found` in `crates/grafeno-memory/tests/knowledge.rs`.
`Question` joined the kinds as well. A merge already joined `sources`; what
it dropped was the merged node's origin, certainty and method when the kept
node had its own, now kept in `merged_from`.


**Kinds of knowledge**

| Kind | Class | Plan |
| --- | --- | --- |
| Fact, Decision, Experience, Task, Question | EXISTENTE | Labels with a meaning (phases 2, 6). |
| Entity | EXISTENTE | Named nodes, resolved by name and match rules. |
| Relation | EXISTENTE | Edges, any label. |
| Episode | PARCIAL | `Episode` is used by maintenance and `ingest` stores conversations; not a kind with a meaning, and a fact does not name the episode it came from. Add it to the kinds; the fact's `sources` names the episode. |
| Concept | PENDIENTE | A label with a meaning: an idea, not a thing in the world. |
| Hypothesis | PENDIENTE | A label: something proposed and not settled; reflection lists hypotheses with nothing supporting them. |
| Evidence | PARCIAL | `sources` and `verified_by` exist; a `supported_by` relation and the label make evidence a thing that can itself be checked. |
| Constraint | PARCIAL | Constraints exist in the engine; a `Constraint` kind of memory (a rule of the domain, written by an agent) does not. A label; engine constraints stay what they are. |
| Goal | PENDIENTE | A label: what a task is for. |

No new node type in the engine: labels with documented meanings.

**Relations with a meaning** — EXISTENTE as extensible edges; PARCIAL as a
vocabulary:

| Relation | Plan |
| --- | --- |
| `related_to`, `depends_on`, `part_of`, `derived_from` | Constants with a meaning; `part_of` already joins communities, and joins memories too (they are told apart by the ends' labels). |
| `supported_by` | A memory or fact supported by evidence. Reflection counts what is supported. |
| `contradicts` | An agent's record that two memories say opposite things, where no constraint can. Reflection lists it beside constraint records. |
| `supersedes` | A newer memory replacing an older one: what `superseded` proposes, an agent confirms. Walks and reflection use it. |
| `caused_by` | Refused without evidence: a source or a `supported_by`, so a causal claim is never only asserted. |

Any other label stays allowed: the vocabulary adds meaning, it does not
close the list.

**Provenance**

| Field | Class | Plan |
| --- | --- | --- |
| Original source | EXISTENTE | `sources` on nodes and edges. |
| Time recorded | EXISTENTE | Creation and record times. |
| Author or agent | EXISTENTE | `recorded_by`. |
| Evidence | PARCIAL | `supported_by` (above). |
| Method of extraction | PARCIAL | A property `method` (`agent`, `rule`, `stated`, `imported`, any other), set by the tools that know it; mentions already say it. |
| Verification state, confidence | EXISTENTE | `origin`, `certainty`, `verified_*`. |
| Related memories | EXISTENTE | Edges. |
| A provenance view | PENDIENTE | `get_node` shows one block of provenance per record. |

**Identity**

| Capability | Class | Plan |
| --- | --- | --- |
| Stable ids | EXISTENTE | Time-ordered UUIDs. |
| Equivalent entities | EXISTENTE | Names, aliases, match rules, merge and undo. |
| Duplicate relations | PENDIENTE | `relate` says when the same current fact is already there and does not add a second; `maintain` lists duplicates already stored. |
| Shared references, a memory in several palaces | EXISTENTE | Edges, anchors. |
| Provenance kept through a merge | PARCIAL | Check that a merge keeps `sources`, origin and certainty of both; fix what it drops. |

**Acceptance:** related knowledge recorded, recovered by its entities,
with relations, identity and evidence kept and no unjustified duplicate.

## Phase 9: explainable retrieval (done)

Built as planned below; what it does is in
[cognitive-memory.md](cognitive-memory.md#explainable-retrieval), and the
measure in [benchmarks.md](../benchmarks.md#questions-that-join-four-memories-cratesgrafeno-memoryexamplesretrieval_qualityrs).
Tests: `crates/grafeno-memory/tests/explainable.rs` (the acceptance). The
measure found two weaknesses, both fixed: rank fusion kept a match's
neighbors below weak matches, so recall never brought a chain back whole
(the MCP tools now scale scores), and a walk by relevance ranked by words
alone (what relations reach now inherits relevance).

| Capability | Class | Plan |
| --- | --- | --- |
| Relations, neighbors, paths, multi-hop, depth limits | EXISTENTE | Traversals, `k-hop`, `path`, walks, GQ. |
| Filters by relation type in walks | PENDIENTE | `relations` (labels to follow) on `palace_walk`. |
| Time filters in walks | PENDIENTE | `valid_at` on `palace_walk`: follow only facts true then. |
| Filters by namespace, palace, rights | EXISTENTE | Namespaces, `palace`, the server's rights. |
| Exact match, text, graph, recency, structure, optional vectors | EXISTENTE | `recall`. |
| Search by entity | PARCIAL | Names find entities; a recall "about an entity" (the memories linked to it) is a walk `from` it. Documented, with an example. |
| Why a result came back (query, path, relations, criteria) | PARCIAL | `why` and `via` exist; join them in one `explanation` per result. |
| What supports it, what is uncertain | PENDIENTE | A `justification` per result, apart from the explanation: sources, origin, certainty, verification, open conflicts. The two are never mixed. |
| Exact token counts | PARCIAL | The tools say `tokens_estimated: true`; an exact count needs the agent's tokenizer, which the engine does not have. |
| Grouping related knowledge in a context | PENDIENTE | Items joined by facts packed next to each other. |
| Incremental retrieval | PENDIENTE | A `skip` on walks and contexts, to ask for the next part. |
| A measure of retrieval quality | PENDIENTE | A fixed graph and labeled questions needing three or more memories joined; recall@k and the paths found, beside latency and tokens, in `docs/benchmarks.md`. |

## Phase 10: knowledge over time (done)

Built as planned below; what it does is in
[cognitive-memory.md](cognitive-memory.md#knowledge-over-time). Tests:
`crates/grafeno-memory/tests/over_time.rs` (the acceptance) and
`a_decision_changed_three_times_keeps_its_reasons` in
`crates/grafeno-server/tests/mcp.rs`. The fix the audit asked for first
is done too: `palace_reflect` looks for overtaken memories among the
scope's own, not the whole namespace.

| Capability | Class | Plan |
| --- | --- | --- |
| Creation, update, valid from and to, known at | EXISTENTE | Bitemporal edges; node times. |
| History of node properties | PARCIAL | Kept under a history constraint only; documented as such, with how to declare one. |
| History of claims, supersession, earlier versions | PARCIAL | Edges keep versions; `supersedes` (phase 8) links memories. |
| Change log | EXISTENTE | `changes`. |
| Comparing two states | PENDIENTE | What facts held at one time and not at another, for a node or a scope. |
| Historical retrieval | EXISTENTE | `VALID AT`, `KNOWN AT`. |
| Change of state against contradiction | PARCIAL | Constraint invalidation is a change of state; flagging is a contradiction. Reflection will say which, and that facts from different sources or contexts are not contradictions unless recorded as such. |
| Consolidation kept reversible and recorded | EXISTENTE | Phase 4. |

**Acceptance:** a decision that changes several times; the current one,
its history and the recorded reasons of each change are recovered.

## Phase 11: the palace as a view (done)

Built as planned below; what it does is in
[mental-palace.md](../mental-palace.md#the-palace-as-a-view). Tests:
`two_palaces_share_a_memory_and_both_see_its_change` and
`a_route_follows_relations_in_order` in `crates/grafeno-server/tests/mcp.rs`
(the acceptance) and `crates/grafeno-memory/tests/palace_views.rs`. The
context reconstruction is `get_node` with `reconstruct: true` rather than
a tool of its own.

| Capability | Class | Plan |
| --- | --- | --- |
| A memory in several palaces, related rooms, shared anchors, crossing palaces | EXISTENTE | Anchors, doors. |
| Reusable loci | DESCARTADA | A locus is a place in one room; sharing it would make "where" ambiguous. A memory in several loci does the same. |
| Routes with a goal | PENDIENTE | `palace_route`: from a start, follow given relations in order (decisions over time, dependencies, history) and return the path; built on walks with relation filters. |
| Overloaded rooms, empty loci, reorganization proposals | EXISTENTE | `palace_review`. |
| Orphan anchors | DESCARTADA | Deleting a locus deletes its anchors; none can be orphaned. |
| Redundant relations, poorly connected areas | PENDIENTE | Added to `palace_review`: duplicate facts, and memories of a room with no relation to any other in it. |
| Reconstructing the context of a decision, event, entity, project, sequence | PARCIAL | One call that gathers a node's timeline, its relations, its room and its reflection. |
| Metacognition per room | EXISTENTE | `palace_reflect` on a room. |

**Acceptance:** two palaces sharing memories; a memory changed by an
authorized call; both views show the same change.

## Phase 12: the quality of knowledge (done)

Built as planned below; what it does is in
[mental-palace.md](../mental-palace.md#reflection). Tests:
`crates/grafeno-memory/tests/quality.rs` (the acceptance) and
`reflection_finds_what_an_expected_shape_lacks` in
`crates/grafeno-server/tests/mcp.rs`. Records are also counted by the
evidence behind them (`by_evidence`), counts and never probabilities.

| Capability | Class | Plan |
| --- | --- | --- |
| What is known, uncertain, contradictory, out of date | EXISTENTE | `palace_reflect`. |
| What sources support it | PARCIAL | Add the sources behind each known item. |
| Gaps against a question or an expected shape | PENDIENTE | An expected shape per kind (the relations and properties a Decision should have, say), declared by the agent; reflection lists what each memory lacks. |
| Verified, inferred, without evidence, conflicting, obsolete | PARCIAL | "Stated with no source" becomes its own category. |
| Not saying the world lacks what Grafeno lacks | EXISTENTE | Reflection answers "nothing stored", never "it does not exist"; the wording is checked by a test. |
| No calibrated probabilities from heuristics | EXISTENTE | Scores are called scores. |
| No outside searches | EXISTENTE | Recommendations only. |

**Acceptance:** a topic with incomplete, contradictory and out-of-date
information; the reflection tells the three apart.

## Phase 13: the graph engine on its own (done)

Checked on 2026-11-13, on Windows, debug build:

- **No dependency on the cognitive layer.** `grafeno-core` depends on
  nothing of the workspace; `grafeno` only on `grafeno-core`;
  `grafeno-query` only on `grafeno` and `grafeno-core`, for its code and
  its tests (`cargo tree -e normal,dev`). None of them depends on
  `grafeno-memory`, `grafeno-server`, an agent, a model or a provider.
  The only mention of a palace in their tests is a word in the list of
  names the documentation must not use (`crates/grafeno/tests/names.rs`).
- **Their tests pass alone.**
  `cargo test -p grafeno-core -p grafeno -p grafeno-query`: 58 test
  binaries, 290 tests passed, 0 failed, 2 ignored (the child processes
  that the crash and durability tests start themselves), in 253 s.
- **What the engine is measured on** stays in
  [benchmarks.md](../benchmarks.md): writes, reads, traversals,
  algorithms, indexes, memory, growth, recovery. Nothing measured there
  pointed to a problem this phase should fix, so, as the phase asks, no
  optimization was made without one.

One test of the engine varies from run to run:
`search_quality_survives_deletes_reinserts_and_churn`
(`crates/grafeno/tests/vector_churn.rs`) asserts a vector recall of 0.95
on an index whose levels come from random ids; it failed once in this
work under a loaded machine and passed alone. It is recorded here rather
than loosened.

In Grafeno Studio, which reaches the engine only through its HTTP API,
the same capabilities were checked: the algorithms run from Scripts
(`CALL algo.*`, with completion from `GET /v1/algorithms`), and so do
filters, conditional traversals, depth limits, ordering, paging and
batches (GQ); Administration checks integrity, rebuilds indexes, exports
and imports JSON Lines and takes a consistent backup, which Grafeno
Desktop opens as a database to restore it; indexes and query traces have
their panels. One gap was found and closed: the engine reads and writes
GraphML and CSV, and Studio offered neither. Administration now exports
and imports both, through `exportAs` and `importAs` in the TypeScript
client (with a test against the engine).

| Capability | Class | Plan |
| --- | --- | --- |
| Breadth-first search, shortest paths, components, cycles, topological order, degrees, centrality, communities | EXISTENTE | `algo.*` (about fifty), traversals. |
| Depth-first order | DESCARTADA | No use shown; breadth-first and paths cover the cases seen. |
| Property filters, conditional traversals, relation queries, depth limits, paging, ordering, batches | EXISTENTE | GQ, the API. |
| Integrity, crash recovery, consistent backups, restore, import, export, version compatibility, safe migrations, diagnostics | EXISTENTE | Phases 4, 10, 15. |
| Indexes, frequent queries, traversal costs, memory, write and read time, growth | EXISTENTE | `docs/benchmarks.md`. |

**Acceptance:** the engine's own tests (`crates/grafeno/tests`,
`crates/grafeno-query/tests`), which use no palace, memory, agent or
model, pass; this phase only checks and records it.

## Order

| Step | Phase | What |
| --- | --- | --- |
| 1 | 8 | Kinds, relation vocabulary, `method`, duplicate facts, provenance kept through merges, a provenance view. |
| 2 | 9 | Walk filters (relations, time), explanation and justification per result, token estimate said, grouping, `skip`, a quality measure. |
| 3 | 10 | Comparing two states, contradiction against change, a decision's history. |
| 4 | 11 | Routes, review of redundant and loose memories, context reconstruction. |
| 5 | 12 | Sources per known item, "no source" category, expected shapes. |
| 6 | 13 | The engine's tests alone, recorded. |

Fixing first, from the audit: `reflect` runs `superseded` over the whole
namespace; it should look at the scope's memories only.
