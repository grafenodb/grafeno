# Batched graph expansion

[Español](expansion-grafo.md) | **English**

Studio loads expanded roots in groups of eight: 18 roots require three requests
(8, 8 and 2), instead of 18. Root order, temporal filters and the existing visual
limit of 150 newly added nodes per expansion are preserved. Shared nodes and
edges are deduplicated while merging results in the original root order.

## API and consistency

`POST /v1/{namespace}/neighborhoods` accepts `seeds`, `direction` and `filter`.
It returns an ordered subgraph for each root, equivalent to
`subgraph([root], {depth: 1, direction, filter})`, including edges among its
members. A union of all roots would lose the separation needed for per-root
limits, so the response retains individual neighborhoods.

```ts
const around = await vault.neighborhoods([ana.id, bob.id], {
  filter: { valid_at: "2026-06-01" },
});
```

Zero through eight roots are accepted, including duplicates. Depth is always
one; `depth` is not an input field. Too many roots, invalid IDs and unknown
fields return 400. A missing root returns 404 for the whole batch, matching the
previous individual read. Namespace read permission is required.

Each group shares one snapshot. Expansions exceeding eight roots use sequential
groups and may observe writes between them; the entire canvas is not a global
snapshot. The engine visits roots without creating a thread pool per request.
`read-many` still retrieves known IDs; `neighborhoods` discovers their neighborhoods.

## Pending replies and older servers

Changing roots, date, namespace or revision invalidates the previous visual load.
Clearing also invalidates pending searches and “Show all” lists. Late replies
neither publish stale data or errors nor start another batch. Already dispatched
requests may finish; this discards results and prevents later work rather than
promising engine interruption.

If an older server returns a generic 404/405 for the new endpoint, Studio falls
back to individual `subgraph` calls, with at most four concurrent requests per
load and ordered merging. Authentication failures, forbidden access, internal
errors and structured missing-root errors do not trigger fallback. Failures stop
unsent roots. Requests from an obsolete load may overlap a newer load; four is
not a global limit across all loads.

The 150-node limit is visual. It neither truncates server traversal nor guarantees
that only 150 records are read: a high-degree hub may return many more. This
change preserves that existing behavior.

## Local measurement

The benchmark now also compares neighborhoods using a private server with 256
nodes and 256 ring edges. Each strategy gets three warmups and 25 rounds in
rotating order. Equal results are checked outside the timed interval.

```bash
cargo build --locked -p grafeno-server
node ../grafeno-sdk/typescript/bench/read-many.mjs   # the SDK checked out next to the engine
```

The `neighborhoods` JSON section holds this comparison. `GRAFENO_BIN` selects
the executable; `GRAFENO_BENCH_ROUNDS` selects rounds. CI runs one functional
round without performance thresholds.

Median milliseconds measured November 6, 2026, Linux x64, Intel i7-10750H,
Node 24.22.2 and the `dev` profile:

| Roots | Sequential | Concurrency 4 | Batches of 8 | Batch requests |
| --- | ---: | ---: | ---: | ---: |
| 8 | 2.731 | 1.491 | 0.564 | 1 |
| 18 | 5.854 | 3.028 | 1.481 | 3 |
| 60 | 18.645 | 10.226 | 4.434 | 8 |

[Full data, p95 and environment](data/graph-neighborhoods-local.json).

This measures local HTTP and JSON over an in-memory database without writes.
It does not measure rendering, FPS, Tauri, disk, high-degree hubs or real user
workloads. `read-many` timings describe a different workload and must not be
attributed to the canvas. These numbers are not guarantees.

## Validation

```bash
npm test --prefix studio   # in the Grafeno Studio repository
cargo test --locked -p grafeno-server --test http neighborhoods
cargo test --locked -p grafeno-server --test access
npm test --prefix ../grafeno-sdk/typescript
```

Studio tests cover grouping, order, per-root limits, deduplication, compatibility,
errors and cancellation. HTTP tests compare individual and grouped neighborhoods
with directions and temporal filters; the access matrix includes the endpoint.
The Tauri test expands 18 roots in three calls, changes the date and holds real
replies to verify canvas clearing. Reply delays are injected only by automation;
no new testing commands are added to the product. Screenshots use the existing
desktop artifact.
