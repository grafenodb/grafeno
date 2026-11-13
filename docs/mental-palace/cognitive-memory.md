# Cognitive memory

Phase 2 of the [implementation plan](IMPLEMENTATION_PLAN.md): turning
what an agent reads into knowledge it can tell apart. Grafeno has no
model inside; the agent does the reading, with its own model or by hand,
and hands over what it found through a structured call. Grafeno resolves
the entities with the rules it has, stores the relations, and keeps how
each thing came to be known.

- [How a memory or a fact is known](#how-a-memory-or-a-fact-is-known)
- [Kinds of memory](#kinds-of-memory)
- [An agent's reading in one call](#an-agents-reading-in-one-call)
- [Verifying](#verifying)
- [Where it shows](#where-it-shows)
- [Compatibility](#compatibility)
- [Limits](#limits)
- [Tests](#tests)

## How a memory or a fact is known

Two properties with a fixed meaning, on memories (nodes) and on facts
(edges):

| Property | Values | Meaning |
| --- | --- | --- |
| `origin` | `stated` | Someone said or wrote it. |
| | `observed` | The agent saw it: a file, a command's output, a page. |
| | `inferred` | The agent concluded it from other things. |
| | `verified` | Checked against a source. |
| `certainty` | 0 to 1 | How sure. |

Either may be left out. A record with neither reads as **not stated**,
never as verified: that is how every record written before this phase
reads. Grafeno does not decide either value; it refuses only an unknown
origin and a certainty outside 0 to 1.

`remember` and `relate` take both:

```json
{"name": "relate", "arguments": {
  "from": "…", "label": "runs_on", "to": "…",
  "origin": "observed", "certainty": 0.9}}
```

## Kinds of memory

Four labels have a meaning, beside any other a memory has:

| Label | For |
| --- | --- |
| `Fact` | Something that is the case. |
| `Decision` | A choice made, with its reasons. |
| `Experience` | Something that happened: an attempt, an incident. |
| `Task` | Something to do. |
| `Question` | Something not known yet: open until a memory is related to it with `answers` (phase 6). |

They are labels, so `remember` takes them in `labels` and `recall` and
`context` filter by them, as by any label. Nothing classifies a memory on
its own; `remember`'s description asks the agent to.

## An agent's reading in one call

`add_mentions` took the entities an agent found in a memory. It now takes
the relations between them too:

```json
{"name": "add_mentions", "arguments": {
  "id": "<the chunk>",
  "mentions": [
    { "surface": "billing service", "label": "Service" },
    { "surface": "Ledger", "label": "Database" },
    { "surface": "Ana Torres", "label": "Person" },
    { "surface": "events queue", "label": "Queue" }
  ],
  "relations": [
    { "from": 0, "label": "runs_on", "to": 1, "origin": "stated", "certainty": 0.95 },
    { "from": "Ana Torres", "label": "maintains", "to": "billing service" },
    { "from": 0, "label": "publishes_to", "to": "events queue", "certainty": 0.6,
      "valid_from": "2026-01-01" }
  ]}}
```

- Each entity is linked to the node it is (by the label's match rule, or
  by name), made when new, or queued for review when ambiguous; the
  memory gets a `mentions` edge to it. This is what `add_mentions` did.
- Each relation names two mentions of the same call, by position (from
  0) or by surface, and becomes a fact from one entity to the other, with
  the memory as its source, its valid dates, and how it is known:
  `inferred` unless the agent says the text `stated` it in so many words.
- A relation whose entity waits in review, or that names a mention not
  in the call, is skipped, and the answer says which and why. A relation
  with a bad origin or certainty, or no label, refuses the whole call
  before anything is written.
- A relation runs into the constraints there are like any fact: under
  "one host per service", a newer `runs_on` ends the older, and the
  answer lists it under `contradicted`.

The answer:

```json
{
  "mentions": 4, "created": 4, "linked": 0, "waiting_for_review": 0,
  "entities": [
    { "surface": "billing service", "id": "…", "name": "billing service" },
    { "surface": "Ledger", "id": "…", "name": "Ledger" },
    …
  ],
  "relations": ["…", "…", "…"]
}
```

In Rust: `grafeno_memory::ingest_reading`, with `AgentMention` and
`AgentRelation`.

## Verifying

`feedback` takes a fourth verdict, `verified`, for a memory or a fact the
agent checked and found to hold:

```json
{"name": "feedback", "arguments": {
  "id": "<the fact>", "verdict": "verified", "source": "<the document>"}}
```

Its origin becomes `verified` and its certainty 1 (or the `certainty`
given), with `verified_by` (the source, when named) and `verified_at`.
What it was before is kept in `verified_from`, so a check never erases
how the thing was first known. The source must be a node of the same
namespace and not the record itself. A refuted memory or fact is the
verdict `wrong`, as before.

In Rust: `grafeno_memory::verify`.

## Where it shows

| Tool | What it shows |
| --- | --- |
| `get_node` | `known` for the node (`"not stated"` when nothing was said), and for each fact that has one, with the fact's `sources`. |
| `recall` | `known` on each result that has one. |
| `context` | After a memory's labels, and after a fact: `[stated, certainty 0.95]`. Nothing when not stated. |
| `query` | The properties, as any: `MATCH ()-[r]->() WHERE r.origin = 'inferred' RETURN r`. |

## Compatibility

- No change to the file layout: `origin`, `certainty`, `verified_by`,
  `verified_at` and `verified_from` are properties.
- A database written before reads the same: its records are not stated,
  and every tool answers as it did. The only new field in an old
  answer is `get_node`'s `known: "not stated"`.
- `add_mentions` without `relations` does what it did; its answer gains
  `entities` and `relations`.
- These property names now have a meaning. A database that used `origin`
  or `certainty` for something else would have them read as these, and a
  value they do not admit read as not stated.

## Limits

- Certainty is what the agent said. Grafeno does not compute it, combine
  it across sources, or lower it with time.
- A fact's earlier origin is kept only by `verified` (`verified_from`).
  Changing `origin` with `update` overwrites it, as any property.
- Entities and relations come from the agent's reading or from
  extraction's rules; nothing reads sentences for relations inside the
  engine.
- `remember`'s `links` take no origin or certainty: use `relate` for a
  fact that needs them.
- Over HTTP, the properties can be written and read like any other, and
  `ingest_reading` and `verify` have no route of their own yet (phase 5).

## Tests

| File | What it covers |
| --- | --- |
| `crates/grafeno-memory/tests/knowledge.rs` | Values checked and read back; an agent's reading with entities, relations, sources and certainty; skipped and refused relations; a relation under a constraint; verifying, with its source and what it was; a file closed and opened again. |
| `crates/grafeno-server/tests/mcp.rs` (`a_technical_description_becomes_entities_relations_and_places`) | The acceptance of the phase: a technical description ingested, the agent's reading in one call, facts with how they are known, a fact verified against the document, a memory of a kind, the context, the entities in a room and found by a walk, and bad input refused. |
