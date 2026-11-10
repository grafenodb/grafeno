# Grafeno performance

How fast Grafeno is, how much memory it needs, and where its limits are,
measured. Every chart below is drawn from a CSV file in [`data/`](data/README.md)
by [`charts/build.py`](charts/build.py); the experiments, the code that runs
them and the discussion of each are in [benchmarks](benchmarks.md), and the
design behind the numbers in [`DESIGN.md`](../DESIGN.md) ("Measurements and
test environments").

## At a glance

| | |
| --- | --- |
| Read a node | 1.6 µs |
| Neighbors of a node | 25 µs |
| Full-text search (BM25, top 10 of 10 000) | 0.27 ms |
| Vector search (top 10 of 1 000 000, 128 dimensions) | 5 ms, 140 MiB resident |
| A durable write (immediate durability, under 4 000 writes/s) | about 2 ms |
| Most writes per second (immediate / eventual durability) | about 11 000 / 16 000 |
| A graph of 1 M nodes and 10 M edges | fits in 512 MiB |
| Hostile queries (cartesian products, huge loops, deep nesting) | 32 of 32 stopped within their limits |
| Finding a person by a misspelled name among 100 000 | 20 ms with a fuzzy index |
| Telling duplicates from namesakes (match rule) | 95% of duplicates found, 99.4% of "same" right |
| One current fact per person (constraint) | 14 000 facts/s, every person one current employer |
| CSV and GraphML | export 340 000 to 400 000 records/s; import about 26 000 to 28 000 records/s |
| Following writes: change log, streams, ingestion | 20% write cost; a commit reaches a stream in 0.15 ms; 25 000 NDJSON lines/s |
| Community summaries of 50 000 nodes | 3.2 s, teams and departments found; a topic finds its summary in 0.06 ms |
| Linking what memories mention to known entities | 3 200 texts/s, 99.4% linked right, none wrong |
| Planning by value distribution (200 000 people) | 355 ms → 11 ms where the rare condition was written second |
| Reads with 16 readers and a busy writer | 8 000 queries/s, p99 under 9 ms |
| Grouping 200 000 rows | 233 ms (609 ms before 12.3); 2 million matches counted within the limits |
| Two hops from 10 000 starts, 2 million matches | 2.7 s (5.6 s before 12.4), 72% fewer reads |
| Vector search with int8 copies | same recall (0.996); 1 M vectors in 512 MiB 3.5 to 4.2 ms a search (about 5 ms before) |

## Test environments

| Id | Where | Storage |
| --- | --- | --- |
| **E1-mem** | Laptop: AMD Ryzen 7 7435HS (8 cores, 16 threads), 32 GB, Windows 11 | in memory: no file, no fsync |
| **E1-disk** | The same laptop | a file on an NVMe SSD; immediate durability is an fsync per commit |
| **E2** | The same laptop, Linux containers (Docker on WSL2) with a hard memory limit of 2 GiB or 512 MiB and no swap | a Docker volume (behind a virtual disk: slower than E1-disk) |

Release builds of Rust 1.99 and redb 4.4, one run per configuration, in
November 2026. The laptop varies by about ±20% between runs, so only
larger differences mean something; E2's times are pessimistic and its
memory numbers are the point. Not yet measured: Linux on bare metal,
spinning disks, ARM, and runs of several days.

## Writes

Every write goes through one writer, which groups the writes waiting into
one transaction and one commit (group commit). With immediate durability a
commit waits for the disk, and the group shares that wait.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/write-latency-p99-dark.svg">
  <img alt="p99 write latency against offered load: flat at a few milliseconds up to 4 000 writes per second, then rising steeply past saturation" src="assets/charts/write-latency-p99-light.svg">
</picture>

Load is offered at a fixed rate whatever the answers, and latency runs
from each write's scheduled time, so falling behind shows. With immediate
durability a write takes about 2 ms (p50) and stays under 10 ms at p99 up
to about 4 000 writes a second: groups grow with the load, which keeps
latency flat. With eventual durability (commits reach the disk within
0.5 s) the median is 0.15 ms; its p99 is the flush every 500 ms, which
holds the writer for 10 to 25 ms.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/write-throughput-dark.svg">
  <img alt="writes committed against writes offered: they match until about 8 000 per second with max_batch 128, 11 000 with 512, and 16 000 with eventual durability" src="assets/charts/write-throughput-light.svg">
</picture>

Past saturation the writer commits a fixed number per second and the rest
waits in its queue. At high load every group is full, so the group size
(`max_batch`) bounds throughput: 128 writes per commit saturate at about
8 000 writes a second, 512 (the default) at about 11 000. A full queue
(4 096 writes) answers 503 with `Retry-After`; `GET /v1/metrics` shows the
queue, group sizes and latencies.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/concurrent-writes-dark.svg">
  <img alt="writes per second from 16 threads: 737 with a transaction each and immediate durability, 8 106 with group commit, 16 865 and 32 210 with eventual durability" src="assets/charts/concurrent-writes-light.svg">
</picture>

### Imports and agents at the same time

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/import-interference-dark.svg">
  <img alt="an agent's p99 write latency during a large import: about 10 seconds when the import holds the database, 88 milliseconds through the bulk lane in chunks of 2 000" src="assets/charts/import-interference-light.svg">
</picture>

Imports, rebuilds and compactions go through a bulk lane: one transaction
per chunk, taken between the groups of interactive writes. An agent writing
200 times a second while 300 000 nodes are imported waits 36 ms at the
median and 88 ms at p99 (chunks of 2 000, `POST /v1/import?bulk=true`),
instead of the whole import (about 12 s). An import in one transaction
(all or nothing) still holds the writer for as long as it runs.

## Search

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/hnsw-latency-dark.svg">
  <img alt="vector search latency against ef, 20 000 vectors: p50 from 0.32 to 1.14 ms and p99 from 0.61 to 2.09 ms as ef goes from 16 to 256" src="assets/charts/hnsw-latency-light.svg">
</picture>

Vector search uses an HNSW graph. On 20 000 vectors of 128 dimensions in
clusters, recall@10 (the share of the true 10 nearest found) is 0.956 at
`ef` 16, 0.996 at 32 and 1.000 from 64 (the default), where a search takes
0.51 ms (p50). On 1 000 000 vectors a search takes about 5 ms and needs
140 MiB of memory.

| `ef` | 16 | 32 | 64 | 128 | 256 |
| --- | --- | --- | --- | --- | --- |
| recall@10 | 0.956 | 0.996 | 1.000 | 1.000 | 1.000 |
| p50 / p99 (ms) | 0.32 / 0.61 | 0.41 / 0.72 | 0.51 / 0.98 | 0.79 / 1.56 | 1.14 / 2.09 |

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/hnsw-churn-dark.svg">
  <img alt="recall@10 at ef 64 through deletes, reinserts and churn: 0.99 or more after the repairs of 10.3, as low as 0.929 before" src="assets/charts/hnsw-churn-light.svg">
</picture>

Search quality holds as vectors are deleted, inserted back and moved:
recall@10 stays at 0.99 or more at `ef` 64 (it fell to 0.929 before the
repairs of phase 10.3). `grafeno check --recall 100` estimates it on your
own data, and `grafeno compact` rebuilds a namespace's graph.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/vector-insert-dark.svg">
  <img alt="seconds per 100 000 vectors inserted: 96 s at first, rising to 809 s for the last 100 000 of a million" src="assets/charts/vector-insert-light.svg">
</picture>

Inserting vectors gets slower as the index grows: each insert searches a
larger graph, and the file outgrows the page cache. A million vectors took
83 minutes in E2. A larger vector cache halves the time (300 000 vectors:
539 s with 128 MiB, 284 s with 1 GiB): set `vector_cache_mb` and write
embeddings in large transactions when loading many.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/property-index-dark.svg">
  <img alt="GQ queries with and without a property index: 233 against 10.7 ms for a range, 200 against 3.6 ms for an equality, 2.4 against 0.38 ms with a LIMIT" src="assets/charts/property-index-light.svg">
</picture>

A property index (`CREATE INDEX FOR (p:Person) ON (p.age)`) turns a label
scan of 100 000 nodes (about 2 µs a node) into a range read: 20 to 55 times
faster for selective conditions.

## Entity resolution

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/resolve-speed-dark.svg">
  <img alt="finding a damaged name among 100 000 people: 20 ms with a fuzzy index, 469 ms comparing with every node, 51 ms for a full match rule" src="assets/charts/resolve-speed-light.svg">
</picture>

A fuzzy index (`CREATE FUZZY INDEX FOR (p:Person) ON (p.name)`) keeps the
trigrams of a text property and finds what looks like a query (typos,
accents, word order) in 20 ms among 100 000 people, instead of comparing
with each of them (469 ms).

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/resolve-quality-dark.svg">
  <img alt="a match rule's decisions: every duplicate with swapped dates, no accents or another word order called same; most typos same and the rest review; initials split; namesakes 96.6% different" src="assets/charts/resolve-quality-light.svg">
</picture>

A match rule per label says how to decide that a record is a node:
strong keys (an email, an id) decide outright; otherwise fields compare
with their own measures and weights, and shared neighbors can count. On
2 000 damaged duplicates and 2 000 namesakes (a real name, another
person), a rule on name, birth date and city found 95% of the duplicates
(81% as "same", the rest "to review") and called 9 namesakes "same": 99.4%
of its "same" decisions were right. Every decision lists each field's
similarity, so a rule can be checked and tuned; with a looser date
tolerance (a year instead of 30 days) 115 namesakes passed as "same".

## Facts that replace facts

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/constraint-cost-dark.svg">
  <img alt="50 000 job changes: 44 000 facts per second without a constraint, 14 000 with one current employer per person" src="assets/charts/constraint-cost-light.svg">
</picture>

`CREATE CONSTRAINT FOR ()-[r:works_at]->() REQUIRE ONE PER SOURCE` makes a
new job end the old one in valid time, while what was believed before
stays visible with `KNOWN AT`. With 50 000 job changes for 10 000 people,
every person ended with exactly one current employer and every earlier
job in history, at a third of the write speed (each new fact looks at
the person's other ones).

## Planning and concurrent reads

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/planner-plans-dark.svg">
  <img alt="Queries a planner without statistics got wrong: 355 ms to 11 ms, 1.8 s to 2.7 ms, 2.6 s to 1.8 ms" src="assets/charts/planner-plans-light.svg">
</picture>

The planner knows how values spread in each index and how many edges of
each label leave and reach a node, so it starts where the fewest rows
are, whichever condition or pattern is written first. A large read scan
splits across threads: 161 ms on 16 threads against 706 ms on one.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/read-concurrency-dark.svg">
  <img alt="Read queries per second against readers: 1 259 with one reader, 9 168 with 16, 7 960 with 16 and a busy writer" src="assets/charts/read-concurrency-light.svg">
</picture>

Reads run on snapshots and never wait for the writer: 16 readers do about
9 200 queries a second (a point lookup and two hops each), and 8 000 while
a writer commits 10 500 nodes a second, with p99 under 9 ms.

## Aggregates over many rows

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/batch-aggregation-dark.svg">
  <img alt="Aggregates over 200 000 people: grouping 609 ms to 233 ms, a filter and four aggregates 479 ms to 162 ms, two hops counting 2 million rows 6.3 s to 4.8 s" src="assets/charts/batch-aggregation-light.svg">
</picture>

A `MATCH` followed by aggregates folds its rows into the groups as it finds
them, a batch at a time, instead of holding every row: grouping 200 000
people is 2.6 times faster, and counting two million matches fits the
server's limits.

## Traversals from many starts

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/traversal-dark.svg">
  <img alt="Two hops from 10 000 people 5.6 s to 2.7 s, two hops from every company 9.5 s to 2.9 s, reach within 3 hops 2.3 s to 2.0 s" src="assets/charts/traversal-light.svg">
</picture>

A pattern grows from many starts at once, one step at a time, and every
node, edge list and record it reads is read once for all the branches that
reach it; large frontiers spread over the cores. Two hops from every
company go from 9.5 s to 2.9 s. The rows are the same, in the same order.

## Compact storage

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/compact-storage-dark.svg">
  <img alt="Adjacency tables 26.3 MiB to 21.3 MiB each; int8 vector copies of 7.8 MiB next to 26.2 MiB of exact vectors" src="assets/charts/compact-storage-light.svg">
</picture>

Adjacency entries are stored compactly (19% smaller), and every vector also
as int8: searches walk the graph with the quarter-size copies and rescore
the best candidates with the exact vectors, at the same recall (0.996 at
`ef` 64). With everything in memory the speed is the same; with 1 M vectors
in a 512 MiB container a search goes from about 5 ms to 3.5 to 4.2 ms, in
the same memory.

## Moving graphs in and out

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/interchange-dark.svg">
  <img alt="200 000 records: CSV exports at 397 000 records per second and imports at 27 600; GraphML at 341 000 and 25 900" src="assets/charts/interchange-light.svg">
</picture>

A namespace goes out as CSV or GraphML for spreadsheets and graph tools,
and comes back the same. Importing takes about as long as
any other write of those records, since each goes through every index.

## Following the writes

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/change-log-cost-dark.svg">
  <img alt="200 000 records written: 35 000 per second without the change log, 29 500 with it" src="assets/charts/change-log-cost-light.svg">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/ingest-runs-dark.svg">
  <img alt="100 000 NDJSON lines: 25 000 per second the first time, 360 000 per second sent again as duplicates" src="assets/charts/ingest-runs-light.svg">
</picture>

Every commit appends what it changed to a change log, numbered per
namespace, at about a fifth of the write speed. Agents and services
follow it with `GET /v1/{ns}/changes` as server-sent events (a commit
arrives in 0.15 ms), with webhooks, or with the MCP `changes` tool.
Events posted as NDJSON with idempotency keys can be retried safely: a
retry of 100 000 lines writes nothing and takes a quarter of a second.
Extraction and community summaries can follow the log in the background.

## Communities that summarize themselves

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/community-refresh-dark.svg">
  <img alt="refreshing community summaries of 50 000 nodes: 3.2 s the first time, 1.35 s when nothing changed, 2.15 s after 1% new nodes" src="assets/charts/community-refresh-light.svg">
</picture>

`summarize_communities` (or `POST /v1/{ns}/communities/refresh`) finds the
communities of the graph and the communities they form, and writes a
summary of each without a model: who is central, which words set it
apart, what kinds of nodes, which facts, and when. On 50 000 people and
notes in 1 000 teams and 125 departments, one level of summaries matched
the teams and the next the departments (99.9% of the nodes of each in
the same department), and a team's topic found its summary first every
time. A refresh with nothing changed writes nothing; agents ask broad
questions with `overview` and open a community to go down.

## Extraction

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/extract-runs-dark.svg">
  <img alt="extracting 20 000 memories: 6.2 s the first time, 0.06 s when nothing changed, 0.15 s when 1% changed" src="assets/charts/extract-runs-light.svg">
</picture>

`grafeno extract` (or `POST /v1/{ns}/extract`, or the `extract` MCP tool)
reads the memories that are new or changed and links what they mention:
the names and aliases the namespace knows, emails, URLs, handles,
hashtags, dates, amounts, and new names once they recur. On 20 000
memories naming 5 000 entities it reads about 3 200 texts a second and
linked 99.4% of the mentions to the right entity, none to a wrong one,
although most people shared their name with someone: the other entities
in the same text decide, and what they cannot decide waits for review.
Later runs read only what changed.

## The engine

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/engine-dark.svg">
  <img alt="core operations before and after the performance pass, in microseconds on a log scale" src="assets/charts/engine-light.svg">
</picture>

Reads take microseconds: a node 1.6 µs, the neighbors of a node 25 µs, a
two-hop neighborhood 16 µs. Traversals read only adjacency entries, which
carry each edge's times, so filters by time need no edge records.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/algorithms-dark.svg">
  <img alt="graph algorithms on 100 000 nodes and 500 000 edges, in milliseconds on a log scale: PageRank 17 ms, Louvain 427 ms, betweenness with 100 sources 1.7 s" src="assets/charts/algorithms-light.svg">
</picture>

Graph algorithms run on a projection of the namespace in memory (about 12
bytes per edge), built in one scan of the adjacency and kept until the next
commit. In GQ the scoring algorithms take `top: n` so that only the best
rows are read.

## Memory

A graph of 1 million nodes and 10 million edges (a 6.4 GB file), in
containers whose memory limit kills the process if it is crossed.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/memory-512-dark.svg">
  <img alt="peak memory per operation in a 512 MiB container: between 5 and 382 MiB for every operation, 509 MiB for a projection with edge ids" src="assets/charts/memory-512-light.svg">
</picture>

In 512 MiB, with a 128 MiB page cache and a 64 MiB projection cache, every
operation completes: reads, queries, export, the integrity check, PageRank,
and a projection with edge ids for path algorithms (509 MiB at its peak).
The price of small caches is time: a projection that does not fit in its
cache is rebuilt for each algorithm call (about a minute here), and GQ
algorithms then reach the 30 s query timeout.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/memory-2g-dark.svg">
  <img alt="peak memory per operation in a 2 GiB container with default caches: under 550 MiB for everything but a projection with edge ids (830 MiB) and a refused query (989 MiB)" src="assets/charts/memory-2g-light.svg">
</picture>

With the default caches (256 MiB of pages, 256 MiB of projections) a
server holding this graph stays near 300 MiB outside graph algorithms. The
989 MiB query asked for a row per node (a million node records) and was
refused by its `max_memory` limit, as intended; `top: 5` answers it in
1.9 s without growing.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/check-cost-dark.svg">
  <img alt="integrity check of 10 M edges: 2 562 s with a lookup per edge, 249 s with fingerprints in 2 GiB, 368 s in 512 MiB" src="assets/charts/check-cost-light.svg">
</picture>

`grafeno check` reads every record and every index once, in order, and
compares them by fingerprints, so it needs little memory and no random
reads: 249 s for 10 M edges (443 MiB at its peak). Its first version held
every record in memory and was killed at 2 GiB.

Sizing: Grafeno's memory is mostly its caches, set in `[memory]`
(`grafeno.toml`). Everything else is per request and bounded by the
`[query]` limits. See [install](install.md#memory).

## Limits that hold under abuse

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/charts/hostile-queries-dark.svg">
  <img alt="32 hostile GQ queries, each stopped by a limit (rows, memory, time, reads, value depth, nesting) or answered, in between 1 ms and 2 s" src="assets/charts/hostile-queries-light.svg">
</picture>

Each GQ query runs within limits of time, records read, rows held, memory
and (optionally) estimated cost, and the parser refuses expressions nested
more than 200 levels deep. 32 hostile queries (cartesian products,
ten-hop paths, million-row sorts, strings doubled forty times, lists nested
100 000 deep, 50 000-term chains) each end with an error naming the limit
in at most 2 s, on the stack of a server worker; a fuzz test feeds random
text to the parser and planner and none panics.

## Correctness

Speed is measured with benchmarks; correctness with tests that run on
every change (about 210, plus the client's): property tests that compare
the engine with a plain model, processes killed in the middle of writes,
databases written by every past version, damaged files, concurrent
readers, isolation between API keys, and fuzzing. `DESIGN.md` lists which
test holds each guarantee.

## Reproducing

The commands for every experiment are in `DESIGN.md` ("Reproducing"); then

```bash
python docs/charts/build.py
```

redraws the charts from `docs/data/`.
