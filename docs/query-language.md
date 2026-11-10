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
| `db.indexes()` | `name`, `label`, `property`, `unique`, `entries`, `fuzzy` | property and fuzzy indexes (also `SHOW INDEXES`); a fuzzy index counts trigram entries |
| `db.analyze()` | `kind`, `name`, `count`, `distinct`, `out_degree`, `in_degree`, `common` | rebuilds the planner's statistics now and lists them: per index its entries, distinct values and ten most common values (`[value, nodes]`); per edge label its edges and average degree each way |
| `db.searchFuzzy(label, property, text, limit = 10, measure = 'dice')` | `node`, `score` | nodes whose property looks like the text, through its fuzzy index (see *Similar text and entity resolution*) |
| `db.match(label, properties, limit = 10)` | `node`, `score`, `decision`, `fields` | nodes that may be the record described, by the label's match rule |
| `db.search(text, limit = 10)` | `node`, `score` | full-text search (BM25), best first |
| `db.searchVector(embedding, limit = 10)` | `node`, `score` | nearest nodes by embedding |
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

**Modes.** Results stream as rows. For statistics, aggregate them
(`CALL algo.louvain() YIELD community RETURN count(DISTINCT community)`);
to keep them, `SET` them, which turns the query into one write
transaction. Algorithms stop at the query's time limit or when it is
cancelled.

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

## Constraints on facts

A person works at one company at a time. An edge constraint says so, and
a new fact that contradicts a current one ends it in valid time instead
of keeping both (or of failing):

```text
CREATE CONSTRAINT FOR ()-[r:works_at]->() REQUIRE ONE PER SOURCE;      // works_at_one_per_source
CREATE CONSTRAINT seat FOR ()-[r:sits_in]->() REQUIRE ONE PER TARGET ON CONFLICT REJECT;
CREATE CONSTRAINT FOR ()-[r:rates]->() REQUIRE ONE PER PAIR ON CONFLICT FLAG;
CREATE CONSTRAINT FOR (p:Person) REQUIRE p.city IS FUNCTIONAL;         // keeps earlier values
SHOW CONSTRAINTS;                                                       // name, kind, label, property, rule
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
CREATE FUZZY INDEX FOR (p:Person) ON (p.name);           // trigrams, for db.searchFuzzy
SHOW INDEXES;                                             // name, label, property, unique, entries, fuzzy
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
  estimated to find (`index Person_age: … < age, ~420 nodes`, `scan of
  :Person, 2000 nodes`); `PROFILE` adds the records each step read.
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
- Indexes on several properties at once, or on edge properties.
- Graph algorithms over several namespaces at once. Hybrid recall is in
  the shell's `\recall` and `\context`, and in the HTTP API.
