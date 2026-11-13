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

The MCP server (`grafeno mcp`) has six tools for palaces. Each takes the
optional `namespace` every tool takes. Where a palace or a room is asked
for, its name or its id will do.

| Tool | Writes | What it does |
| --- | --- | --- |
| `palace_create` | yes | Makes a palace, or returns the one of that name. |
| `palace_create_room` | yes | Makes a room in a palace, with its entrance and the loci asked for; or returns the one of that name. |
| `palace_anchor` | yes | Puts an existing memory at a locus of a room (the entrance by default), with an optional note; makes the locus when it is missing. With `remove`, takes the anchor away. Returns every place the memory is at. |
| `palace_map` | no | With no arguments, every palace with its rooms, loci and counts. With `palace`, that palace. With `room`, that room and the memories at each locus. |
| `palace_walk` | no | From a `topic` or a node (`from`): the rooms and loci that match, the memories anchored there, the anchored memories that match, and what the graph's relations lead to from those, up to `hops` (2 by default, 3 at most). |
| `palace_enter` | no | A room's context for the start of a session: its loci, its memories (strongest first) and the facts that connect them as text within a token `budget`, and the latest checkpoint about the room. |

A read-only server (`--read-only`) offers only the three that read.
An agent fenced to some namespaces (`--allow`) reaches palaces in those
only.

What these tools do not do, because a tool already does:

| To | Use |
| --- | --- |
| Save where the work in a room stands | `checkpoint` with `about` set to the room's id |
| Rename or describe a palace, room or locus | `update` |
| Delete one | `forget` (see [Known limits](#known-limits)) |
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

## Known limits

- **Deleting is not recursive.** `forget` on a room deletes the room and
  its edges and leaves its loci without a room; on a palace, it leaves
  the rooms without a palace. The memories are never touched, and
  `palace_anchor` reports such places with no room or palace. To delete
  a room cleanly, forget its loci first.
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
| `crates/grafeno-server/tests/mcp.rs` | The six tools end to end beside the existing ones; bad input; namespaces and the agent's fence; a read-only server on a file another server wrote. |
| `crates/grafeno-server/tests/http.rs` | A palace read through the subgraph and query routes. |

## Planned

None of this exists yet.

- Routes: ordered paths between loci and between rooms, and walking one.
- Doors: links between rooms of different palaces.
- Deleting a room or a palace with what is under it, in one call.
- Suggested placement: proposing a room for a new memory, for the agent
  to accept or not.
- Rooms proposed from community summaries.
- Reorganization: splitting a crowded room, merging two.
- Palace routes in the HTTP API and in the client libraries.
- A palace view in Grafeno Studio.
- Palaces shared between namespaces, under the rights there are.
