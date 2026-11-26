# Measurement data

The numbers behind `DESIGN.md` ("Measurements") and
[benchmarks](../benchmarks.md), one CSV per experiment, for charts and for
comparing later runs. Environments (E1-mem, E1-disk, E2) are described in
`DESIGN.md`; each file says which it used. Times are in the unit its column
names (`_us`, `_ms`, `_s`); memory is in MiB (2^20 bytes); empty cells were
not measured or do not apply. Every number comes from one run of a release
build unless the file says otherwise; on E1, runs vary by about ±20%.

| File | Experiment | Source | Environment | Date | Suggested chart |
| --- | --- | --- | --- | --- | --- |
| `engine.csv` | Core operations before and after the performance pass (layout 2 → 3) | `crates/grafeno/benches/engine.rs` | E1-mem | 2026-11-03 | horizontal bars, before / after, log scale |
| `single_writes.csv` | Cost of one write in its own transaction | engine bench | E1-mem | 2026-11-03 | bars |
| `hnsw_recall.csv` | Recall@10 and latency against `ef`, 20 000 vectors of 128 dimensions | `crates/grafeno/examples/hnsw_recall.rs` | E1-mem | 2026-11-03 | recall and p99 against `ef` (two axes, or two panels) |
| `hnsw_churn.csv` | Recall@10 through deletes, reinserts and churn, before and after 10.3 | `crates/grafeno/examples/hnsw_churn.rs` | E1-mem | 2026-11-04 | lines per state |
| `property_index.csv` | GQ queries with and without a property index, 100 000 nodes | `crates/grafeno-query/examples/index_bench.rs` | E1-mem | 2026-11-03 | paired bars, log scale |
| `concurrent_writes.csv` | 16 writers and 4 readers, with and without group commit | `crates/grafeno/examples/concurrency.rs` | E1-disk | 2026-11-03 | grouped bars |
| `bulk_load.csv` | Imports of 200 000 and 1 000 000 nodes | `crates/grafeno/examples/bulk_load.rs` | E1-disk | 2026-11-03 | bars, nodes per second |
| `algorithms.csv` | Graph algorithms on 100 000 nodes and 500 000 edges | `crates/grafeno/examples/algo_bench.rs` | E1-mem | 2026-11-04 | horizontal bars, log scale |
| `hostile_queries.csv` | 32 hostile GQ queries and the limit that stopped each | `crates/grafeno-query/tests/adversarial.rs` | E1-mem | 2026-11-05 | bars colored by outcome |
| `ram_graph.csv` | Memory and time per operation, 1 M nodes and 10 M edges, 2 GiB and 512 MiB | `crates/grafeno-query/examples/ram_bench.rs` | E2 | 2026-11-05 | steady and peak bars per operation, one panel per container |
| `ram_graph_before_10_6.csv` | The same operations before the changes of 10.6 | `ram_bench.rs` | E2 | 2026-11-05 | before / after |
| `check_cost.csv` | `check` on 10 M edges, three implementations | `ram_bench.rs` | E2 | 2026-11-05 | bars |
| `projection_size.csv` | Projection memory with and without edge ids | `ram_bench.rs` | E2 | 2026-11-05 | bars |
| `vector_insert_progress.csv` | Inserting 1 M vectors of 128 dimensions, per 100 000 | `ram_bench.rs vectors` | E2 (2 GiB) | 2026-11-05 | step seconds against vectors inserted |
| `vector_insert_cache.csv` | 300 000 vectors with a 128 MiB and a 1 GiB vector cache | `ram_bench.rs vectors` | E2 (2 GiB) | 2026-11-05 | grouped bars per 100 000 |
| `vector_search.csv` | 1 000 searches on 1 M and 300 000 vectors | `ram_bench.rs search` | E2 | 2026-11-05 | bars, ms per search and MiB |
| `saturation_immediate.csv` | Write latency against offered load, immediate durability, `max_batch` 128 and 512 | `crates/grafeno/examples/saturation.rs` | E1-disk | 2026-11-05 | p50 / p99 / p999 against offered load (log–log), throughput against offered load |
| `saturation_eventual.csv` | The same with eventual durability | `saturation.rs` | E1-disk | 2026-11-05 | as above |
| `resolve_speed.csv` | Fuzzy index against a scan, and `find_matches`, 100 000 people | `crates/grafeno/examples/resolve_bench.rs` | E1-mem | 2026-11-05 | bars, log scale |
| `resolve_quality.csv` | A match rule's decisions on 2 000 damaged duplicates and 2 000 namesakes | `resolve_bench.rs` | E1-mem | 2026-11-05 | grouped bars of shares per kind |
| `constraint_cost.csv` | 50 000 job changes with and without an edge constraint | `crates/grafeno/examples/constraint_bench.rs` | E1-mem | 2026-11-05 | bars |
| `interchange.csv` | CSV and GraphML export and import of 200 000 records | `crates/grafeno/examples/interchange_bench.rs` | E1-mem | 2026-11-05 | bars, log scale |
| `change_log_cost.csv` | Writes with the change log off, on, and with a small window | `crates/grafeno/examples/changes_bench.rs` | E1-mem | 2026-11-05 | bars |
| `ingest_runs.csv` | NDJSON ingestion over HTTP: first time, and the same lines again | `crates/grafeno-server/examples/events_bench.rs` | E1-mem | 2026-11-05 | bars, log scale |
| `community_refresh.csv` | Refreshes of community summaries: first, unchanged, 1% new nodes | `crates/grafeno-memory/examples/community_bench.rs` | E1-mem | 2026-11-05 | bars |
| `community_levels.csv` | Summaries per level and their purity against planted teams and departments | `community_bench.rs` | E1-mem | 2026-11-05 | table |
| `extract_runs.csv` | Extraction runs: first, unchanged, 1% changed | `crates/grafeno-memory/examples/extract_bench.rs` | E1-mem | 2026-11-05 | bars, log scale |
| `extract_quality.csv` | Links against the known mentions of 20 000 memories | `extract_bench.rs` | E1-mem | 2026-11-05 | a table, or stat tiles |
| `import_interference.csv` | An agent's write latency during a 300 000-node import | `saturation.rs` | E1-disk | 2026-11-05 | bars of p99 per import mode, log scale |
| `planner_plans.csv` | Queries a planner without statistics got wrong, before and after 12.2, and a repeated query | `crates/grafeno-query/examples/planner_bench.rs` | E1-mem | 2026-11-05 | paired bars, log scale |
| `parallel_scan.csv` | A scan of 200 000 people on one thread and on 16, before and after 12.2 | `planner_bench.rs` | E1-mem | 2026-11-05 | bars |
| `read_concurrency.csv` | Read queries per second and latency with 1 to 16 readers, without and with a busy writer, before and after 12.2 (best of three runs) | `planner_bench.rs readers` | E1-mem | 2026-11-05 | queries per second against readers, a line per build and writer |
| `batch_aggregation.csv` | Scans, filters and aggregates over 200 000 people, before and after 12.3 (best of two runs each) | `crates/grafeno-query/examples/batch_bench.rs` | E1-mem | 2026-11-05 | paired bars, log scale |
| `traversal.csv` | Traversals from many starts (two hops, friends of friends, variable length), time and records read, before and after 12.4 | `crates/grafeno-query/examples/traversal_bench.rs` | E1-mem | 2026-11-05 | paired bars, log scale |
| `vector_search_int8.csv` | 1 000 searches on 1 M vectors in 512 MiB and 2 GiB, 12.4 against 12.5's int8 copies, four runs each | `ram_bench.rs search` | E2 | 2026-11-06 | paired bars per container, best and worst run |
| `replication_log.csv` | Writes with the replication log off and on, its size and a replica's apply speed (13.1) | `crates/grafeno/examples/replication_bench.rs` | E1-mem | 2026-11-06 | bars, or a table |
| `cluster.csv` | Sequential writes on one server and through a three-member Raft cluster, and failover times (13.2) | `crates/grafeno-cluster/examples/cluster_bench.rs` | E1-disk | 2026-11-06 | a table |
| `auth.csv` | The cost of Argon2id passwords: a hash, a check, sign-ins a second on one core and on all (14.1) | `crates/grafeno-server/examples/auth_bench.rs` | E1-mem | 2026-11-06 | a table |
| `audit.csv` | The audit log: events appended one write each and a thousand a write, read back, and an account change with its event (14.5) | `crates/grafeno/examples/audit_bench.rs` | E1-mem | 2026-11-06 | a table |
| `visibility.csv` | Reads without a view, with a view of the whole namespace and with one decided per record, and vector recall with 45% visible (14.4) | `crates/grafeno-query/examples/visibility_bench.rs` | E1-mem | 2026-11-06 | a table, or paired bars |
| `rights.csv` | The cost of roles: 5 000 roles written, an account's rights computed, a namespace check and a label or property check (14.2) | `crates/grafeno/examples/rights_bench.rs` | E1-mem | 2026-11-06 | a table |
| `drill.csv` | Cluster runbooks on the compose under load: operations, availability, longest outage, linearizability (13.6) | `packaging/compose/drill.sh`, `crates/grafeno-server/examples/drill.rs` | E1-disk | 2026-11-06 | a table |
| `groups.csv` | Write throughput of three members with 1, 2 and 3 Raft groups, 24 clients (13.5) | `crates/grafeno-server/examples/groups_bench.rs` | E1-disk | 2026-11-06 | a table |
| `namespace_move.csv` | Namespaces moved between groups under writes: copy, catch-up rounds, the pause, writes found (13.5) | `groups_bench.rs`, `crates/grafeno-server/tests/groups.rs` | E1-disk | 2026-11-06 | a table |
| `split.csv` | Reads on one server and split across 3 and 5 members (3 scan threads each), and one server with 9 and 15 threads (13.4) | `crates/grafeno-server/examples/split_bench.rs` | E1-disk | 2026-11-06 | a table |
| `faults.csv` | Fault-injection runs: operations checked for linearizability, time for writes to resume after the leader fails, availability (13.3) | `crates/grafeno-cluster/tests/linearizable.rs`, `crates/grafeno-server/tests/faults.rs` | E1-disk | 2026-11-06 | a table |
| `compact_storage.csv` | Table sizes, edge reads and vector search before and after 12.5 (layout 13), 100 000 nodes, 300 000 edges, 50 000 vectors | `crates/grafeno/examples/storage_bench.rs` | E1-disk | 2026-11-05 | paired bars per table |
| `recall_quality.csv` | BM25, BM25 stemmed, HNSW, hybrid and hybrid with the graph on 70 agent questions of four kinds | `crates/grafeno-memory/examples/recall_quality.rs` | E1-mem | 2026-11-20 | grouped bars per kind: first, in 3, in 10 |
| `long_term.csv` | Old decisions among 3 000 and 15 000 conversations, six ways of recalling | `crates/grafeno-memory/examples/long_term.rs` | E1-mem | 2026-11-20 | grouped bars, in the context, per size |
| `many_writers.csv` | Agents writing memories at once on 1, 2 and 4 files (writers) | `crates/grafeno/examples/many_writers.rs` | E1-disk | 2026-11-20 | memories/s against agents, a line per number of writers |
| `hyperedge_index.csv` | `MATCH HYPEREDGE` by a property on 100 000 hyperedges, without and with an index, beside event nodes | `crates/grafeno-query/examples/hyperedge_index_bench.rs` | E1-mem | 2026-11-21 | paired bars, log scale |
| `hyperedge_recall.csv` | Recall strategies with and without hyperedges on 92 questions of six kinds, English and Spanish | `crates/grafeno-memory/examples/hyperedge_recall_eval.rs` | E1-mem | 2026-11-21 | grouped bars per kind: complete, answered in role, tokens |
| `hyperedge_recall_latency.csv` | Latency of those strategies, 15 runs per question | `hyperedge_recall_eval.rs` | E1-mem | 2026-11-21 | bars, p50 / p95 / p99 |
| `entity_search.csv` | GQ lookups, sorts and pages over 1 000 000 `Person` nodes, before and after the entity lookups | `crates/grafeno-query/examples/entity_search.rs` | E1-disk | 2026-11-26 | paired bars per query and indexes, log scale |
| `rewire.csv` | Rewiring 1 000 000 edges with `CALL { … } IN TRANSACTIONS` against one transaction, with an agent's write latency and a reader's | `crates/grafeno-query/examples/rewire.rs` | E1-disk | 2026-11-26 | bars of agent p99 and max per mode, log scale |
| `many_connections.csv` | 1 000 to 4 000 HTTP connections at once, reads, GQ and writes mixed, before and after writes give their turn back; and with 1 024 open files | `crates/grafeno-server/examples/many_connections.rs` (`tools/bench-http/Dockerfile`) | E2 (musl, mimalloc, no memory limit) | 2026-11-26 | requests per second and p99 per connections, before / after |

`python docs/charts/build.py` draws the charts of
[performance](../performance.md) from these files (light and dark SVG in
`docs/assets/charts/`). To add a run, keep the columns, add rows with a new date (or a new file
for a new environment), and say in `DESIGN.md` what changed.
