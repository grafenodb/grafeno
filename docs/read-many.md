# Batched node and edge reads

`POST /v1/{namespace}/read-many` accepts optional `nodes` and `edges` UUID lists,
with **512 combined positions** at most, including duplicates. One read
transaction retrieves both lists. Nodes include the same text and memory fields
as `GET /nodes/{id}`.

```ts
const records = await vault.readMany({
  nodes: [ana.id, bob.id, ana.id],
  edges: [relationship.id],
});
```

Each output list preserves its input order and duplicates. An ID missing from
that namespace produces `null` without interrupting the rest. Omitted lists are
empty. Invalid UUIDs, unknown fields and more than 512 positions return HTTP
400. Namespace read permission is required; IDs from another namespace reveal
no records. Retracted edges are returned as with individual reads by ID.

Rust provides `ReadTx::nodes_by_id(ns, ids)` and `ReadTx::edges_by_id(ns, ids)`.
The 512-position limit belongs to HTTP, not these methods. Reuse one `ReadTx`
to read nodes and edges from the same snapshot.

The operation neither modifies records nor records memory accesses. It is
separate from the writing `/batch` API. Filtered searches and neighborhood
traversals keep their existing APIs; `read-many` retrieves records whose IDs
are already known.

## Reproduce the measurement

The script starts and stops a private in-memory server on a system-assigned
loopback port. It does not use the user's database or server. It creates 256
nodes with text and 256 edges, then compares sequential individual requests,
at most eight concurrent requests and a single batch request. It verifies equal
results outside the timed interval, warms up each strategy three times and
rotates their order for 25 rounds.

```bash
cargo build --locked -p grafeno-server
node clients/typescript/bench/read-many.mjs
```

`GRAFENO_BIN` selects the executable; `GRAFENO_BENCH_ROUNDS` changes the number of
rounds. Output is JSON with medians, p95 and environment details. In PowerShell,
set these options with `$env:GRAFENO_BIN` and `$env:GRAFENO_BENCH_ROUNDS`.

Median milliseconds measured on November 6, 2026, Linux x64, Intel i7-10750H,
Node 24.22.2, using the repository's `dev` profile:

| Nodes + edges | Individual requests | Sequential | Concurrency 8 | Batch (1 request) |
| --- | ---: | ---: | ---: | ---: |
| 1 + 1 | 2 | 0.976 | 0.735 | 0.606 |
| 16 + 16 | 32 | 8.544 | 4.770 | 0.590 |
| 64 + 64 | 128 | 28.755 | 15.319 | 1.020 |
| 256 + 256 | 512 | 114.492 | 55.404 | 3.179 |

[Full results, p95 and environment](data/read-many-local.json).

This measures local HTTP, JSON and client work with an in-memory database. It
does not measure Tauri, disk, cold caches, concurrent writes or larger text
payloads. Individual requests use independent snapshots; no writes occur during
the benchmark. The batch provides a shared snapshot. These timings are neither
guarantees nor CI thresholds. Fewer requests explain part of the gain; this
does not demonstrate an equivalent speedup inside the Rust engine.

## Scope and next decision

The engine visits IDs within one snapshot; this change does not create a thread
pool for each batch. Concurrency eight is a benchmark comparison strategy.
Reducing UI-to-engine crossings comes first; internal parallelism needs separate
measurements under load, on disk and with memory bounds before adoption.

Studio now batches expansion neighborhoods through a separate operation,
`neighborhoods`. That workload differs from `read-many`; see the
[graph expansion guide](graph-expansion.md) for its contract and measurement.

## Tests

```bash
cargo test --locked -p grafeno --test read_many
cargo test --locked -p grafeno-server --test http read_many
cargo test --locked -p grafeno-server --test access
npm run build --prefix clients/typescript
npm test --prefix clients/typescript
```

Tests cover order, duplicates, missing records, isolation, limits and a snapshot
that retains nodes and edges after later writes. The window test also calls
`read-many` through the Tauri bridge.
