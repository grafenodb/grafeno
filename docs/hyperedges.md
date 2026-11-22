# Hyperedges

A hyperedge is one fact that relates two or more nodes at once, each in a
role of its own: a decision with who proposed it, who implemented it, who
reviewed it, the task it is about and the evidence for it. It is a record of
its own in the engine, beside nodes and edges, not a node joined to its
participants by binary edges.

The model, its storage and indexes and the Rust API of the `grafeno`
crate came first; GQ reads and writes them with `MATCH HYPEREDGE`, `CREATE
HYPEREDGE`, `MERGE HYPEREDGE`, `SET`, `DELETE`, `DETACH DELETE` and
`RETRACT` (see
[query-language.md](query-language.md#hyperedges)), so does every place
that runs GQ (the shell, `POST /v1/{ns}/query`, the MCP `query` tool). The
HTTP API has routes of its own for them, batches take them, and MCP has
three tools ([below](#http-and-mcp)). Property indexes and unique or key
constraints apply to them ([below](#property-indexes-and-constraints)).
The replication log carries them, so cluster members and read replicas
hold them too, and the change log and event subscriptions record their
changes, and exports (JSON lines) carry them. Recall and context do not
return them, by measurement (see [Limits](#limits)).

## When to use one

| Use | When |
| --- | --- |
| An edge | The fact relates two things and has a direction: `ana works_at acme`. Traversals, paths, algorithms and GQ patterns walk edges. |
| A hyperedge | The fact relates three or more things, or two in roles that are not a direction, and it must be read whole: every participant and role at once, from any of them. |
| A node with edges (an event node) | The fact needs what a node has: text and an embedding to be found by search, labels, importance and memory state, a place in a palace, or edges to it from other facts. |

A hyperedge does not replace either. Facts already stored as edges or event
nodes keep their meaning; nothing converts them.

## Model

`grafeno_core::HyperEdge`:

| Field | Meaning |
| --- | --- |
| `id` | `HyperEdgeId`, a UUID v7 like node and edge ids |
| `label` | the kind of relation (`decision`, `collaboration`…); it never changes |
| `participants` | `Vec<Participant>`: a `node` and an optional `role` each |
| `props` | properties, as a node's or an edge's |
| `sources` | the nodes it was derived from (provenance) |
| `valid_from`, `valid_to` | valid time: when the fact holds in the world |
| `recorded_at`, `expired_at` | record time: when the engine learned it and when it was retracted |
| `recorded_by` | the principal that recorded it, when known |

The namespace is not a field: like nodes and edges, a hyperedge lives in the
namespace it was written to, and every key starts with it.

Time works as for edges: `invalidate_hyperedge` closes the valid time (the
world changed), `retract_hyperedge` sets `expired_at` (it was a mistake, kept
in history), and `HyperEdgeFilter` reads current knowledge by default, or
what held at an instant (`valid_at`) or what was known at one (`known_at`).

### Invariants

The engine keeps these on every write, and `ReadTx::check` verifies them:

1. At least two distinct nodes take part.
2. Every participant is a node of the same namespace that exists.
3. No (node, role) pair appears twice. A node may take part more than once
   in different roles (an agent that both proposes and reviews); a node
   without a role and the same node with a role are different pairs.
4. Participants are stored sorted by node, then role (no role first). The
   order is for determinism only: it carries no direction or rank. A role
   says what a participant is.
5. A role, when given, is not empty and has no surrounding spaces (as
   labels).
6. A hyperedge has at most `max_participants` participants:
   `DEFAULT_MAX_PARTICIPANTS` (256), set per database with
   `Grafeno::set_max_participants`.
7. The incidence and label indexes hold exactly what the records call for.

## Storage

Three redb tables, added by layout 19:

| Table | Key | Value |
| --- | --- | --- |
| `hyperedges` | `(ns, hyperedge_id)` | the `HyperEdge` record (JSON, like nodes and edges) |
| `hyperedge_incidence` | `(ns, node_id, label, hyperedge_id)` | `()` |
| `hyperedge_labels` | `(ns, label, hyperedge_id)` | `()` |

- The incidence index has one entry per distinct node: a node in two roles
  of one hyperedge has one entry. A node's hyperedges are one range read,
  and those of one label a narrower one, as the adjacency of edges is keyed
  by `(node, label)`.
- The label index lists a label's hyperedges in id (creation) order, for
  pages.
- Every write of a hyperedge changes its record and its index entries in the
  same transaction. Changing participants removes the entries of the nodes
  that left and adds those of the nodes that joined; the label never
  changes, so its entry is written once.
- No count is kept: nothing reads one yet.
- `ReadTx::check` compares both indexes with the records and reports
  hyperedges that name a missing node or have fewer than two.
  `WriteTx::rebuild_indexes` writes the indexes again from the records;
  a hyperedge that names nodes that no longer exist loses them, and is
  dropped when fewer than two are left (`RebuildReport::dropped_hyperedges`).
- Backups copy the three tables, and dropping a namespace clears them.

## API

All in the `grafeno` crate, on the transactions everything else uses.

Writes (`WriteTx`):

| Method | Does |
| --- | --- |
| `create_hyperedge(ns, NewHyperEdge)` | creates one; returns its id |
| `set_hyperedge_props(ns, id, props)` / `remove_hyperedge_props(ns, id, keys)` | as for edges |
| `set_hyperedge_participants(ns, id, participants)` | replaces them, checked as on creation |
| `invalidate_hyperedge(ns, id, at)` / `retract_hyperedge(ns, id)` | valid time and record time, as for edges |
| `delete_hyperedge(ns, id)` | deletes it and its entries, without history |
| `delete_node_with(ns, id, OnHyperEdges)` | deletes a node, saying what to do with its hyperedges |
| `hyperedge(ns, id)` | the record as stored, inside the transaction |

Reads (`ReadTx`), each through the snapshot's view when it has one:

| Method | Returns |
| --- | --- |
| `hyperedge(ns, id)` | the hyperedge, or `None` |
| `node_hyperedges(ns, node, &filter)` | a node's hyperedges, in label then id order |
| `hyperedges_with_label(ns, label, &filter, after, limit)` | a page of a label's, in id order; pass the last id for the next page |
| `hyperedges_by_participants(ns, nodes, ParticipantMatch, &filter)` | those with all of `nodes` (`Contains`) or exactly them (`Exact`), in id order |
| `hyperedges_page(ns, after, limit)` | a page of all of them, in id order |

`hyperedges_by_participants` reads the incidence list of each node asked for
and intersects them, so it reads only the records that match. It compares
nodes, not roles: the roles are in what it returns.

Errors are the engine's: `Error::Invalid` for a malformed hyperedge,
`Error::NodeNotFound` for a participant that does not exist (or that the
actor may not see), `Error::HyperEdgeNotFound`, `Error::Forbidden` and
`Error::Conflict`.

### Example

```rust
use grafeno::{Grafeno, HyperEdgeFilter, NewHyperEdge, NewNode, ParticipantMatch};

let db = Grafeno::open("memory.redb")?;
let decision = db.write(|tx| {
    let planner = tx.create_node("team", NewNode::new("Agent").prop("name", "planner"))?;
    let builder = tx.create_node("team", NewNode::new("Agent").prop("name", "builder"))?;
    let checker = tx.create_node("team", NewNode::new("Agent").prop("name", "checker"))?;
    let task = tx.create_node("team", NewNode::new("Task").prop("name", "Task-001"))?;
    let evidence = tx.create_node("team", NewNode::new("Evidence").prop("name", "Evidence-001"))?;
    tx.create_hyperedge(
        "team",
        NewHyperEdge::new("technical_decision")
            .member(planner, "proposer")
            .member(builder, "implementer")
            .member(checker, "reviewer")
            .member(task, "subject")
            .member(evidence, "supporting_evidence")
            .prop("summary", "keep a single writer"),
    )
})?;

let read = db.begin_read()?;
let whole = read.hyperedge("team", decision)?.unwrap();
for participant in &whole.participants {
    println!("{} as {:?}", participant.node, participant.role);
}
Ok::<(), grafeno::Error>(())
```

## Deleting a node

`WriteTx::delete_node` deletes a node's edges with it, as always, but refuses
(`Error::Conflict`) a node that takes part in a hyperedge: a decision with
five participants should not disappear because one piece of evidence was
deleted, nor keep naming a node that no longer exists. `delete_node_with`
says what to do:

- `OnHyperEdges::Restrict`: refuse, as `delete_node`.
- `OnHyperEdges::Cascade`: delete the node's hyperedges with it.
- `OnHyperEdges::Detach`: take the node out of each hyperedge (every role
  it plays there); a hyperedge left with fewer than two distinct nodes is
  deleted.

Everything happens in the caller's transaction: if any step fails, nothing
was deleted. Whatever deletes nodes goes through this rule: GQ `DELETE`
(`DETACH DELETE` detaches), `DELETE /v1/{ns}/nodes/{id}` (`?hyperedges=cascade`
or `detach` says otherwise), the MCP tools and the memory features that
delete nodes all fail with `Error::Conflict` for a node in a hyperedge, so
no hyperedge is ever left naming a missing node.

Merging two nodes is not deleting one: the merged node's hyperedges name
the kept node instead (a (node, role) pair it already had counts once),
and one that only the two of them were in is deleted, as an edge between
them is. The merge record keeps them as they were, so `undo_merge` puts
them back: a moved one still there, a deleted one still gone, when all
their nodes exist. `MergeReport` counts `hyperedges_moved` and
`hyperedges_dropped`.

## Namespaces and authorization

- Every key starts with the namespace, so a read in one namespace cannot
  reach another's hyperedges, by id, node, label or participants, and nodes
  of another namespace cannot take part.
- Through a view (an account's rights, phase 14.4) a hyperedge is visible
  when its label may be read as an edge label and every node in it is
  visible. A hidden one is absent, as hidden nodes and edges are; reading a
  hidden node's hyperedges returns nothing. Properties denied on its label
  are left out.
- Writes check the same as for edges: the label must be writable for the
  operation, write policies must hold before and after, and every node in
  it must be visible to the actor. A hidden participant is reported as not
  found, a hidden hyperedge as not found, without naming what hides them.
- An `own_only` actor changes only the hyperedges it recorded.

## Compatibility

- **Layout 19.** Opening a file of layout 18 or before creates the three
  tables, empty, and records layout 19; nothing else is rewritten. As with
  every layout change, the file as it was is copied first to
  `<name>.layout-<old>.redb` beside it (unless `copy_before_upgrade` is off).
  The upgrade test opens files written by every layout from 1 to 18.
- **Older versions.** Grafeno 0.4 and before refuse a file of layout 19
  ("newer than this Grafeno") instead of opening it: they would delete a
  node without its hyperedges and leave them naming it. To go back, use the
  copy kept before the upgrade (it lacks what was written after).
- **Nodes and edges** keep their meaning, tables and counters. A hyperedge
  is not counted as an edge, adds no adjacency entries and is not walked by
  traversals or algorithms.
- **API.** The additions are new types, methods and an `Error` variant
  (`HyperEdgeNotFound`): code that matches every variant of `Error` must
  add it. `delete_node` now refuses a node in a hyperedge, which no
  existing database has. `CheckReport` and `RebuildReport` have new fields.

## Limits

These are not done, and the engine refuses rather than dropping data
silently where it can:

- **Fuzzy, full-text and vector indexes** do not apply to hyperedges;
  property indexes, unique and key constraints, not null, type and
  functional rules and property history do (see below).
- **GQ** reads and writes them with clauses of their own (`MATCH`,
  `CREATE` and `MERGE HYPEREDGE`); ordinary patterns, paths and graph
  algorithms do not see them. Counting or testing them per row is written
  with a subquery (see below).
- **Recall and context** do not return hyperedges, by measurement: event
  nodes answer as well (see "Next stages"). `find_group` and GQ read them.
- **The memory palace** does not see them: they are not anchored, and
  entering, walking and reflecting on a room leave them out.

## Next stages

1. **GQ, further.** Done: `MATCH HYPEREDGE`, the writes, `MERGE
   HYPEREDGE`, property indexes and rules. Next, if needed: hyperedges
   inside ordinary patterns and `EXISTS { … }`.
2. **The client SDK**: done in `@grafenodb/client` 0.3.0, published with
   the engine release that has hyperedges.
3. **Memory.** Measured, and not done: recall and context do not return
   hyperedges. On 92 questions about decisions, runs, sessions,
   contradictions and dates, expanding the recalled nodes' hyperedges
   answered every collective question, but the same expansion over event
   nodes (already supported) did as well, ranked the event first and named
   fewer participants the answer did not need
   ([benchmarks](benchmarks.md#hyperedges-in-recall-an-experiment-cratesgrafeno-memoryexampleshyperedge_recall_evalrs)).
   What helped was packing an event with its participants and roles,
   which does not need hyperedges; that is the thing to measure next, on
   real memories.
4. **More kinds.** Weights per participant, directed hyperedges (roles that
   order) and hyperedges among hyperedges.

## HTTP and MCP

| Method | Route | Body or query | Returns |
| --- | --- | --- | --- |
| `POST` | `/v1/{ns}/hyperedges` | `{label, participants: [{node, role}], props, sources, valid_from, valid_to}` | `201 {id}` |
| `GET` | `/v1/{ns}/hyperedges` | `?label=&after=&limit=` | `{items, next}`, retracted ones too |
| `POST` | `/v1/{ns}/hyperedges/find` | `{nodes: [id], exact, label, valid_at, known_at}` (256 nodes at most) | those with all of `nodes` (only them with `exact`) |
| `GET` | `/v1/{ns}/hyperedges/{id}` | | the hyperedge |
| `PATCH` | `/v1/{ns}/hyperedges/{id}` | `{props, remove_props, participants}` | the hyperedge |
| `DELETE` | `/v1/{ns}/hyperedges/{id}` | | `204` |
| `POST` | `/v1/{ns}/hyperedges/{id}/invalidate` | `{at}` | the hyperedge |
| `POST` | `/v1/{ns}/hyperedges/{id}/retract` | | the hyperedge |
| `GET` | `/v1/{ns}/nodes/{id}/hyperedges` | `?label=a,b&valid_at=&known_at=` | the node's |
| `DELETE` | `/v1/{ns}/nodes/{id}` | `?hyperedges=restrict\|cascade\|detach` | `204`; `409` for a node in a hyperedge without it |

They need what the same operations on edges need: reading the namespace
to read and find, creating, updating, deleting or retracting to write,
and per-record rights through the caller's view. `/v1/about` lists the
`hyperedges` capability.

MCP tools, for agents (also through `POST /v1/{ns}/tools/{name}`):

- `relate_group`: one fact relating two or more nodes, each in a role.
- `find_group`: by id, by a node taking part, or by nodes that all take
  part (`exact`: only them); each participant comes with its name and
  role.
- `forget_group`: invalidate (default), retract or delete one.

`relate_group` and `forget_group` write, so a read-only server refuses
them.

### Batches

`POST /v1/{ns}/batch` takes `create_hyperedge`, `update_hyperedge`,
`delete_hyperedge`, `invalidate_hyperedge` and `retract_hyperedge` beside
the node and edge operations, all in one transaction: if one fails,
nothing of the batch stays. A hyperedge's participants may name nodes
created earlier in the batch (`"$name"`); a name given later is unknown
at that point. `create_hyperedge` takes a `ref` of its own, and later
operations reach the hyperedge by it; the response lists those in
`hyperedge_refs`, a field left out when empty so that batches without
hyperedges answer as before (`refs` keeps holding node ids only). One name
is one record: a hyperedge's name may not be given again, and a node's
name where a hyperedge goes, or a hyperedge's where a node goes, fails
with `invalid`. `delete_node` takes `hyperedges` (`restrict`, `cascade` or
`detach`), as its route does.

Each operation goes through the same `WriteTx` call as its route, so the
same checks apply: participants, roles and their limit, the caller's view
(a hidden participant fails the batch without saying whether it exists),
`own_only`, and who recorded each write. Retracting or invalidating needs
the `retract` privilege, checked before the batch starts. The batch's size
is bounded by the request body limit (32 MiB); there is no separate count
of operations, so as not to refuse batches that work today.

## Exports

A JSON lines export (`ReadTx::export`, `GET /v1/export`, `grafeno export`)
writes each namespace's hyperedges after its nodes and edges, one line
each, as they are (id, participants, times), as the reader sees them:

```text
{"type":"hyperedge","ns":"team","hyperedge":{"id":"…","label":"decision","participants":[{"node":"…","role":"proposer"},…],…}}
```

An export with a hyperedge has version 3; one without keeps version 2, so
Grafeno 0.4 still reads it (and refuses version 3 rather than lose the
hyperedges). An import writes them as they were, checking their
participants exist (a missing one fails the import with the line), and
replaces a hyperedge with the same id.

CSV and GraphML hold current knowledge, as they do for edges: hyperedges
that are not retracted, with their valid dates.

- **CSV**: a third file (`ReadTx::export_hyperedges_csv`,
  `GET /v1/{ns}/export/hyperedges.csv`, `hyperedges.csv` beside
  `nodes.csv` and `edges.csv` in `grafeno export --format csv`), one row
  per hyperedge: `id:ID`, `:TYPE`, `:PARTICIPANTS` (a JSON list of
  `{node, role}`, or of plain node ids), `valid_from`, `valid_to` and its
  properties with typed headers. `Grafeno::import_hyperedges_csv`,
  `POST /v1/{ns}/import/hyperedges.csv` and `grafeno import --hyperedges`
  read it after the nodes: a participant is a Grafeno id or an id an
  earlier import mapped, and a row whose node is missing or that names
  fewer than two distinct nodes is skipped and reported.
- **GraphML**: GraphML's own `<hyperedge>`, an `<endpoint node="…"/>` per
  participant, its label in the `label` data, the roles in `roles` (a JSON
  list in endpoint order, `null` for none), `valid_from`, `valid_to` and
  its properties as keys `for="hyperedge"`. An import reads the
  `<hyperedge>` elements of any tool (roles absent, label `RELATED_TO`
  unless given). `ReadTx::export_graphml_all` returns how many it wrote.

Import statistics have `hyperedges`, left out when none were written.

## The change log and events

Every commit that creates, changes or deletes a hyperedge adds an entry
to the change log, folded per hyperedge as for nodes and edges: `op`,
`hyperedge` (its id), `labels` (its label) and `nodes` (its nodes after
the change, or before it for a delete). A reader with a view sees the
entry when it sees the hyperedge, or, once deleted, its label and every
node it had. An event subscription `ON HYPEREDGES` (in Rust,
`EventTarget::HyperEdges`; over HTTP, `"hyperedges"`) gets those changes
with the properties before and after; `ON PROPERTIES` covers their
properties too.

## Hyperedges in other GQ patterns

Hyperedges stay out of ordinary patterns (`(a)-[…]-(b)`), `EXISTS { … }`,
`COUNT { … }`, paths and graph algorithms. What those would be asked for
GQ already says:

| Question | Today |
| --- | --- |
| How many hyperedges per row (`COUNT { … }`) | `CALL { WITH a MATCH HYPEREDGE h:decision [(a) AS proposer] RETURN count(h) AS n }` |
| Whether a row has one (`EXISTS { … }`), or none | the same, with `WHERE n > 0`; or `OPTIONAL MATCH HYPEREDGE h [(a)] WITH a, count(h) AS n WHERE n = 0` |
| Nodes and hyperedges together | `MATCH (a:Agent)-[:member_of]->(t:Team) MATCH HYPEREDGE h:decision [(a) AS proposer]`: the bound node anchors the hyperedge through its incidence list |
| Who shared an event with whom | `MATCH HYPEREDGE h:session [(a), (b)] WHERE a <> b RETURN a, b`, one row per pair |

These run on the incidence and label indexes, without cartesian products
beyond the pairs asked for (`grafeno-query/tests/hyperedges.rs`,
`counting_and_testing_hyperedges_per_row`). What would need new semantics
is postponed until a use case shows it is needed:

- **Traversing a hyperedge inside a path** (`(a)-[h:decision]-(b)` or
  variable-length steps through hyperedges): which participant pairs a
  step joins, whether roles order it, and how a path holds a hyperedge are
  open choices, and each would change what existing patterns mean if made
  implicit. A hyperedge is not a set of binary edges here.
- **`EXISTS { MATCH HYPEREDGE … }` and `COUNT { MATCH HYPEREDGE … }`**: a
  shorter way to write the subqueries above, with no new meaning.
- **Graph algorithms** (paths, centrality, communities) over hyperedges:
  they need a projection (clique, star through an event node, or a
  bipartite graph), each with different results; a caller can build any
  of them with GQ today.

Where a match starts is decided without statistics: a bound participant's
incidence list, intersected for several; but when a property index on
the label also fits and finds fewer (a key among a busy node's
hyperedges), the index is read instead, asked for no more than the
incidence lists hold, so the choice never costs more than either. Against
a label scan an index never reads more.

## Property indexes and constraints

An index may be on hyperedges: `CREATE INDEX FOR HYPEREDGE h:execution ON
(h.execution_id)` in GQ, `IndexDef::on_hyperedges` in Rust
(`hyperedges: true`). It uses the property indexes nodes and edges have:
the same table, the same encoding (booleans, numbers and strings; one
property or several, read by a prefix), filled when declared and kept by
every write of a hyperedge in its transaction (create, properties,
participants, invalidate, retract, delete, a node deleted with
`cascade`), by merges, imports, `rebuild_indexes` and replicated entries.
`check` compares it with the records. `ReadTx::index_seek_hyperedges`
reads it through the caller's view. Nothing is indexed unless declared:
pick the properties queries look up.

`MATCH HYPEREDGE` starts from such an index when the pattern or its
`WHERE` fix its first properties and no participant is bound; see
docs/query-language.md.

Unique and key constraints work on hyperedges too (`REQUIRE h.key IS
UNIQUE`, `REQUIRE (h.a, h.b) IS KEY`). Uniqueness counts every hyperedge
that is not retracted, invalidated ones included: an invalidated
hyperedge was true, so its key stays taken, while a retracted one never
was. A key also needs every current hyperedge of the label to have the
properties.

Not null, type and functional rules hold too (`REQUIRE h.status IS NOT
NULL`, `IS :: INTEGER`, `IS FUNCTIONAL`; in Rust, `hyperedges: true` on
`Constraint::Exists`, `Typed` and `Functional`), checked at each write or
at the end of a GQ statement, and against the data already there when
declared. A functional property's earlier values are kept with when they
held: `ReadTx::hyperedge_property_history`, GQ `history(h, key)`, `GET
/v1/{ns}/hyperedges/{id}/history/{key}`. Fuzzy, full-text and vector
indexes are not offered on hyperedges.

The definition is stored with the others, so the layout stays 19; a
replication entry declaring one (or a rule on hyperedges) has format 8
and an export holding one has version 3, so that 0.4 refuses them instead of taking it for an index on
nodes.

## Replication and clusters

Every write of a hyperedge is logged as an `Op::HyperEdge`: the record
after the transaction (`None` when it was deleted). A member applying it
writes the record and derives the incidence and label entries, as it does
with an edge's adjacency, and runs none of the checks the leader made.
Dropping a namespace and rebuilding its indexes are logged as that work,
and members redo it with the hyperedges in it. A namespace moving to
another cluster group takes its hyperedges with it, after its nodes.

A read query that starts with `MATCH HYPEREDGE` is split across cluster
members as one that starts with `MATCH` is: each part takes its share of
the candidate hyperedges, in id order, and the coordinator merges the
parts into one server's result.

Entries that carry a hyperedge have format 8; the others keep the oldest
format that holds them. A member of an earlier version refuses an entry
of format 8 instead of losing what it says, and while one such member
votes, the leader refuses writes of hyperedges ("upgrade every member
first"), as it does for other work that needs a newer format. Upgrade
every member before writing hyperedges in a cluster.

## Measurements

See [benchmarks.md](benchmarks.md#hyperedges-cratesgrafenoexampleshyperedge_benchrs).
