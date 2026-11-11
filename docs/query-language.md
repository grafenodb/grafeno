# GQ, the Grafeno query language

GQ is Grafeno's declarative graph query language: patterns of nodes and
edges, filters, projections, aggregates, subqueries and writes, plus
features of its own: facts with valid time and record time, retraction, and
pseudo-properties for what is not a property (ids, indexed text,
embeddings). It runs in `grafeno shell`, over HTTP (`POST /v1/{ns}/query`),
over MCP (the read-only `query` tool), and from Rust
(`grafeno_query::execute`).

```text
MATCH (a:Person {name: 'Ana'})-[w:works_at]->(c:Company)
VALID AT '2022-06-01'
WHERE c.name STARTS WITH 'Ac'
RETURN a.name, c.name, date(w.@valid_from) AS since
ORDER BY since DESC
LIMIT 10;
```

Every statement runs on one namespace. Keywords are case-insensitive;
labels, property names and variables are not. `//` starts a comment.

## Reading

### Patterns

| Pattern                          | Matches                                             |
| -------------------------------- | --------------------------------------------------- |
| `(n)`                            | any node                                            |
| `(n:Person:Admin)`               | nodes with all of these labels                      |
| `(n {name: 'Ana', age: $age})`   | nodes whose properties equal these values           |
| `(n {@id: '0190…'})`             | the node with this id (fast lookup)                 |
| `(a)-[r:knows]->(b)`             | an outgoing edge labeled `knows`                    |
| `(a)<-[:knows]-(b)`, `(a)-[]-(b)`| an incoming edge; an edge in either direction       |
| `(a)-->(b)`, `(a)<--(b)`, `(a)--(b)` | the same, without brackets                      |
| `(a)-[:knows\|likes]->(b)`        | an edge with any of these labels                    |
| `(a)-[:knows*1..3]->(b)`         | 1 to 3 hops (`*` alone: 1 to 32; `*2`: exactly 2)   |
| `(a)-[r {role: 'lead'}]->(b)`    | edges whose properties equal these values           |
| `p = (a)-[:knows*]->(b)`         | `p` holds the path: `nodes(p)`, `relationships(p)`, `length(p)` |
| `p = shortestPath((a)-[:knows*]-(b))` | the shortest path (`allShortestPaths` gives one per end too) |

- Patterns chain: `(a)-[:knows]->(b)-[:works_at]->(c)`. A `MATCH` can list
  several patterns separated by commas; they share variables, and
  unrelated ones combine every match of each.
- One edge is used at most once per pattern.
- A variable-length edge's variable holds the list of edges of the path.
  Variable-length matches are shortest paths: each end node is reached once,
  at its shortest distance (not once per path).
- Several `MATCH` clauses can follow each other; each extends the rows of
  the previous one.
- `OPTIONAL MATCH` keeps each row even when its patterns do not match: the
  new variables are then `null`. Its `WHERE` filters the optional part
  only.

```text
MATCH (p:Person)
OPTIONAL MATCH (p)-[:works_at]->(c)
RETURN p.name, c.name;     // people without a company get null
```

### Time: `VALID AT` and `KNOWN AT`

By default a `MATCH` sees current knowledge: edges that were not
retracted, whatever their valid time. After the patterns:

- `VALID AT t` keeps only facts true at `t` (`@valid_from <= t <
  @valid_to`).
- `KNOWN AT t` shows what Grafeno knew at `t`, including facts retracted
  later.

`t` is an ISO 8601 string (`'2024-05-01'`, `'2024-05-01T10:30:00Z'`), a
time value such as `datetime()`, or microseconds since the Unix epoch.

```text
MATCH (:Person {name: 'Ana'})-[:works_at]->(c) VALID AT '2021-01-01' RETURN c.name;
MATCH (p)-[r]->(q) KNOWN AT '2026-11-01T12:00:00Z' RETURN p, r, q;
```

### `WHERE`

Comparisons `= <> != < <= > >=`, `AND`, `OR`, `XOR`, `NOT`,
`IS [NOT] NULL`, `IN [list]`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, and
label tests (`n:Admin`). Logic is three-valued, as in SQL: a
comparison with `null` is unknown, and `WHERE` keeps only rows where the
condition is true.

**Pattern predicates** test whether a pattern matches from the row, with
the `MATCH`'s `VALID AT` and `KNOWN AT`:

```text
MATCH (p:Person) WHERE (p)-[:knows]->()            RETURN p;  // knows someone
MATCH (p:Person) WHERE NOT (p)<-[:knows]-()        RETURN p;  // nobody knows p
MATCH (p:Person) WHERE exists((p)-[:works_at]->()) RETURN p;
MATCH (p:Person)
WHERE EXISTS { MATCH (p)-[:knows]->(f) WHERE f.age > 40 }
RETURN p;
```

`exists(n.key)` is true when the property is set.

### `RETURN`

```text
RETURN [DISTINCT] expression [AS name], …  |  RETURN *
       [ORDER BY expression [ASC|DESC], …] [SKIP n] [LIMIT n]
```

- Column names are the `AS` names, or the expressions as written.
- Aggregates: `count(*)`, `count(x)`, `sum`, `avg`, `min`, `max`,
  `collect`, each also with `DISTINCT` (`count(DISTINCT x)`). Items that
  are not aggregates are the grouping keys. Aggregates over no rows give
  one row (`count(*)` = 0) unless there are grouping keys.
- `ORDER BY` can use return names, variables, or expressions; an aggregate
  in `ORDER BY` must also be returned.
- `null` sorts last; values of different types sort by type.

### `WITH`: steps between clauses

`WITH` is a `RETURN` in the middle of a query: it projects, groups and
sorts the rows, and only its names stay in scope. A `WHERE` after it
filters the projected rows, so it can filter aggregates (SQL's `HAVING`).
Expressions other than variables need a name (`AS`). `WITH *` keeps every
variable.

```text
// Companies with more than one person
MATCH (p:Person)-[:works_at]->(c)
WITH c, count(p) AS people
WHERE people > 1
RETURN c.name, people ORDER BY people DESC;

// The two oldest people, then their companies
MATCH (p:Person) WITH p ORDER BY p.age DESC LIMIT 2
MATCH (p)-[:works_at]->(c) RETURN p.name, c.name;

// An aggregate of aggregates
MATCH (p:Person) OPTIONAL MATCH (p)-[:knows]->(f)
WITH p, count(f) AS friends
RETURN avg(friends), max(friends);
```

### `UNWIND`

One row per item of a list (`null` gives no rows):

```text
UNWIND [1, 2, 3] AS x RETURN x * 10;
MATCH (p:Person) UNWIND p.langs AS lang RETURN lang, count(*) AS people;
```

### `UNION`

Joins the rows of several queries with the same column names. `UNION`
drops duplicate rows; `UNION ALL` keeps them (one statement uses one or
the other).

```text
MATCH (p:Person) RETURN p.name AS name
UNION
MATCH (c:Company) RETURN c.name AS name;
```

### `CALL { … }`: subqueries

The clauses inside run once per row and see its variables; the rows their
`RETURN` gives are added to it, and a row with none is dropped. Without
`RETURN` (writes only) rows stay as they were.

```text
MATCH (p:Person)
CALL { WITH p MATCH (p)-[:knows]->(f) RETURN count(f) AS friends }
RETURN p.name, friends;
```

A name returned may not already exist outside. `UNION` inside and `IN
TRANSACTIONS` are not supported: a query is one transaction.

### `CALL`: procedures

```text
CALL db.labels();                                     // label, count
CALL db.edgeLabels() YIELD label, count AS edges WHERE edges > 10 RETURN label;
CALL db.search('borrow checker', 5) YIELD node, score RETURN node.title, score;
```

| Procedure | Yields | |
| --- | --- | --- |
| `db.labels()` | `label`, `count` | node labels and their counts |
| `db.edgeLabels()` (`db.relationshipTypes()`) | `label`, `count` | edge labels and their counts |
| `db.propertyKeys()` | `key` | property names of nodes and edges (reads every record) |
| `db.stats()` | `nodes`, `edges`, `texts`, `vectors`, `dimension` | counts of the namespace |
| `db.indexes()` | `name`, `label`, `property`, `unique`, `entries`, `fuzzy` | property and fuzzy indexes (also `SHOW INDEXES`); several labels are joined by `\|` and several properties by `, `; a fuzzy index counts trigram entries |
| `db.analyze()` | `kind`, `name`, `count`, `distinct`, `out_degree`, `in_degree`, `common` | rebuilds the planner's statistics now and lists them: per index its entries, distinct values and ten most common values (`[value, nodes]`); per edge label its edges and average degree each way |
| `db.searchFuzzy(label, property, text, limit = 10, measure = 'dice')` | `node`, `score` | nodes whose property looks like the text, through its fuzzy index (see *Similar text and entity resolution*) |
| `db.match(label, properties, limit = 10)` | `node`, `score`, `decision`, `fields` | nodes that may be the record described, by the label's match rule |
| `db.search(text, limit = 10)` | `node`, `score` | the node text (BM25), best first |
| `db.search(text, {limit, mode, embedding, index, fusion, k, text_weight, candidates})` | `node`, `score` | text, vector or hybrid search (see *Full-text search*) |
| `db.searchIndex(index, query, limit = 10 \| {limit, fragments})` | `node`, `score`, `fragments` | a full-text index, with its query language and the fragments that matched |
| `db.searchVector(embedding, limit = 10 \| {limit, index, ef, labels, where})` | `node`, `score` | nearest nodes by embedding, or in a vector index (see *Vector indexes*) |
| `db.searchVectors(embeddings, limit \| {…})` | `query`, `node`, `score` | the same for several embeddings at once (`query` is the position in the list) |
| `db.similar(node, limit \| {…})` | `node`, `score` | the nodes nearest to a node's own vector, itself left out |
| `temporal.series(start, end, step)` | `at` | times from start to end every step (see *Time and durations*) |
| `dbms.procedures()` | `name`, `arguments`, `columns`, `description` | this list |

`YIELD` picks and renames columns (`YIELD label AS l`); without it, every
column. Arguments can use the row's variables, so a `CALL` in the middle of
a query runs once per row. A query may end with `CALL`: it returns what the
procedure yields.

### Graph algorithms

`CALL algo.<name>(…, {settings})` runs a graph algorithm on the namespace
and streams one row per result. `CALL algo.<name>` with no settings uses
the whole namespace.

```text
// The five most central people, and the communities they form.
CALL algo.pageRank({labels: 'Person', edgeLabels: 'knows'}) YIELD node, score
RETURN node.name, score LIMIT 5;
CALL algo.louvain() YIELD node, community
RETURN community, count(*) AS size, collect(node.name)[..5] ORDER BY size DESC;

// The cheapest route, weighted by the km property.
MATCH (a:City {name: 'Lima'}), (b:City {name: 'Cusco'})
CALL algo.shortestPath(a, b, {weight: 'km'}) YIELD nodes, cost
RETURN [c IN nodes | c.name], cost;

// Write mode: store what the procedure yields.
CALL algo.pageRank() YIELD node, score SET node.rank = score;
CALL algo.fastRP({dimension: 64}) YIELD node, embedding SET node.@embedding = embedding;
```

**The projection.** Algorithms run on an in-memory copy of the graph's
structure (the nodes and, for each, its edges as an array), built from the
adjacency index without reading node records. Read queries share it
through a cache until the next commit changes the data, so running
several algorithms on the same part of the graph projects it once. These
settings choose what it holds, for every procedure:

| Setting | |
| --- | --- |
| `labels` | nodes with these labels (a string or a list); all nodes by default |
| `edgeLabels` | edges with these labels; all edges by default |
| `weight` | the edge property with each edge's weight; edges without it weigh 1 |
| `direction` | `'out'`, `'in'` or `'both'` (default per procedure, below) |
| `validAt`, `knownAt` | facts true at a time, or what was known then (as in `MATCH`) |

A setting the procedure does not know is an error, so typos do not pass
unnoticed. Node arguments are nodes or their ids as strings.

| Procedure | Yields | Settings (defaults) |
| --- | --- | --- |
| `algo.shortestPath(source, target)` | `nodes`, `edges`, `cost` | Dijkstra; without `weight` it is the fewest hops. `direction: 'out'` |
| `algo.astar(source, target)` | `nodes`, `edges`, `cost` | A* guided by coordinates: `latitude` and `longitude` (straight-line km), or `x` and `y`. The straight line must never be longer than the weights say |
| `algo.kShortestPaths(source, target, k)` | `rank`, `nodes`, `edges`, `cost` | the k cheapest loopless paths (Yen), cheapest first |
| `algo.allShortestPaths(source, target)` | `nodes`, `edges`, `cost` | every cheapest path, `limit: 100` |
| `algo.distances(source)` | `node`, `cost` | the cost to every node reached, nearest first |
| `algo.degree()` | `node`, `score` | edges per node, or the sum of their weights. `direction: 'both'` |
| `algo.pageRank()` | `node`, `score` | `damping: 0.85`, `iterations: 20`, `tolerance: 1e-7`, `sources` (personalized PageRank: jumps land only on these nodes). Scores add up to 1 |
| `algo.betweenness()` | `node`, `score` | shortest paths through each node (Brandes, on every core); `sample: n` estimates it from n sources, `seed` |
| `algo.closeness()` | `node`, `score` | Wasserman-Faust closeness, or `harmonic: true` |

The four scoring procedures above also take `top: n`, which keeps only the
n best rows. Use it on large graphs: without it, every node's record is read
and held (`CALL algo.pageRank({top: 10}) YIELD node, score` on a million
nodes reads ten).
| `algo.wcc()`, `algo.scc()` | `node`, `component` | weakly and strongly connected components |
| `algo.labelPropagation()` | `node`, `community` | `iterations: 10`, `seed` |
| `algo.louvain()` | `node`, `community`, `modularity` | `resolution: 1` (higher, smaller communities), `levels: 10`, `iterations: 10` |
| `algo.triangles()` | `node`, `triangles`, `coefficient` | triangles per node and the local clustering coefficient |
| `algo.nodeSimilarity()` | `node`, `other`, `score` | by shared neighbors: `metric: 'jaccard'` (or `'overlap'`, `'cosine'`), `topK: 10` per node, `cutoff: 0`, `node` (only that one). `direction: 'out'`: people who like the same things |
| `algo.knn()` | `node`, `other`, `score` | nearest nodes by embedding (cosine, from the vector index): `topK: 10`, `cutoff`, `node` |
| `algo.linkPrediction(node)` | `other`, `score` | likely new edges: nodes two hops away, by `metric: 'adamicAdar'` (or `'commonNeighbors'`, `'resourceAllocation'`, `'preferentialAttachment'`, `'totalNeighbors'`, `'jaccard'`), `limit: 10` |
| `algo.fastRP()` | `node`, `embedding` | structural embeddings: `dimension: 128`, `iterationWeights: [0, 1, 1]`, `selfInfluence: 0`, `normalizationStrength: 0`, `seed: 42` |

More analysis (16.8):

| Procedure | Yields | Settings (defaults) |
| --- | --- | --- |
| `algo.kCore()` | `node`, `core` | each node's core number: the largest k for which it is in a subgraph where every node has at least k neighbors |
| `algo.leiden()` | `node`, `community`, `modularity` | Louvain's communities, each split into its connected parts, so none is disconnected; Louvain's settings |
| `algo.hits()` | `node`, `hub`, `authority` | hubs point to good authorities, authorities are pointed to by good hubs: `iterations: 20`, `tolerance: 1e-8`, `top`; ordered by authority |
| `algo.eigenvector()` | `node`, `score` | scored by the scores of those whose edges reach the node: `direction: 'in'` (or `'both'`), `iterations: 100`, `tolerance: 1e-6`, `top` |
| `algo.harmonic()` | `node`, `score` | the sum of 1 / distance to every other node (closeness with `harmonic: true`), `top` |
| `algo.articulationPoints()` | `node` | nodes whose removal splits their component |
| `algo.bridges()` | `edge` | edges whose removal splits their component |
| `algo.spanningTree()` | `edge`, `weight` | a minimum spanning forest by `weight` (one tree per component), or `maximum: true` |
| `algo.maxFlow(source, sink)` | `edge`, `flow`, `value`, `cut` | the most that can go from source to sink along the edges' directions, each edge carrying at most its `weight`: a row per edge that carries flow, `cut` true on the edges of a minimum cut |
| `algo.bipartiteMatching()` | `left`, `right`, `edge` | the most pairs between the nodes with the label `left` and the others, over edges with one end on each side |
| `algo.topologicalOrder()` | `node`, `position` | an order where every edge goes forward; a query error naming a cycle when there is one |
| `algo.cycle()` | `nodes` | one cycle, in order (no row when there is none) |
| `algo.randomWalks(starts)` | `start`, `walk` | walks from a node, a list of nodes, or every node (`null`), each step chosen by edge weight: `length: 10`, `walks: 1`, `seed: 42`; the same seed, the same walks |
| `algo.clustering()` | `node`, `coefficient` | the share of a node's neighbor pairs that are linked |
| `algo.conductance()` | `community`, `conductance` | for each group of nodes by the integer in `property` (`'community'`): the weight of its edges going out over its volume, 0 for a closed group |

Simulation (16.8): places, agents and resources. Edge weights are
costs for fields and influence, shares for diffusion, capacities and costs
for flows; nodes' amounts and states are properties.

| Procedure | Yields | Settings (defaults) |
| --- | --- | --- |
| `algo.distanceField(sources)` | `node`, `cost`, `nearest` | from every node, the cost to the nearest source and which one, in one search: an agent steps to the neighbor with the lowest cost to reach its nearest goal. `maxCost`, `direction: 'both'` |
| `algo.influence(sources)` | `node`, `influence` | each source's value (its property `value`, or 1) times `decay^cost` (`decay: 0.5`) along the cheapest way, down to `threshold: 0.01` and up to `maxCost`, combined by `combine: 'sum'` (or `'max'`) |
| `algo.floodFill(seeds)` | `node`, `steps` | the region reachable from the seeds through nodes with the property values in `where` (and the labels in `inside`), with how many steps away |
| `algo.diffusion()` | `node`, `amount` | the property `amount` spreading over `steps: 10`: at each step `rate: 0.5` of a node's amount leaves it, split among its neighbors by edge weight; a node holds at most its `capacity` property, and what does not fit stays; the total stays the same |
| `algo.automaton()` | `node`, `state`, `changedAt` | a rule on the boolean property `state`, for every node at once, until nothing changes or `steps: 10`: `rule: 'life'` (turn on with a count in `birth: [3]`, stay on with one in `survive: [2, 3]`), `'threshold'` (turn on when `share: 0.5` of the neighbors are on; with `recover: true`, turn off below it) or `'majority'` |
| `algo.assignment()` | `agent`, `task`, `cost` | agents (label `agents`) to tasks (label `tasks`) at least total cost: an edge between them is an allowed pair, its `weight` the cost |
| `algo.minCostFlow()` | `edge`, `flow`, `value`, `cost`, `unmet` | sends what nodes supply (property `supply` > 0) to those that need it (< 0) at least total cost, each edge carrying at most its `capacity` property at `cost` per unit; `unmet` is the supply no way could carry |

Unless noted, directions default to `'out'`; FastRP defaults to
`'both'`, and weak components, label propagation, Louvain, triangles and
link prediction always follow edges both ways. Centralities come highest first; groups are numbered
from 0 in node order.

Functions score one pair of nodes from their neighbors (both
directions), with no projection: `algo.commonNeighbors(a, b)`,
`algo.adamicAdar(a, b)`, `algo.resourceAllocation(a, b)`,
`algo.preferentialAttachment(a, b)`, `algo.totalNeighbors(a, b)`,
`algo.jaccard(a, b)`, `algo.overlap(a, b)`; and `algo.cosine(list, list)`
compares two vectors (`algo.cosine(a.@embedding, b.@embedding)`).

**Modes.** Results stream as rows: aggregate them (`CALL algo.louvain()
YIELD community RETURN count(DISTINCT community)`), or `SET` them, which
turns the query into one write transaction. Two settings do this for
every algorithm, when the settings are written in the query (a literal
map, so that whether it writes is known before it runs):

- `mode: 'stats'` yields `column`, `count`, `min`, `max`, `mean`, `sum`:
  a row `rows` with how many rows there were, and one per numeric column.
- `write: 'property'` stores the first column after the node in that
  property of each node, and `write: {hub: 'hub', authority: 'auth'}`
  several columns; it yields `property`, `column`, `nodes` (how many were
  written). The query is then a write, with the rights a write needs.

```text
CALL algo.kCore({mode: 'stats'}) YIELD column, max;
CALL algo.kCore({write: 'core'}) YIELD nodes;
CALL algo.pageRank({mode: 'stats', labels: $labels});   // other settings may be parameters
```

`mode` and `write` themselves must be in the map written in the query: in
a parameter (`CALL algo.kCore($settings)` with `$settings = {write:
'core'}`), they are refused with an error rather than ignored. Over HTTP,
`POST /v1/{ns}/algo/{name}` takes them beside the settings: `stats:
true`, or `write` (and `column`).

Algorithms run on what the caller may see (14.4), within the query's
limits, and stop at its time limit or when it is cancelled. `EXPLAIN`
estimates their cost like the other procedures'.

### `EXPLAIN` and `PROFILE`

`EXPLAIN` before a statement returns its plan as a table (step, operation,
details: where each pattern starts and how it expands) without running it.
Its last two columns are estimates from the namespace's counts and the
planner's statistics (see *Indexes and performance*): `rows`, the rows
after the step, and `cost`, the work so far (rows touched, records read,
items computed in loops such as `reduce` or a comprehension). They are
rough, assuming for instance that a condition keeps half the rows, but
they tell a query that touches thousands of rows from one that touches
billions; the cost of the last step is the statement's total, which
`max_cost` compares. Literal values and the statement's parameters are
looked up in the statistics; values from earlier rows are not known yet,
so they count as an average value.
`PROFILE` runs the statement and adds, per step, the rows it produced and
the milliseconds it took (`profile` in the JSON result, a second table in
the shell, the *Profile* view in the Studio).

```text
EXPLAIN MATCH (a:Person {name: 'Ana'})-[:knows]->(f) RETURN f.name;
PROFILE MATCH (p:Person) WHERE p.age > 30 RETURN count(p);
```

### Statistics

Besides `count`, `sum`, `avg`, `min`, `max` and `collect`:

| Aggregate | |
| --- | --- |
| `stdev(x)`, `stdevp(x)` | standard deviation of a sample, of a population |
| `variance(x)`, `variancep(x)` | variance of a sample, of a population |
| `median(x)` | the middle value (the mean of the two middle ones) |
| `percentileCont(x, p)` | the `p` percentile (0 to 1), interpolated between values |
| `percentileDisc(x, p)` | the `p` percentile, one of the values |
| `mode(x)` | the most frequent value (ties: the one that sorts first) |

They skip `null`s, take `DISTINCT` (`stdev(DISTINCT x)`), and give `null`
over no values (the spread of one value is 0). `sum` and `avg` also add up
durations. Their result does not depend on the order of the rows:
integers add in 128 bits (only a total outside 64 bits is an overflow)
and floats exactly, rounded once (`sum` of 0.1, 0.2 and 0.3 is 0.6), so a
query split across cluster members returns what one server does.

```text
MATCH (p:Person)
RETURN count(p), avg(p.age), stdev(p.age), median(p.age),
       percentileCont(p.age, 0.9) AS p90;

// Per group, keeping the groups with a high median
MATCH (s:Sale)
WITH s.region AS region, median(s.amount) AS typical, stdevp(s.amount) AS spread
WHERE typical > 100
RETURN region, typical, round(spread, 2) ORDER BY typical DESC;
```

### Time and durations

Times are UTC instants (`datetime('2024-05-01')`, `datetime()` for now,
pseudo-properties such as `r.@valid_from`). Their parts are properties:
`t.year`, `t.quarter`, `t.month`, `t.week` and `t.weekYear` (ISO weeks),
`t.day`, `t.dayOfWeek` (Monday is 1), `t.dayOfYear`, `t.hour`, `t.minute`,
`t.second`, `t.millisecond`, `t.microsecond`, `t.epochSeconds`,
`t.epochMillis`.

A **duration** has months, days and microseconds, which do not convert into
each other exactly (a month has 28 to 31 days). It is written in ISO 8601
(`duration('P1Y2M10DT2H30M')`, `duration('PT90M')`, `duration('-P1D')`)
or as a map (`duration({days: 3, hours: 12})`), and prints in ISO 8601.

| Expression | Gives |
| --- | --- |
| `t + d`, `t - d` | a time: months added on the calendar (Jan 31 + P1M = Feb 29), then days, then the rest |
| `d + d`, `d - d`, `d * n`, `d / n`, `-d` | a duration |
| `b - a` (two times) | the exact duration between them, in hours, minutes and seconds |
| `duration.between(a, b)` | the calendar duration: months, then days, then the rest (`P1Y2M5DT6H`) |
| `duration.inMonths(a, b)`, `inDays`, `inSeconds` | the same span in one unit |
| `d.years`, `d.months`, `d.days`, `d.hours`, `d.minutes`, `d.seconds` | a component (`d.monthsOfYear`, `d.minutesOfHour`… for the remainder) |
| `date.truncate(unit, t)` | the start of the unit: `year`, `quarter`, `month`, `week` (Monday), `day`, `hour`, `minute`, `second`, `decade`, `century`, `millennium` |

Durations compare by approximate length (a month counts as 30.44 days).

```text
// How long each fact held (still-true facts until now)
MATCH (p)-[r:works_at]->(c)
RETURN p.name, c.name,
       duration.between(r.@valid_from, coalesce(r.@valid_to, datetime())) AS held;

// Notes per month
MATCH (n:Note)
WITH date.truncate('month', n.@created_at) AS month, count(*) AS notes
RETURN date(month) AS month, notes ORDER BY month;
```

**What was true over time.** `VALID AT` and `KNOWN AT` may use a variable,
and `temporal.series(start, end, step)` yields one time per step, so one
query counts what was valid at each point of a period:

```text
CALL temporal.series('2024-01-01', '2024-12-01', 'P1M') YIELD at
OPTIONAL MATCH (p:Person)-[:works_at]->(:Company {name: 'Acme'}) VALID AT at
RETURN date(at) AS month, count(p) AS people ORDER BY month;
```

A series has at most 10,000 points; month steps stay on the calendar
(Jan 31, Feb 29, Mar 31…).

## Writing

A statement with any write runs in one transaction: all of its changes or
none. Clauses after a write see what it wrote (a `MATCH` after a `CREATE`
finds the new nodes). Without `RETURN`, a writing statement reports only
what changed.

| Clause                                   | Effect                                                    |
| ---------------------------------------- | --------------------------------------------------------- |
| `CREATE (n:Note {title: 'x', @text: '…'})` | creates nodes (one label at least) and edges (one label, a direction) |
| `CREATE (a)-[:knows {@valid_from: '2024-01-01'}]->(b)` | creates an edge between bound or new nodes |
| `SET n.key = value`                      | sets a property (`null` is stored: the key stays)         |
| `SET n += {a: 1, b: null}`               | merges properties (`b` becomes `null`)                    |
| `SET n = {a: 1}`                         | replaces all properties (keys the map lacks are removed)  |
| `SET n:Label:Other`                      | adds labels                                               |
| `SET n.@text = '…'`, `SET n.@embedding = […]` | indexes text or a vector (`null` removes it)         |
| `SET n.@importance = 0.9`                | sets the importance, from 0 to 1                         |
| `SET r.@valid_to = '2025-01-01'`         | the fact stopped being true then (history is kept)        |
| `REMOVE n.key, n:Label, n.@text`         | removes properties (the key goes), labels, the text or the embedding |
| `DELETE n` (`DETACH DELETE n`)           | deletes nodes with their edges, or edges                  |
| `RETRACT r`                              | the fact was a mistake: hidden from current knowledge, visible to `KNOWN AT` earlier |
| `MERGE (n:Tag {name: 'rust'})`           | matches the pattern, or creates it when nothing matches  |
| `FOREACH (x IN list \| CREATE …)`         | runs writes once per item                                 |

`CREATE` accepts `@text`, `@embedding` and `@sources` (a list of ids) for
nodes, and `@valid_from`, `@valid_to` and `@sources` for edges. After a
write, `RETURN` sees the written values.

### Null values

A property can hold `null`, and that is not the same as not having the
property: `CREATE (:P {a: null})` and `SET p.a = null` keep the key `a`,
which `keys(p)`, `properties(p)` and exports show. `REMOVE p.a` (and
`remove_props` over HTTP and MCP) deletes the key. Reading `p.a` gives
`null` either way, comparisons with it are unknown, and `p.a IS NULL` is
true for both; `hasKey(p, 'a')` tells them apart. Nulls are kept inside
lists and maps too. Indexes hold no nulls: an index lookup for `= null`
finds nothing (as the comparison does), and several nodes with `null`
under a unique index are no conflict. CSV and GraphML have no way to write
a null that differs from an empty value, so there a null property becomes
an absent one; the JSON Lines export keeps it.

### `MERGE`

`MERGE pattern` matches the whole pattern from the row; when nothing
matches, it creates the parts that are not bound. `ON CREATE SET` and
`ON MATCH SET` run in each case. Rows of the same statement see what
earlier rows merged, so `UNWIND ['a', 'a'] AS x MERGE (:Tag {name: x})`
creates one node. Only facts true now count as existing: if a `knows` edge
was ended, `MERGE (a)-[:knows]->(b)` records the fact again. An undirected
edge in `MERGE` is created from left to right.

```text
MERGE (t:Tag {name: $name})
ON CREATE SET t.created = datetime()
ON MATCH SET t.uses = coalesce(t.uses, 0) + 1
RETURN t;

MATCH (a:Person {name: 'Ana'}), (b:Person {name: 'Bob'})
MERGE (a)-[:knows]->(b);
```

### `FOREACH`

Runs write clauses (`CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`,
`RETRACT`, `FOREACH`) once per item; what it binds stays inside.

```text
MATCH (a:Person {name: 'Ana'})
FOREACH (topic IN ['graphs', 'rust'] |
  MERGE (t:Topic {name: topic})
  CREATE (a)-[:likes]->(t));
```

## Pseudo-properties

Read with `n.@name`. They are what a node or edge has besides properties.

| Nodes          | Edges          |
| -------------- | -------------- |
| `@id`          | `@id`          |
| `@labels`      | `@label`       |
| `@text`        | `@from`, `@to` (node ids) |
| `@embedding`   | `@valid_from`, `@valid_to` |
| `@sources`     | `@recorded_at`, `@expired_at` |
| `@created_at`, `@updated_at` | `@sources` |
| `@recorded_by` (the API key that created it) | `@recorded_by` |
| `@importance` (0 to 1), `@accesses`, `@last_access` | |

Times are time values; they compare with ISO strings and print as ISO 8601.
`SET n.@importance = 0.9` changes a node's importance, which weighs in
recall ranking and decay (see the memory section of
[DESIGN.md](../DESIGN.md)).

## Expressions and functions

Literals: `1`, `2.5`, `1e3`, `'text'` or `"text"` (with `\n`, `\t`,
`\uXXXX`), `true`, `false`, `null`, lists `[1, 2]`, maps `{a: 1}`.
Parameters: `$name`. Operators: `+ - * / %` (integer division for
integers; `+` also joins strings and lists), `^` (power, a float),
`list[i]` (negative from the end), `list[from..to]` (either end may be
left out), `map.key`.

```text
RETURN CASE WHEN p.age < 30 THEN 'young' ELSE 'experienced' END;  // searched
RETURN CASE p.status WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 0 END;  // simple
RETURN [x IN range(1, 10) WHERE x % 2 = 0 | x * x];               // comprehension
RETURN all(x IN p.scores WHERE x > 50), any(…), none(…), single(…);
RETURN reduce(total = 0, x IN [1, 2, 3] | total + x);             // 6
RETURN [(a)-[:knows]->(f) WHERE f.age > 30 | f.name];             // pattern comprehension
RETURN COUNT { (a)-[:knows]->() }, EXISTS { MATCH (a)-->(b) WHERE b.x = 1 };
RETURN p {.name, .age, friends: 3, other}, p {.*};                // map projection
RETURN 'abc' =~ 'a.c', 'ABC' =~ '(?i)abc';                        // true, true
```

`=~` matches the whole string. The syntax is that of Rust's `regex` crate:
no look-around or backreferences, and matching always takes linear time.

`CASE` without a matching `WHEN` and no `ELSE` gives `null`. Quantifiers
are three-valued: unknown items make the answer unknown unless the known
ones decide it.

| Function | |
| --- | --- |
| `id(x)`, `labels(n)`, `type(r)` / `label(r)` | a record's id, labels and edge label |
| `properties(x)`, `keys(x)`, `text(n)`, `startNode(r)`, `endNode(r)` | |
| `size(x)` / `length(x)`, `head(list)`, `last(list)`, `range(a, b[, step])` | |
| `lower`, `upper`, `trim`, `substring(s, start[, length])`, `replace(s, a, b)`, `split(s, sep)` | |
| `coalesce(…)`, `toString`, `toInteger`, `toFloat` | |
| `abs`, `round(x[, digits])`, `floor`, `ceil`, `sqrt`, `sign` | |
| `log` (natural), `log10`, `exp`, `sin`, `cos`, `tan`, `pi()`, `e()` | |
| `datetime([x])` / `time([x])`, `date([t])` (`YYYY-MM-DD`), `now()`, `timestamp()` (milliseconds) | |
| `duration(x)`, `duration.between`, `duration.inMonths` / `inDays` / `inSeconds`, `date.truncate(unit, t)` | see *Time and durations* |
| `exists(pattern)`, `exists(n.key)` | pattern predicate; whether a property is set and not null |
| `hasKey(x, 'key')` | whether a node, an edge or a map has the key, even with a `null` value |
| `nodes(p)`, `relationships(p)` / `rels(p)`, `length(p)` | the parts of a path |
| `reverse(x)`, `tail(list)`, `isEmpty(x)`, `left(s, n)`, `right(s, n)`, `ltrim`, `rtrim`, `char_length` | |
| `toBoolean`, `toIntegerOrNull`, `toFloatOrNull`, `toStringOrNull`, `toBooleanOrNull`, `toIntegerList`, `toFloatList`, `toStringList`, `toBooleanList`, `nullIf(a, b)` | conversions; `…OrNull` gives null instead of an error |
| `asin`, `acos`, `atan`, `atan2(y, x)`, `cot`, `degrees`, `radians`, `haversin`, `isNaN`, `rand()`, `randomUUID()`, `elementId(x)` | |
| `text.levenshtein`, `text.damerau`, `text.jaroWinkler`, `text.similarity(a, b[, measure])`, … | text similarity (see *Similar text and entity resolution*) |

## Full-text search

Each node may have one text (`text` when it is written), searched word by
word with BM25 (`db.search(text)`). A full-text index searches chosen
string properties of a label instead, each with a weight, with an
analyzer and a query language:

```text
CREATE FULLTEXT INDEX notes FOR (n:Note|Page) ON EACH [n.title, n.body]
  OPTIONS {analyzer: 'english', weights: {title: 3}};
CALL db.searchIndex('notes', '"graph memory" +agent -draft', 10) YIELD node, score, fragments;
```

- **Analyzers**: every one lowercases and removes accents; `standard`
  (the default) also leaves out common English and Spanish words;
  `english` and `spanish` leave out that language's common words and
  reduce words to their stem (`graphs`, `graphing` → `graph`; `grafos`,
  `grafo` → `graf`); `simple` keeps every word.
- **Fields**: a property holds a string or a list of strings; a list's
  items are far apart, so a phrase does not run from one into the next.
  `weights` multiply a field's score (1 by default).
- **Queries**: words (any may match, the more the better), `"a phrase"`
  (in order, next to each other), `"words near"~3` (all within three
  words, in any order), `+required`, `-excluded` (or `NOT`), `a AND b`,
  `a OR b`, parentheses, `prefix*`, `fuzzy~1` (one edit away; two at
  most), and `title:word` or `title:"a phrase"` for one field. A query of
  only excluded terms is refused.
- **Ranking**: BM25 per field (k1 1.2, b 0.75), weighted and summed; a
  prefix or a fuzzy term counts its best expansion (a fuzzy one a fifth
  less per edit). Phrases and nearness look at the first 64 positions of
  a term in a field.
- **Fragments**: `{field, text, matches}` per field that matched (at most
  three, heaviest first): the whole value when short, otherwise the words
  around the first match; `matches` are `[start, end)` offsets in
  characters of `text`.
- The index is kept in the same transaction as every write, rebuilt by
  `rebuild`, checked by `check`, exported, replicated and dropped as any
  index (`SHOW INDEXES` shows `kind` `fulltext`).

**Text, vector or both.** `db.search(text, {…})` and `POST
/v1/{ns}/search` take a `mode`:

```text
CALL db.search('graph memory', {index: 'notes'});                                   // text
CALL db.search('', {mode: 'vector', embedding: $e});                               // vector
CALL db.search('graph memory', {mode: 'hybrid', index: 'notes', embedding: $e});   // both, rank fusion
CALL db.search('graph memory', {mode: 'hybrid', embedding: $e, fusion: 'weighted', text_weight: 0.7});
```

Hybrid search takes `candidates` (50) from each list and fuses them: by
rank (`fusion: 'rank'`, the default: each list adds `1 / (k + rank)`,
`k` 60), or by score (`fusion: 'weighted'`: each list's scores scaled to
0..1, then `text × text_weight + vector × (1 − text_weight)`). Without
`index` the text side is the node text. Recall (`POST /v1/{ns}/recall`,
MCP `recall`) takes the same `index`, `mode`, `fusion`, `k` and
`text_weight`.

## Vector indexes

Each node may have one embedding (`embedding` when it is written), all of
one dimension per namespace, compared by cosine (`db.searchVector`). A
vector index holds instead a property of the nodes with its labels (a list
of numbers), with its own dimension, similarity and parameters; a
namespace may have several:

```text
CREATE VECTOR INDEX docs FOR (d:Doc|Page) ON (d.embedding)
  OPTIONS {dimension: 768, similarity: 'cosine', m: 16, ef_construction: 100, ef_search: 64};
CALL db.searchVector($e, {index: 'docs', limit: 10, labels: ['Doc'], where: {lang: 'es'}});
CALL db.searchVectors([$e1, $e2], {index: 'docs', limit: 5}) YIELD query, node, score;
MATCH (d:Doc {title: 'Graphs'}) CALL db.similar(d, {index: 'docs', limit: 5}) YIELD node, score;
```

- **Similarity**: `cosine` (vectors are normalized; the score is the
  cosine), `dot` (the dot product, unnormalized), or `euclidean` (the
  score is `1 / (1 + distance)`). `m` (links per node, 2 to 128),
  `ef_construction` (candidates while inserting) and `ef_search`
  (candidates while searching, unless a search gives `ef`) tune the HNSW
  graph: higher is slower and finds more of the true nearest.
- **Writes**: a node with the labels and a list of numbers of the index's
  dimension in the property is in it; another dimension is refused; a
  value that is not a list of numbers is left out. The index is kept in
  the same transaction as the write, exported, replicated and dropped as
  any index.
- **Filters** (`labels`: any of them; `where`: property values) are
  applied while the graph is walked: nodes that fail them are stepped
  through, not kept, so a filter that keeps a tenth of the nodes still
  returns `limit` of them. When at most 4 096 nodes have the filter's
  labels, those nodes are scored exactly instead. Filtering the results
  afterwards would keep about the share the filter keeps (see the phase's
  measurements).
- **Background builds**: `OPTIONS {…, background: true}` creates the
  index empty, then a server fills it a chunk at a time (2 000 nodes per
  write, replicated as it goes), while writes go on and keep the nodes
  they touch indexed; `SHOW INDEXES` shows `state` `building`, then
  `online`. The local shell finishes such builds before its next prompt.
- Hybrid search may use one: `db.search(text, {mode: 'hybrid',
  embedding: $e, vector_index: 'docs'})`; recall takes `vector_index` too.

## Constraints on facts

A person works at one company at a time. An edge constraint says so, and
a new fact that contradicts a current one ends it in valid time instead
of keeping both (or of failing):

```text
CREATE CONSTRAINT FOR ()-[r:works_at]->() REQUIRE ONE PER SOURCE;      // works_at_one_per_source
CREATE CONSTRAINT seat FOR ()-[r:sits_in]->() REQUIRE ONE PER TARGET ON CONFLICT REJECT;
CREATE CONSTRAINT FOR ()-[r:rates]->() REQUIRE ONE PER PAIR ON CONFLICT FLAG;
CREATE CONSTRAINT FOR (p:Person) REQUIRE p.city IS FUNCTIONAL;         // keeps earlier values
SHOW CONSTRAINTS;                                                       // name, kind, label, property, rule, on
DROP CONSTRAINT works_at_one_per_source;
```

| `ON CONFLICT` | A new fact that overlaps a current one in valid time |
| --- | --- |
| `INVALIDATE` (default) | the old fact ends where the new one starts (a new fact without `@valid_from` starts now); a new fact older than the current one ends where that one starts |
| `FLAG` | both are kept, and the contradiction is recorded |
| `REJECT` | the write fails |

`ONE PER SOURCE` is one current fact per node it starts from (to another
target), `ONE PER TARGET` per node it ends at, `ONE PER PAIR` per pair
(any two facts between them). The same fact again is no contradiction.
The old fact is not edited: its record is expired and a new version ends
where the new fact starts, so `KNOWN AT` an earlier time still shows what
was believed then:

```text
MATCH (p:Person {name: 'Ana'})-[:works_at]->(c) RETURN c.name;                         // now
MATCH (p:Person {name: 'Ana'})-[:works_at]->(c) VALID AT '2021-01-01' RETURN c.name;   // then
```

A write reports `facts_invalidated` and `conflicts_flagged` in its stats;
`GET /v1/{ns}/contradictions` lists what constraints did. A functional
property keeps its earlier values: `history(p, 'city')` is a list of
`{value, from, to, by}`, oldest first.

## Constraints on properties

A property every node (or edge) of a label must have, the type its
values must have, and keys:

```text
CREATE CONSTRAINT FOR (u:User) REQUIRE u.email IS NOT NULL;           // User_email_not_null
CREATE CONSTRAINT FOR (u:User) REQUIRE u.age IS :: INTEGER;           // User_age_integer
CREATE CONSTRAINT FOR (u:User) REQUIRE u.seen IS TYPED DATETIME;
CREATE CONSTRAINT FOR (b:Book) REQUIRE (b.isbn, b.edition) IS UNIQUE; // Book_isbn_edition_unique
CREATE CONSTRAINT book_key FOR (b:Book) REQUIRE (b.isbn, b.edition) IS KEY;
CREATE CONSTRAINT FOR ()-[r:rated]-() REQUIRE r.stars IS NOT NULL;    // the same on edges
CREATE CONSTRAINT FOR (n:Person|Company) REQUIRE n.name IS UNIQUE;    // across both labels
```

- `IS NOT NULL`: every node with the label has the property, and not as
  a stored `null`.
- `IS :: TYPE` (or `IS TYPED TYPE`): when the property is set, its value
  is a `BOOLEAN`, an `INTEGER` (a whole number), a `FLOAT` (a number with
  a fraction or an exponent), a `STRING`, a `LIST`, a `MAP` or a
  `DATETIME` (a string holding a date and time, as `datetime()` stores
  it). A `null` has every type: whether it may be `null` is the rule
  above.
- `IS UNIQUE` on one property or several: no two nodes share the values
  of all of them. A node that lacks one of them, or holds `null`, a list
  or a map there, is not checked.
- `IS KEY`: unique, and every node with the label has every property,
  each a boolean, a number or a string.
- On edges (`FOR ()-[r:label]-()`) the same rules hold for each current
  fact with the label; a retracted fact holds none.
- Declaring a rule checks the data already there: the first records that
  break it are named, and nothing is declared.
- Writes are checked as they go, except in a query, where a statement's
  end state is checked: `MERGE (b:Book {isbn: '7'}) ON CREATE SET
  b.edition = 1` creates the book and then gives it its key. Uniqueness
  is checked at each write. Replicated entries are not checked again
  (the member that made them did).
- `SHOW CONSTRAINTS` lists every kind (`kind`: `unique`, `key`,
  `not_null`, `type`, `edge`, `functional`; `on`: `nodes` or `edges`);
  unique and key constraints are indexes too, and `SHOW INDEXES` lists
  them.

## Similar text and entity resolution

Functions that say how alike two strings are. Distances count characters;
similarities go from 0 to 1. `text.similarity` and the indexes compare
normalized text (lowercase, without accents or punctuation); the other
functions compare what they get. A `null` argument gives `null`.

| Function | |
| --- | --- |
| `text.levenshtein(a, b)` | edits (insert, delete, replace) between them, an integer |
| `text.damerau(a, b)` | the same, with two swapped neighbors as one edit (`'jonh'`, `'john'`: 1) |
| `text.jaro(a, b)`, `text.jaroWinkler(a, b)` | for short strings such as names; Winkler favors a common start |
| `text.dice(a, b[, n])`, `text.jaccard(a, b[, n])` | shared n-grams (3 by default), robust to word order and long names |
| `text.lcs(a, b)` | length of the longest common subsequence |
| `text.soundex(s)`, `text.phonetic(s[, 'es' \| 'en'])` | how it sounds: Soundex (English), or a Spanish key (`b`/`v`, `c`/`s`/`z`, silent `h`, `ll`/`y`) |
| `text.normalize(s)`, `text.ngrams(s[, n])` | the normalized text, its n-grams |
| `text.similarity(a, b[, measure])` | any measure by name: `exact`, `edit`, `jaro`, `jaro_winkler` (default), `dice`, `bigram`, `jaccard`, `tokens` (shared words), `lcs`, `soundex`, `phonetic_es`, `date` (1 the same day, 0.8 with day and month swapped, falling to 0 a year apart), `number` |

```text
RETURN text.jaroWinkler('martha', 'marhta');                   // 0.961
MATCH (p:Person) WHERE text.similarity(p.name, $name) > 0.9 RETURN p;
```

**Fuzzy indexes.** A `WHERE text.similarity(…)` compares with every node.
A fuzzy index keeps the trigrams of a text property (of each string of a
list, such as `aliases`) and finds the nodes that look like a text
without comparing with every node: typos, accents, case and word order
included.

```text
CREATE FUZZY INDEX FOR (p:Person) ON (p.name);                 // Person_name_fuzzy
CALL db.searchFuzzy('Person', 'name', 'jonh smit', 5) YIELD node, score RETURN node.name, score;
CALL db.searchFuzzy('Person', 'name', 'maria garsia', 5, 'jaro_winkler') YIELD node RETURN node.name;
```

Candidates are the nodes that share the most trigrams with the text;
each is then scored with the measure against its value. A fuzzy index is
never unique and is not used for `=` or ranges (an exact index can sit
beside it on the same property).

**Match rules.** Deciding that a record and a node are the same thing
takes more than one field. A match rule, declared per label through the
API (`PUT /v1/{ns}/match-rules/{label}`, see the server's README), says
which properties identify (strong keys: equal means the same, different
means not), which properties compare and with what measure and weight,
whether shared neighbors count, and two thresholds: `same` and `review`.
`db.match` scores the candidates its indexes find (fuzzy indexes for
similar text, exact ones for equal values; without either, the first
20 000 nodes of the label) and explains every score:

```text
CALL db.match('Person', {name: 'Jon Smyth', born_on: '1980-03-12'}, 3)
YIELD node, score, decision, fields
RETURN node.name, score, decision, [f IN fields | f.key + ': ' + toString(f.similarity)];
```

## Indexes and performance

Without an index, a `MATCH` by property reads every node of its label:
`MATCH (p:Person) WHERE p.age > 32` reads all `Person` nodes. An index on
the label and property reads only the matching ones.

```text
CREATE INDEX FOR (p:Person) ON (p.age);                   // named Person_age
CREATE INDEX people_by_name IF NOT EXISTS FOR (p:Person) ON (p.name);
CREATE CONSTRAINT FOR (p:Person) REQUIRE p.email IS UNIQUE; // Person_email_unique
CREATE INDEX FOR (p:Person) ON (p.last, p.first);        // Person_last_first: several properties
CREATE INDEX FOR ()-[r:rated]-() ON (r.stars);           // rated_stars: on edges
CREATE INDEX named FOR (n:Person|Company) ON (n.name);   // over several labels
CREATE FUZZY INDEX FOR (p:Person) ON (p.name);           // trigrams, for db.searchFuzzy
SHOW INDEXES;                                             // name, label, property, unique, entries, fuzzy, on, key, kind, state
SHOW NAMESPACES;                                          // name, nodes, edges, created_at, description (those the caller may read)
SHOW CONSTRAINTS;
DROP INDEX people_by_name;
DROP CONSTRAINT Person_email_unique IF EXISTS;
```

- An index holds the nodes with the label whose property is a number, a
  string or a boolean (lists, maps and `null` are not indexed). It is
  filled when created and kept in the same transaction as every write.
- A unique constraint is an index that refuses a second node with the
  same value; creating one fails if duplicates already exist.
- An index on several properties holds the nodes that have all of them
  (each a number, a string or a boolean). The planner uses it for equal
  values on its first properties, in order, and then any of the lookups
  below on the next one: one on `(last, first)` serves `last = 'Ruiz'`,
  `last = 'Ruiz' AND first STARTS WITH 'A'` and both equal, not `first`
  alone.
- An index over several labels (`(n:A|B)`) holds the nodes with any of
  them, and serves a pattern with either label; the pattern's labels
  still decide what matches.
- An index on edges serves a pattern whose edge has the property,
  `(p)-[r:rated {stars: 5}]->(f)` or `WHERE r.at >= 10`: the match starts
  at the ends of the edges it finds, on the side of the pattern's start.
- The planner uses an index for `n.key = v`, `{key: v}` in the pattern,
  `n.key IN [...]`, `n.key STARTS WITH 'x'` and ranges (`>`, `>=`, `<`,
  `<=`, and both ends together), when the value can be computed before the
  match (literals, parameters, variables of earlier clauses). It compares
  numbers as numbers (`= 7` finds `7.0`). A comparison with a time value,
  or a property stored as another type, falls back to scanning the label.
- A pattern starts from a node bound by an earlier clause or given by
  `@id`. Otherwise each node's best way in (an index, its smallest label,
  or every node) is estimated, together with the edges followed from it,
  and the cheapest start wins: `status = 'blocked'` (1% of the nodes)
  over `city = 'big'` (half of them) whichever is written first, and a
  label of 1 000 companies over a range that holds every person.
- The estimates come from the **planner's statistics**: for each index,
  its entries, distinct values, the 64 most common values with their
  counts and 64 buckets of equal size (for ranges); for each edge label,
  its edges and the distinct nodes they leave and reach (so 200 people per
  company inward, one company per person outward). They are built from a
  snapshot the first time a namespace is planned, kept in memory, and
  rebuilt when its nodes or edges change by more than a tenth or an index
  is created or dropped (`CALL db.analyze()` rebuilds them at once and
  lists them). They are not stored.
- The patterns of one `MATCH` run in the order that costs least, not as
  written: `MATCH (p:Person)-->(c), (o:Owner {name: 'x'})-->(c)` finds the
  one owner first, then reaches `c` from it and `p` from `c`. A pattern
  that shares a variable with the ones before it goes next, so patterns
  are joined through the variables they share.
- Repeated statements (the same text, any parameters) are parsed once and
  kept in a cache of 1 024; the plan is still made at each run, from the
  current statistics, so a cached statement never uses a dropped index.
- A read-only `MATCH` that starts from 4 096 nodes or more (a label, or an
  index range) and needs every row (no `LIMIT` right after it) splits the
  candidates across threads: the rows, and their order, are the same as on
  one thread, and the query's limits count every thread's reads. The
  process shares at most one thread per CPU among all queries
  (`scan_threads` in `[query]`; 1 turns it off); a query that finds none
  free runs on its own.
- `MATCH … RETURN … LIMIT n` without `ORDER BY`, `DISTINCT` or aggregates
  stops after the rows it needs.
- A match grows from up to 64 starts at a time, one step at a time, and
  reads each node, edge list and edge record once for all the branches
  that reach it (so `PROFILE` counts fewer reads than rows); a step with
  more than 4 096 partial matches goes on in slices, spread over the
  shared threads in a read-only query. Rows come out in the same order as
  one start at a time. With a `LIMIT` right after, starts go one at a time.
- A `MATCH` followed by a `RETURN` or `WITH` with aggregates folds its
  rows into the groups as it finds them, a batch of up to 1 024 at a time,
  instead of holding them all: `count`, `sum`, `avg`, `min` and `max` keep
  one value per group, and only `collect`, `median`, percentiles, `stdev`,
  `variance` and `mode` keep the values they need. Counting millions of
  matches stays within `max_rows`; returning them whole does not. A
  parallel scan computes each batch's group keys and aggregate arguments
  on its own thread; the results, group order and floating-point sums
  included, are the same as on one thread.
- `EXPLAIN` shows where each pattern starts and how many nodes that is
  estimated to find (`index Person_age: … < age, ~420 nodes`, `index
  Person_last_first: last = …, first = …`, `edge index rated_at, where the
  edges leave: at >= …`, `scan of :Person, 2000 nodes`); `PROFILE` adds
  the records each step read.
- Indexes travel with exports and survive `rebuild`; `check` verifies them.

**Limits.** Over HTTP and MCP, each query runs within limits, so one query
cannot take the service down. By default (`[query]` in `grafeno.toml`):

| Limit | Default | What it bounds |
| --- | --- | --- |
| `timeout_secs` | 30 | wall time, checked while reading and inside loops that only compute (`reduce`, comprehensions, `UNWIND`) |
| `max_reads` | 50 000 000 | nodes, edges and index entries read |
| `max_rows` | 1 000 000 | rows held at once, between clauses and inside a match, an `UNWIND` or a sort (rows an aggregate folds are not held) |
| `max_memory_mb` | 512 | one value the query builds (a string, list or map) and, estimated from a sample, the rows held at once |
| `max_cost` | off | the `EXPLAIN` estimate: a statement above it is refused before it runs |

A query over a limit fails with the code `query_limit` and changes nothing;
a query whose client disconnects is cancelled. Two more bounds hold
everywhere: an expression may nest at most 200 levels deep (parentheses,
lists, calls, and chains of operators: `a OR b OR …` with hundreds of terms
is better written `x IN [...]`), and a value at most 128 lists or maps
deep. Past either, the query fails with an error instead of exhausting the
stack. The library and the local shell run without the other limits
(`grafeno_query::execute_with` takes them).

## Transactions

Every statement runs in one transaction: all of its changes or none,
checked against constraints and indexes, invisible to others until it
commits, and durable once it has. To make several statements one
transaction, put them between `BEGIN` and `COMMIT` (or `ROLLBACK`) in the
shell or a script, or use `POST /v1/{ns}/transactions` over HTTP:

```text
BEGIN;
MATCH (a:Account {id: 'a'}) SET a.balance = a.balance - 100;
MATCH (b:Account {id: 'b'}) SET b.balance = b.balance + 100;
COMMIT;
```

`BEGIN READ ONLY` reads one snapshot throughout. A writing transaction
holds the writer while it is open: writes are one at a time, so
transactions are serializable, and other writes wait for it (a server
rolls back one that stays idle too long). If a statement in it fails, the
transaction can only be rolled back: the failed statement may have
written part of its work. `BEGIN`, `COMMIT` and `ROLLBACK` sent to `/query`
are refused, since that endpoint runs one statement as one transaction.

## Namespaces

A namespace is created by its first write, or explicitly, with a comment
that says what it is for:

```text
CREATE NAMESPACE crm COMMENT 'customers and their orders';
CREATE NAMESPACE 'team-a' IF NOT EXISTS;     // quote names with a dash
ALTER NAMESPACE crm SET COMMENT 'customers';
ALTER NAMESPACE crm REMOVE COMMENT;
DROP NAMESPACE 'team-a' IF EXISTS;           // with everything in it
SHOW NAMESPACES;
```

Each returns the namespace's row (`name`, `nodes`, `edges`, `created_at`,
`description`); a `DROP` returns how many nodes and edges went, and the
write stats count `namespaces_created` and `namespaces_dropped`. Run over
HTTP, a statement may be sent through any namespace's `/query`; what it
needs is on the namespace it names: creating needs the right to create
that namespace, `ALTER` the namespace's `schema` privilege, `DROP` its
`drop` privilege (as `POST /v1/namespaces`, `PATCH` and `DELETE
/v1/namespaces/{ns}` need). In a cluster with several groups, it must be
sent through a namespace of the group the named one belongs to (group 0
for a new one).

A namespace, its indexes and constraints and some first data can be set
up from one script, kept with an application's code:

```text
// setup.gq
CREATE NAMESPACE crm IF NOT EXISTS COMMENT 'customers and their orders';
\use crm
CREATE INDEX IF NOT EXISTS FOR (c:Customer) ON (c.name);
CREATE CONSTRAINT IF NOT EXISTS FOR (c:Customer) REQUIRE c.email IS UNIQUE;
MERGE (:Customer {email: 'ana@example.org', name: 'Ana'});
```

```bash
grafeno shell --config /etc/grafeno/grafeno.toml --stop-on-error -f setup.gq
```

`--stop-on-error` stops at the first statement that fails; without it the
rest of the script still runs and the exit code says something failed.

## Users, roles and privileges

Accounts and roles (see the server's README) are managed in GQ by those who
may: `manage_accounts` for users and for giving roles to them,
`manage_roles` for roles and grants; only superusers make or change
superusers. These statements need no privilege on the namespace they are
sent to. Names with `-`, and namespace patterns, are quoted.

```text
CREATE USER ana SET PASSWORD $password;          // a user signs in
CREATE SERVICE ACCOUNT indexer;                 // a service account uses API keys
ALTER USER ana SET STATUS SUSPENDED;            // or ACTIVE; ends its sessions
ALTER USER ana SET EXPIRES '2027-01-01';        // null: never
ALTER USER ana SET PASSWORD $new SET DESCRIPTION 'research';
DROP USER indexer IF EXISTS;                    // with its keys and sessions

CREATE ROLE analyst SET DESCRIPTION 'reads the teams';
GRANT read, algorithms ON NAMESPACE 'team-*' TO analyst;
DENY read ON NAMESPACE 'team-secret' TO analyst;   // a denial wins
GRANT editor ON NAMESPACE notes TO analyst;        // a level: reader, editor, admin
GRANT read ON NODES Person IN crm TO analyst;      // EDGES works_at IN crm for edges
DENY read ON PROPERTY Person.email IN crm TO analyst;
GRANT metrics, backup ON SERVER TO analyst;
REVOKE DENY read ON NAMESPACE 'team-secret' FROM analyst;   // REVOKE GRANT …, or both
GRANT ROLE analyst TO ana;                      // to an account or another role
REVOKE ROLE analyst FROM ana;
DROP ROLE analyst;                              // every holder loses it

SHOW USERS;                  // name, kind, roles, grants, superuser, status, expires_at, created_at
SHOW ROLES;                  // name, built_in, roles, privileges, description
SHOW PRIVILEGES;             // the catalog: privilege, on
SHOW PRIVILEGES FOR ana;     // source, access, privilege, on (a role's, or an account's through its roles)
SHOW CURRENT USER;           // name, kind, superuser, roles, effective_roles
```

Record policies (14.4) limit a role to the records that match a condition,
for reads, writes or both; they narrow, like denials, so a record must
match every policy that covers it:

```text
RESTRICT analyst ON NAMESPACE crm NODES Customer WHERE tenant = $account.tenant;
RESTRICT writer ON NAMESPACE 'team-*' FOR WRITES WHERE @recorded_by = current_user();
RESTRICT auditor ON NAMESPACE crm FOR READS WHERE region = 'eu';
UNRESTRICT analyst ON NAMESPACE crm NODES Customer WHERE tenant = $account.tenant;
ALTER USER ana SET ATTRIBUTE tenant = 'acme';    // what $account.tenant is for ana
```

What a caller may not read is absent from every result, as if it did not
exist: nodes it may not read (a denied label, or a policy), edges whose
label it may not read or that lead to such nodes, and hidden properties,
from records, maps, `keys()`, `properties()`, filters (`WHERE n.email = …`
finds nothing) and indexes. `count`, paths, `EXISTS`, procedures,
algorithms and `history()` see only the rest; `EXPLAIN` shows no counts or
estimates there. A writing statement that would change something its
caller may not (a label, a property, a policy) fails whole and names the
privilege; records it may not see are not matched.

The audit log (14.5) is read with `db.audit(since, limit)`, by an account
with the `audit` privilege:

```text
CALL db.audit(0, 1000) YIELD seq, at, kind, account, via, address, target, outcome, detail
WHERE kind IN ['signin_failed', 'lockout', 'refused']
RETURN seq, at, kind, target, address ORDER BY seq DESC LIMIT 20;
```

Writing statements answer one row, `result`, saying what changed
(`granted 2 privileges on namespace team-* to analyst`). `EXPLAIN` of a
statement the caller may not run says which privilege it lacks, in a
`DENIED` step, and runs nothing; this holds for any statement
(`EXPLAIN MATCH (n) DETACH DELETE n` names `create`, `update` or `delete`
when one is missing).

## Choices to know

These are pinned in `crates/grafeno-query/tests/conformance/scenarios.txt`,
the language's conformance scenarios.

- Every node needs a label: `CREATE ({n: 1})` is refused.
- A variable-length edge reaches each end node once, at its shortest
  distance, not once per path: where several paths lead to a node there is
  one row, and `(a)-[:T*2]->(x)` does not find a node that also has a
  shorter way from `a`.
- `allShortestPaths` therefore gives one path per end.
- `id(x)` is a string (a UUID), not an integer.

## Not supported (yet)

- `UNION` inside `CALL { … }`, `IN TRANSACTIONS`, and path variables in
  `CREATE` and `MERGE`.
- A binary wire protocol: clients use the HTTP API or MCP.
- Graph algorithms over several namespaces at once. Recall with graph
  expansion and memory ranking is in the shell's `\recall` and
  `\context`, and in the HTTP API; hybrid search itself is
  `db.search(text, {mode: 'hybrid', …})`.
