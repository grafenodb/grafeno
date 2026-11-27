# Benchmarks and validation

The environments (E1-mem, E1-disk, E2), the methodology and a summary are
in `DESIGN.md`, "Measurements and test environments"; every table here
has its numbers as CSV in [`data/`](data/README.md), and
[performance](performance.md) shows them as charts.

Measured on 2026-11-03 on an AMD Ryzen 7 7435HS with 32 GB of RAM, Windows 11,
Rust 1.99 (release builds). Databases are in memory, so the numbers measure
the engine and redb without disk syncs; a durable commit to a file adds an
fsync per transaction. Runs on this laptop vary by about ±20%: compare
changes larger than that.

```bash
cargo bench -p grafeno --bench engine
```

```bash
cargo run --release -p grafeno --example hnsw_recall -- 20000 128 200
```

## Engine (`benches/engine.rs`)

Corpus: notes of 60 words from a 2 000-word skewed vocabulary, four random
links per note. "Before" is layout version 2 (phase 4), "after" is the
performance pass (layout version 3).

| Operation                                           | Before  | After   |
| --------------------------------------------------- | ------- | ------- |
| Write 100 nodes with text in one transaction        | 26 ms   | 17 ms   |
| Write 1 node with text and a 64-d vector (2k nodes) | 3.0 ms  | 3.5 ms  |
| Write 1 edge (own transaction)                      | 98 µs   | 120 µs  |
| Get a node (10k nodes)                              | 1.4 µs  | 1.6 µs  |
| Neighbors, both directions (10k nodes, 40k edges)   | 26 µs   | 25 µs   |
| k-hop, depth 2                                      | 66 µs   | 16 µs   |
| Shortest path, at most 6 hops                       | 11 ms   | 3.2 ms  |
| BM25, 3-word query, top 10 (10k notes)              | 0.90 ms | 0.27 ms |
| HNSW top 10, 10k uniform 128-d vectors, `ef` 32     | 1.5 ms  | 1.1 ms  |
| HNSW top 10, same, `ef` 64 / 128                    | 2.4 / 3.7 ms | 1.8 / 2.8 ms |

The single-write rows did not change in code paths that matter (getting a
node was not touched at all), so their differences are noise.

## Vector index quality (`examples/hnsw_recall.rs`)

20 000 vectors of dimension 128 around 64 cluster centers (closer to real
embeddings than uniform noise), 200 queries, recall@10 against exact
brute-force results, after the performance pass:

| `ef` | recall@10 | p50     | p99     |
| ---- | --------- | ------- | ------- |
| 16   | 0.956     | 0.32 ms | 0.61 ms |
| 32   | 0.996     | 0.41 ms | 0.72 ms |
| 64   | 1.000     | 0.51 ms | 0.98 ms |
| 128  | 1.000     | 0.79 ms | 1.56 ms |
| 256  | 1.000     | 1.14 ms | 2.09 ms |

Brute force takes about 3 ms per query on the same data. Inserting the
20 000 vectors (batches of 1 000) went from 22.5 s to about 9 s (890 → about
2 200 per second); the integrity check passes with no dangling links.
`ef_construction` 64 instead of 100 gave the same recall and a difference
within the noise, so it stays at 100.

## Where single writes spend their time

Measured with each write in its own transaction, on 2 000 notes:

| Write                         | Time    |
| ----------------------------- | ------- |
| Empty node (commit cost)      | 90 µs   |
| Node with 60 words of text    | 740 µs  |
| Node with a 64-d vector       | 1.8 ms  |

redb is a copy-on-write B-tree: the first change to a page in a transaction
copies it. A text touches one posting page per distinct term, and a vector
rewrites the link lists of about 17 nodes spread over the table. Batching
writes (the HTTP API's `/batch`, or one `write` for many nodes) shares those
copies: 100 notes with text cost 17 ms together, 170 µs each.

## What changed in the performance pass

1. **Traversals read no edge records to filter.** Adjacency entries carry
   the edge's record time and valid time (layout version 3), so `k_hop`,
   `shortest_path`, `subgraph` and recall's graph expansion only read
   adjacency ranges. `neighbors` still reads the record of each edge it
   returns.
2. **Postings carry the text length,** so BM25 ranking needs no lookup per
   matching document.
3. **Vector distances need no copies.** Searches compute dot products
   straight from the stored bytes, with eight accumulators the compiler
   vectorizes.
4. **Write transactions cache the vector graph.** Decoded vectors and link
   lists used by inserts and removes stay in a cache that every write updates
   and that dies with the transaction, so batch inserts stop re-reading the
   same entries.

## Correctness tests

| Test                         | What it shows                                                                 |
| ---------------------------- | ----------------------------------------------------------------------------- |
| `tests/model.rs` (proptest)  | Random sequences of every write (including aborted transactions, valid times and invalidations) leave the engine equal to a plain in-memory model: records, labels, counters, neighbors, complete text search, complete vector search. Traversal filters on adjacency entries agree with the same filters on the edge records, for every direction, valid time, record time and label. The same holds after export → import and after rebuilding the indexes. Passed 3 000 cases. |
| `tests/durability.rs`        | A child process writing transactions is killed three times at arbitrary moments; every reported commit survives, nothing is half-written, and the integrity check passes. |
| `tests/concurrency.rs`       | Four readers running while a writer commits always see whole transactions, and successive snapshots never go back. |
| `src/check.rs` tests         | Damage to each derived structure is detected, and `rebuild_indexes` repairs it. |
| `src/db.rs` tests            | Layout version 1 and 2 files are upgraded on open (counters; new adjacency and postings, old tables dropped); newer layouts are refused. |

## Vector index under change (`examples/hnsw_churn.rs`)

20 000 vectors of dimension 128 in 64 clusters, 200 queries, recall@10
against exact search over the vectors alive at that moment. Deleted
vectors are removed from nodes that stay; "inserted back" sets the same
vectors on the same nodes; churn moves a fifth of the vectors elsewhere.
Release build, 2026-11-04.

| State | ef 64 before 10.3 | ef 64 after | ef 16 / 32 after |
| --- | --- | --- | --- |
| built | 1.000 | 1.000 | 0.965 / 0.998 |
| 10% deleted | 1.000 | 1.000 | 0.974 / 0.996 |
| 10% inserted back | **0.929** | 1.000 | 0.953 / 0.993 |
| 30% deleted | 0.999 | 0.999 | 0.985 / 0.998 |
| 30% inserted back | 0.980 | 0.999 | 0.957 / 0.995 |
| 50% inserted back | 0.988 | 1.000 | 0.950 / 0.990 |
| 5 rounds of churn (worst round) | **0.956** | 0.990 | 0.950 / 0.984 |

Dangling links after deleting 10% went from 8 047 to under 1 000. What
was wrong, and is now fixed:

1. **A node could become its own neighbor.** Links left pointing at a
   removed vector led the insertion of the same id back to the node
   itself (distance 0), and its link list was overwritten with a single
   self-link: a dead end that searches could enter and not leave (some
   returned fewer than 10 results).
2. **Repairs could close islands.** The neighbors of a removed node were
   reconnected only among the removed node's own links, which can turn a
   neighborhood into a group with no way out. They now also search from
   where they are (`ef` 16) for candidates.
3. **Only one direction was repaired.** Nodes two hops away that linked
   to the removed one kept a link to nothing; they are now repaired too.
4. **Repairs wrote link lists for vectors already removed** in the same
   transaction ("links without a vector" in the check).

The price is in deletions: about 2.5 ms per removed vector here instead
of 0.5 ms (searches and inserts are unchanged). A handful of vectors (up
to 10 of 20 000 after heavy churn) can still lose every link pointing at
them, as in any HNSW; the check reports them as `unreachable_vectors`
and `grafeno compact` (or `WriteTx::compact_vectors`) rebuilds the graph
of a namespace, which takes about as long as inserting every vector.
`grafeno check --recall 100` estimates recall@10 from the namespace's own
vectors.

The same run with the removals each graph counts since it was built
(`vector_health`, what automatic compaction looks at), unreachable
vectors, and a compaction at the end; release build, 2026-11-12:

| State | Removed since built | recall@10 ef 16 / 32 / 64 | Unreachable |
| --- | --- | --- | --- |
| built | 0 | 0.957 / 0.996 / 1.000 | 0 |
| 10% deleted | 2 000 | 0.974 / 0.997 / 1.000 | 1 |
| 30% inserted back | 8 000 | 0.947 / 0.989 / 0.999 | 2 |
| 50% inserted back | 18 000 | 0.954 / 0.990 / 0.995 | 3 |
| churn round 4 (20% updated) | 34 000 | 0.961 / 0.987 / 0.990 | 8 |
| churn round 5 | 38 000 | 0.965 / 0.992 / 0.995 | 8 |
| compacted (6.4 s) | 0 | 0.963 / 0.998 / 1.000 | 0 |

Repairs hold recall@10 at `ef` 64 at 0.99 or above through 38 000
removals (almost twice the graph), while the vectors no search reaches
creep up; compacting brings both back to a fresh graph's. The server
compacts at half by default (`compact_vectors_at`), which this run passes
at the 30% round.

## Property indexes (`crates/grafeno-query/examples/index_bench.rs`)

One GQ query on 100,000 `:Person` nodes (ages 0 to 99), in memory, release
build, before and after `CREATE INDEX FOR (p:Person) ON (p.age)`. Reads are
the records counted by `PROFILE`.

| Query | Without index | With index |
| --- | --- | --- |
| `WHERE p.age > 97` (2% of the nodes), `count(p)` | 233 ms, 100,000 reads | 10.7 ms, 6,000 reads |
| `{age: 42}` (1%), `count(p)` | 200 ms, 100,000 reads | 3.6 ms, 2,000 reads |
| `WHERE p.age = 42 … LIMIT 10` | 2.4 ms, 1,024 reads | 0.38 ms, 1,064 reads |

Without an index, a `LIMIT` already stops the scan after the first page of
candidates. A full label scan costs about 2 µs per node, most of it
decoding the node record.

## Concurrent writes (`crates/grafeno/examples/concurrency.rs`)

16 threads, 200 writes each of one node, on a database file (Windows,
NVMe), while 4 threads read pages of nodes:

| Writes | Durability | Writes/s | Reads/s meanwhile |
| --- | --- | --- | --- |
| one transaction each | immediate | 737 | 79,900 |
| group commit (`Writer`) | immediate | 8,106 | 63,356 |
| one transaction each | eventual | 16,865 | 61,093 |
| group commit (`Writer`) | eventual | 32,210 | 64,903 |

A commit with immediate durability waits for the disk; group commit shares
that wait among every write in the queue. Readers are not blocked by the
writer (MVCC).

## Loading (`crates/grafeno/examples/bulk_load.rs`)

Imports of nodes with 30-word texts, chained by edges, into a file:

| Nodes | One transaction | Bulk (`--bulk`) |
| --- | --- | --- |
| 200,000 (no texts) | 8.4 s (23,900 nodes/s) | 7.7 s (25,800 nodes/s) |
| 200,000 | 32.4 s (6,200 nodes/s) | 25 s (8,000 nodes/s) |
| 1,000,000 | — | 134 s (7,450 nodes/s) |

The rate holds as the database grows. Most of the time goes to the text
postings (about 25 per text): bulk mode sorts them per chunk, which helps
by a third; a different postings layout would be the next step.

## Graph algorithms (`crates/grafeno/examples/algo_bench.rs`)

100,000 nodes and 500,000 edges, in communities of 100 nodes with 90% of
the edges inside one; in memory, release build, 2026-11-04:

| Step | Time |
| --- | --- |
| Projection (one scan of the adjacency) | 102 ms |
| The same projection again (cached) | 0 ms |
| Projection with an edge weight property | 718 ms |
| Dijkstra, one pair (13 hops) | 27 ms |
| Yen, 5 shortest paths | 756 ms |
| PageRank, 20 rounds | 17 ms |
| Weakly connected components | 3 ms |
| Label propagation | 309 ms |
| Louvain (384 communities, modularity 0.899) | 427 ms |
| Triangles (89,654) | 77 ms |
| Betweenness, 100 sampled sources | 1.7 s |
| FastRP, 64 dimensions | 104 ms |
| Node similarity, top 10 per node (1,000,000 pairs) | 374 ms |

A weight property costs a read of each edge record while projecting;
without one, building reads only the adjacency. In GQ, each row an
algorithm yields also reads its node (about 2 µs), so streaming a score
for every one of 100,000 nodes adds about 0.2 s; `LIMIT` does not save
that work, since rows are ranked first, but the `top` setting does (10.6).

## Memory under a large graph (`crates/grafeno-query/examples/ram_bench.rs`)

1 million nodes and 10 million edges (four labels, skewed toward a few
hubs), a 6.4 GB file, in Docker containers with a hard memory limit (no
swap) on a Windows host: the file lives on a Docker volume, whose disk is
slow, so times are pessimistic. Steady is the resident memory after an
operation, peak the most during it (`VmRSS` and `VmHWM`, the peak reset
before each). 2 GiB with the default caches (256 MiB of pages, 256 MiB of
projections); 512 MiB with `cache_mb = 128`, `projection_cache_mb = 64`.
Release build, 2026-11-05.

| Operation | 2 GiB: s | steady / peak MiB | 512 MiB: s | steady / peak MiB |
| --- | --- | --- | --- | --- |
| create 1 M nodes (transactions of 20 000) | 17 | 286 / 286 | | |
| create 10 M edges (transactions of 20 000) | 3 598 | 298 / 299 | | |
| open (closed cleanly) | 0.0 | 5 / 5 | 0.0 | 5 / 5 |
| open after a crash (redb repairs the file) | 461 | 274 / 274 | 523 | 139 / 139 |
| list every node id | 1.8 | 293 / 293 | 1.3 | 207 / 207 |
| 10 000 neighbor lookups | 54 | 324 / 324 | 62 | 207 / 207 |
| 100 two-hop neighborhoods | 0.3 | 324 / 324 | 0.3 | 207 / 207 |
| GQ `MATCH ()-[r]->() RETURN count(r)` | 30 (timeout) | 365 / 371 | 30 (timeout) | 221 / 221 |
| GQ lookup by property, no index | 4.2 | 385 / 385 | 1.9 | 223 / 223 |
| export (streamed) | 145 | 385 / 385 | 180 | 223 / 223 |
| check | 249 | 393 / 443 | 368 | 223 / 273 |
| projection, no edge ids (138 MiB) | 68 | 535 / 535 | 63 | 356 / 356 |
| PageRank (projection cached, or built again) | 1.8 | 550 / 550 | 66 | 223 / 382 |
| projection with edge ids (291 MiB) | 61 | 830 / 830 | 61 | 509 / 509 |
| GQ `algo.pageRank({top: 5})` | 1.9 | 550 / 550 | 30 (timeout) | 242 / 355 |
| GQ `algo.pageRank()`, every row, `LIMIT 5` | 8 (refused, `max_memory`) | 989 / 989 | 30 (timeout) | 242 / 355 |

What the first runs found, and what changed:

- **`check` read every record into memory** (10 M decoded edges): killed
  at 2 GiB. It now reads records and indexes once each, in order, and
  compares fingerprints (a sum of 128-bit hashes per index); only an
  index whose fingerprint differs is looked at entry by entry. The first
  streaming version, with a point lookup per edge, took 2 562 s; the
  fingerprints take 249 s, about twice an export.
- **A GQ algorithm yields a row per node**: `CALL algo.pageRank() …
  ORDER BY score DESC LIMIT 5` held a million node records (1.8 GiB) and
  was killed. Rows built by a procedure now count against `max_memory` as
  they are built (it stops at 512 MiB of estimated rows), a node's
  estimated size counts its properties, and `top: n` keeps only the n best
  rows (1.9 s, no growth).
- **Projections** kept a 16-byte edge id and a hash map entry per node and
  per edge twice; they now take about 12 bytes per edge without edge ids
  (asked for only by path algorithms), 28 with them, and 24 per node, and
  reserve their arrays up front instead of doubling them. The projection
  cache is bounded by bytes (it held four projections of any size).
- **The page cache** was redb's default (1 GiB), over a 512 MiB container
  by itself; it is now 256 MiB by default and set with `[memory]`.

- **Opening after a crash** takes minutes on a file this size: redb
  repairs it (the runs killed above left it so). A file closed cleanly
  opens at once.

What 512 MiB holds: everything, with small caches. A projection of 10 M
edges (138 MiB) and PageRank on it fit, and so does one with edge ids (291
MiB, for path algorithms on the whole graph; before projections reserved
their arrays, building it was killed). But a 64 MiB projection cache
keeps neither, so each algorithm builds its projection again (about 60 s
here), and GQ algorithms then hit the 30 s query timeout: on a small
machine with a large graph, raise `timeout_secs` for algorithms or give
the projection cache room. Counting every edge in GQ hits the 30 s limit
on any machine: aggregates do not stream yet (12.3); `db.stats()` has the
counts.

### Vectors

1 M vectors of dimension 128 (256 clusters) on 1 M nodes, set in
transactions of 5 000, then 1 000 searches for the 10 nearest (`ef` 64),
same containers.

| Operation | s | steady / peak MiB |
| --- | --- | --- |
| set 1 M vectors (2 GiB, vector cache 128 MiB) | 4 976 | 442 / 442 |
| 1 000 searches (2 GiB) | 5.0 | 442 / 442 |
| 1 000 searches (512 MiB, page cache 128 MiB) | 5.0 | 140 / 140 |
| set 300 000 vectors, vector cache 128 MiB | 539 | about 440 |
| set 300 000 vectors, vector cache 1 GiB | 284 | 535 / 583 |

Inserts slow down as the index grows: each 100 000 took 96 s at first and
809 s at the end (an HNSW insert searches a larger graph, and reads more
of a file larger than the page cache). A larger vector cache halves the
time; it lives for one write transaction, so large transactions make the
most of it. Searches stay at about 5 ms and need little memory: 140 MiB
with a million vectors (512 MiB of vectors on disk).

## The one writer under load (`crates/grafeno/examples/saturation.rs`)

Writes of a node and an edge each, offered at a fixed rate by 512 client
threads for 3 s per step (open load: latency runs from the scheduled
time, so falling behind shows), through the group-commit writer, on a
database file on a laptop's NVMe SSD (Windows 11). Release build,
2026-11-05.

Immediate durability, `max_batch` 128 (before) and 512 (now):

| offered/s | 128: done/s | p50 / p99 / p999 ms | writes/commit | 512: done/s | p50 / p99 / p999 ms | writes/commit |
| --- | --- | --- | --- | --- | --- | --- |
| 250 | 250 | 1.8 / 4.1 / 11 | 1.0 | 250 | 1.8 / 2.7 / 11 | 1.0 |
| 1 000 | 999 | 2.9 / 4.9 / 11 | 1.9 | 999 | 3.3 / 6.1 / 11 | 2.2 |
| 2 000 | 1 998 | 3.5 / 5.6 / 11 | 4.6 | 1 998 | 3.9 / 7.5 / 12 | 5.2 |
| 4 000 | 3 992 | 5.5 / 9.9 / 25 | 14.7 | 3 993 | 6.1 / 11 / 16 | 16.2 |
| 8 000 | 7 851 | 29 / 71 / 73 | 120 | 7 891 | 30 / 70 / 78 | 159 |
| 16 000 | 8 129 | 1 290 / 2 872 / 2 903 | 127 | 11 200 | 571 / 1 288 / 1 305 | 495 |
| 32 000 | 7 233 | 5 048 / 10 171 / 10 270 | 128 | 10 024 | 3 127 / 6 510 / 6 580 | 505 |

Eventual durability (`max_batch` 128):

| offered/s | done/s | p50 / p99 / p999 ms | writes/commit |
| --- | --- | --- | --- |
| 250 | 250 | 0.17 / 1.3 / 9.4 | 1.0 |
| 1 000 | 1 000 | 0.14 / 11 / 18 | 1.0 |
| 4 000 | 4 000 | 0.12 / 26 / 51 | 1.1 |
| 8 000 | 7 928 | 0.22 / 63 / 82 | 1.4 |
| 16 000 | 15 775 | 35 / 122 / 128 | 107 |
| 32 000 | 16 589 | 1 415 / 2 768 / 2 784 | 127 |

What the numbers say:

- With immediate durability a write takes about 2 ms (the disk's flush)
  until about 4 000 writes a second; groups grow with the load, which is
  what keeps latency flat. Past that the groups reach `max_batch` and it
  bounds throughput: 128 writes per commit saturate at about 8 000 a
  second, 512 at about 11 000 (the default now). Beyond saturation
  latency grows with the queue, and at the queue's limit (4 096) writes
  get 503.
- With eventual durability a write takes about 0.15 ms until about 8 000
  a second and saturates at about 16 000. Its p99 is the flush: every
  500 ms the writer makes the commits durable, which holds it for 10 to
  25 ms; a write arriving then waits for it.
- No step lost or failed a write; none reached the queue limit (512
  clients keep at most 512 waiting).

An agent writing 200 times a second for 12 s while 300 000 nodes with
texts are imported:

| import | import s | agent p50 / p99 / max ms |
| --- | --- | --- |
| none (the agent alone) | - | 1.9 / 2.8 / 10 |
| around the writer, one transaction (the server before 10.7) | 12.2 | 6 208 / 12 035 / 12 152 |
| around the writer, chunks of 20 000 (`?bulk=true` before 10.7) | 9.7 | 3 745 / 9 564 / 9 681 |
| bulk lane, one transaction | 10.3 | 4 269 / 10 088 / 10 208 |
| bulk lane, chunks of 20 000 | 10.5 | 291 / 696 / 758 |
| bulk lane, chunks of 2 000 (`?bulk=true` now) | 11.6 | 36 / 88 / 161 |

An import that took the database for itself stopped the agent for as
long as it ran. Through the bulk lane, the agent's writes go between
chunks: with chunks of 2 000 records an agent waits 36 ms at the median
and 88 ms at p99 while the import runs about 10% slower. An import in one
transaction (all or nothing) still holds the writer for its whole length,
in either lane: that is what all or nothing means; `?bulk=true` is the
way for large loads.

## Entity resolution (`crates/grafeno/examples/resolve_bench.rs`)

100 000 synthetic people (first names and one or two last names from
lists of Spanish and English names, so many people share a name; a birth
date; a city), in memory, release build, 2026-11-05. A fuzzy index on the
name holds 17 trigram entries per person and takes 4.9 s to build.

| Finding a damaged name, top 10 | p50 | p99 |
| --- | --- | --- |
| fuzzy index | 20 ms | 39 ms |
| comparing with every node (Dice) | 469 ms | 542 ms |
| `find_matches` with a rule of four fields | 51 ms | 91 ms |

The true name was in the fuzzy top 10 for 280 of 300 damaged names (the
misses are initials and heavy damage to short names).

The rule: name by Dice over trigrams (weight 2) and by shared words
(0.5), birth date within 30 days (2; day and month swapped scores 0.8),
city exact (0.5); `same` from 0.85, `review` from 0.7. 2 000 duplicates
(one person's record with one kind of damage, and no city a quarter of
the time) and 2 000 namesakes (an existing name with another person's
birth date and city):

| Record | Records | same | review | different |
| --- | --- | --- | --- | --- |
| day and month swapped | 349 | 349 | 0 | 0 |
| no accents, upper case | 339 | 339 | 0 | 0 |
| word order | 318 | 318 | 0 | 0 |
| typo | 338 | 244 | 93 | 1 |
| two letters swapped | 329 | 218 | 108 | 3 |
| initial for the first name | 327 | 152 | 82 | 93 |
| **all duplicates** | 2 000 | 1 620 | 283 | 97 |
| namesakes (should not be same) | 2 000 | 9 | 59 | 1 932 |

Precision of `same`: 0.994 (1 620 of 1 629); recall at `same` 0.81, at
`review` or better 0.95. The same rule with a 365-day date tolerance
called 115 namesakes `same` (precision 0.932): tolerances decide how many
namesakes pass, and each decision lists its fields so they can be tuned.
Initials are the hard case for trigram measures; a field compared with
Jaro-Winkler, or an `initials` alias, would catch more of them.

## Extraction (`crates/grafeno-memory/examples/extract_bench.rs`)

5 000 entities (1 000 companies, 4 000 people with names from short lists,
so 3 982 share a name with another; each person works at one company) and
20 000 memories, each naming one to three people and their companies,
sometimes with an email, a hashtag, an amount, or a project that recurs.
In memory, release build, 2026-11-05.

| Run | Texts read | Seconds |
| --- | --- | --- |
| first | 20 000 | 6.2 (3 236 texts/s) |
| again, nothing changed | 0 (20 000 unchanged) | 0.06 |
| again, 1% changed | 200 | 0.15 |

The first run found 79 820 names, 10 064 emails, 6 658 hashtags, 6 678
amounts and 1 923 phrases, and wrote 97 857 `mentions` edges. Against what
the generator put in each memory: 79 348 of 79 787 mentions (0.994) were
linked to the right entity, and none to a wrong one. 39 713 mentions were
of a name several people share; the company named in the same sentence,
linked to one of them by `works_at`, settled all but 263 names, which
wait in the review queue. The recurring projects became entities, but
phrases are words only: the 200 projects, named with a number ("Proyecto
Andina 3"), collapsed into 16 entities by their words. Names with digits
need an identifier pattern.

## Edge constraints (`crates/grafeno/examples/constraint_bench.rs`)

10 000 people and 500 companies; 50 000 job changes, an hour apart in
valid time, in transactions of 1 000, in memory, release build,
2026-11-05.

| Writes | Seconds | Facts per second |
| --- | --- | --- |
| without a constraint | 1.14 | 44 013 |
| one employer per person (`invalidate`) | 3.52 | 14 202 |

With the constraint every person who ever had a job has exactly one
current employer (9 931; the other 69 were never picked), and 40 069
earlier jobs stay as facts that ended in valid time, each invalidation
recorded. Each write reads the source's other facts of the label to find
contradictions, and an invalidation writes a new version of the old fact.

## Community summaries (`crates/grafeno-memory/examples/community_bench.rs`)

1 000 teams of 50 (people and notes on the team's own topic), three
links inside the team per node, one in four to another team of the same
department of eight, one in fifty to anyone: 50 000 nodes and 160 430
edges, in memory, release build, 2026-11-05.

| Run | Seconds | Summaries written |
| --- | --- | --- |
| first refresh | 3.23 | 9 131 created |
| nothing changed | 1.35 | none (9 131 unchanged) |
| after 500 new people (1%) | 2.15 | 1 013 created, 1 220 updated, 998 removed; 6 913 unchanged |

| Level | Summaries | Purity against teams | Against departments | Coverage |
| --- | --- | --- | --- | --- |
| 0 | 8 052 | 0.993 | 0.999 | 0.79 |
| 1 | 889 | 0.835 | 0.999 | 1.00 |
| 2 | 167 | 0.169 | 0.999 | 1.00 |
| 3 | 38 | 0.124 | 0.974 | 0.31 |

Purity is the share of the nodes of each summary that belong to its
majority team (or department). Level 0 splits teams into parts, level 1
is close to the teams, level 2 to the departments. A team's topic word
finds its summary first (level 0) for 200 of 200 teams, 0.06 ms a query.
Every refresh recomputes Louvain and the summaries; what it saves is
writes. With 100 teams (4 000 nodes) the first refresh takes 0.25 s.

## CSV and GraphML (`crates/grafeno/examples/interchange_bench.rs`)

100 000 nodes with text and 99 999 edges, in memory, release build,
2026-11-05.

| Format | Operation | Seconds | Records per second | Size |
| --- | --- | --- | --- | --- |
| CSV | export | 0.50 | 396 595 | 19.3 MiB |
| CSV | import | 7.25 | 27 568 | |
| GraphML | export | 0.59 | 340 742 | 40.0 MiB |
| GraphML | import | 7.73 | 25 869 | |

## Change log and streaming ingestion (`changes_bench.rs`, `grafeno-server/examples/events_bench.rs`)

In memory, release build, 2026-11-05.

| Writes of 100 000 nodes with text and 99 999 edges | Seconds | Records per second |
| --- | --- | --- |
| change log off | 5.67 | 35 266 |
| change log on | 6.78 | 29 510 |
| on, with a window of 10 000 entries (pruned at each commit) | 6.80 | 29 415 |

The log adds one entry per record changed (about 20% of write
throughput) and reads back at 1.45 million entries a second.

| NDJSON over HTTP (in process), 100 000 lines | Seconds | Lines per second |
| --- | --- | --- |
| 50 000 nodes, then 50 000 edges by key | 4.02 | 24 902 |
| the same request again: all duplicates | 0.28 | 359 978 |

A commit reaches a server-sent events stream in 0.15 ms at the median and
0.36 ms at p99 (300 commits). Before reading lines in place, ingestion
went at 3 000 lines a second: each line was cut from the front of the
buffer (quadratic).

## The planner and concurrent reads (`crates/grafeno-query/examples/planner_bench.rs`)

200 000 `:Person` nodes (99% `active`, half in one city, ages 0 to 99,
indexes on `status`, `city`, `age` and `uid`), 1 000 companies, each person
working at one, and one owner of three companies; in memory, release
build, 2026-11-05. "Before" is a build of 12.1 run in the same session,
alternating with this one, because the laptop's speed varied by more than
a factor of two that afternoon.

| Query | Before 12.2 | After | Start after |
| --- | --- | --- | --- |
| `city = 'big' AND status = 'blocked'` (the rare value second) | 354.7 ms | 11.1 ms | index on `status`, ~2 080 nodes |
| `status = 'blocked' AND city = 'city1'` (the rare value first) | 3.3 ms | 3.7 ms | index on `city`, ~963 nodes |
| `(p:Person)-[:works_at]->(c:Company {name: 'c7'}) WHERE p.age >= 0` | 1 770.5 ms | 2.7 ms | the 1 000 companies |
| `(p:Person)-[:works_at]->(c), (o:Owner {name: 'x'})-[:owns]->(c)` | 2 593.1 ms | 1.8 ms | the owner, then `c` bound |
| 50 000 point queries with a parameter | 17 682 /s | 18 616 /s | index on `uid` |

Before, a start was picked by kind (an equality on any index over a range,
a range over any label), so the first equality written won and a range
holding every person beat a label of 1 000; patterns ran as written. The
statistics took 0.3 s to build. Caching the parse gains about 5% on a
point query, whose time is mostly reading.

| Scan of every person (`age > 50 AND name CONTAINS '7'`, 40 000 rows) | ms |
| --- | --- |
| before 12.2 | 738 |
| after, one thread | 706 |
| after, 16 threads | 161 |

With `readers` (`planner_bench 200000 readers`), 1 to 16 threads run, for
3 s each, a query per `uid` that goes two hops (about 200 rows), without
and with a writer committing 10 nodes per transaction as fast as it can.
Best of three runs per cell (`read_concurrency.csv`):

| Readers | Before, alone | After, alone | After, busy writer | Writes/s meanwhile | p99 after (alone / busy) |
| --- | --- | --- | --- | --- | --- |
| 1 | 1 111 | 1 259 | 657 | 26 357 | 1.3 / 1.9 ms |
| 4 | 3 053 | 3 322 | 3 622 | 25 733 | 2.3 / 1.8 ms |
| 8 | 5 893 | 6 850 | 5 288 | 16 500 | 1.8 / 2.6 ms |
| 16 | 9 221 | 9 168 | 7 960 | 10 553 | 4.3 / 8.5 ms |

The query already started from its index, so 12.2 does not change it;
differences between builds are within the runs' noise. Reads scale about
8 times from 1 to 16 threads (8 cores, 16 hardware threads), and a busy
writer costs them 5 to 15%; the writer slows as readers take the CPU.

## Batched aggregation (`crates/grafeno-query/examples/batch_bench.rs`)

200 000 `:Person` nodes (200 cities, ages 0 to 89), 1 000 companies, each
person working at one; in memory, release build, 2026-11-05. "Before" is
12.2 built in the same session; best of two runs each, alternating.

| Query | Before 12.3 | After |
| --- | --- | --- |
| `MATCH (p:Person) RETURN count(*)` | 290.8 ms | 204.3 ms |
| `… RETURN p.city, count(*), avg(p.age)` (200 groups) | 609.3 ms | 233.2 ms |
| `… WHERE p.age > 30 RETURN p.status, sum, min, max, count` | 478.9 ms | 162.4 ms |
| `MATCH (c:Company)<-[:works_at]-(p:Person) RETURN c.name, count(p)` | 989.0 ms | 650.2 ms |
| `… WHERE p.uid < 20000 RETURN p.city, collect(p.uid)` | 54.0 ms | 23.2 ms |
| two hops from 10 000 people, 1 990 000 rows counted | 6 309 ms; fails with the standard limits (`max_rows`) | 4 825 ms; completes with them |

Before, every row of the match was kept as a map of variables, then
grouped by a string key into lists of rows, and each aggregate walked its
group's rows again. Now rows fold into one accumulator per group as the
match makes them. The two-hop count gains least: its time is the
expansion, one anchor at a time, which 12.4 batches.

## Batched and parallel traversal (`crates/grafeno-query/examples/traversal_bench.rs`)

200 000 `:Person` nodes working at 1 000 companies (200 each), each knowing
5 people at random; in memory, release build, 2026-11-05. "Before" is 12.3
built in the same session, alternating; best of four runs before and two
after. Reads are the records counted by `PROFILE`.

| Query | Before 12.4 | After | Reads before → after |
| --- | --- | --- | --- |
| colleagues of 10 000 people (`p.uid < 10000`), 1 990 000 rows counted | 5 590 ms | 2 705 ms | 4 030 002 → 1 115 305 |
| two hops from every company (`(c)<-[:works_at]-(p)-[:knows]->(f)`), grouped | 9 486 ms | 2 884 ms | 2 401 000 → 1 401 959 |
| friends of friends of 20 000 people, distinct | 1 139 ms | 1 180 ms | 1 240 002 → 714 714 |
| reach within 3 hops (`*1..3`) of 2 000 people | 2 328 ms | 2 017 ms | 623 873 → 459 521 |
| colleagues of 1 000 people returned whole, 199 000 rows | 1 468 ms | 1 509 ms | 403 002 → 403 002 |

Before, every partial match read its node's edges and decoded each target
again: a company's 200 employees were read once per colleague who reached
it. Now each is read once per traversal, and a frontier of thousands of
partial matches is split across threads (the company query, whose 1 000
anchors were too few for 12.2's parallel scans). Friends of friends reads
42% less but takes as long: its 20 000 anchors were already split across
threads, and its time is building and comparing rows. Rows returned whole
share nothing.

## Compact storage and int8 vectors (`crates/grafeno/examples/storage_bench.rs`)

A file with 100 000 nodes, 300 000 edges (a third with a valid-time range)
and 50 000 clustered vectors of 128 dimensions, measured after closing it;
release build, 2026-11-05, against 12.4 built in the same session.

| | Before 12.5 | After |
| --- | --- | --- |
| `adjacency_out` / `adjacency_in`, stored | 26.3 MiB each | 21.3 MiB each |
| the same with B-tree metadata | 28.0 / 28.2 MiB | 24.0 / 24.2 MiB |
| exact vectors | 26.2 MiB | 26.2 MiB |
| int8 copies (`vectors_q8`) | | 7.8 MiB |
| whole file | 514 MiB | 514 MiB |
| every node's edges, both ways | 1 253 to 1 404 ms | 1 422 to 1 465 ms |
| vector search, `ef` 64: recall@10, p50, p99 | 0.996, 1.35 ms, 2.51 ms | 0.996, 1.40 ms, 3.06 ms |

The adjacency is 19% smaller; reading it is not faster, as decoding the
varints costs about what the smaller entries save while everything is in
memory. Searches keep their recall and walk a quarter of the vector bytes;
in memory that changes nothing. Where the vectors do not fit in the page
cache it does (E2, 10.6's file of 1 M vectors of 128 dimensions, copied
once per build, `ram_bench search`, four runs each, alternating):

| Container, page cache | 12.4 | 12.5 | Resident |
| --- | --- | --- | --- |
| 512 MiB, 128 MiB | 4.9 to 5.2 ms a search | 3.5 to 4.2 ms | 140 MiB both |
| 2 GiB, 256 MiB | 5.0 to 6.0 ms | 3.2 to 3.7 ms | 272 to 278 MiB both |

Upgrading the file to layout 13 took 4.5 s; its int8 copies (about 126
MB) fit in free pages, so it kept its 2.06 GB. An earlier run of
this benchmark used clusters so spread that recall was 0.31 to 0.33 in
both versions: the data, not the index; the numbers above use clusters
like `hnsw_recall`'s.

## The replication log (`crates/grafeno/examples/replication_bench.rs`)

100 000 nodes with a 10-word text and 99 999 edges, in transactions of
1 000 nodes, in memory, release build, 2026-11-06; three runs of each,
alternating, with the log off and on.

| | |
| --- | --- |
| writes, log off (best) | 22 925 records/s |
| writes, log on (best) | 18 384 records/s (80%) |
| log size | 100 entries, 108 MiB, 568 bytes per record |
| applied by an empty replica | 22 298 records/s |

Each entry holds the final value of every record its transaction wrote,
texts included, and the change log entries of the commit, as JSON; a
binary encoding comes with 13.2, when entries go over the network.

## A Raft cluster (`crates/grafeno-cluster/examples/cluster_bench.rs`)

Sequential writes of one node each, on files on one NVMe disk with
immediate durability, 2026-11-06; the cluster's three members run in one
process, connected by the memory transport.

| | Writes/s | p50 | p99 |
| --- | --- | --- | --- |
| one server | 618 | 1.58 ms | 2.27 ms |
| three-member cluster | 137 | 7.22 ms | 9.94 ms |

A write through the cluster is a dry run on the leader, an fsync of the
Raft log on the leader and on each follower, and an apply with its own
fsync on every member, all sharing one disk here; grouping writes in one
entry is the lever (not built yet). With the leader cut off, another
member is elected and commits a write in 510 to 831 ms (median 595, five
runs) with 50 ms ticks (election timeouts of 0.5 to 1 s).

## Passwords (`crates/grafeno-server/examples/auth_bench.rs`)

Argon2id with the server's parameters (19 MiB, 2 passes, 1 lane),
2026-11-06:

| | |
| --- | --- |
| a hash (making or changing a password) | 18.3 ms |
| a check (signing in) | 18.9 ms |
| sign-ins a second, one core | 53 |
| sign-ins a second, 16 threads | 141 |

Checks are bound by memory more than by cores, which is what makes them
slow to guess offline; sessions and keys (SHA-256) cost a lookup.

## Audit log (`crates/grafeno/examples/audit_bench.rs`)

110 000 events in memory, 2026-11-06:

| | |
| --- | --- |
| appended one write each (a refused request, a failed sign-in) | 24 600 events/s |
| appended a thousand a write | 211 000 events/s |
| read back in pages of a thousand | 1.4 M events/s |
| a change of an account, with its event | 49 µs |

A refused request costs one small write; the change of an account writes
its event in the same transaction.

## Enforcement on reads (`crates/grafeno-query/examples/visibility_bench.rs`)

50 000 Person nodes (a tenth also `Salary`, half in team red, each with an
email and a 32-dimension embedding) and about three `knows` edges each, in
memory, best of three, 2026-11-06. The same queries without a view, with a
view that reads the whole namespace, and with one decided per record
(`Salary` denied, `email` hidden, a policy keeping the account's team):

| Query | No view | Whole namespace | Per record |
| --- | --- | --- | --- |
| count every node | 65.2 ms | 64.8 ms | 98.8 ms (+51%) |
| filter on a property | 15.8 ms | 16.3 ms | 22.2 ms (+40%) |
| one hop from 1 000 people | 19.2 ms | 19.0 ms | 23.9 ms (+25%) |
| two hops from 100 people | 16.7 ms | 17.7 ms | 22.6 ms (+36%) |
| full-text search | 12.3 ms | 12.0 ms | 12.1 ms |
| vector search, top 10 | 1.16 ms | | 4.79 ms |

Per record, every node read is checked against the labels, the hidden
properties and the policy; a view of the whole namespace is decided once.
Vector search under the per-record view (45% of the vectors visible)
filters after the walk and widens it until ten visible hits are found: its
recall@10 among the visible is 0.972, against 0.912 for the plain search
among all, which walks less.

## Roles (`crates/grafeno/examples/rights_bench.rs`)

5 000 roles, each with four privileges (read and create on its own
namespace, a denial of delete on a pattern, a denial of reading a label)
and two roles below it; one account holds 1 000 of them and reaches
2 811. In memory, 2026-11-06:

| | |
| --- | --- |
| writing the 5 000 roles (each checked for cycles) | 0.6 s |
| reading them and computing the account's rights | 17.3 ms |
| a namespace check | 93 ns |
| a label or property check | 271 ns |

The rights are computed once per change of an account or a role, then a
request costs a check. Before the cycle check walked only the roles a
role reaches, writing the 5 000 took 33 s.

## Cluster runbooks under load (`packaging/compose/drill.sh`)

Three members in Docker on this laptop, two Raft groups, three clients of
the `drill` example writing distinct values to a register and reading it
linearizably on random members, 2026-11-06. Every history was checked
for linearizability.

| Step | Operations | Writes succeeded in | Longest stretch without a write | Linearizable |
| --- | --- | --- | --- | --- |
| rolling upgrade 13.5 → 13.6, the leader first | 11 218 | 92% of 100 ms windows | 4.8 s | yes |
| the leader down for 10 s | 1 754 | 65% | 2.3 s | yes |
| two of three members down for 10 s | 1 220 | 28% | 19.6 s | yes |
| a disk replaced from a follower's backup | 1 779 | 63% | 3.0 s | yes |

Each step is short and holds its outage, and clients also pick members
that are down, so the share is low by design; what matters is that no
history broke linearizability. Details in `docs/cluster-runbooks.md`.

## Several Raft groups (`crates/grafeno-server/examples/groups_bench.rs`)

Three members on this laptop (one NVMe disk), 50 ms ticks, immediate
durability; 24 clients, 8 for each of three namespaces, writing one node
per request through all members for 10 s, 2026-11-06. With one group the
three namespaces share it; with three, each has its own.

| Groups | Writes/s | p50 | p99 |
| --- | --- | --- | --- |
| 1 | 109 | 218 ms | 907 ms |
| 2 | 153 | 116 ms | 227 ms |
| 3 | 181 | 135 ms | 156 ms |

A group commits one write at a time (each waits for a majority's fsync
and its own apply), so more groups commit more at once; here they share
one disk and eight cores, which caps the gain. Moving a namespace of
20 000 nodes (41 557 effects) to another group while a client writes to
it took 11 s, with 2 catch-up rounds and a pause of 39 ms (3 entries
copied under the fence); the client's 264 writes were all there, once,
after retrying 2 busy answers.

## Reads split across members (`crates/grafeno-server/examples/split_bench.rs`)

200 000 people (name, age, score, city), each working at one of 1 000
companies and knowing 3 others; every member starts from a copy of the
same file and may use 3 scan threads, to stand for a machine of that
size. All members run on this laptop (8 cores, 16 threads), 2026-11-06;
median of 5 runs after a warm-up, every split answer checked against the
unsplit one.

| Query | One server, 3 threads | 3 members | 5 members | One server, 9 threads |
| --- | --- | --- | --- | --- |
| scan with a text filter | 233 ms | 129 ms (1.8×) | 112 ms (2.1×) | 115 ms |
| aggregate by city | 412 ms | 176 ms (2.3×) | 141 ms (2.9×) | 255 ms |
| top 20 by score (`ORDER BY … LIMIT`) | 745 ms | 337 ms (2.2×) | 250 ms (3.0×) | 546 ms |
| two hops, counted | 2 538 ms | 1 828 ms (1.4×) | 1 775 ms (1.4×) | 1 364 ms |

Scans, aggregates and sorted limits gain most: each member does a share
and returns little. Two hops land on random nodes, so every member reads
the whole graph; on one machine each process needs its own cache of it
and they share the memory bandwidth, which one server with 9 threads
does not (on separate machines this is not shared). With 15 threads one
server does no better than with 9 here.

## Faults under load (`linearizable.rs`, `faults.rs`)

Clients write distinct values to one register and read it with
linearizable reads while a nemesis injects faults; every history is
checked by `grafeno_cluster::check`. 20 ms ticks (election timeouts of
200 to 400 ms), 2026-11-06. "Resume" is the time from a fault that hit the
leader to the first write sent after it that succeeded; "available" is the
share of 100 ms windows in which a write succeeded.

| Setup | Operations | Faults (on the leader) | Resume, median / max | Available | Linearizable |
| --- | --- | --- | --- | --- | --- |
| five members in one process, 30 s (three runs) | 23 645 | 130 (60) | 370-384 / 1 015 ms | 68-75% | all |
| three processes killed and restarted, 40 s (two runs) | 8 114 | 32 (19) | 529-531 / 697 ms | 73-78% | all |

In one process the faults are cutting off one member, the leader or two
members, restarting a member or the leader, and compacting a log, one
every 0.5 to 1.3 s; with processes, a kill every 1.5 to 3.5 s, half of
them the leader's. Availability is low because the rate of faults is
high: a fault aimed at the leader costs about half a second of writes.
With real processes, the time to resume was 1 545 ms before connections
between members got a timeout (Windows takes 2 s to refuse a connection
to a port nobody listens on), 1 023 ms with a 1 s timeout, and 530 ms with
the 500 ms one kept. The control run, with reads served from a member's
own state without ReadIndex, fails the check every time.

## Hubs in recall (`crates/grafeno-memory/examples/hub_bench.rs`)

20 000 notes of 12 words from a 3 000-word skewed vocabulary, each linked
to 2 of 200 topics and all to one user node (a hub of 20 000 edges); 300
queries of 2 words from random notes, recall of 10 with text alone, in
memory, release build, 2026-11-12. `hubs` 0 and `fanout` 0 are the
expansion before damping.

| depth | hubs | fanout | user in top 10 | topics in top 10 (mean) | p50 ms | p99 ms |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 0 | 0 | 100% | 0.2 | 0.2 | 0.3 |
| 1 | 1 | 0 | 0% | 0.0 | 0.5 | 0.7 |
| 1 | 1 | 100 | 0% | 0.0 | 0.4 | 0.9 |
| 2 | 0 | 0 | 100% | 0.0 | 57.5 | 77.6 |
| 2 | 1 | 0 | 0% | 0.0 | 97.7 | 117.8 |
| 2 | 1 | 100 | 0% | 0.0 | 45.8 | 62.0 |

Undamped, the user node is in every answer: it collects a share from all
5 seeds. Damped, it is in none, and the results are the notes the words
match. Damping counts each reached node's edges, up to 1 024 (a first
version counted all of them and took depth 1 from 0.2 to 4.6 ms); at
depth 2 the cap on neighbors per node (100, the default) makes recall
faster than before, as the hub no longer hands a share to its 20 000
notes. The multi-hop example (`examples/multi_hop.rs`, also a test) shows
the other side: an answer three hops away still comes back, ranked above
a near miss.

## Retrieval over a palace (`crates/grafeno-memory/examples/palace_bench.rs`)

Synthetic notes in 20 rooms of 5 loci, each note of about 12 words (its
room's subject, eight common words, its neighbor room's subject), two
`relates_to` facts per note to notes of its room or the next one, and a
door from each room to the next. The topic is `billing timeout incident`;
budgets are 1 500 tokens. Each line is 50 runs after one warm-up, in
memory, release build, on one laptop (16 threads), 2026-11-13. "Returned"
is results, walk memories or context items.

| What | 1 000 notes: median / p95 ms | 5 000 notes | 20 000 notes | Returned | Tokens |
| --- | ---: | ---: | ---: | ---: | ---: |
| `recall`, 10 results | 0.28 / 0.34 | 0.92 / 1.10 | 4.23 / 4.83 | 10 | — |
| `context` | 0.58 / 0.73 | 1.29 / 1.47 | 4.58 / 5.66 | 20 | 1 484–1 496 |
| `context`, repeats left out | 0.69 / 0.84 | 1.41 / 1.69 | 4.85 / 7.12 | 20 | 1 484–1 496 |
| `palace_walk` by distance, 2 hops | 0.95 / 1.15 | 2.62 / 3.07 | 8.17 / 9.81 | 20 | — |
| `palace_walk` by relevance, 2 hops | 2.94 / 3.42 | 3.49 / 4.21 | 16.26 / 19.35 | 20 | — |
| `palace_walk` by relevance, with a context | 3.41 / 4.22 | 4.08 / 4.68 | 16.99 / 24.87 | 20 | 1 486–1 497 |
| `palace_walk` by distance, 3 hops | 0.95 / 1.17 | 2.66 / 3.26 | 8.25 / 9.42 | 20 | — |
| `palace_enter` (a room of 50, 250, 1 000 notes) | 0.65 / 0.82 | 1.79 / 2.04 | 7.04 / 8.66 | 34–35 | 1 478–1 495 |

A walk costs about two recalls by distance: it reads every note anchored
in the rooms the topic names (1 000 a room at 20 000 notes) before it
follows relations. By relevance it gathers up to ten times its limit and
looks up a thousand text matches to rank them, about twice a walk by
distance. Three hops cost what two do here: the limit of 20 is reached
before the third. Leaving repeats out costs a tenth of a millisecond to
half a millisecond, a comparison of word sets per packed item. Entering
grows with the room, since every note in it is read to order them by
strength.

Since phase 9 each memory a walk returns carries its justification (what
supports it, contradicts it or replaced it: three reads of its relations)
and a walk's context packs related memories together. Measured again at
5 000 notes: by distance 3.03 / 3.35 ms (from 2.62 / 3.07), by relevance
3.81 / 5.08 (from 3.49 / 4.21), by relevance with a context 4.59 / 5.16
(from 4.08 / 4.68), the same memories returned.

## Questions that join four memories (`crates/grafeno-memory/examples/retrieval_quality.rs`)

200 incidents, each with its chain: incident `hit` service `depends_on`
store, `maintains` from a person, and three notes about the service that
share its words. Incidents are anchored in one room. The question names
only the incident: `who keeps the store behind the <word> outage`. "Whole
chain" is how often the four memories all come back; "chain found", the
share of the four found. Recall's limit is 20, budgets 1 500 tokens;
release build, in memory, one laptop, 2026-11-13.

| Way | Whole chain | Chain found | p50 ms | p95 ms | Tokens (median) |
| --- | ---: | ---: | ---: | ---: | ---: |
| `context`, recall at depth 1, rank fusion | 0 % | 27 % | 0.39 | 0.50 | 930 |
| `context`, recall at depth 2, rank fusion | 0 % | 28 % | 0.49 | 0.72 | 930 |
| `context`, recall at depth 3, rank fusion | 0 % | 28 % | 0.52 | 0.69 | 930 |
| `context`, recall at depth 3, scores scaled | 100 % | 100 % | 0.49 | 0.62 | 798 |
| `palace_walk` from the question's words, by relevance, 3 hops | 100 % | 100 % | 0.66 | 0.84 | 273 |
| `palace_walk` from the question's words, by distance, 3 hops | 100 % | 100 % | 0.53 | 0.60 | 273 |
| `palace_walk` from the incident, 3 hops | 100 % | 100 % | 0.16 | 0.20 | 273 |
| `palace_walk` from the incident, 3 hops, only `hit`, `depends_on`, `maintains` | 100 % | 100 % | 0.11 | 0.20 | 209 |

What it found:

- **Rank fusion hid the graph.** Recall spreads each seed's score to its
  neighbors. Under rank fusion the first match scores 1/61 and the
  fiftieth 1/110, so a neighbor of the best match (half its score at one
  hop) never outranks a weak match on a common word, and the limit fills
  with those: 28 % at any depth. With each search's scores scaled to 0..1
  the best match stands apart and the chain comes back whole. The MCP
  tools `recall` and `context` now scale scores by default (`fusion:
  "rank"` keeps the old order); the library's default is unchanged.
- **Walks by relevance needed the graph too.** Before this measure, a walk
  by relevance ranked by words alone and found 25 % of the chain from the
  question's words; a memory reached through relations now inherits half
  its origin's relevance per hop, which the rows above include.
- **Filters cost nothing and save tokens.** Following only the relations
  that matter is the fastest way here and packs a quarter fewer tokens.

The counts of tokens are the engine's estimate (about four characters a
token); the tools say so (`tokens_estimated`).

## Writes with vectors on the one writer (`crates/grafeno/examples/vector_writes.rs`)

A database file with immediate durability, on a laptop's NVMe SSD
(Windows 11), release build, 2026-11-12, before inserts were planned on a
snapshot (see the end of this section for after). Each write is a node with a
40-word text (from a 5 000-word vocabulary) and a 128-dimension vector
(64 clusters); four threads search the graph without pause meanwhile.
First what each part of a write costs, alone in its transaction and 200
to a transaction (as the group commit runs them under load):

| write | 10 000 vectors: alone, p50 ms | 200 to a transaction, ms each | 100 000 vectors: alone, p50 ms | 200 to a transaction, ms each |
| --- | --- | --- | --- | --- |
| empty commit | 0.86 | 0.00 | 0.88 | 0.00 |
| node | 1.57 | 0.04 | 1.75 | 0.04 |
| node + text | 6.69 | 0.57 | 8.80 | 1.54 |
| node + vector | 4.92 | 0.98 | 8.25 | 3.74 |
| node + text + vector | 10 | 2.24 | 15 | 4.78 |

Then 256 clients offering writes at a fixed rate (open load; latency from
the scheduled time):

| vectors | offered/s | done/s | p50 ms | p95 ms | p99 ms | writes/commit | search p50 / p99 ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 10 000 | 100 | 99 | 36 | 50 | 52 | 2.2 | 0.59 / 0.97 |
| 10 000 | 250 | 226 | 338 | 501 | 532 | 39.5 | 0.61 / 1.00 |
| 10 000 | 500 | 402 | 676 | 984 | 1 067 | 136.4 | 0.62 / 1.01 |
| 10 000 | 1 000 | 415 | 2 327 | 4 257 | 4 374 | 200.0 | 0.67 / 1.08 |
| 100 000 | 100 | 88 | 445 | 685 | 745 | 17.6 | 2.19 / 3.35 |
| 100 000 | 250 | 152 | 1 904 | 2 673 | 2 793 | 107.1 | 2.19 / 2.97 |
| 100 000 | 500 | 154 | 4 125 | 6 855 | 6 974 | 166.7 | 2.18 / 3.08 |

What this says:

- **The commit and the record are not the cost.** Grouped, a commit is
  paid once per group and a node's record is 0.04 ms. The writer's time
  goes to the indexes: the vector graph first (an insert searches the
  graph with `ef_construction` 100 and rewrites its neighbors' links), the
  text's postings second. Both grow with what is indexed: the vector part
  from 1.0 to 3.7 ms a write between 10 000 and 100 000 vectors.
- **So the one writer tops out early with vectors**: about 400 writes a
  second at 10 000 vectors and 150 at 100 000, against 11 000 small
  writes (above). Past about half that rate, writes wait in line and
  latency climbs into seconds; none was refused (the queue holds 4 096).
- **Searches do not suffer**: they read snapshots, at the same latency
  under any write load.
- 1 000 000 vectors was not measured here: filling the graph alone takes
  over an hour (see *Memory under a large graph*), and the trend is
  already clear.

Indexing from the change log, after the commit, would make writes return
at the cost of their record and text, but the vectors would still be
inserted by the one writer (in the bulk lane), so they would fall behind
by as much as they now wait; searches would miss the newest vectors for
that long. What raises the ceiling is taking the graph search out of the
writer, which is what the engine now does:

### Inserts planned on a snapshot

A vector written in a transaction is stored at once and joins the graph
when the transaction commits. When a commit brings several to a space (a
group commit, a batch, a background index build), each one's neighbors
are searched on a read snapshot of what the transaction started from, on
as many threads as the machine has, against the snapshot's graph and the
other new vectors (compared one by one); the writer then writes the link
lists, working out again only those that changed since the snapshot (a
neighbor two new vectors both chose, a list a removal repaired). The
graph depends only on the vectors and their order, so the members of a
cluster that apply the same entry build the same one.

The same benchmark, the version before and after run alternately on the
same machine (2026-11-12), writes of a node, a 40-word text and a
128-dimension vector:

| vectors | | vector, ms each in groups of 200 | node + text + vector, ms each | writes/s at most | p50 ms at 100/s | p50 ms at 250/s |
| --- | --- | --- | --- | --- | --- | --- |
| 30 000 | before | 1.60 | 2.10 | about 350 | 28 | 399 |
| 30 000 | planned | 0.72 | 1.07 | about 500 | 22 | 213 |
| 100 000 | before | 3.16 | 4.11 | about 210 | 189 | 1 197 |
| 100 000 | planned | 1.26 | 1.65 | about 375 | 44 | 417 |

Searches kept their latency in both (0.7 ms at 30 000 vectors, 1.5 ms at
100 000). A write alone in its transaction costs what it did: planning
starts at two new vectors. Building is faster too: `hnsw_recall` inserts
its 20 000 vectors (in transactions of 1 000) in 4.9 s instead of about
9 s, with the same recall@10 (0.951 / 0.996 / 1.000 at `ef` 16 / 32 /
64), and `hnsw_churn` keeps recall@10 at `ef` 64 at 0.999 or above
through all its rounds. What is left of a vector write on the writer is
writing about 17 link lists per layer and checking the few that moved;
past this ceiling, send writes in batches (`/batch`, `/ingest`,
`?bulk=true`), keep vectors in their own namespace or group when a
cluster has several, or give write-heavy vector indexes a lower
`ef_construction` (64 kept recall@10 at the same level on 20 000
vectors, above).

## Record encodings (`crates/grafeno/examples/record_formats.rs`)

Nodes and edges are stored as JSON. A database file with 50 000 nodes (two
labels, seven properties: strings, numbers, a boolean, a list, a date
string) and 200 000 edges (a label, two properties, a valid time), its
pages cached; release build, 2026-11-12. Allocations come from a counting
allocator.

| path | µs each | allocations each |
| --- | --- | --- |
| a node by id (`ReadTx::node`) | 2.21 | 18 |
| its bytes alone, fetched from the file without decoding | 1.22 | |
| an edge by id | 2.14 | 6 |
| its bytes alone | 1.32 | |
| neighbors with their edge records, per neighbor | 3.24 | 8 |
| neighbor ids (adjacency only), per neighbor | 0.48 | 1 |
| k-hop depth 2 (adjacency only), per node reached | 2.08 | |
| k-hop depth 2, each node reached read too | 3.65 | 20 |

So decoding is about half of reading a record by id, and nothing at all
for the paths that only need ids. The same records, decoded and encoded
in four ways:

| records | encoding | bytes each | decode µs | decode allocations | encode µs |
| --- | --- | --- | --- | --- | --- |
| nodes | JSON (today) | 294 | 1.47 | 17 | 0.97 |
| nodes | CBOR (serde, field names kept) | 197 | 2.46 | 17 | 1.02 |
| nodes | binary header for the fixed fields + JSON properties | 187 | 1.26 | 17 | 0.66 |
| nodes | JSON, fixed fields only (properties skipped) | 294 | 0.58 | 3 | |
| edges | JSON (today) | 306 | 0.93 | 5 | 0.77 |
| edges | CBOR | 184 | 1.17 | 5 | 0.82 |
| edges | JSON, fixed fields only | 306 | 0.62 | 1 | |

- **The format is not where the time goes.** CBOR is a third smaller but
  slower to decode with the serde implementation at hand, and a binary
  header for the ids, labels and times saves 0.2 µs a node: ids and
  timestamps are cheap in either form.
- **Building the properties is.** Skipping them cuts a node's decode from
  1.47 to 0.58 µs and its allocations from 17 to 3: every property becomes
  a heap string key and a heap value in a map.
- **Recommendation: keep JSON, and decode properties lazily** — keep a
  record's properties as their raw JSON until something reads them, and
  then only the keys it reads (most traversals and many queries touch
  none or one). The bytes on disk stay the same, so no layout migration or
  rollback path is needed, and the gain is about the 0.9 µs a node the
  table shows, half of a read by id. A binary encoding is worth
  revisiting only together with reading single properties without
  building the map, which JSON does not allow cheaply; it would need a new
  layout version, a migration that rewrites every record (with the copy
  kept before an upgrade) and decoding both formats during the change.

## Retrieval quality on agent questions (`crates/grafeno-memory/examples/recall_quality.rs`)

```bash
cargo run --release -p grafeno-memory --example recall_quality
```

80 memories an agent keeps over six projects, in Spanish and English,
with the relations between them, and 70 questions of four kinds: the same
words as the answer, a paraphrase, the other language, and one relation
away from what the question names (`crates/grafeno-memory/data/recall`).
Embeddings come from a small multilingual model run locally, stored with
the fixtures (`tools/fixtures/make-recall-fixtures.mjs`), so every run
gives the same numbers. Recall without importance or recency, the first
ten results; "stemmed" is the namespace with `language` `es`. Release
build, 2026-11-20.

| Way | Questions | First | In 3 | In 10 | ms a question |
| --- | --- | ---: | ---: | ---: | ---: |
| BM25 | all (70) | 44% | 70% | 79% | 0.02 |
| BM25 | words (20) | 100% | 100% | 100% |  |
| BM25 | paraphrase (20) | 30% | 60% | 75% |  |
| BM25 | crosslingual (15) | 13% | 40% | 47% |  |
| BM25 | multihop (15) | 20% | 73% | 87% |  |
| BM25, stemmed (es) | all (70) | 47% | 67% | 77% | 0.03 |
| BM25, stemmed (es) | words (20) | 100% | 100% | 100% |  |
| BM25, stemmed (es) | paraphrase (20) | 35% | 60% | 70% |  |
| BM25, stemmed (es) | crosslingual (15) | 20% | 33% | 47% |  |
| BM25, stemmed (es) | multihop (15) | 20% | 67% | 87% |  |
| HNSW | all (70) | 63% | 86% | 96% | 0.17 |
| HNSW | words (20) | 100% | 100% | 100% |  |
| HNSW | paraphrase (20) | 60% | 90% | 100% |  |
| HNSW | crosslingual (15) | 60% | 80% | 87% |  |
| HNSW | multihop (15) | 20% | 67% | 93% |  |
| hybrid | all (70) | 61% | 89% | 97% | 0.18 |
| hybrid | words (20) | 100% | 100% | 100% |  |
| hybrid | paraphrase (20) | 55% | 90% | 100% |  |
| hybrid | crosslingual (15) | 53% | 80% | 93% |  |
| hybrid | multihop (15) | 27% | 80% | 93% |  |
| hybrid + graph | all (70) | 61% | 89% | 99% | 0.19 |
| hybrid + graph | words (20) | 100% | 100% | 100% |  |
| hybrid + graph | paraphrase (20) | 55% | 85% | 100% |  |
| hybrid + graph | crosslingual (15) | 53% | 80% | 93% |  |
| hybrid + graph | multihop (15) | 27% | 87% | 100% |  |
| hybrid + graph, stemmed | all (70) | 60% | 89% | 99% | 0.19 |
| hybrid + graph, stemmed | words (20) | 100% | 100% | 100% |  |
| hybrid + graph, stemmed | paraphrase (20) | 55% | 85% | 100% |  |
| hybrid + graph, stemmed | crosslingual (15) | 47% | 73% | 93% |  |
| hybrid + graph, stemmed | multihop (15) | 27% | 93% | 100% |  |

Words alone find what shares words and little else; meaning (HNSW) finds
paraphrases and the other language; the hybrid keeps both. Graph
expansion adds the answers one relation away: 87 % in the first three
against 80 %, and 99 % in the first ten. Before 2026-11-20 it cost the
first place (61 % to 36 %): fused scores are close to one another, and a
weak match next to the best one overtook it; a node the graph reaches now
rises up to just below the memory that led to it. Spanish stemming helps
words in Spanish ("decidimos" finds "decidir"); in this mixed corpus it
matters little beside the vectors. `tests/recall_quality.rs` holds these
as floors.

## Old decisions among thousands of conversations (`crates/grafeno-memory/examples/long_term.rs`)

```bash
cargo run --release -p grafeno-memory --example long_term -- 1
cargo run --release -p grafeno-memory --example long_term -- 5
```

3 000 conversations over two years (15 000 with `5`, each copy a little
changed), 52 decisions among them made in the first months, 12 replaced a
year later by a newer one (`supersedes`), and a question about each
decision in other words. Each memory is as old as it is, so recency ranks
the recent chatter up; much of it names the same topics ("someone asked
about the mail provider; nothing decided"). Recall as an agent calls it
(hybrid, graph, importance and recency), the first ten, and the context
of 600 tokens. Release build, 2026-11-20.

3 000 conversations:

| Way | First | In 3 | In the context (600 tokens) | New ahead of replaced | ms a recall |
| --- | ---: | ---: | ---: | ---: | ---: |
| BM25 | 18% | 32% | 38% | 42% | 0.13 |
| HNSW | 12% | 38% | 48% | 75% | 0.96 |
| hybrid + graph (default) | 18% | 38% | 55% | 75% | 1.02 |
| hybrid + graph, relevance only | 30% | 42% | 55% | 75% | 0.87 |
| hybrid + graph, decisions only | 78% | 82% | 95% | 100% | 1.24 |
| hybrid + graph, decisions important | 28% | 48% | 58% | 75% | 1.01 |

15 000 conversations:

| Way | First | In 3 | In the context (600 tokens) | New ahead of replaced | ms a recall |
| --- | ---: | ---: | ---: | ---: | ---: |
| BM25 | 18% | 30% | 32% | 42% | 0.27 |
| HNSW | 12% | 15% | 28% | 42% | 1.41 |
| hybrid + graph (default) | 18% | 30% | 40% | 58% | 1.66 |
| hybrid + graph, relevance only | 22% | 38% | 42% | 58% | 1.39 |
| hybrid + graph, decisions only | 65% | 80% | 95% | 100% | 2.23 |
| hybrid + graph, decisions important | 25% | 38% | 42% | 58% | 1.66 |

Asked as a question about decisions (`labels: ["Decision"]`), the right
one is in the context 95 % of the time, as many conversations as there
are, and a new decision ranks ahead of the one it replaced every time.
Recall searches within the labels it is given (their own best
candidates join the lists): before that, it filtered the best 50 of
everything, and the share fell to 72 % with 3 000 conversations and 55 %
with 15 000. Without the label, chatter on the same topic competes, and
the more of it there is, the more often it wins; marking decisions
important (0.8) helps a little. `tests/long_term.rs` holds the floor.

## Many agents writing at once (`crates/grafeno/examples/many_writers.rs`)

```bash
cargo run --release -p grafeno --example many_writers -- 4
```

Agents saving memories as fast as they can, each a text, a 384-number
embedding and an edge, with immediate durability, through the
group-commit writer: every agent on one database file, or spread over 2
and 4 files with a writer each (as namespaces kept in separate files
would be). 16 CPUs, NVMe, Windows 11, 4 s a step, 2026-11-20.

| Agents | Files (writers) | Memories/s | p50 ms | p99 ms |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 1 | 215 | 4.5 | 7.0 |
| 8 | 1 | 549 | 12.6 | 27.2 |
| 8 | 2 | 505 | 16.8 | 22.2 |
| 8 | 4 | 532 | 15.4 | 22.3 |
| 32 | 1 | 1 094 | 29.0 | 69.0 |
| 32 | 2 | 1 328 | 22.8 | 52.8 |
| 32 | 4 | 1 448 | 18.2 | 58.8 |
| 128 | 1 | 1 660 | 76.9 | 151.1 |
| 128 | 2 | 2 101 | 56.5 | 157.8 |
| 128 | 4 | 2 864 | 43.5 | 105.1 |

One writer takes about 1 100 memories a second from 32 agents writing
without pause, and 1 660 from 128. An agent saves a memory every few
seconds, so one writer serves hundreds at once. More writers help only
past dozens of agents writing without pause, and less than in
proportion (1.7 times with four): the disk's flush is shared.

## Recovery drills (`crates/grafeno-server/tests/recovery.rs`)

```bash
cargo test -p grafeno-server --test recovery
```

On real `grafeno serve` processes while an agent keeps writing memories:
the server is killed without warning and started again (the file checks
clean, every acknowledged memory is there, recall finds them); its file
is then damaged (the check refuses it) and the backup taken over HTTP
while the agent wrote restores into a new file serving every memory
acknowledged before it; and the leader of a three-member cluster is
killed while memories come through the members (a new leader takes the
writes; once the old one is back as a follower, every acknowledged
memory is on every member). About 6 s for both.

## Remaining limits

- **Vector search reads each visited vector from redb** (about 0.4 µs per
  B-tree lookup). At 20k vectors that is most of a search's time; an
  in-memory copy of a namespace's vectors would remove it, at the cost of
  memory and of keeping it in sync across transactions.
- **Single small transactions** pay for page copies and the commit; clients
  should batch.
- **Edge filters by label alone** (`edges_page` with a label) scan the
  namespace's edges.

## Hyperedges (`crates/grafeno/examples/hyperedge_bench.rs`)

A file with 10 000 nodes; for each size, 5 000 hyperedges whose
participants are distinct random nodes (one `lead`, the rest `member`),
half labeled `even`, half `odd`. Writes go in transactions of 500 with
eventual durability (no fsync per commit); reads are single calls on one
snapshot, timed one by one. Release build, Windows 11 development machine,
2026-11-21, one run. Binary edges, created the same way between random
nodes: 43 263 a second (23.1 µs each), 447 bytes each for the record and
both adjacency entries.

| Participants | Create /s (µs each) | By id p50 / p99 µs | A node's hyperedges p50 / p99 µs | A label's page of 100 p50 / p99 µs | Contains 2 nodes p50 / p99 µs | Exact p50 / p99 µs | Replace one participant /s | Delete /s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 3 | 54 687 (18.3) | 3.3 / 6.0 | 6.3 / 13.6 | 159 / 184 | 8.6 / 12.4 | 10.9 / 14.9 | 55 712 | 68 388 |
| 10 | 21 354 (46.8) | 4.6 / 5.6 | 18.9 / 38.3 | 281 / 287 | 10.8 / 18.6 | 34.4 / 44.9 | 26 381 | 30 363 |
| 50 | 4 443 (225) | 10.5 / 15.6 | 238 / 423 | 916 / 1 162 | 22.6 / 39.9 | 303 / 461 | 7 059 | 6 440 |
| 100 | 2 051 (488) | 18.1 / 32.5 | 828 / 1 466 | 1 605 / 1 771 | 38.5 / 103 | 919 / 1 170 | 3 781 | 2 878 |

Since 2026-11-22 a record is binary but for its properties (ids as 16
bytes, roles and times inline; `crates/grafeno/src/hyperedge_codec.rs`).
The same benchmark, the commit before and after alternated twice (means):

| Participants | Record bytes before / after | By id p50 µs | A node's hyperedges p50 µs | A label's page of 100 p50 µs | Create /s |
| --- | --- | --- | --- | --- | --- |
| 3 | 409 / 131 | 1.65 / 0.90 | 4.6 / 3.9 | 161 / 82 | 38 089 / 37 813 |
| 10 | 865 / 294 | 2.9 / 1.1 | 18.5 / 9.5 | 292 / 120 | 16 313 / 15 833 |
| 50 | 3 466 / 1 229 | 9.5 / 3.2 | 246 / 85 | 911 / 308 | (one run) |
| 100 | 6 666 / 2 416 | 25.4 / 5.8 | 876 / 289 | 1 691 / 561 | (one run) |

Writes cost the same; reads that return many records gain most, as they
spent their time decoding participants. The tables below are from the
JSON records.

Bytes stored with B-tree metadata, per hyperedge:

| Participants | `hyperedges` | `hyperedge_incidence` | `hyperedge_labels` |
| --- | --- | --- | --- |
| 3 | 409 | 143 (48 a participant) | 31 |
| 10 | 865 | 477 | 31 |
| 50 | 3 466 | 2 387 | 31 |
| 100 | 6 666 | 4 774 | 31 |

A three-node hyperedge costs about what one binary edge does, to write and
to store, and holds what three edges and a node would. Cost grows with the
participants, as each needs an incidence entry and its part of the record.
The reads that return many records (a node's hyperedges, where each node
of the 100-participant run is in about 50; a label's page; exact matches,
which read every hyperedge the first nodes share) spend their time decoding
JSON records with many participants: the incidence ranges themselves are
short. "Contains" with two nodes stays under 40 µs at p50 because the
intersection leaves few records to read. If those reads matter, the next
step is a binary encoding of participants or ids-only reads, measured
against these numbers.

## Hyperedges by a property (`crates/grafeno-query/examples/hyperedge_index_bench.rs`)

100 000 hyperedges labeled `execution`, three participants each among
1 000 agents, with a unique `execution_id`, one of 100 `project_id`s and
one of three statuses; beside them the same 100 000 events as `Execution`
nodes. Each query runs 50 times without indexes and 300 times with them,
a different value each time; reads come from `PROFILE`. Release build,
in memory, 2026-11-21, one run ([data](data/hyperedge_index.csv)).

| Query | Without index p50 / p95 / p99 | Reads | With index p50 / p95 / p99 | Reads | Rows |
| --- | --- | --- | --- | --- | --- |
| Hyperedge by `execution_id` | 1 247 / 1 420 / 1 432 ms | 500 000 | 52 / 85 / 142 µs | 5 | 1 |
| Hyperedge by `project_id` and `status` (~0.3%) | 1 163 / 1 279 / 1 359 ms | 500 000 | 4.3 / 6.2 / 8.2 ms | 1 670 | 334 |
| Event node by `execution_id` | 45 / 48 / 49 ms | 200 000 | 64 / 294 / 468 µs | 2 | 1 |
| Event node by `project_id` and `status` | 46 / 54 / 58 ms | 200 000 | 2.5 / 3.6 / 4.0 ms | 668 | 334 |

Building both hyperedge indexes over the 100 000 took 1.0 s. Writing
10 000 hyperedges ran at 26 557 a second without the two indexes and
16 731 with them. Without an index a lookup reads every hyperedge of the
label, decoding each JSON record (five reads a hyperedge in the profile),
which was 27 times slower than the same scan over nodes (since 10 times:
see the next section); with one it reads what it returns. A composite index fetches each matching record: 334 rows
cost 4.3 ms, against 2.5 ms for nodes.

The first run of this benchmark found a bug: with an index on hyperedges,
the planner's statistics counted it but did not hold it, so they looked
stale and were rebuilt on every query (about 70 ms each, for every
indexed query of the namespace). Fixed, with a test, before these numbers.

## Reading hyperedges without an index (`crates/grafeno-query/examples/hyperedge_scan_bench.rs`)

100 000 hyperedges of three participants among 1 000 agents, and 100 000
nodes beside them; the best of five runs. Release build, in memory,
2026-11-22, one run per build, the three builds alternated on one machine.

| Read | Before | Tables opened once | And participants read on demand | And binary records, `WHERE` as rows are made |
| --- | --- | --- | --- | --- |
| Every hyperedge by id | 3.31 µs each | 1.68 µs | 1.65 µs | 1.02 µs |
| Every hyperedge in pages of 1 000 | 1.30 µs | 1.32 µs | 1.30 µs | 0.67 µs |
| `MATCH HYPEREDGE h:execution WHERE h.execution_id = …` | 9.54 µs a hyperedge | 7.92 µs | 3.69 µs | 1.99 µs |
| The same scan over nodes | 0.37 µs a node | 0.37 µs | 0.39 µs | 0.37 µs |

The last column was measured as an A/B of two release builds (the commit
before and after), run alternately twice: the figures are the means.

Two costs had nothing to do with hyperedges themselves: each read of one
opened two tables (a snapshot now opens the three hyperedge tables once,
as it does those of nodes), and matching a pattern read every
participant's node, even when the pattern named none (now only the nodes
of participants a member may take, when it is first needed). Then two
that were about them: the record held its participants as JSON (ids as
text), and `MATCH HYPEREDGE` made every row before its `WHERE` dropped
them. Records are now binary but for their properties (see the hyperedge
section above), and the `WHERE` runs as each row is made. A label scan is
five times a node scan, from 26; the rest is fetching each record by id
and making its row, and an index is still the answer for lookups by
property.

A participant in 300 hyperedges looked up with a key (`MATCH (a:Agent {n:
77}) MATCH HYPEREDGE h:execution [(a)] WHERE h.execution_id = …`): 2.81
ms by its incidence list, 1.33 ms once the key has an index, because the
match then reads the one record the index finds instead of 300 (most of
what is left is finding the agent, by scanning its label).

## Hyperedges in recall: an experiment (`crates/grafeno-memory/examples/hyperedge_recall_eval.rs`)

Whether recall should return hyperedges. One synthetic team memory (12
agents, 24 tasks, 48 evidence documents, 200 unrelated notes) holds the
same events twice: as hyperedges with roles, and as `Event` nodes with one
edge per role. 46 questions per language (English and Spanish, with the
namespace's language set): who proposed and reviewed a decision (8),
which agents ran a job (8), who attended a session (8), what backs a
decision and what contradicts it (6), which version held at a date (6),
and single facts that need no event (10). The same recall settings (10
results, 50 candidates, one hop) and a budget of 400 tokens, three
quarters for recalled lines and the rest for facts among them. No
embeddings (no strategy has a model), so recall is deterministic: runs
differ only in latency. Release build, 2026-11-21
([data](data/hyperedge_recall.csv), [latency](data/hyperedge_recall_latency.csv)).

- **A**: today's recall on the hyperedge memory, which does not see them.
- **B**: A, then every hyperedge of each recalled node (five a node).
- **C**: A, then only hyperedges sharing two recalled nodes or a word
  with the question, and of at most 16 participants.
- **D**: today's recall on the event-node memory.
- **D2**: D with B's expansion: each recalled node packed with its
  outgoing facts inline (an event node with its participants and roles).

"Complete": every node the answer needs, and the event, in the context.
"In role": the node asked for appears in the role asked ("who proposed").
"Stray": participants named that the answer does not need. English;
Spanish is within a few points (one session question fewer complete for
B, as its five hyperedges a node ran out first).

| Kind | A complete / in role | B | C | D | D2 | Tokens A / B / C / D / D2 | Stray B / C / D2 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Decisions | 0% / 0% | 100% / 100% | 100% / 100% | 0% / 12% | 100% / 100% | 287 / 284 / 284 / 336 / 330 | 25.6 / 7.2 / 11.0 |
| Multi-agent runs | 0% / 0% | 100% / 100% | 100% / 100% | 0% / 0% | 100% / 100% | 166 / 283 / 281 / 329 / 338 | 26.8 / 8.2 / 11.2 |
| Sessions | 0% / 0% | 100% / 100% | 100% / 100% | 0% / 0% | 100% / 100% | 166 / 269 / 281 / 296 / 321 | 25.1 / 7.2 / 13.4 |
| Contradictions | 0% / 0% | 100% / 100% | 100% / 100% | 100% / 100% | 100% / 100% | 122 / 272 / 272 / 357 / 342 | 9.2 / 9.2 / 11.2 |
| At a date | 0% / 0% | 100% / 100% | 100% / 100% | 17% / 17% | 100% / 100% | 161 / 285 / 285 / 352 / 365 | 10.0 / 10.0 / 14.2 |
| Single facts | 100% | 100% | 100% | 100% | 100% | 246 / 274 / 275 / 246 / 246 | 12.4 / 6.5 / 0 |

MRR of the first useful line on collective questions: about 0.3 for B and
C (the event comes after the task or report that led to it), 1.0 for D
and D2 on most kinds (the event node's own text matches the question).
Latency p50 / p95 / p99: A 53 / 115 / 141 µs, B 160 / 562 / 655, C 160 /
570 / 622, D 286 / 425 / 522, D2 326 / 445 / 664.

What it says:

- Expanding events is what answers collective questions: today's recall
  finds the task or the event but rarely everyone in it (A, D).
- The same expansion over event nodes (D2) answers as completely as over
  hyperedges (B, C), ranks the event first, and names fewer strays than B,
  for about 15% more tokens and still well under a millisecond. Hyperedges
  bring no gain in recall that event nodes do not.
- Single facts lose nothing with any strategy; B and C add 30 tokens of
  hyperedges to each.
- Safety: expansion goes through the snapshot's view (a hyperedge with a
  hidden participant is not found), leaves retracted ones out, and bounds
  a hyperedge of 151 participants to one line of 129 tokens. Text in a
  hyperedge's properties enters the context verbatim, as node text does:
  instructions written there reach the model as data the caller must
  treat as such.

So recall and context stay as they are: by the criteria set before
measuring, an equivalent result with less complexity (event nodes, already
supported) rules out the default integration. The candidate the data does
point to is independent of hyperedges: packing a recalled node's outgoing
facts inline, measured on real memories first. Limits: one synthetic
memory from one generator, no embeddings, and "in role" is checked from
the structure, not by a model reading the answer.

### At scale, and with word vectors

The same generator with more events, 2026-11-22. A number argument
writes that many decisions, runs and sessions per topic, one per sprint
(each summary names its sprint, "sprint Vega"), and the collective
questions ask about one sprint ("Sprint Lyra: who proposed…"); the
contradictions, the versions at a date and the single facts stay as
they were, now among many more events. `vectors` gives every node, event
node and question a hashed bag-of-words vector of 256 dimensions: not
semantic (two texts are near only when they share words), the same for
every strategy, so recall fuses text and vector ranks as it would with a
model. A sixth strategy joins:

- **C2**: A, then the five best hyperedges of all the recalled nodes,
  ranked by the words each shares with the question, then the shorter
  text, then the recalled nodes it holds; packed after the first
  recalled node.

C2 was written after the first runs at 10 events per topic showed B and
C failing, and its order was changed twice on what those runs showed
(recalled nodes first, then text length first): it is fitted to this
generator, an upper bound for expanding hyperedges, not a measure of a
strategy picked blind.

```
cargo run --release -p grafeno-memory --example hyperedge_recall_eval -- 10 vectors
```

"Complete" on English questions, in percent: collective questions
(decisions, runs and sessions together, 24) / at a date (6) / single
facts (10). Spanish is within a few points, except single facts with
vectors (70 to 90% for every strategy, A included: the vectors of short
Spanish questions pull in other agents) ([data](data/hyperedge_recall_scale.csv),
[latency](data/hyperedge_recall_scale_latency.csv)).

| Events per topic | Vectors | A | B | C | C2 | D | D2 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | no | 0 / 0 / 100 | 100 / 100 / 100 | 100 / 100 / 100 | 100 / 100 / 100 | 0 / 17 / 100 | 100 / 100 / 100 |
| 1 | yes | 0 / 0 / 100 | 100 / 100 / 60 | 100 / 100 / 100 | 100 / 100 / 100 | 0 / 0 / 100 | 100 / 100 / 100 |
| 10 | no | 0 / 0 / 100 | 0 / 0 / 100 | 0 / 0 / 100 | 100 / 100 / 100 | 0 / 0 / 100 | 96 / 100 / 100 |
| 10 | yes | 0 / 0 / 100 | 0 / 0 / 60 | 0 / 0 / 100 | 100 / 100 / 100 | 0 / 0 / 100 | 100 / 100 / 100 |
| 30 | no | 0 / 0 / 100 | 0 / 0 / 100 | 0 / 0 / 100 | 100 / 100 / 100 | 0 / 0 / 100 | 96 / 100 / 100 |
| 30 | yes | 0 / 0 / 100 | 0 / 0 / 60 | 0 / 0 / 100 | 100 / 100 / 100 | 0 / 0 / 100 | 100 / 100 / 100 |

Latency p50 / p95 in µs, English, without vectors (vectors add about
300 µs to every strategy, the vector search):

| Events per topic | A | B | C | C2 | D | D2 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 45 / 94 | 111 / 292 | 108 / 328 | 118 / 370 | 258 / 371 | 291 / 448 |
| 10 | 48 / 99 | 177 / 2 118 | 180 / 2 135 | 254 / 3 379 | 617 / 1 437 | 633 / 1 442 |
| 30 | 48 / 96 | 360 / 6 577 | 386 / 6 662 | 592 / 11 168 | 1 296 / 2 156 | 1 353 / 2 160 |

What it adds:

- **Crowding breaks expansion that does not choose.** With ten events
  per topic a task takes part in 30 hyperedges or more; B and C take
  the first five of each recalled node, which are rarely the sprint
  asked about, and answer no collective question and no question at a
  date. The token budget is spent on the wrong events.
- **Choosing needs a ranking of its own.** C2 answers everything, but
  only by reading every hyperedge of every recalled node and ranking
  them with a text measure written for this purpose; its p95 grows with
  the events (3.4 ms at 10 per topic, 11 ms at 30), above D2's (1.4 and
  2.2 ms).
- **Event nodes need nothing new.** D2 holds at 96 to 100%: the event
  node is a node, so the existing text search (and vector search) ranks
  it among the rest and its summary decides; the one question it misses
  at 10 and 30 without vectors is a run, where other sprints' runs fill
  the budget first. D without expansion still answers no collective
  question.
- **Hashed vectors change little**, as expected from vectors that only
  repeat the words: B loses four single facts in ten (the answer is no
  longer the first node recalled, and the five hyperedges added after
  each node before it use up the budget), the others hold.

The conclusion stands and is firmer: the default stays as it is. Making
recall expand hyperedges well would mean a second ranking beside the
one recall has, at a higher latency, to reach what event nodes reach
with the ranking that exists. Limits as above, plus: sprints differ in a
single word, a sharper test than real memories may pose; the vectors are
not a model's; and C2 is tuned on the data it is scored on.

## CPU per request under concurrent users (`tools/bench-http`)

Measured in containers, 2026-11-23: the server's CPU per request with 1,
16 and 64 closed-loop users, on 100,000 nodes, for the published 0.5.1
image (musl's allocator), the same build with mimalloc, and a glibc build
([data](data/http_allocator.csv); the method and the full table are in
tools/bench-http/README.md).

| CPU per request (ms), 1 / 16 / 64 users | musl (0.5.1) | musl + mimalloc | glibc |
| --- | ---: | ---: | ---: |
| `GET /v1/namespaces` | 0.57 / 2.45 / 1.76 | 0.54 / 0.62 / 0.55 | 0.51 / 0.58 / 0.54 |
| GQ `MATCH` by code (indexed) | 0.43 / 1.53 / 1.26 | 0.42 / 0.45 / 0.46 | 0.40 / 0.46 / 0.42 |

musl's allocator serializes allocations across threads, so every request
that allocates (a snapshot, JSON) got dearer as users were added; the
Linux builds now link mimalloc, which keeps the cost flat, as glibc does.

## Full-text search on the ICD-11 catalogue (2026-11-24)

An outside benchmark against another search engine, on the WHO's ICD-11
in Spanish (MMS: 37,211 entities; index over title, terms and context with
weights 5, 3 and 0.5, `spanish` analyzer), sends each search as a phrase,
its words and a fuzzy variant of each word of four letters or more
(`"fractura hueso" fractura hueso fractura~2 hueso~1`). Measured in
containers with that benchmark's engine phase (closed-loop users calling
`db.searchIndex(..., {limit: 25, fragments: false})` with 400 realistic
queries), the same machine and data:

| `db.searchIndex`, 1 / 16 / 64 users | requests per second | CPU per query, 1 user |
| --- | ---: | ---: |
| 0.5.2 | 41 / 284 / 288 | 20 ms |
| after (term lists per commit, scoring without copies) | 81 / 426 / 310 | 9 ms |
| the other engine, same run | 101 / 506 / 541 | 11 ms |

Single-word queries cost little once their entries are read: "sistema"
(13,715 matches) answers 25 results in about 6 ms. What remains is
proportional to the matches of common words, every one of which is
scored; the other engine skips most of them with per-block bounds, which
explains the gap with 64 users.

## Entities by their properties (`crates/grafeno-query/examples/entity_search.rs`)

1,000,000 `Person` nodes on a file (E1-disk), with `name` (strings),
`country` (20 codes, `PE` one in 16) and `age`; each query through GQ
under the server's limits, median of five runs, before and after the
lookup work of this section (`docs/data/entity_search.csv`):

| Query | Indexes | Before | After |
| --- | --- | ---: | ---: |
| `PE` by name, first 20 | none | 658 ms | 478 ms |
| everyone by name, first 20 | none | fails (`max_memory`) | 1,418 ms |
| `PE` by name, first 20 | `country` | 225 ms | 103 ms |
| `PE` by name, first 20 | `country`, `name` | 327 ms | 1.4 ms |
| `PE` by name, rows 10,001 to 10,020 | `country`, `name` | 275 ms | 150 ms |
| everyone by name, first 20 | `name` | fails (`max_memory`) | 0.13 ms |
| everyone by name descending, first 20 | `name` | fails (`max_memory`) | 0.15 ms |
| `PE` by name, first 20 | `(country, name)` | 182 ms | 0.12 ms |
| `PE` aged 30, by name | `(country, name)` | 40 ms | 2.5 ms |
| `PE` by name, rows 10,001 to 10,020 | `(country, name)` | 193 ms | 57 ms |
| `find_nodes`: every `PE` person, 100 a page (625 pages) | `(country, name)` | (no API) | 0.56 ms a page |

What changed: a sort with a `LIMIT` keeps only the rows it returns while
it matches (it held every match: a million rows went over `max_memory`);
a sort by an indexed property, after equal values for the index's first
ones, reads the index in order and stops; and `POST /v1/{ns}/nodes/find`
(`ReadTx::find_nodes`) pages with a cursor, so the last page costs what
the first does. Reading in order is chosen when it reads fewer entries
than sorting the candidates of the best other index: with only `name` and
`country` indexed, rows 10,001 to 10,020 of `PE` would read 160,000 name
entries (763 ms), so the `country` index and a sort are used instead. A
count of the `PE` people, which needs every match, is unchanged.

While measuring, a planner fault came up: an index on `(last, first)`
served `last = 'Ruiz'` alone and missed the nodes without a `first`.
A composite index now serves only conditions on every one of its
properties (or a key constraint's).

## Rewiring edges in bulk (`crates/grafeno-query/examples/rewire.rs`)

1,000,000 `Person` nodes each with a `visits` edge to one of 1,000
`Paciente` nodes, on a file (E1-disk); every edge is moved to the `Prueba`
of the same code, keeping the old one as history (a new edge, the old one
ended with `SET r.@valid_to`), through the writer as the server runs it.
Meanwhile an agent writes a memory every 20 ms (interactive lane) and a
reader looks a node up through an index every 20 ms
(`docs/data/rewire.csv`):

| Rewiring | People | Seconds | Edges/s | Agent p50 / p99 / max | Reader p99 / max |
| --- | ---: | ---: | ---: | --- | --- |
| none (agent and reader alone) | | | | 2.4 / 3.8 / 4.5 ms | 0.3 / 519 ms |
| one statement, one transaction | 200,000 | 19.7 | 10,165 | 2.3 ms / 19.7 s / 19.7 s | 0.3 / 289 ms |
| `IN TRANSACTIONS OF 5000 ROWS`, snapshot open throughout (first version) | 1,000,000 | 143.5 | 6,971 | 1.9 ms / 888 ms / 14.3 s | 0.7 ms / 15.3 s |
| `IN TRANSACTIONS OF 5000 ROWS` | 1,000,000 | 185.6 | 5,388 | 1.9 ms / 1.5 s / 1.7 s | 112 ms / 2.7 s |
| `IN TRANSACTIONS OF 1000 ROWS` | 1,000,000 | 346.6 | 2,885 | 273 / 802 / 1,077 ms | 133 ms / 3.5 s |

One transaction holds the writer for the whole run: every interactive
write waits until it ends (19.7 s for 200,000 people; a million would
also hold every row in memory). In batches, an interactive write waits
at most for the batch being committed. The first version kept the outer
snapshot open while the batches committed; redb cannot reuse the pages a
snapshot still sees, and once in each run every reader and writer stalled
for 14 to 23 s (reproduced with plain bulk commits and a snapshot held
throughout: the agent's worst write went from 1.1 s to 15.5 s). The rows
are now read first and wait as ids, the snapshot goes before the first
commit, and the worst wait is one batch. Smaller batches shorten that
wait but cost more per row (each commit writes the pages it touched):
5,000 rows a batch moved twice as many edges a second as 1,000.

## A thousand connections and more (`crates/grafeno-server/examples/many_connections.rs`)

A server and its clients in one process, on Linux (the musl and mimalloc
build of the release, in a container: E2 without a memory limit): 100,000
`Person` nodes with five `knows` edges each, then 1,000, 2,000 and 4,000
HTTP/1.1 keep-alive connections at once, each sending its next request as
soon as the answer arrives, for 15 s: 35% a node by id, 25% a GQ lookup
through an index, 20% a GQ hop with an aggregate, 20% writes (a new node,
a property set). Requests per second and latency of every request
answered (`docs/data/many_connections.csv`):

| Connections | Before: req/s | p50 / p99 | 503s | After: req/s | p50 / p99 | 503s |
| ---: | ---: | --- | ---: | ---: | --- | ---: |
| 1,000 | 3,260 | 193 ms / 1.0 s | 0 | 3,936 | 57 ms / 1.3 s | 0 |
| 2,000 | 1,791 | 770 ms / 2.5 s | 40,891 | 3,342 | 84 ms / 3.0 s | 0 |
| 4,000 | 953 | 2.3 s / 3.9 s | 40,326 | 2,570 | 538 ms / 7.6 s | 3,217 |

Reads alone after the change, at 2,000 connections: a node by id 55 ms
at the median, a lookup 72 ms (before: 673 and 704 ms).

What broke:

- **Waiting writes held the turns.** The writer commits one group at a
  time (here about 700 writes a second for this mix); the writes waiting
  for it each held one of the `max_requests` turns (512) and a blocking
  thread. With a fifth of a thousand clients writing, the waiting writes
  held almost every turn: reads queued behind them and, past 1,536
  requests, got 503. A write now gives its turn back while it waits for
  the writer (its own queue, `[writer] queue`, bounds it), and waits
  without a thread. At 4,000 connections the 503s that remain are reads
  past `max_requests` + `queue_requests`; raise `queue_requests` for more
  clients than that.
- **Open files.** Each connection holds a file descriptor, and on Linux a
  shell or systemd leaves the soft limit at 1,024: with it, 756 of 1,500
  connections failed. The server raises the soft limit to the hard one
  (up to 65,536) when it starts, and the systemd units set
  `LimitNOFILE=65536` (their system call filter does not allow changing
  it from inside).

The writes' latency is the writer's queue: one writer, as many groups a
second as it can commit. On Windows the same load is bound by the
loopback network (8,000 `GET /health` a second, and the same uneven
tail), not by the engine, so these numbers are from Linux.

## Large loads while others work (`crates/grafeno-server/examples/import_pressure.rs`)

A server in the process on a file (E1-disk); an agent writes a memory
over HTTP every 20 ms (`POST /v1/agent/nodes`) and a reader runs a GQ
lookup through an index every 20 ms, while one load runs at a time
(`docs/data/import_pressure.csv`):

| Load | Records | Seconds | Agent p50 / p99 / max | Reader p50 / p99 / max |
| --- | ---: | ---: | --- | --- |
| none | | | 3.6 / 5.5 / 8 ms | 2.1 / 3.4 / 10 ms |
| JSON Lines import, `?bulk=true` | 4,000,000 | 146 | 50 / 85 / 731 ms | 2.3 / 4.0 / 351 ms |
| GQ `CALL { … } IN TRANSACTIONS OF 2000 ROWS` | 500,000 | 19 | 52 / 82 / 98 ms | 2.3 / 3.4 / 19 ms |
| CSV import, before | 500,000 | 21 | 50 / 1,744 / 2,124 ms | 2.5 / 3.6 / 6 ms |
| CSV import, after | 500,000 | 20 | 54 / 97 / 109 ms | 2.3 / 3.4 / 5 ms |
| streaming ingestion (NDJSON) | 500,000 | 16 | 9 / 26 / 28 ms | 2.2 / 3.4 / 6 ms |
| GQ, one statement, one transaction | 200,000 | 7 | 4 ms / 6.9 s / 6.9 s | 2.5 / 3.7 / 5 ms |

Reads do not wait for writes at all (snapshots). An agent's write waits
for the bulk transaction being committed, about 50 ms for 2,000 records,
in every load that uses the bulk lane: JSON imports, `IN TRANSACTIONS`,
ingestion (500 lines a batch) and now CSV and GraphML imports, which
wrote around the writer and took its lock in turn with it. One statement
of 200,000 rows still holds the writer for its whole run: large writes in
GQ belong in `IN TRANSACTIONS`.

## Storage baseline (`crates/grafeno-server/examples/storage_suite.rs`)

Phase 1 of `docs/roadmap/STORAGE_AND_INDEXING_OPTIMIZATION.md`: six
datasets loaded into files on E1-disk the way an application would, then
what each takes and how fast it reads and writes
(`docs/data/storage_baseline.csv`, two runs; the results of fixed queries
in `docs/data/storage_fixtures/`, which later phases compare against with
`COMPARE=docs/data/storage_fixtures`):

| Dataset | Records | File MiB | Stored MiB | Fragmented MiB | After `vacuum` MiB | Largest tables (stored MiB) |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| catalogue: entities with a code, a Spanish title and definition, a 128-dimension embedding, a hierarchy | 100,000 | 1,028 | 342 | 144 | 384 | full-text and property entries 105, vectors 51, HNSW links 46 |
| documents: about 2,000 characters each, a full-text index and node text | 20,000 | 2,056 | 488 | 353 | 618 | full-text entries 237, node text postings 162, records 45 |
| rows: orders with six properties, two indexes | 300,000 | 268 | 162 | 25 | 178 | records 79, change log 42, index entries 33 |
| graph: ten edges a node, a third with a valid time | 550,000 | 514 | 339 | 23 | 377 | edges 140, change log 121, adjacency 34 each way |
| hyperedges: three participants with roles | 70,000 | 129 | 39 | 4 | 43 | change log 16, hyperedges 7, incidence 7 |
| agent memory: a palace with ten rooms, memories anchored and related, through MCP | 10,000 | 32–42 | 21 | 5 | 24 | change log 6, edges 5, text postings 4 |

The stored, fragmented and vacuumed sizes were the same in both runs to
the tenth of a MiB (only the memory file itself differed, 42 and 32 MiB,
by where its last pages fell); every query result matched the first
run's. Latencies of the second run (the first
ran while the machine was locked and throttled, about twice as slow, so
these are indicative until phase 2 repeats them on an idle machine):

| Dataset | Operation | p50 ms | p99 ms |
| --- | --- | ---: | ---: |
| catalogue | lookup by code (index) | 0.31 | 1.32 |
| catalogue | full-text search, 10 results | 109 | 224 |
| catalogue | vector search, 10 | 3.4 | 78 |
| catalogue | two hops up the hierarchy | 0.16 | 0.67 |
| documents | full-text search, 10 | 2.6 | 5.2 |
| documents | node text search (BM25), 10 | 1.5 | 2.6 |
| rows | an order by number | 0.04 | 0.10 |
| rows | a country's latest 20 (`(country, placed)`) | 10.4 | 18.4 |
| rows | unindexed filter, count of 300,000 | 170 | 181 |
| graph | neighbors of a node | 0.15 | 0.29 |
| graph | two hops, count | 15.7 | 26.3 |
| hyperedges | a hyperedge by property | 0.04 | 0.06 |
| memory | `context` of a question | 6.5 | 12.1 |
| memory | `palace_enter` | 28.7 | 43.9 |
| every dataset | one node written (its own transaction) | 1.6–3.4 | 2.6–5.3 |

What it shows for the roadmap:

- The files are two to four times what their tables store: free pages
  (F1) and fragmentation inside pages (F7). `vacuum` takes them to
  1.1–1.3 times the stored bytes; the fragmented bytes stay.
- Full-text entries are the largest structure wherever there is text
  (F2): 237 MiB for 20,000 documents, with as much again fragmented, and
  node text postings another 162 MiB.
- The change log is the second largest table of rows, graphs and
  hyperedges (F5): 121 MiB for 550,000 records of the graph.
- A full-text search of the catalogue takes 109 ms against 2.6 for the
  documents: its terms are few and shared by most entries (a skew of the
  synthetic text, and a case phase 9 has to measure on the real
  catalogue).

### redb 5.0 (phase 2)

The same suite on an idle machine, redb 4.4.0 and then 5.0.0
(`docs/data/redb5.csv`; the 4.4.0 run is also the idle latency baseline,
run 3 of `docs/data/storage_baseline.csv`). Every result matched the
fixtures on both; the bytes stored, fragmented and after `vacuum` were
the same to the tenth of a MiB.

| Dataset | File MiB, 4.4 → 5.0 | Load s, 4.4 → 5.0 | Operation | p50 ms, 4.4 → 5.0 | p99 ms, 4.4 → 5.0 |
| --- | --- | --- | --- | --- | --- |
| catalogue | 1,028 → 544 | 69 → 59 | lookup by code | 0.060 → 0.061 | 0.130 → 0.122 |
| catalogue | | | full-text search | 41.5 → 37.2 | 68.6 → 65.7 |
| catalogue | | | vector search | 1.05 → 0.69 | 3.10 → 3.01 |
| catalogue | | | write a node | 1.96 → 1.60 | 3.50 → 2.58 |
| documents | 2,056 → 2,176 | 111 → 101 | full-text search | 2.48 → 2.65 | 3.50 → 4.72 |
| documents | | | node text search | 1.57 → 1.43 | 3.38 → 2.39 |
| rows | 268 → 272 | 11.1 → 10.3 | a country's latest 20 | 7.82 → 6.77 | 12.8 → 11.7 |
| rows | | | unindexed filter, count | 128 → 125 | 129 → 143 |
| graph | 514 → 544 | 16.5 → 16.8 | two hops, count | 13.4 → 13.0 | 22.5 → 18.7 |
| hyperedges | 129 → 68 | 6.2 → 6.0 | a hyperedge by property | 0.037 → 0.034 | 0.051 → 0.073 |
| memory | 42 → 31 | 90 → 75 | `context` | 6.75 → 5.99 | 10.6 → 8.95 |

The data takes the same; the files differ by how redb grows them (4.6
starts a file small and grows it as it is written), not by what they
hold. Times move within run-to-run noise, a little faster on the whole.
`vacuum --maximum` (redb 5's `CompactionMode::Maximum`) takes the
catalogue file of section 3 from 1,028 MiB to 403 MiB, against 555 with
the plain `vacuum`: the fragmented bytes went from 166 MiB to 17.

## What a write costs (`crates/grafeno/examples/write_cost.rs`)

Phase 3 of the storage roadmap: 20,000 writes of each kind, a thousand to
a transaction on a file with eventual durability (the engine's work, not
the disk's), with no index, property indexes and a full-text index on a
40-word text (`docs/data/write_cost.csv`), µs a write:

| Indexes | Operation | Before | After |
| --- | --- | ---: | ---: |
| none | create a node | 23.0 | 23.1 |
| none | set an unindexed property | 12.5 | 11.5 |
| a property index | create a node | 40.5 | 30.4 |
| a property index | set an unindexed property | 22.4 | 13.8 |
| a property index | set the indexed property | 28.8 | 20.0 |
| five property indexes | set an unindexed property | 24.9 | 22.5 |
| a full-text index | create a node | 142.4 | 89.0 |
| a full-text index | set an unindexed property | 123.2 | 14.1 |
| full-text and a property index | set an unindexed property | 149.1 | 13.8 |
| any | set the embedding (an HNSW insert) | 207–242 | 207–242 |

Building over 20,000 existing nodes: a full-text index in 0.89 s instead
of 1.10, leaving 0.4 MiB fragmented in the index table instead of 11.9; a
property index in 0.08 s instead of 0.10.

What changed:

- An index whose labels and properties are the same before and after a
  change keeps its entries without computing them: a full-text index
  tokenized its fields four times (old and new entries, old and new
  BM25 counts) for a change that did not touch them.
- A full-text index's fields are analyzed once for both its entries and
  its BM25 counts.
- A write transaction reads the namespace's index definitions once
  (decoded from JSON) instead of at every write, and forgets them when one
  is created, changed or dropped.
- Building an index inserts its entries in key order: pages fill instead
  of splitting half empty across the table.

Every result of the storage suite matched its fixtures after the change.

### The writer under mixed load

`crates/grafeno-server/examples/writer_mix.rs` drives the group-commit
writer alone with the write mix of `many_connections.rs` (a new node, or a
property set on one of 100,000 indexed nodes), immediate durability:

| Writers | Windows, writes/s | p50 ms | Linux container, writes/s | p50 ms | Writes a commit |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 50 | 6,389 | 7.1 | 6,724 | 7.3 | 49 |
| 200 | 9,824 | 19.5 | 7,034 | 27.9 | 197 |
| 800 | 11,722 | 78.2 | 7,211 | 124.6 | 504 |

So the about 700 writes a second of `many_connections` on Linux are not
the writer's limit. Over HTTP with 300 connections (Windows), the writer
committed 54 writes a group, 20 ms a commit at the disk, and each write
took about 0.5 ms inside its group against 0.14 ms alone: the writer's
thread shares the process's CPUs with the requests around it. Giving
reads fewer threads (512 down to 32) changed nothing; isolating the
writer's thread is left for a later phase.

## A storage migration (`grafeno storage apply`, phase 4)

On the file of section 3 of the storage roadmap (100,000 catalogue
entities, 1,028 MiB, 165.9 MiB lost inside pages), Windows, release build:

```bash
grafeno storage apply --db catalogue.redb --reindex-existing --encoding envelope --verify --compact
```

| Step | Records | Result |
| --- | ---: | --- |
| reindex | 3 indexes | rebuilt in key order |
| reencode | 199,974 | JSON → envelope (no compression yet) |
| verify | 199,974 | check ok, every record's value as before |
| compact | | 1,028 → 402.8 MiB; lost inside pages 165.9 → 16.2 MiB |

65.9 s in all, the backup included. The stored bytes grow by 1 MiB (five
bytes of envelope header a record); compression comes in phase 5.


## Compression (phase 5)

### Codecs (`crates/grafeno/examples/compression.rs`)

The values of the file of section 3 of the storage roadmap (100,000
catalogue entities, written uncompressed), one by one, Windows, release
build. Stored: what the engine would keep, each value compressed only
when it saves 12% (with an 8-byte allowance for the envelope).

| values | codec | MiB | stored MiB | of the original | compress µs | decompress µs |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| nodes (491 bytes) | LZ4 | 46.9 | 46.9 | 100% | 0.82 | 0.20 |
| | Zstandard, pure-Rust encoder, fastest | 46.9 | 46.9 | 100% | 22.09 | 2.52 |
| | Zstandard 1 | 46.9 | 34.0 | 72.5% | 5.09 | 2.48 |
| | Zstandard 1, dictionary | 46.9 | 6.7 | 14.4% | 1.39 | 0.86 |
| edges (274 bytes) | LZ4 | 26.1 | 26.1 | 100% | 0.62 | 0.16 |
| | Zstandard 1 | 26.1 | 19.2 | 73.6% | 3.93 | 2.29 |
| | Zstandard 1, dictionary | 26.1 | 7.4 | 28.4% | 1.45 | 1.07 |
| changes (167 bytes) | LZ4 | 47.9 | 47.9 | 100% | 0.43 | 0.13 |
| | Zstandard 1 | 47.9 | 42.7 | 89.2% | 3.60 | 1.81 |
| | Zstandard 1, dictionary | 47.9 | 17.5 | 36.5% | 1.18 | 0.63 |

Level 3 compresses these values no better than level 1, and more
slowly. Each dictionary is 64 KiB, trained in under 0.1 s on 2% of the
values. LZ4 saves 2–10% before the 12% rule, so nothing is kept. The
pure-Rust port of the reference library (`libzstd-rs-sys`, a prerelease)
gave the same sizes at the same speed. The LZ4 and pure-Rust rows come
from a one-off program; `compression.rs` measures the Zstandard rows.

### A file compressed (`grafeno storage apply --encoding compressed`)

The same file, as in "A storage migration" above:

```bash
grafeno storage apply --db catalogue.redb --encoding compressed --reindex-existing --verify --compact
```

| Table | Before MiB | After MiB |
| --- | ---: | ---: |
| nodes | 49.2 | 10.3 |
| edges | 28.4 | 11.0 |
| changes | 52.4 | 26.2 |
| all tables, stored | 360.7 | 278.4 |
| file | 1,028 | 311.2 |

66.7 s in all (training, rebuilding the indexes, rewriting 299,974
change log entries, verifying 199,974 records, compacting); three
dictionaries, 169 KiB. `vacuum --maximum` alone leaves 403 MiB. What is
left is mostly index entries (139 MiB), keys that are not compressed
(phase 6.4).

### The storage suite compressed

`crates/grafeno-server/examples/storage_suite.rs` now compresses each
dataset once it is loaded (`COMPRESSION=off` measures without); numbers
in `docs/data/compression.csv`, against the redb 5 run of
`docs/data/redb5.csv`. Every result matches the fixtures.

| dataset | stored MiB before | after | after vacuum before | after | train and rewrite s |
| --- | ---: | ---: | ---: | ---: | ---: |
| catalog | 341.8 | 277.3 | 384.4 | 311.9 | 3.1 |
| documents | 488.1 | 425.2 | 618.0 | 482.8 | 2.7 |
| rows | 162.2 | 95.0 | 177.9 | 107.4 | 3.7 |
| graph | 339.1 | 185.8 | 376.8 | 210.6 | 10.0 |
| hyperedges | 39.0 | 27.2 | 42.8 | 30.6 | 1.0 |
| memory | 21.4 | 13.9 | 23.7 | 16.0 | 0.7 |

| dataset | operation | p50 ms before | after |
| --- | --- | ---: | ---: |
| catalog | lookup by code (index) | 0.061 | 0.063 |
| catalog | two hops up the hierarchy | 0.059 | 0.065 |
| catalog | write a node | 1.595 | 1.577 |
| documents | node text search (BM25), 10 | 1.429 | 1.542 |
| documents | read a document by number | 0.052 | 0.054 |
| rows | a country's latest 20 | 6.766 | 7.561 |
| rows | unindexed filter, count | 124.9 | 136.2 |
| graph | neighbors of a node | 0.140 | 0.140 |
| graph | two hops, count | 13.04 | 14.50 |
| memory | context of a question | 5.994 | 6.294 |

The other operations are within ±5% (full list in the CSV). Reads that
go through many records pay for decompressing each one: 8–12%.

## Automatic indexing (phase 6)

The storage suite with every namespace automatic (`INDEXING=automatic`,
`docs/data/indexing.csv`) against the same suite with manual namespaces
(`docs/data/compression.csv`), compression on in both. Every result
matches the fixtures.

| dataset | load s manual | automatic | stored MiB manual | automatic |
| --- | ---: | ---: | ---: | ---: |
| catalog | 53.6 | 59.1 | 277.3 | 308.5 |
| documents | 76.6 | 78.9 | 425.2 | 427.5 |
| rows | 8.6 | 18.4 | 95.0 | 151.3 |
| graph | 13.8 | 14.6 | 185.8 | 188.2 |
| hyperedges | 4.7 | 5.2 | 27.2 | 27.2 |
| memory | 67.5 | 72.9 | 13.9 | 13.9 |

| dataset | operation | p50 ms manual | automatic |
| --- | --- | ---: | ---: |
| rows | unindexed filter, count | 136.2 | 0.104 |
| graph | two hops, count | 14.50 | 0.316 |
| rows | an order by number | 0.040 | 0.059 |
| catalog | write a node | 1.577 | 1.800 |
| documents | write a node | 1.533 | 1.793 |
| rows | write a node | 1.613 | 1.740 |
| graph | write a node | 1.533 | 1.660 |

Queries on properties nobody indexed by hand become lookups; the rows
dataset, whose records are scalar properties, pays for it in loading time (rows:
twice as long) and space (rows: 59% more, in index entries). A single
write costs 5–17% more, for the index entries of its properties.

