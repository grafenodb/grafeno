# Grafeno design

Grafeno is a graph database engine written in Rust, built as **memory and a
knowledge base for agents**. It depends on no particular application: any
agent or program can use it embedded as a library, as a server, or over
MCP.

## Goals

1. **Property graph**: nodes with one or more labels, typed directed edges,
   JSON properties on both.
2. **Two time axes** on edges (facts):
   - *valid time* (`valid_from` / `valid_to`): when the fact is true in the world;
   - *record time* (`recorded_at` / `expired_at`): when the engine learned it
     and when it was retracted.
   Invalidating or retracting does not delete: history stays queryable.
3. **Provenance**: nodes and edges keep `sources`, the nodes they came from
   (episode, document, chunk, conversation). This allows citing and inheriting
   permissions.
4. **Namespaces**: every piece of data lives in a namespace (user,
   organization, agent). No query crosses namespaces unless asked to.
5. **Hybrid retrieval**: vectors (HNSW) + full text (BM25) + graph expansion,
   fused with RRF, and output that fits a token budget.
6. **Memory primitives** (phase 7): episodes, entities, facts, importance and
   decay, duplicate merging, consolidation. LLM extraction stays outside the
   core (a plugin or the client), so the engine is not tied to a provider.
7. **Interfaces**: Rust API, HTTP/gRPC server, MCP server, Node (napi-rs) and
   Python (PyO3) bindings.

### Out of scope (for now)

- A custom storage engine (pager, WAL): an embedded KV store is used.
- Every graph query form there is: GQ covers patterns, filters,
  aggregates, subqueries and writes, and grows when a need is shown.
- An external coordinator or a distributed storage layer:
  replication and failover (phase 13) live in the `grafeno` binary itself.
- Splitting one namespace across servers: phase 13 replicates whole
  databases and, at most, places whole namespaces on different groups.

## Architecture

```
grafeno/
├── crates/
│   ├── grafeno-core      model (Node, Edge, ids, time), errors            [phase 1]
│   ├── grafeno           redb storage, transactions, traversals,          [phases 1-2]
│   │                     full-text index (BM25), vector index (HNSW)
│   ├── grafeno-memory    hybrid recall, token-budgeted context            [phase 2]
│   ├── grafeno-query     GQ: lexer, parser, executor                      [phase 5]
│   └── grafeno-server    `grafeno` binary: HTTP API, MCP over stdio,      [phases 3-5]
│                         shell, administration commands
├── clients/
│   └── typescript        `grafeno-client`, fetch-based HTTP client         [phase 3]
├── studio/               Grafeno Studio, the web interface                 [phase 6]
└── packaging/            Windows installer, .deb/.rpm, systemd unit        [phase 6]
```

The indexes live in the `grafeno` crate, not in a separate one, because they
are written in the same redb transaction as the graph: a node, its text and its
embedding are always consistent, and a failed write leaves no stale entries.

### Storage

[redb](https://github.com/cberner/redb) 4: pure Rust, ACID, one file, MVCC with
one writer and concurrent readers, and an in-memory backend for tests and
short-lived agents.

| Table          | Key                           | Value                      |
| -------------- | ----------------------------- | -------------------------- |
| `nodes`        | `(ns, node_id)`               | `Node` record              |
| `edges`        | `(ns, edge_id)`               | `Edge` record              |
| `adjacency_out` | `(ns, from, label, edge_id)` | `to` + the edge's times, compact (layout 13) |
| `adjacency_in` | `(ns, to, label, edge_id)`    | `from` + the edge's times  |
| `node_labels`  | `(ns, label, node_id)`        | `()`                       |
| `texts`        | `(ns, node_id)`               | indexed text               |
| `text_lengths` | `(ns, node_id)`               | number of terms            |
| `text_postings` | `(ns, term, node_id)`        | term frequency, text length |
| `text_stats`   | `ns`                          | `(documents, total terms)` |
| `vectors`      | `(ns, node_id)`               | normalized `f32` vector    |
| `vectors_q8`   | `(ns, node_id)`               | the same as int8 (layout 13), for searches |
| `hnsw_links`   | `(ns, node_id, layer)`        | neighbor ids               |
| `vector_meta`  | `ns`                          | dimension, count, entry    |
| `replication_log` | `index`                    | a committed write transaction as its effects (layout 14) |
| `meta`         | key, such as `schema_version` | `u64`                      |
| `namespace_counts` | `ns`                      | `(nodes, edges)`           |
| `label_counts` | `(ns, label)`                 | nodes with the label       |
| `edge_label_counts` | `(ns, label)`            | edges with the label       |
| `names`        | `(ns, normalized name, node_id)` | `()`                    |
| `memory`       | `(ns, node_id)`               | `(importance, accesses, last_access)` |
| `indexes`      | `(ns, index name)`            | index definition (JSON)    |
| `index_entries` | `(ns, index, sortable value, node_id)` | `()`              |
| `namespaces`   | `ns`                          | creation time, description (JSON) |
| `principals`   | key name                      | secret hash, grants, owner, `own_only` (JSON) |
| `merges`       | `(ns, merge_id)`              | merge record: who, when, both nodes before, edges moved and dropped |
| `match_rules`  | `(ns, label)`                 | match rule: strong keys, fields with measures and weights, thresholds (JSON) |
| `changes`      | `(ns, seq)`                   | the change log: what a commit changed (a node or edge, created, updated or deleted, its labels, which parts) |
| `constraints`  | `(ns, name)`                  | a constraint on facts (one per source, target or pair) or a functional property (JSON) |
| `state`        | `(ns, kind, key)`             | bytes kept by features on the engine (extraction's text hashes, review queue and decisions) |

- The layout has a version (`SCHEMA_VERSION`, now 12) in `meta`. Opening an
  older file upgrades it in one transaction: version 1 gets its counters
  computed, version 2 gets the new adjacency and posting tables (built from
  the records) and loses the old ones, version 3 gets the names index,
  version 4 the (empty) property index tables, version 5 the namespace
  registry (every namespace with data), version 6 the (empty) principals,
  version 7 the (empty) merge records, and version 8 the (empty) match
  rules and the possibility of fuzzy indexes (whose trigram entries an
  older Grafeno would read as values), version 9 the (empty) state
  table, version 10 the (empty) constraints, and version 11 the (empty)
  change log, which starts with the first commit after. A newer layout is
  refused.
- Adjacency entries hold the edge's record time and valid time
  (`recorded_at`, `expired_at`, `valid_from`, `valid_to`), so traversals
  filter without reading edge records. Postings hold the length of their
  text, so BM25 needs no extra lookup.
- Counters are updated in the same transaction as each write, so listing
  namespaces and their statistics never scans records. A namespace is a
  record of its own: created explicitly or by the first write to it, it
  stays (empty) until it is dropped, which deletes everything in it with
  one range deletion per table.

- Ids are UUID v7 (time-ordered), generated by Grafeno itself and stored as
  `u128`.
- Adjacency lists keyed by `(node, label)` make "neighbors of X through
  `mentions`" one contiguous range.
- Records are serialized as JSON for now. Moving to a binary format is an
  internal change with no effect on the API.

### Time semantics

- `EdgeFilter::any()` (the default) returns **current knowledge**: edges that
  were not retracted, regardless of their valid time.
- `valid_at(t)`: only facts true at `t` (`valid_from <= t < valid_to`).
- `known_at(t)`: what the engine knew at `t` (`recorded_at <= t < expired_at`).
- `invalidate_edge(t)`: the fact stopped being true at `t` (the world changed).
- `retract_edge()`: the fact was a mistake (what we know changed).

### Full-text index

- Tokenizer: Unicode NFD, accents and case removed ("Canción" → "cancion"),
  split on non-alphanumeric characters, one-character terms and English and
  Spanish stopwords dropped. No stemming yet.
- Ranking: BM25 (`k1 = 1.2`, `b = 0.75`).
- Removing a text re-tokenizes the stored copy to find its postings, so a
  tokenizer change requires rebuilding the index.

A hand-written index was chosen over tantivy: it shares the graph's file and
transactions, and it brings no C dependencies. Tantivy remains an option if
richer linguistics (stemming per language, phrase queries) become necessary.

### Vector index

- HNSW (Malkov & Yashunin): `M = 16` (32 on layer 0), `ef_construction = 100`,
  neighbor selection with the paper's diversity heuristic.
- Cosine similarity: vectors are normalized on insert.
- One graph and one fixed dimension per namespace, set by its first vector.
- A node's level comes from the random bits of its id, so it is never stored
  and the entry point can always be recomputed after a delete.
- Deleting a node reconnects each of its neighbors using the removed node's
  links as candidates; links that still point to removed nodes are skipped at
  search time.
- Searches compute distances straight from the stored bytes (no decoding,
  eight-lane dot products). Inserts and removes in one write transaction
  share a cache of decoded vectors and link lists that every write updates
  and that ends with the transaction; a rebuild drops it for its namespace.
- Embeddings are sent by the client. A local model (ort or candle) may be
  offered later for clients without one.

### Hybrid recall and context (`grafeno-memory`)

```
query ─┬─ text ──────▶ BM25 ───┐
       └─ embedding ─▶ HNSW ───┴─▶ RRF ─▶ graph expansion ─▶ label filter ─▶ packing by tokens
```

- `recall`: reciprocal rank fusion (`k = 60`) of both result lists. The best
  `seeds` results then pass `decay^hops` of their score to nodes up to `depth`
  hops away, so nodes without text or embedding (people, companies, tags) are
  found through what mentions them. Every result says why it was recalled
  (`Signal::Text`, `Signal::Vector`, `Signal::Graph`).
- `context_for`: packs recalled nodes, best first, into a token budget; the
  last one that does not fit is truncated. A share of the budget is kept for
  facts: edges between recalled nodes first, then edges that touch one, with
  their valid time. Tokens are estimated (4 characters per token) unless the
  client passes its model's tokenizer.

## Phases

Grafeno is built in phases, each in its own file under
[docs/phases/](docs/phases/README.md), with its steps and what was built
in each:

- [Phase 1: Core](docs/phases/phase-01-core.md)
- [Phase 2: Retrieval](docs/phases/phase-02-retrieval.md)
- [Phase 3: Server](docs/phases/phase-03-server.md)
- [Phase 4: Foundations and validation](docs/phases/phase-04-foundations-and-validation.md)
- [Phase 5: Shell and query language](docs/phases/phase-05-shell-and-query-language.md)
- [Phase 6: Studio and installers](docs/phases/phase-06-studio-and-installers.md)
- [Phase 7: Memory](docs/phases/phase-07-memory.md)
- [Phase 8: Performance, query language and analysis](docs/phases/phase-08-performance-query-language-and-analysis.md)
- [Phase 9: Desktop application](docs/phases/phase-09-desktop-application.md)
- [Phase 10: Hardening](docs/phases/phase-10-hardening.md)
- [Phase 11: Memory that keeps itself](docs/phases/phase-11-memory-that-keeps-itself.md)
- [Phase 12: Compatibility and speed](docs/phases/phase-12-compatibility-and-speed.md)
- [Phase 13: Distributed operation](docs/phases/phase-13-distributed-operation.md)
- [Phase 14: Users, roles and permissions (RBAC)](docs/phases/phase-14-users-roles-and-permissions.md)
- [Phase 15: Engine and interface apart](docs/phases/phase-15-engine-and-interface-apart.md)
- [Phase 16: Data model, search, analysis and events](docs/phases/phase-16-model-search-and-events.md)
- [Phase 17: Integrations](docs/phases/phase-17-integrations.md)

## Measurements and test environments

What Grafeno has been measured to do, where, and how; the base for the
official documentation's charts. The raw numbers are CSV files in
`docs/data/` (one per experiment, described in its `README.md`); the
discussion of each experiment is in `docs/benchmarks.md`, and
`docs/performance.md` presents them with charts (drawn from the CSV files
by `docs/charts/build.py`). Every number below comes from one of three
environments.

### Environments

| Id | What | Hardware and system | Storage | Build |
| --- | --- | --- | --- | --- |
| E1-mem | The engine alone, without a disk | Lenovo laptop (model 83JC): AMD Ryzen 7 7435HS, 8 cores / 16 threads, 31.7 GiB of RAM; Windows 11 Home 10.0.26200 | redb's in-memory backend: no file, no fsync | Rust 1.99.1, release profile, redb 4.4.0 |
| E1-disk | The engine on a file | The same laptop | a file in `%TEMP%` on an NVMe SSD (Micron MTFDKCD512QFM, NTFS); immediate durability is an fsync per commit | as E1-mem |
| E2 | Memory limits | The same laptop, Docker Desktop 29.6.1 on WSL2 (Linux 6.18.54.1, 16 CPUs and 15.5 GiB visible to Docker) | a named Docker volume (ext4 inside WSL2's virtual disk, on the laptop's NVMe); slower than E1-disk for random reads | built in `rust:1.91` with `cargo build --release --locked`, run in `debian:bookworm-slim` with `--memory` equal to `--memory-swap` (a hard limit, no swap): 2 GiB or 512 MiB |

E1 is a laptop on mains power with other programs running: runs vary by
about ±20%, so only larger differences mean something. E2's times are
pessimistic (its disk sits behind a virtual disk); its memory numbers are
the point. Nothing has been measured yet on Linux bare metal, on a
spinning disk, on ARM, or for days at a time.

### Methodology

- **Builds and runs.** Release builds; one run per configuration unless a
  file says otherwise; the date of each run is in `docs/data/README.md`.
  Benchmarks call the library in-process, so HTTP is not in their times
  (its cost has not been measured).
- **Latency under load** (`examples/saturation.rs`). Open load: each write
  is scheduled at a fixed rate whatever the answers, and its latency runs
  from that scheduled time, so a system that falls behind shows it instead
  of slowing the clients down (no coordinated omission). 512 client
  threads; each step lasts 3 s; percentiles are exact over every write of
  the step. Each write is a node and an edge to an earlier node.
- **Memory** (`examples/ram_bench.rs`). Resident memory (`VmRSS`) after an
  operation is "steady"; the most it reached while the operation ran
  (`VmHWM`, reset through `/proc/self/clear_refs` before each operation)
  is "peak". Both include redb's page cache, which lives in the process,
  and not the kernel's file cache. A run that crosses the container's
  limit is killed by the kernel (exit code 137), as a server would be.
- **Recall** (`examples/hnsw_recall.rs`, `hnsw_churn.rs`, `vector_recall`).
  Recall@10 against exact answers from a pass over every vector alive at
  that moment, over 200 queries drawn from the data (clustered vectors,
  closer to real embeddings than uniform noise).
- **Hostile queries** (`tests/adversarial.rs`). Each query runs on a
  thread of its own with the 2 MiB stack of a server worker, under limits
  of 2 s, 2 million reads, 200 000 rows and 64 MiB, on 2 000 nodes and
  6 000 edges; the test fails on a panic, a crash or more than 4 s.
- **Correctness** is shown by tests, not benchmarks (below): property
  tests against a model, crash tests in child processes, fixtures of every
  past layout, fuzzing.

### Headline numbers

| Capability | Result | Env | Data |
| --- | --- | --- | --- |
| Read a node | 1.6 µs | E1-mem | `engine.csv` |
| Neighbors of a node (10k nodes, 40k edges) | 25 µs | E1-mem | `engine.csv` |
| BM25 search, top 10 of 10 000 notes | 0.27 ms | E1-mem | `engine.csv` |
| Vector search, top 10 of 20 000 (128-d, `ef` 64) | recall@10 1.000, p50 0.51 ms, p99 0.98 ms | E1-mem | `hnsw_recall.csv` |
| Vector search, top 10 of 1 000 000 (128-d) | 5 ms, 140 MiB resident in a 512 MiB container | E2 | `vector_search.csv` |
| Recall@10 through deletes, reinserts and churn | 0.99 or more at `ef` 64 (0.929 before 10.3) | E1-mem | `hnsw_churn.csv` |
| A write with immediate durability | about 2 ms up to 4 000 writes/s | E1-disk | `saturation_immediate.csv` |
| Most writes per second, immediate / eventual durability | about 11 000 / 16 000 | E1-disk | `saturation_*.csv` |
| An agent's p99 write latency during a 300 000-node import | 88 ms (12 s before 10.7) | E1-disk | `import_interference.csv` |
| Bulk import, nodes with 30-word texts | about 7 500 nodes/s, steady up to 1 M | E1-disk | `bulk_load.csv` |
| Property index lookup against a label scan (100 000 nodes) | 3.6 ms against 200 ms | E1-mem | `property_index.csv` |
| PageRank, 100 000 nodes and 500 000 edges | 17 ms (projection 102 ms, cached afterwards) | E1-mem | `algorithms.csv` |
| Graph of 1 M nodes and 10 M edges in 512 MiB | every operation completes; peak 509 MiB (a projection with edge ids) | E2 | `ram_graph.csv` |
| Integrity check of 10 M edges | 249 s, 443 MiB peak (killed at 2 GiB before 10.6) | E2 | `check_cost.csv` |
| Projection memory | about 12 bytes per edge, 28 with edge ids | E2 | `projection_size.csv` |
| Hostile GQ queries | 32 of 32 end within their limits, none crashes | E1-mem | `hostile_queries.csv` |
| Fuzzy search for a damaged name among 100 000 people | 20 ms p50 (469 ms comparing with every node) | E1-mem | `resolve_speed.csv` |
| Entity resolution by a match rule (2 000 damaged duplicates, 2 000 namesakes) | 95% found as same or review, precision of `same` 0.994 | E1-mem | `resolve_quality.csv` |
| One current fact per source (50 000 job changes) | 14 000 facts/s (44 000 without); every person one current employer | E1-mem | `constraint_cost.csv` |
| CSV and GraphML of 200 000 records | export 397 000 and 341 000 records/s; import 27 600 and 25 900 | E1-mem | `interchange.csv` |
| Change log on 200 000 records | 29 500 records/s (35 000 without); reads 1.4 M entries/s | E1-mem | `change_log_cost.csv` |
| NDJSON ingestion of 100 000 lines | 25 000 lines/s; a retry 360 000 lines/s, nothing written; commit to event 0.15 ms | E1-mem | `ingest_runs.csv` |
| Community summaries of 50 000 nodes in 4 levels | 3.2 s first, 1.35 s unchanged; teams and departments found (purity 0.999); global search 0.06 ms | E1-mem | `community_*.csv` |
| Extraction of 20 000 memories naming 5 000 entities | 3 200 texts/s; 99.4% of mentions linked right, none wrong; 0.06 s when nothing changed | E1-mem | `extract_*.csv` |
| Queries a planner without statistics got wrong (200 000 people) | 355 ms → 11 ms, 1.8 s → 2.7 ms, 2.6 s → 1.8 ms; a scan 4.4 times faster on 16 threads | E1-mem | `planner_plans.csv`, `parallel_scan.csv` |
| Read queries with 16 readers and a busy writer | 9 200 a second alone, 8 000 with 10 500 writes a second; p99 under 9 ms | E1-mem | `read_concurrency.csv` |
| Aggregates over 200 000 rows, folded as they come (12.3) | 2.3 to 2.9 times faster grouped; 2 M rows counted within the standard limits (failed before) | E1-mem | `batch_aggregation.csv` |
| Traversals from many starts, batched and parallel (12.4) | two hops from 10 000 people 5.6 → 2.7 s, from every company 9.5 → 2.9 s; reads down 30 to 72% | E1-mem | `traversal.csv` |
| Compact adjacency and int8 vectors (12.5) | adjacency 19% smaller; searches walk a quarter of the vector bytes at the same recall (0.996) and speed in memory | E1-disk | `compact_storage.csv` |
| Vector search, 1 M vectors in 512 MiB, int8 copies (12.5) | 3.5 to 4.2 ms (4.9 to 5.2 ms before), same resident memory | E2 | `vector_search_int8.csv` |
| Replication log (13.1) | writes at 80% of the speed with the log on; 568 bytes per record; a replica applies 22 300 records/s | E1-mem | `replication_log.csv` |
| Raft cluster of three (13.2) | 137 sequential writes/s (618 on one server), p50 7.2 ms; a leader replaced in 0.5 to 0.8 s | E1-disk | `cluster.csv` |
| Passwords (14.1) | Argon2id at 19 ms a check: 53 sign-ins/s on one core, 141 on 16 threads | E1-mem | `auth.csv` |
| Audit log (14.5) | 24 600 events/s one write each, 211 000/s a thousand a write; 1.4 M/s read; an account change with its event 49 µs | E1-mem | `audit.csv` |
| Enforcement on reads (14.4), 50 000 people | a view of the whole namespace costs nothing; per record +25% to +51% on scans and traversals, nothing on full-text search; vector recall@10 0.972 with 45% visible | E1-mem | `visibility.csv` |
| Roles (14.2), 5 000 roles, 2 811 reached by one account | rights computed in 17 ms once per change; a namespace check 93 ns, a label or property check 271 ns | E1-mem | `rights.csv` |
| Runbooks on a compose cluster (13.6) | rolling upgrade 13.5 → 13.6, a member lost, a majority lost, a disk replaced: 15 971 operations, all linearizable | E1-disk | `drill.csv` |
| Several Raft groups (13.5) | 109, 153 and 181 writes/s with 1, 2 and 3 groups; 20 000 nodes moved under writes in 11 s with a 39 ms pause, no write lost or doubled | E1-disk | `groups.csv`, `namespace_move.csv` |
| Reads split across members (13.4) | 200 000 people, 3 threads a member: scans 2.1, aggregates 2.9, top-20 3.0, two hops 1.4 times faster with 5 members | E1-disk | `split.csv` |
| Faults injected under reads and writes (13.3) | 31 759 operations, all linearizable; writes resume 0.4 s (one process) and 0.5 s (real processes) after the leader fails | E1-disk | `faults.csv` |

### Guarantees and the tests that hold them

`cargo test --workspace` runs about 300 Rust tests (unit and
integration); the TypeScript client has 8 end-to-end tests against the
real binary. The ones that carry each guarantee:

| Guarantee | Tests | How |
| --- | --- | --- |
| The engine does what a plain model does | `grafeno/tests/model.rs` | proptest: random sequences of every write, aborted transactions, valid and record times; records, labels, counters, neighbors, text and vector search compared with an in-memory model, also after export → import and after a rebuild (3 000 cases) |
| Commits survive crashes | `grafeno/tests/durability.rs`, `crash.rs` | a child process killed at random moments during writes, imports, rebuilds, namespace drops and upgrades; every reported commit is there, nothing is half-written, the check passes |
| Old files open | `grafeno/tests/upgrade.rs` and `fixtures/layout-v1` to `v13` | a database written by the commit of every past layout version, upgraded on open, then written and reopened |
| Damage is found, not trusted | `grafeno/tests/corruption.rs`, `src/check.rs` tests | files with flipped bytes, zeroed pages or cut short are refused or read back exactly as they were, never a crash or a different answer; damage to each derived index is reported and repaired by a rebuild |
| Backups are consistent | `grafeno/tests/backup.rs` | a backup taken while writes go on is the snapshot it started from |
| Readers see whole transactions | `grafeno/tests/concurrency.rs` | readers during commits never see part of one, and snapshots never go back |
| Group commit is correct | `grafeno/tests/writer.rs` | concurrent writes all commit; a failing write leaves the others; a full queue answers busy; imports through the bulk lane let interactive writes in; metrics count what happened |
| Vector search keeps its quality | `grafeno/tests/vector_churn.rs` | deletes, reinserts, three rounds of churn and a compaction keep recall@10 at 0.95 or more, and reinserted vectors are found |
| Records are matched by rule, with reasons | `grafeno/tests/resolve.rs`, `property_index.rs` (fuzzy), `src/textsim.rs` tests | measures against published values; strong keys decide either way; typos, swapped dates and namesakes; neighbors as evidence (and no links as no evidence); scans without indexes; pairs for deduplication; fuzzy indexes through updates, check and export |
| Contradicting facts are ended, not lost | `grafeno/tests/constraints.rs` | invalidation as a new version (`KNOWN AT` sees what was believed), late facts shortened, facts without dates starting now, the same fact again, flag, reject, target and pair scopes, `own_only` agents flagged, existing contradictions, property history |
| GQ behaves as specified | `grafeno-query/tests/conformance.rs` (hand-written scenarios), `language_more.rs`, `adversarial.rs`, `fuzz.rs` | 146 scenarios over MATCH, paths, RETURN, WITH, UNION, aggregation, nulls, numbers, strings, lists, subqueries, writes and errors; GQ's own choices pinned; nesting and budgets for the new forms |
| Graphs go out and come back | `grafeno/tests/interchange.rs`, `grafeno-server/tests/http.rs` | CSV and GraphML round trips with types, text and dates; re-imports that update; files from other tools with their own ids; bad rows reported |
| Every write is in the change log | `grafeno/tests/changes.rs`, `grafeno-server/tests/events.rs`, `upgrade.rs` | entries folded per record and numbered per namespace, nothing from aborted or failed writes in a group commit, the window and the gap, listeners; pages and server-sent events resuming after `Last-Event-ID`; NDJSON with keys, references and failing lines, retries as duplicates; signed webhooks, label filters, failures kept in place; extraction and summaries from the log; layout 11 upgrades with an empty log |
| Communities summarize themselves | `grafeno-memory/tests/communities.rs`, `grafeno-server/tests/http.rs` and `mcp.rs` community tests | a summary per community naming its center and topic, refreshes that change nothing, members joining in place, an agent's text kept and marked stale, levels as communities of communities, global search, drill-down, recall without summaries, context with an overview |
| Texts link to what they mention | `grafeno-memory/tests/extraction.rs`, `src/extract.rs` tests | patterns and their normalized values and offsets, phrases and sentence openers, the gazetteer on normalized text; known names, emails by index, value nodes, new names after two texts, namesakes settled by context or queued, decisions that last, dismissals, incremental runs, an agent's mentions through match rules |
| Merges are right and reversible | `grafeno/tests/merges.rs`, `grafeno-memory/tests/maintenance.rs` | strong keys, labels, conflicting facts and functional edges; namesakes, typos, nicknames, translations; a merge undone exactly (indexes, counters and the check agree) |
| Isolation between principals | `grafeno-server/tests/access.rs` | a matrix of keys, grants and endpoints: no read or write outside a key's grants, `own_only` keys change only their own records |
| GQ's clauses and functions | `grafeno-query/tests/language.rs`, `queries.rs`, `statistics.rs`, `indexes.rs`, `algorithms.rs` | clauses, functions, aggregates, indexes, planner choices and algorithms on known graphs |
| The planner estimates from the data, and parallel scans change nothing | `grafeno-query/tests/planner.rs` | statistics against known distributions and degrees; starts chosen by value distribution, parameters and pattern cost; join order; statistics rebuilt after drift and index changes; a cached statement after its index is dropped; parallel rows and order equal one thread's, and limits hold across threads |
| Folded aggregates equal row-at-a-time ones and hold no rows | `grafeno-query/tests/batch.rs` | every aggregate with DISTINCT, nesting, ties, empty groups, overflow and duration sums; 90 000 rows counted under a `max_rows` of 2 000; parallel folds equal to one thread's |
| Batched and parallel traversal give the same rows in the same order | `grafeno-query/tests/traversal.rs` | eight pattern shapes (both directions from a middle anchor, edge variables and properties, targets filtered by earlier nodes, edge uniqueness, variable length with paths, bound nodes, row filters) against one anchor at a time; a hub's 12 000 two-hop rows on one and four threads; shared nodes read once; `max_rows` across slices |
| int8 searches keep their quality and compact entries read back exactly | `grafeno/tests/quantized.rs`, `src/store.rs` and `src/vector.rs` tests | recall@10 of 0.95 or more with exact scores; the copies through removal, replacement, compaction, backup and namespace drop; adjacency values round trip at every bound; quantized dot products within 0.02 |
| A replica that applies the log holds the same data | `grafeno/tests/replication.rs`, `model.rs`, `grafeno-query/tests/replication.rs` | every kind of write replayed onto an empty database and compared (exports, counters, change log, constraints and their events, merges, state, principals); idempotence, gaps, catching up from a backup after truncation, group commits with failures; the model's random sequences replayed |
| Raft never loses an acknowledged write and members agree | `grafeno-raft/tests/simulation.rs`, `grafeno-cluster/tests/cluster.rs`, `grafeno-server/tests/cluster.rs` | random schedules of drops, delays, duplicates, partitions, crashes, membership changes and snapshots checked for one leader per term, one entry per index, no lost acknowledged write and convergence; members in one process and three real processes through failover, restart, joining and leaving |
| Reads and writes stay linearizable through failures | `grafeno-cluster/tests/linearizable.rs`, `grafeno-server/tests/faults.rs`, `grafeno-cluster/src/check.rs`, `clients/typescript/test/cluster.test.ts` | a register written and read by concurrent clients while members are cut off, restarted, compacted and killed, every history checked by a linearizability checker (which rejects the unsafe control); forwarding, read-your-writes and keyed writes across real processes |
| A read split across members returns one server's rows | `grafeno-query/tests/common/mod.rs` (used by `conformance.rs`, `planner.rs`, `language.rs`, `queries.rs`, `traversal.rs`, `batch.rs`), `grafeno-server/tests/split.rs`, `aggregate.rs` tests | every read of those suites that can be split, whole against 2, 3 and 5 parts through JSON, row order included; exact sums independent of order; three processes, with a member down |
| A namespace moves between groups without losing or doubling a write | `grafeno/tests/replication.rs` (copy, catch-up, fence), `grafeno-server/tests/groups.rs` | a namespace copied and caught up equals the source, change log numbers included; a fenced namespace refuses writes; three processes with two more groups, writes routed through any member, a move under three writers retrying busy answers, every acknowledged write found once on every member |
| A cluster restarts, rejoins and upgrades without anomalies | `grafeno-cluster/tests/operations.rs`, `packaging/compose/drill.sh` | a new cluster from a follower's backup, a member joining from an older backup without a snapshot, a restart; the leader refusing an entry format a voter cannot read; on the compose, every runbook under load checked for linearizability |
| Every security event is audited once, and the log survives | `grafeno/tests/audit.rs`, `grafeno-server/tests/audit.rs`, `cluster.rs`, `upgrade.rs` | changes of access in their transaction; sign-ins, lockouts, refusals over HTTP; replication, failover, reopening, backups |
| Nothing hidden is returned or changed | `grafeno-query/tests/visibility.rs`, `grafeno-server/tests/leaks.rs` | 300 random graphs and rights for reads and for writes; the leak matrix over every surface of the server |
| Access is managed in GQ, the API and the tools, by those who may | `grafeno-query/tests/conformance/scenarios.txt`, `grafeno-query/tests/access.rs`, `grafeno-server/tests/admin.rs`, `shell.rs`, `clients/typescript/test/accounts.test.ts` | every access statement and admin endpoint; refusals without `manage_accounts` or `manage_roles`; superusers only a superuser's; `EXPLAIN` naming what is missing; expiry; an open server keeping a superuser |
| Rights are the union of roles, and a denial wins | `grafeno/tests/rights.rs`, `grafeno-server/tests/roles.rs` | 2 000 random role graphs against a direct model; over HTTP, patterns with denials, nested roles, a custom editor, server privileges, `public`, scoped keys, cycles refused |
| Accounts sign in, and keys and sessions do only what they may | `grafeno-server/tests/accounts.rs`, `access.rs`, `shell.rs`, `cluster.rs`, `clients/typescript/test/accounts.test.ts` | sign-in, refresh, expiry, revocation, lockouts, the password policy, disabled accounts, scoped, expired and rotated keys over HTTP, the shell and the client; in a cluster, keys and sessions on every member |
| Costly queries stop | `grafeno-query/tests/adversarial.rs` | 32 hostile queries within their limits; nesting just under the limit runs on a 2 MiB stack; `max_cost` refuses; procedure rows count against `max_memory` |
| Nothing a client sends crashes the parser | `grafeno-query/tests/fuzz.rs` | proptest: random tokens, valid queries with random edits, arbitrary text, through the lexer, parser, planner, `EXPLAIN` and executor |
| The API keeps its contract | `grafeno-server/tests/http.rs`, `mcp.rs`, `shell.rs`, `clients/typescript/test` | every endpoint and MCP tool, limits, conflicts (409 with details), metrics, the CLI shell, and the client against the real binary |

### Reproducing

```bash
cargo bench -p grafeno --bench engine
cargo run --release -p grafeno --example hnsw_recall -- 20000 128 200
cargo run --release -p grafeno --example hnsw_churn
cargo run --release -p grafeno --example concurrency
cargo run --release -p grafeno --example bulk_load -- 200000
cargo run --release -p grafeno --example algo_bench
cargo run --release -p grafeno --example saturation -- 3
cargo run --release -p grafeno-query --example index_bench
cargo run --release -p grafeno --example resolve_bench -- 100000
cargo run --release -p grafeno-memory --example extract_bench -- 5000 20000
cargo run --release -p grafeno --example constraint_bench -- 10000 50000
cargo run --release -p grafeno-memory --example community_bench -- 1000 50
cargo run --release -p grafeno --example interchange_bench -- 100000
cargo run --release -p grafeno-query --example planner_bench -- 200000
cargo run --release -p grafeno-query --example batch_bench -- 200000
cargo run --release -p grafeno-query --example traversal_bench -- 200000
cargo run --release -p grafeno --example storage_bench -- 100000 50000
cargo run --release -p grafeno --example replication_bench -- 100000
cargo test --release -p grafeno-raft --test simulation   # RAFT_SEEDS=3000 for more
cargo run --release -p grafeno-cluster --example cluster_bench -- 500
FAULT_SECONDS=30 cargo test --release -p grafeno-cluster --test linearizable -- --nocapture
cargo run --release -p grafeno-server --example split_bench -- 200000 3 5   # build grafeno first
cargo run --release -p grafeno-server --example groups_bench -- 10 8 20000
OLD_IMAGE=grafeno:13.5 NEW_IMAGE=grafeno:13.6 packaging/compose/drill.sh   # images built from each commit
cargo run --release -p grafeno-server --example auth_bench -- 60
cargo run --release -p grafeno --example rights_bench -- 5000 1000
cargo run --release -p grafeno-query --example visibility_bench -- 50000
cargo run --release -p grafeno --example audit_bench -- 100000
FAULT_SECONDS=40 cargo test --release -p grafeno-server --test faults -- --nocapture
cargo run --release -p grafeno --example changes_bench -- 100000
cargo run --release -p grafeno-server --example events_bench -- 100000
cargo test --release -p grafeno-query --test adversarial -- --nocapture
```

The memory runs (E2) build the benchmark for Linux and run it with a hard
limit; the file lives on a named volume:

```bash
docker run --rm -v "$PWD:/src:ro" -v grafeno-target:/target -e CARGO_TARGET_DIR=/target -w /src rust:1.91 cargo build --release --locked -p grafeno-query --example ram_bench
docker run --rm --memory 2g --memory-swap 2g -v grafeno-target:/target -v grafeno-data:/data debian:bookworm-slim /target/release/examples/ram_bench build /data/big.redb 1_000_000 10_000_000
docker run --rm --memory 512m --memory-swap 512m -e CACHE_MB=128 -e PROJECTION_CACHE_MB=64 -v grafeno-target:/target -v grafeno-data:/data debian:bookworm-slim /target/release/examples/ram_bench ops /data/big.redb
```

(`vectors DB COUNT DIM` and `search DB DIM` measure vectors the same way.)

### What the measurements leave open

- Counting every edge of 10 M in GQ took longer than the 30 s limit.
  Since 12.3 aggregates fold as rows come and hold none, but each edge is
  each edge was matched one at a time; 12.4 batches the traversal and
  shares its reads. Not measured again in E2.
- Vector inserts slow down as the index grows: each 100 000 took 96 s at
  first and 809 s at the millionth (E2); a vector cache that keeps the
  most used entries, larger transactions and quantization (12.3) are the
  levers.
- With a projection cache smaller than a projection, every algorithm call
  rebuilds it (about 60 s on 10 M edges in E2) and GQ algorithms hit the
  30 s timeout.
- Opening a large file after a crash takes minutes while redb repairs it
  (461 s for 6.4 GB in E2).
- With eventual durability, the flush every 500 ms holds the writer for
  10 to 25 ms, which is the p99 of a write.
- A parallel scan of 200 000 candidates gains about 4.4 times on 16
  threads, not 16: each candidate still decodes its node and clones its
  row one at a time (12.3 is the lever).
- Not yet measured: Linux bare metal, spinning disks, ARM, multi-day
  runs, and HTTP end to end under load.

## Open decisions

- Binary record format (postcard, rkyv).
- Whether an application in front of Grafeno signs its users in itself and
  calls Grafeno as one service account, or maps them to Grafeno accounts
  (phase 14 allows both).
- (Decided in 16.1: the license is MIT or Apache-2.0, at the user's
  choice.)
- On Windows, building needs the MSVC toolchain (Visual Studio Build Tools):
  tokio's `windows-sys` does not build with the self-contained GNU toolchain.
- Phase 13: TLS between members (13.2 authenticates with a shared secret
  only). (Decided in 13.2: a Raft of our own; HTTP between members. In 13.3: reads
  are `local` unless a request asks for more, and linearizable reads use
  ReadIndex, not leases. In 13.4: sums do not depend on the order
  of the rows, so split aggregates equal one server's.)
- Phase 14: whether denials exist (they make exceptions to broad grants
  easy, but a model can do without them); whether hidden
  properties are absent or masked; session tokens signed (stateless,
  checked on any member) or stored (revocable at once); and the names of
  the built-in roles.
- Phase 15: Tauri or Electron for Grafeno Desktop; whether the interface is
  also published as static files for a web server of one's own (never
  served by the engine); whether the desktop app may download an engine
  when none is installed or only points to its installer; where the package
  repositories are hosted (GitHub Pages, a bucket, a service such as
  Cloudsmith or packagecloud) and who holds the signing keys; and a macOS
  build, not planned yet.
