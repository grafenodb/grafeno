# Phase 4: Foundations and validation

[All phases](README.md) · [← Phase 3: Server](phase-03-server.md) · [Phase 5: Shell and query language →](phase-05-shell-and-query-language.md)

**Status:** done.

Layout versioning, counters, namespaces and statistics, paging, integrity check and rebuild, export/import, property/durability/concurrency tests, benchmarks.

## Administration and validation (phase 4)

- **Introspection**: `namespaces()`, `stats(ns)` (counts per label, texts,
  vectors and their dimension), and paged listings of nodes and edges
  (`after` + `limit`, in id order).
- **Integrity**: `check(ns)` compares every derived structure (label index,
  adjacency, postings, text lengths and statistics, vector metadata, HNSW
  links, counters) with the records, and verifies that every vector can be
  reached from the entry point. `rebuild_indexes(ns)` rebuilds them all from
  the records (node, edge, text and vector records are the source of truth).
- **Export and import**: JSON Lines with a header, then each namespace's
  nodes (with text and embedding) and edges, keeping ids and timestamps.
  Import runs in one transaction and replaces records with the same ids.
- All of this is available from the library, the HTTP API
  (`/v1/namespaces`, `/v1/{ns}/stats`, `/check`, `/rebuild`, `/v1/export`,
  `/v1/import`) and the CLI (`grafeno namespaces | stats | check | rebuild |
  export | import`), which opens the file directly, like `pg_dump`.
- **Tests**: a property test against an in-memory model, a kill-the-writer
  durability test, a concurrency test, corruption tests, benchmarks and a
  recall measurement. Results and bottlenecks: [docs/benchmarks.md](../benchmarks.md).

Node bindings (napi-rs) were left out: Grafito runs Grafeno as a sidecar
container and talks HTTP, so a native addon would add per-platform builds
without a current user. They remain possible for embedded use.
