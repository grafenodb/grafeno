# Mental Palace

A Mental Palace arranges the memories an agent already has into places,
so they are found again by where they are: a palace has rooms, a room has
places inside it, and memories are anchored at those places. An agent
enters the room of the work at hand at the start of a session and gets
back what is there and where the work was left; from a topic, it walks to
the memories around it.

It is a layer over the graph and the memory library. A palace is ordinary
nodes and edges in a namespace, written through the same transactions as
everything else: there is no second store, no file format of its own and
nothing to migrate.

- [Concepts](#concepts)
- [Design](#design)
- [Model](#model)
- [Tools](#tools)
- [An example](#an-example)
- [With the rest of the memory](#with-the-rest-of-the-memory)
- [From Rust](#from-rust)
- [In Grafeno Studio](#in-grafeno-studio)
- [Known limits](#known-limits)
- [Running and testing](#running-and-testing)
- [Planned](#planned)

How it was built, stage by stage, is in
[mental-palace-roadmap.md](mental-palace-roadmap.md).

Where it is going, in six phases, is
[mental-palace/IMPLEMENTATION_PLAN.md](mental-palace/IMPLEMENTATION_PLAN.md);
how memories and facts say how they are known is
[mental-palace/cognitive-memory.md](mental-palace/cognitive-memory.md).

## Concepts

| Concept | What it is |
| --- | --- |
| **Palace** | The main space: `Work`, `Home`, one per large area of knowledge. |
| **Room** | A group of memories about one project, domain or subject. |
| **Locus** | A place inside a room: `Decisions`, `Open questions`. Every room has one named `entrance`. |
| **Memory** | A node that already exists: a note, a document, a person, a checkpoint. Nothing new is stored for it. |
| **Anchor** | The link from a memory to a locus. A memory can have several. |

## Design

**No second store.** Palaces, rooms and loci are nodes; what joins them
are edges. They are saved, replicated, backed up, exported and restricted
by the same code as any node, and a database made before palaces existed
opens unchanged, with no palaces in it.

**Memories are not copied.** An anchor is one edge from the memory to a
locus. The memory's node, text, importance and own relations are the
same before and after, and removing an anchor removes that edge only.

**A palace is not a memory.** Recall, context, community summaries,
maintenance and extraction read a namespace as memories; they leave the
nodes a palace is made of out, so they answer the same over a namespace
with a palace as over the same memories without one. A room named after
a project is not taken for the project.

**Nothing new to search with.** Finding a room by its topic is the
full-text search that recall uses; walking from a memory is the graph's
own traversal; packing a room into a token budget is the packing
`context` uses. No embeddings of its own, no model, no new index.

**The agent decides.** Nothing is anchored, moved or regrouped on its
own. A palace holds what an agent put there.

**One namespace.** A palace and everything under it live in one
namespace, and an anchor cannot join a memory of one namespace to a
locus of another. Two namespaces may each have a palace of the same name.

## Model

```
(Palace) -[has_room]-> (Room) -[has_locus]-> (Locus) <-[anchored_at]- (memory)
```

| Node | Label | Properties | Text (searched) |
| --- | --- | --- | --- |
| Palace | `Palace` | `name`, `description` | name and description |
| Room | `Room` | `name`, `topic`, `description` | name, topic and description |
| Locus | `Locus` | `name`, `position` | name |

| Edge | From → to | Properties |
| --- | --- | --- |
| `has_room` | palace → room | — |
| `has_locus` | room → locus | — |
| `anchored_at` | memory → locus | `note` (why it is there); who recorded it is the edge's `recorded_by` |

Rules:

- Names are compared without case, accents or punctuation. A palace's
  name is unique in its namespace, a room's in its palace, a locus's in
  its room.
- Creating one that exists returns the one there is, unchanged. Creating
  a room that exists with more loci adds the ones it lacks.
- Loci keep the order they were made in (`position`); the entrance is
  first.
- Anchoring a memory twice at the same locus keeps one anchor.
- A palace, a room or a locus cannot itself be anchored.

These three labels and three edge labels now have a meaning. A database
that already used `Palace`, `Room`, `Locus`, `has_room`, `has_locus` or
`anchored_at` for something of its own would be read as holding palaces.

## Tools

The MCP server (`grafeno mcp`) has eight tools for palaces. Each takes the
optional `namespace` every tool takes. Where a palace or a room is asked
for, its name or its id will do.

| Tool | Writes | What it does |
| --- | --- | --- |
| `palace_create` | yes | Makes a palace, or returns the one of that name. |
| `palace_create_room` | yes | Makes a room in a palace, with its entrance and the loci asked for; or returns the one of that name. |
| `palace_anchor` | yes | Puts an existing memory at a locus of a room (the entrance by default), with an optional note; makes the locus when it is missing. With `remove`, takes the anchor away. Returns every place the memory is at. |
| `palace_map` | no | With no arguments, every palace with its rooms, loci and counts. With `palace`, that palace. With `room`, that room and the memories at each locus. |
| `palace_walk` | no | From a `topic` or a node (`from`): the rooms and loci that match, the memories anchored there, the anchored memories that match, the rooms their doors lead to, and what the graph's relations lead to from those, up to `hops` (2 by default, 3 at most). `order`: `distance` (default) or `relevance`; `relations` and `valid_at` narrow what is followed; `skip` gives the next part; `budget`: also the memories reached and the facts joining them as text within that many tokens. Each memory says how it was found (`found_by`) and what supports it (`justification`). |
| `palace_enter` | no | A room's context for the start of a session: its loci, its memories (strongest first) and the facts that connect them as text within a token `budget`, and the latest checkpoint about the room. |
| `palace_review` | no | Proposals to arrange a palace better, each with its reason and the call that carries it out; nothing applied (see [below](#review-removal-and-undoing)). |
| `palace_reflect` | no | What is known about a topic, a room or a node, and what is not: known, uncertain, conflicts, stale, gaps, what to check next (see [below](#reflection)). |

A read-only server (`--read-only`) offers only the five that read.
An agent fenced to some namespaces (`--allow`) reaches palaces in those
only.

What these tools do not do, because a tool already does:

| To | Use |
| --- | --- |
| Save where the work in a room stands | `checkpoint` with `about` set to the room's id |
| Rename or describe a palace, room or locus | `update` |
| Delete one, with what is under it | `forget` (`dry_run: true` first) |
| Search every memory, anchored or not | `recall`, `context` |
| See a memory with its relations | `get_node` |

Errors come back as tool errors with a sentence that says what to do: an
unknown room names the tool that lists or makes rooms; a room name that
two palaces share asks for the palace.

## An example

Every result below is what the server returned, with ids shortened.

A palace and a room for one project:

```json
palace_create      { "name": "Work", "description": "Projects I work on" }
→ { "id": "01a1…cb46", "name": "Work", "created": true }

palace_create_room { "palace": "Work", "name": "Storage",
                     "topic": "the storage engine", "loci": ["Decisions"] }
→ { "id": "01a1…181d", "created": true,
    "palace": { "id": "01a1…cb46", "name": "Work" },
    "room": { "name": "Storage", "topic": "the storage engine", "memories": 0,
              "loci": [ { "name": "entrance",  "position": 0, "count": 0 },
                        { "name": "Decisions", "position": 1, "count": 0 } ] } }
```

Memories are stored as always (`remember`, `relate`), then anchored:

```json
remember { "text": "Only one transaction writes at a time", "labels": ["Note"] }   → 01a1…7361
remember { "text": "The whole database is one file", "labels": ["Note"] }          → 01a1…1c39
remember { "text": "A replicated log orders the writes", "labels": ["Note"] }      → 01a1…69d6
relate   { "from": "01a1…7361", "label": "leads_to", "to": "01a1…69d6" }

palace_anchor { "memory": "01a1…7361", "room": "Storage", "locus": "Decisions",
                "note": "why writes are serial" }
→ { "anchored": true, "locus_created": false,
    "memory": { "id": "01a1…7361", "name": "Only one transaction writes at a time" },
    "places": [ { "palace_name": "Work", "room_name": "Storage",
                  "locus_name": "Decisions", "note": "why writes are serial" } ] }

palace_anchor { "memory": "01a1…1c39", "room": "Storage" }
```

The third note is anchored nowhere. A walk from the room's topic finds
the two that are in the room and, one relation further, the third:

```json
palace_walk { "topic": "storage engine" }
→ { "places": [ { "kind": "Room", "path": "Work / Storage" } ],
    "memories": [
      { "name": "“The whole database is one file”", "hops": 0,
        "places": [ { "room_name": "Storage", "locus_name": "entrance" } ] },
      { "name": "“Only one transaction writes at a time”", "hops": 0,
        "places": [ { "room_name": "Storage", "locus_name": "Decisions" } ] },
      { "name": "“A replicated log orders the writes”", "hops": 1,
        "origin": "01a1…7361",
        "via": [ { "label": "leads_to", "forward": true } ] } ] }
```

Before stopping, the agent saves where the work stands, with the tool
that was already there:

```json
checkpoint { "task": "Storage design", "summary": "One writer settled",
             "next": ["Measure the file"], "about": "01a1…181d" }
```

In a later session, this agent or another one enters the room:

```json
palace_enter { "room": "Storage" }
→ { "palace": { "name": "Work" },
    "room": { "name": "Storage", "memories": 2, "loci": [ … ] },
    "context": "## Memory\n[1] (Note)\nOnly one transaction writes at a time\n\n[2] (Note)\nThe whole database is one file\n\n## Facts\n- “Only one transaction writes at a time” leads_to “A replicated log orders the writes”\n",
    "tokens": 74,
    "memories": [ { "id": "01a1…7361", "truncated": false },
                  { "id": "01a1…1c39", "truncated": false } ],
    "left_out": 0,
    "checkpoint": { "task": "Storage design", "summary": "One writer settled",
                    "next": ["Measure the file"] } }
```

## With the rest of the memory

| Feature | With a palace |
| --- | --- |
| `remember`, `relate`, `update` | Unchanged. A memory named like a room is a new memory, not the room. |
| `recall`, `context` | Return memories, never palaces, rooms or loci, and do not treat two memories as related because they share a room. A recall that asks for `Room`, `Locus` or `Palace` by label does return them. Where a memory is anchored is not listed among its facts. |
| `checkpoint`, `resume` | Unchanged. A checkpoint whose `about` is a room is what `palace_enter` returns; `resume` with that `about` returns it too. |
| Community summaries | Palace nodes are not clustered, so a palace does not become the center of every community. |
| `maintain`, `consolidate` | Palace nodes are never proposed as duplicates, as weak memories to archive, or as what a group of memories is about. |
| `extract` | A palace node's name is not a known entity, and its text is not read. |
| Archived memories | Stay anchored, and are left out of walks and of a room's context; a map still counts them. |
| `forget` on a memory | Removes its anchors with it. |
| Namespaces, `--allow`, `--read-only`, `--as` | Apply as to any tool: anchors carry who recorded them. |

`palace_enter` records an access on the memories it returns, as `recall`
and `context` do, and not on a read-only server.

## From Rust

The tools are thin: the model is in `grafeno-memory`
(`crates/grafeno-memory/src/palace.rs`).

```rust
use grafeno::{Grafeno, NewNode};
use grafeno_memory::{
    EnterOptions, NewRoom, WalkQuery, anchor, create_palace, create_room, enter, find_locus, walk,
};

let db = Grafeno::open("memory.redb")?;
let (room, note) = db.write(|tx| {
    let palace = create_palace(tx, "me", "Work", None)?.id;
    let room = create_room(tx, "me", palace, NewRoom::new("Storage").locus("Decisions"))?.id;
    let note = tx.create_node("me", NewNode::new("Note").text("One writer at a time"))?;
    Ok((room, note))
})?;
let decisions = find_locus(&db.begin_read()?, "me", room, "Decisions")?.unwrap();
db.write(|tx| anchor(tx, "me", note, decisions, Some("why")))?;

let read = db.begin_read()?;
let found = walk(&read, "me", &WalkQuery::topic("storage"))?;
let entered = enter(&read, "me", room, &EnterOptions::with_budget(1_500))?;
println!("{}", entered.context.rendered);
```

| Function | What it does |
| --- | --- |
| `create_palace`, `create_room`, `create_locus` | Make one, or find it. |
| `find_palace`, `find_room`, `find_locus` | By name. |
| `anchor`, `unanchor`, `anchors_of` | Anchors of a memory. |
| `palaces`, `map_palace`, `map_room` | The map. |
| `walk` | From a topic or a node. |
| `enter` | A room's context and checkpoint. |
| `palace::is_structure`, `palace::structure` | Tell palace nodes from memories. |
| `context_of` | Packs any list of nodes into a token budget, as `context_for` does a recall's. |

## In Grafeno Studio

Grafeno Studio reads an engine through its HTTP API, and a palace needs
nothing more from it: the subgraph route returns palaces, rooms, loci,
anchors and the anchored memories as the nodes and edges they are, and a
query walks them as any path.

```
MATCH (p:Palace)-[:has_room]->(r:Room)-[:has_locus]->(l:Locus)<-[:anchored_at]-(m)
RETURN p.name, r.name, l.name, m
```

A test asks both routes for a palace and checks what comes back
(`a_palace_is_read_as_ordinary_nodes_and_edges`). Palaces, rooms and
loci have a `name`, so they are drawn with it, and their labels tell
them apart.

What was not checked: the Studio's own screen. It is a separate product
in its own repository, and this work did not open it. There is no view
made for palaces; they appear in the graph views there are.

## Doors, relevance and contexts

Added in phase 3 of the [plan](mental-palace/IMPLEMENTATION_PLAN.md).

- **Doors.** A `leads_to` fact from one room to another, recorded with
  `relate` like any fact, is a door. `palace_map` lists a room's doors;
  `palace_walk` goes through the doors of the rooms it found, one door,
  and what is anchored beyond is one step away, with the door in its way
  (`via`) and the room it came from (`origin`, and `through` on the
  place). A walk inside one palace does not take a door to another. A
  `leads_to` between memories is an ordinary fact.
- **Relevance.** With `order: "relevance"`, a walk gathers up to ten times
  its limit, ranks what it reached by how well its words match the topic
  (the full-text score), then by distance, and keeps the best. A memory
  reached through relations inherits half the relevance of the one it
  was reached from per relation, when that is more than its own (since
  phase 9 of the [roadmap](mental-palace/ROADMAP.md)). Each memory says
  its `relevance`.
- **A context.** With `budget`, a walk also packs the memories it reached,
  in its order, and the facts that join them, into that many tokens: a
  question that needs several memories joined is answered by the text,
  with the way that joined them in `via`.
- **Repeats.** `context`, `palace_enter` and a walk's context leave out a
  memory whose words are nearly those of one already packed (85 % in
  common), and say how many they left out.

## Review, removal and undoing

Added in phase 4 of the [plan](mental-palace/IMPLEMENTATION_PLAN.md).

- **`palace_review`** (reads only) proposes how to arrange a palace, each
  proposal with its reason and the call that carries it out, and applies
  nothing:

  | Proposal | When |
  | --- | --- |
  | `split_room` | A room holds more memories than `crowded` (60): one context cannot carry it. The largest communities among them say how to split it. |
  | `empty_room`, `empty_locus` | Nothing anchored there (the entrance is not counted as an empty locus). |
  | `move_memory` | A memory's community lives mostly (two thirds) in another room. |
  | `merge_rooms` | Two rooms share half of their memories or more. |
  | `new_room` | A community of four memories or more has none in the palace. |

  What depends on communities needs community summaries
  (`summarize_communities`); without them the review says so and proposes
  the rest.
- **`forget` on a palace, a room or a locus** deletes it with what is under
  it: a palace's rooms, a room's loci, and the anchors at them. No memory
  is deleted; the answer lists those left in no place. `dry_run: true`
  says what would go and changes nothing.
- **Superseded memories.** `maintain` lists pairs of memories about the
  same entity where a newer one shares many of the older one's words (40 %
  to 85 % of them: past that it is a repeat): the newer may say what the
  older said, as it is now. Nothing is changed; `feedback` `obsolete`
  archives the older one when it is.
- **Undoing a consolidation.** `consolidate` with `undo` and a summary's
  id deletes the summary and brings back the memories it archived; those
  archived before it stay archived.

## Several agents, one memory

Added in phase 5 of the [plan](mental-palace/IMPLEMENTATION_PLAN.md).

Only one process can open a database file. For several agents to share
a memory, one `grafeno serve` owns the file and every agent is its
client: each agent's MCP process runs its tools on the server, with the
key of an account of its own.

```bash
grafeno serve --config grafeno.toml
```

```bash
grafeno mcp --url http://127.0.0.1:7810 --token "$AGENT_KEY" --namespace team
```

- **Rights.** The server checks each agent's rights by its key. An agent
  that may only read a namespace gets the tools that read there and is
  refused the rest; an agent with no right on a namespace is refused
  there with 403. Private and shared palaces are namespaces: a palace in
  a namespace only one account may use is that account's; a palace in a
  namespace several may read is shared.
- **Who did what.** Each write is recorded as the agent's account
  (`@recorded_by`), and refusals are in the audit log with the account
  they refused.
- **At once.** The server serializes the writes of every agent on its one
  writer; reads run beside them. In the test two agents write at once,
  each also reading, and every write lands, with the file checked after.
- **Over HTTP.** `GET /v1/{ns}/tools` lists the tools the caller may use
  there; `POST /v1/{ns}/tools/{name}` runs one with a JSON object of
  arguments and answers what MCP answers (`content`, `structuredContent`,
  `isError`). GQ runs on `POST /v1/{ns}/query`.

Limits of the shared way: rights by namespace only (a caller with rules on
labels, properties or records is refused by the tools route and uses the
API's own routes), and no tools route on a cluster yet.

## Reflection

Added in phase 6 of the [plan](mental-palace/IMPLEMENTATION_PLAN.md).

`palace_reflect` says what is known about a topic, a room or a node, and
what is not. It reads only, and every item names the record it comes
from, so an agent can check it with `get_node` or `timeline` before it
answers:

| Section | What is in it |
| --- | --- |
| `known` | The memories and facts found, counted by kind (Fact, Decision, Experience, Task, Question) and by how they are known (stated, observed, inferred, verified, not stated); the verified ones named. |
| `uncertain` | What was inferred with no source, and what has a certainty under 0.5. |
| `conflicts` | Clashes recorded under constraints that involve them, or recorded by an agent with `contradicts`, the unsettled ones first. Each is a `change` (the constraint ended or shortened the older fact: the state changed) or a `contradiction` (a flagged clash, or a `contradicts`: both stand and wait for someone). |
| `stale` | Facts no longer true, memories archived or marked obsolete, memories long unused, and older memories a newer one may overtake. |
| `gaps` | Open questions, things mentioned and never described, places in a room with nothing at them, and steps left in the room's or node's checkpoint. |
| `next` | What to check, each step with the call that does it and the records it is about. |
| `limits` | How much it looked at, whether there were more matches, and what it cannot see. |

A topic with nothing stored answers `nothing: true` and empty sections:
no unrelated memory is offered in place of one that is not there.

**Questions** are memories labeled `Question`: something not known yet,
kept so it is not forgotten. A question is open until a memory is related
to it with `answers` (`relate` from the answer to the question).

What a reflection does not do: decide that something is true or false,
look anything up, or change anything. Origin and certainty are what agents
stated; conflicts are those recorded under constraints or with
`contradicts`, so two facts that clash with neither to say so are not
found, and facts from different sources are not taken for contradictions; a topic is matched by
its words, whole, so a memory that says it in other words is not found.

## Known limits

- **A walk is about what is in a palace.** A memory that matches the
  topic and is anchored nowhere is not returned; `recall` is for that. A
  walk with no result says so.
- **Walks order by strength, not relevance.** The topic chooses the
  places and memories to start from; what is reached from them is
  ordered by distance and then by importance and recent use.
- **Very connected memories stop a walk.** A memory with more than 50
  relations is returned and not walked through.
- **Text scores shift a little.** Palace nodes have text, and full-text
  scoring counts every text in a namespace. In the tests the order of
  recall's results is the same with and without a palace; the scores are
  not equal to the last digit. Recall takes the best 50 text matches
  before filtering, so in a query that matches more than 50 texts, palace
  nodes that match can take the place of a memory at the tail.
- **Large rooms are read whole.** A map counts every anchor of a locus,
  a walk reads every memory at the places it starts from, and entering a
  room reads all its memories to order them. In a namespace that has a
  palace, recall also counts a node's relations by reading them. Rooms
  with tens of thousands of memories were not measured.
- **A room is found among its palace's rooms**, and a locus among its
  room's loci, by reading them. Palaces with thousands of rooms were not
  measured.
- **One checkpoint.** `palace_enter` returns the latest checkpoint about
  the room, from any task. Checkpoints about the palace or about a memory
  in the room are not looked for.
- **MCP only.** The HTTP API and the client libraries have no palace
  routes; over them, a palace is read and written as nodes and edges.
- **A checkpoint's time** (`at`) is in microseconds, as `resume` gives it.

## Running and testing

An MCP server on a file, for any MCP client:

```bash
grafeno mcp --db ~/.grafeno/memory.redb --namespace me
```

The tests:

```bash
cargo test -p grafeno-memory
```

```bash
cargo test -p grafeno-server --test mcp
```

```bash
cargo test --workspace
```

| File | What it covers |
| --- | --- |
| `crates/grafeno-memory/tests/palace.rs` | Creating and finding; one of each when created twice; anchoring at several loci with the memory unchanged; namespaces; a file closed and opened again. |
| `crates/grafeno-memory/tests/palace_isolation.rs` | Recall, context, duplicates, weak memories, consolidation, communities and extraction answer the same with a palace as without. |
| `crates/grafeno-memory/tests/palace_walk.rs` | Maps; walks by topic, from a node, through relations, inside one palace, with a hop limit; entering a room, within a budget, after a restart, with its checkpoint. |
| `crates/grafeno-server/tests/mcp.rs` | The palace tools end to end beside the existing ones; bad input; namespaces and the agent's fence; a read-only server on a file another server wrote. |
| `crates/grafeno-server/tests/http.rs` | A palace read through the subgraph and query routes. |

## Planned

None of this exists yet.

- Routes: ordered paths between loci and between rooms, and walking one.
- Suggested placement: proposing a room for a new memory, for the agent
  to accept or not.
- Rooms proposed from community summaries.
- Reorganization: splitting a crowded room, merging two.
- Palace routes in the HTTP API and in the client libraries.
- A palace view in Grafeno Studio.
- Palaces shared between namespaces, under the rights there are.
