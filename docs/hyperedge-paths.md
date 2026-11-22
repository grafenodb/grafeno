# Paths through hyperedges: a design (proposed)

Status: **proposed, not implemented.** GQ matches hyperedges with
`MATCH HYPEREDGE` and keeps them out of `(a)-[…]-(b)` patterns, paths and
graph algorithms ([hyperedges.md](hyperedges.md#hyperedges-in-other-gq-patterns)).
This page says what paths through hyperedges would mean, how they would
run, and what must be shown before building them.

## What already works

A single step through a hyperedge needs nothing new:

```
MATCH HYPEREDGE h:decision [(a) AS proposer, (b) AS reviewer] RETURN a, b
```

and neither does a fixed number of steps: chain `MATCH HYPEREDGE` clauses
on shared variables. What GQ cannot say today is a path of *any* length
through hyperedges ("how is Ana connected to Hugo through sessions"), the
shortest one, or one that mixes edges and hyperedges, and graph
algorithms cannot run over hyperedges without a projection the caller
builds first.

## Rules the design keeps

1. **Never implicit.** `(a)-[…]-(b)`, `-[*]-` and `shortestPath` keep
   their meaning: they walk edges only. A hyperedge step has its own
   syntax, so no existing query changes its results.
2. **A hyperedge is not a set of binary edges.** A step goes from one
   participant into the hyperedge and out to another; the hyperedge is
   one element of the path, not one edge per pair.
3. **Roles order a step only when written.** Without roles a step joins
   any two different participants, in either order.
4. **Time, views and retraction as everywhere else.** `VALID AT` and
   `KNOWN AT` filter hyperedges as they filter edges; a hyperedge with a
   participant the caller may not see is not walked; retracted ones are
   not walked unless `KNOWN AT` reaches back to them.

## Syntax

A hyperedge step is written between tildes:

| Pattern | Means |
| --- | --- |
| `(a)~[h:session]~(b)` | `a` and `b` are different participants of one `session` hyperedge `h` |
| `(a)~[h:decision AS proposer -> AS reviewer]~(b)` | `a` is its proposer and `b` its reviewer |
| `(a)~[:session*1..3]~(b)` | 1 to 3 hyperedge steps (`*` alone: 1 to 32, as for edges) |
| `p = (a)~[:session|execution*]~(b)` | `p` holds the path |
| `p = shortestPath((a)~[:session*]~(b))` | the fewest hyperedge steps |
| `(a)-[:works_at]->(c)~[:decision]~(b)` | an edge step, then a hyperedge step |

`~[` cannot start anything in GQ today, so the grammar stays unambiguous.
A variable-length step's variable holds the list of hyperedges in order,
as a variable-length edge step holds its edges. Labels may alternate with
`|`; `WHERE` on `h` filters each hyperedge as it is walked.

A path holds nodes and the edges or hyperedges between them, alternating.
`nodes(p)` and `length(p)` work as they do (a hyperedge step counts one);
`relationships(p)` keeps returning edges only, so existing code reading
it is not surprised; a new `hyperedges(p)` returns the hyperedges, and
`steps(p)` both, in order.

## How it runs

- **One step**: from a bound participant, its incidence list
  (`HYPEREDGE_INCIDENCE`) gives its hyperedges with the label; each
  hyperedge's participants, filtered by role, give the next nodes. From
  an unbound start with a role, the label index or a property index
  anchors it, as `MATCH HYPEREDGE` does now.
- **Variable length and shortest paths**: breadth first, as edge paths
  are, keeping a set of hyperedges already expanded besides the nodes
  already reached. Each hyperedge is opened once per search, so a search
  costs the sum of the participants of the hyperedges it opens, not the
  pairs inside them: a hyperedge of 150 participants is 150 reads, not
  11 175 pairs. Each end node is reached once, at its shortest distance,
  as for variable-length edges.
- **Crowds**: one big hyperedge reaches many nodes in one step. A
  `max_participants` option on the step (a hyperedge with more is not
  walked) bounds that; `PROFILE` reports participants read per step.
- **Planning**: the cost model already estimates hyperedges per label and
  participants per hyperedge from planner statistics; a step costs
  incidence reads plus participants read.
- **Cluster**: read-only, so it runs on any member that holds the data;
  replication and entry formats do not change. Splitting a first step
  across members works as for `MATCH HYPEREDGE`.

## Graph algorithms

`algo.*` procedures would take a `hyperedges` option naming a projection,
since each gives different results and none is the obvious one:

| Projection | A hyperedge becomes | Fits |
| --- | --- | --- |
| `'star'` | a node joined to each participant (two hops between participants) | paths and centrality where the event counts as a step |
| `'clique'` | an edge between every pair of participants | communities; cost grows with the square of the participants |
| `'bipartite'` | nodes and hyperedges as two kinds of vertex | ranking events and participants together |

Without the option nothing changes. A caller can build any of these with
GQ today (an event node per hyperedge, or edges per pair) and run the
algorithms on it.

## What would change in the code

- `grafeno-query`: the parser (the `~[ … ]~` step, `hyperedges(p)`,
  `steps(p)`); a hyperedge expand operator next to the edge expand, with
  role filters; the breadth-first search with the expanded-hyperedge set;
  the path value holding hyperedges; cost and `PROFILE` counters; splits.
- `grafeno`: nothing in storage; `node_hyperedges` with a label and the
  hyperedge read through the view already exist.
- Graph algorithms: the projection option and three projections.
- Tests: rules 1 to 4 above each with a test; paths equal to the same
  question asked over event nodes (`(a)<-[:proposer]-(:Event)-[:reviewer]->(b)`,
  variable length over two hops); crowds bounded; views and time.

## Before building it

Event nodes already answer these questions: a hyperedge step is two edge
hops through an `Event` node, and variable-length paths and every graph
algorithm already walk those. The [recall experiment](benchmarks.md#hyperedges-in-recall-an-experiment-cratesgrafeno-memoryexampleshyperedge_recall_evalrs)
found no gain from hyperedges over event nodes. Paths through hyperedges
are worth building when a real use case shows one of:

- a query that is markedly clearer than its event-node form, asked
  often enough to matter;
- a measured cost of the event-node form (an extra node and one edge per
  role for every event) that a memory cannot afford;
- an algorithm result that needs the hyperedge itself (bipartite
  ranking), not one of the projections a caller can build.

Until then the design stays here, so that if it is built it keeps the
rules above.
