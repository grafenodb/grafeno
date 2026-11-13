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

## Writes with vectors on the one writer (`crates/grafeno/examples/vector_writes.rs`)

A database file with immediate durability, on a laptop's NVMe SSD
(Windows 11), release build, 2026-11-12. Each write is a node with a
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
writer: find each new vector's neighbors on a read snapshot, on several
threads, and leave the writer only the link lists to write, re-checking
the few that changed meanwhile. Until then: send writes in batches
(`/batch`, `/ingest`, `?bulk=true`), keep vectors in their own namespace
or group when a cluster has several, and give write-heavy vector indexes a
lower `ef_construction` (64 kept recall@10 at the same level on 20 000
vectors, above).

## Remaining limits

- **Vector search reads each visited vector from redb** (about 0.4 µs per
  B-tree lookup). At 20k vectors that is most of a search's time; an
  in-memory copy of a namespace's vectors would remove it, at the cost of
  memory and of keeping it in sync across transactions.
- **Single small transactions** pay for page copies and the commit; clients
  should batch.
- **Edge filters by label alone** (`edges_page` with a label) scan the
  namespace's edges.
