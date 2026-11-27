# Storage and indexing optimization

**Status: approved on 2026-11-26, with the answers proposed in
[section 14](#14-questions-for-approval); phases 1 to 3 done, phase 4 next.** This
document is its source of truth: every step done or left is recorded
here, in [Progress](#progress), as the work goes on.

The initiative makes the engine's storage smaller and its indexing
cheaper and configurable, on its own terms: redb stays the only store,
BM25, HNSW and GQ stay what they are, and every change keeps the data's
meaning, the graph's shape and the quality of retrieval exactly as they
are. It covers the redb upgrade, versioned and compressed records,
specialized encodings for the indexes, an indexing policy per namespace,
maintenance (compaction) and an official tool that brings existing
databases up to date.

## Contents

1. [Constraints](#1-constraints)
2. [Audit: how storage works today](#2-audit-how-storage-works-today)
3. [Baseline measurements](#3-baseline-measurements)
4. [Findings](#4-findings)
5. [redb: current and target version](#5-redb-current-and-target-version)
6. [Target architecture](#6-target-architecture)
7. [Decisions](#7-decisions)
8. [Phases and tasks](#8-phases-and-tasks)
9. [Migration of existing databases](#9-migration-of-existing-databases)
10. [Testing](#10-testing)
11. [Benchmarks](#11-benchmarks)
12. [Risks](#12-risks)
13. [Acceptance criteria](#13-acceptance-criteria)
14. [Questions for approval](#14-questions-for-approval)
15. [Progress](#progress)

## 1. Constraints

- redb is the only persistence engine; no other storage or search engine
  is added.
- BM25 (node texts and full-text indexes), HNSW (vector indexes) and the
  property indexes keep their algorithms; their **storage** may change.
- GQ stays the query language; every query keeps its results (rows,
  order, scores).
- No subsystem is duplicated: compression is one layer under the tables,
  the indexing policy builds on the existing `IndexDef`, migrations build
  on the existing layout upgrade.
- Every platform and deployment stays: Linux (static musl binaries,
  `.deb`, `.rpm`), Windows (installer, MSIX), the container image,
  Compose and clusters (Raft, groups).
- No lossy encoding by default: no vector quantization as storage, no
  change to graph topology, HNSW links, relations or memories to save
  space.
- Each physical change is versioned and has a migration and a way back.

## 2. Audit: how storage works today

Verified in the code of `main` at `8241baf` (2026-11-26).

### 2.1 Engine and files

| Item | Where | What it is |
| --- | --- | --- |
| redb | `Cargo.toml` (`redb = "4"`), `Cargo.lock` | **4.4.0**; one file per database (plus `<db>.group-<id>.redb` per Raft group) |
| Layout version | `crates/grafeno/src/db.rs` (`SCHEMA_VERSION` 19, `BASE_LAYOUT` 18) | in `meta` under `schema_version`; a file newer than the build is refused; older files are upgraded in place at open (`migrate_to_3`, `migrate_to_4`, `migrate_to_6`, `quantize_vectors`, `upgrade_principals`), after a copy (`Grafeno::copy_before_upgrade`, `<name>.layout-<old>.redb`) |
| Rust | `Cargo.toml` `rust-version = "1.88"`; CI pins 1.99.1 | redb 4.4 already declares 1.90: the declared minimum is stale (finding F12) |
| Page cache | `db.rs` `DEFAULT_CACHE` | 256 MiB (`[memory] cache_mb`) |

### 2.2 Tables and their encodings

| Table | Key | Value | Encoding | Where |
| --- | --- | --- | --- | --- |
| `nodes`, `edges` | `(ns: &str, id: u128)` | record | JSON (`serde_json`) of `Node` / `Edge`, ids as UUID strings, times as numbers | `store.rs` `encode`/`decode` |
| `hyperedges` | `(ns, id)` | record | JSON | `hyperedge.rs`, `hyperedge_codec.rs` |
| `adjacency_out` / `_in` | `(ns, node, label: &str, edge)` | other end and times | **compact**: flag byte, 16-byte end, zigzag varints (layout 13) | `store.rs` `Adj` |
| `node_labels` | `(ns, label, node)` | `()` | | `store.rs` |
| `names` | `(ns, normalized name, node)` | `()` | entity resolution | `store.rs`, `resolve.rs` |
| `texts` | `(ns, node)` | `&str` | the node's text, raw | `store.rs` |
| `text_lengths`, `text_postings`, `text_stats` | `(ns, term, node)` → `(tf, len)` | BM25 of node texts | one entry per term and node; the length repeated in each | `text.rs` |
| `indexes` | `(ns, name)` | `IndexDef` | JSON | `index.rs` |
| `index_entries` | `(ns, index name: &str, bytes, id: u128)` | `()` | property entries: order-preserving value bytes; **full-text** entries (tag 5): term, field, field length, frequency and up to 64 positions, all in the key | `index.rs`, `fulltext.rs` |
| `fulltext_stats` | `(ns, index)` | documents and terms per field | JSON | `fulltext.rs` |
| `vectors` | `(ns, node)` | `f32` little endian, **normalized** for cosine | | `vector.rs` |
| `vectors_q8` | `(ns, node)` | scale + one `i8` a dimension | an auxiliary copy for walking the graph; searches rescore with the exact vectors | `vector.rs` |
| `hnsw_links` | `(ns, node, layer)` | neighbor ids, 16 bytes each | | `vector.rs`, `vindex.rs` |
| `vector_meta` | `ns` | `VectorMeta` | JSON | `vector.rs` |
| `memory` | `(ns, node)` | importance, accesses, last access | tuple | `memory.rs` |
| `changes` | `(ns, seq)` | `Change` | JSON; retention 1,000,000 entries or 7 days per namespace | `changes.rs` |
| `replication_log` | seq | `Entry` (logical effects) | JSON | `replication.rs` |
| `audit` | seq | event | JSON | `audit.rs` |
| `state` | `(ns, kind, key)` | bytes | per feature (idempotency, import ids, aliases, directory) | `state.rs` |
| `namespaces`, `namespace_counts`, `label_counts`, `edge_label_counts`, `meta` | | | JSON / tuples / counters | `namespace.rs`, `stats.rs` |
| `accounts`, `api_keys`, `sessions`, `roles` | | | JSON | `access.rs`, `rights.rs` |
| `merges`, `match_rules`, `constraints` | | | JSON | `merge.rs`, `resolve.rs`, `constraints.rs` |

Every key of every per-namespace table starts with the namespace's name
as a string, and the property and full-text index keys also hold the
index's name: B-tree leaves do not share prefixes, so these strings are
stored once per entry.

### 2.3 Indexes

- **Property indexes** (`index.rs`): declared with `CREATE INDEX`
  (single, composite, several labels, on edges and hyperedges), kept in
  the same transaction as every write, rebuilt by `rebuild`, checked by
  `check`. Values are bytes in sort order (booleans, numbers, strings).
  Planner statistics (`planstats.rs`) are built from snapshots and kept
  in memory. Since 4556f6d a sort can read an index in order
  (`find.rs`).
- **Full-text indexes** (`fulltext.rs`): declared with `CREATE FULLTEXT
  INDEX`, per-field BM25, analyzers (English, Spanish), phrases, fuzzy and
  prefix terms (term lists cached per commit).
- **Node text** (`text.rs`): every node with a `text` is indexed for
  BM25 without a declaration (`text_postings`), stemmed in the
  namespace's language.
- **Vector indexes** (`vector.rs`, `vindex.rs`): HNSW over normalized
  `f32`, int8 copies for the walk, built in the background in chunks;
  since 0.5.3 also on `@embedding`.
- **Names** (`names`): every node's normalized names, for resolution.
- **Structural** tables (`node_labels`, adjacency, hyperedge incidence)
  are indexes the graph needs; no policy may turn them off.

There is **no automatic indexing of properties**: an unindexed property
is found by scanning its label.

### 2.4 Writes, concurrency and durability

- One write transaction at a time (redb). The server's `Writer`
  (`writer.rs`) group-commits interactive writes (up to 512 a
  transaction) and runs bulk writes one per transaction between them
  (imports, ingestion, `CALL { … } IN TRANSACTIONS`, and since 8241baf CSV
  and GraphML imports), bulk commits with eventual durability and a flush
  at the end.
- Reads are snapshots; large scans split across threads
  (`grafeno-query/src/parallel.rs`).
- A snapshot held open while many commits happen keeps redb from reusing
  pages: measured as a 15 s stall (`docs/benchmarks.md`, "Rewiring edges
  in bulk").
- Clusters replicate **logical** effects (`Op::Node { node }`, …), not
  pages: each member encodes its own file. Layout changes can therefore
  roll member by member, as long as the replication entry format
  (`ENTRY_FORMAT`) stays compatible.

### 2.5 Maintenance and recovery

| Operation | Where | What it does |
| --- | --- | --- |
| `grafeno vacuum` | `backup.rs` `Grafeno::vacuum` | `Database::compact()` until nothing moves, with the server stopped: gives back free pages |
| `POST /v1/{ns}/compact` | `check.rs` `compact_vectors` | rebuilds a vector graph worn by removals |
| `POST /v1/{ns}/rebuild` | `check.rs` | rebuilds the namespace's indexes |
| `GET /v1/{ns}/check` | `check.rs` | verifies counters and index entries against records |
| `GET /v1/backup`, `grafeno backup` | `backup.rs` | a consistent copy of every table |
| export / import | `dump.rs` | JSON Lines, the logical format (version 3); CSV and GraphML in `interchange.rs` |
| `examples/table_sizes.rs` | `crates/grafeno/examples` | stored, metadata and fragmented bytes per table |
| `grafeno migrate` | `crates/grafeno-server/src/migrate.rs` | **a different migration**: copies a Windows setup's database into the user's settings (the Store package). The name is taken (finding F11). |

### 2.6 Memory, palaces and history

- Agent memory: nodes (`Memory`, `Decision`, …) and facts (edges with
  valid and record times), the `memory` table (importance, accesses), the
  change log, checkpoints (`grafeno-memory/src/checkpoint.rs`).
- Mental palaces (`grafeno-memory/src/palace.rs`): `Palace`, `Room` and
  `Locus` nodes and their edges; no table of their own. Whatever is done
  to node and edge storage applies to them unchanged.
- History: edges keep `valid_from`/`valid_to` and `recorded_at`/
  `expired_at`; functional properties keep earlier values; the change log
  keeps recent commits.

## 3. Baseline measurements

A synthetic catalogue on a file (E1-disk, release build of `8241baf`,
2026-11-26): 100,000 `Entity` nodes with a code, a title, a 230-character
Spanish definition, four more properties and a 64-dimension embedding; a
child-of hierarchy (99,974 edges); a property index on `code`, a
full-text index on title and definition (Spanish), a vector index on
`@embedding`. Loaded with GQ `IN TRANSACTIONS` (74 s), then measured with
`examples/table_sizes.rs`:

| Table | Entries | Stored MiB | Metadata MiB | Fragmented MiB |
| --- | ---: | ---: | ---: | ---: |
| `index_entries` (full-text and `code`) | 2,499,963 | 139.0 | 15.8 | 137.3 |
| `changes` | 299,974 | 52.4 | 3.0 | 1.5 |
| `nodes` | 100,000 | 49.2 | 1.5 | 6.5 |
| `hnsw_links` | 106,676 | 33.3 | 1.5 | 13.9 |
| `edges` | 99,974 | 28.4 | 1.2 | 1.2 |
| `vectors` | 100,000 | 26.7 | 1.1 | 0.8 |
| `vectors_q8` | 100,000 | 8.8 | 0.9 | 0.3 |
| `adjacency_in` / `_out` | 99,974 each | 7.1 each | 0.9 each | 0.3 each |
| `names` | 100,000 | 5.9 | 0.6 | 3.6 |
| `node_labels` | 100,000 | 3.0 | 0.4 | 0.1 |
| **all tables** | | **360.7** | **27.9** | **165.9** |
| **file** | | | | **1,028.0 MiB** |

- `grafeno vacuum` took 3.2 s and left **554.8 MiB** (473 MiB of free
  pages given back); the fragmentation inside pages (166 MiB) stayed.
- A node record averages **518 bytes** of JSON (without its vector), an
  edge record **274**. Compressed one by one with deflate they keep 66%
  and 71% of their size; with a dictionary shared by the records, 14%
  and 31%. The data is synthetic and repetitive: the dictionary figure is
  an upper bound to confirm on real catalogues, not a promise.
- The full-text index is 25 entries a document and 58 bytes an entry,
  plus as much again fragmented: the largest structure, ahead of the
  records.

These numbers are the first line of the baseline (phase 1 makes it a
repeatable benchmark on several datasets). Its embeddings repeat: a
hundred distinct vectors, a thousand copies of each, which leaves the
HNSW graph unable to reach most of them (`check` says so; HNSW cannot
keep copies apart). The phase 1 suite uses distinct vectors.

## 4. Findings

| # | Finding | Evidence | Weight |
| --- | --- | --- | --- |
| F1 | The file holds twice what its tables store: free pages are not given back while the server runs. | 1,028 MiB file, 361 MiB stored, 555 MiB after `vacuum` | high |
| F2 | The full-text index is the largest structure: one B-tree key per term, field and node, with the namespace and index names and up to 64 positions inside each key; half its space again is fragmented. | 139 + 137 MiB for 100,000 documents | high |
| F3 | Node and edge records are JSON with ids as 36-character strings and property names repeated in every record. | 518 B a node, 274 an edge; 14–31% with a shared dictionary | high |
| F4 | Every per-namespace key repeats the namespace's name (and index keys the index's name). | `store.rs` key types; about 11 to 25 bytes an entry, 3.3 M entries here | medium |
| F5 | The change log stores JSON entries; with defaults it keeps up to a million a namespace. | 52 MiB for 300,000 entries (183 B each) | medium |
| F6 | HNSW links store each neighbor as a 16-byte id. | 33 MiB of links for 100,000 vectors of 64 dimensions, more than the vectors (27 MiB) | medium |
| F7 | Fragmentation inside pages (166 MiB) is not reclaimed by `compact()` in redb 4; redb 5 adds `CompactionMode::Maximum` for it. | `table_sizes`, redb 5.0 changelog | medium |
| F8 | There is no automatic property indexing: a query on an unindexed property scans its label. | `index.rs`; 4556f6d's measurements (0.5 s for 1 M nodes) | medium |
| F9 | Vectors are stored normalized in cosine spaces, so their original length is not kept. Lossless vector compression can only preserve what is stored today. | `vector.rs` ("Vectors are normalized on insert") | note |
| F10 | Text values are a `&str` table (`texts`): it cannot hold compressed bytes without a new table. | `store.rs` | note |
| F11 | `grafeno migrate` already names the Store-package copy; the new tool needs another name or sub-commands that keep the current meaning. | `migrate.rs` | decision |
| F12 | The declared minimum Rust (1.88) is below what redb 4.4 and 5.0 declare (1.90). | `Cargo.toml`, `cargo info redb` | low |
| F13 | Records are decoded whole even when a traversal reads no property: half the time of a read by id. | `docs/benchmarks.md`, "Record encodings" | medium |
| F14 | Values of the record tables start with `{` (JSON objects): a first byte below `0x20` can tag a new encoding with no ambiguity, so old and new records can live side by side. | `store.rs` `encode` | enabler |
| F15 | Replication carries logical records, so members may store them differently; only a change of `Entry` needs the whole cluster upgraded first. | `replication.rs` | enabler |

What already exists and is reused rather than built again: the layout
upgrade with a copy kept first (`db.rs`), `rebuild` and `check`, the
compact adjacency encoding (`store.rs` `Adj`, the model for compact
encodings), `vacuum`, `table_sizes`, the bulk lane and `IN
TRANSACTIONS` for batched work, the background index builder
(`vindex.rs`, `build_indexes`), `IndexDef` (every policy decision becomes
one), planner statistics, and export/import as the logical escape hatch.

## 5. redb: current and target version

- **Used**: 4.4.0. **Latest stable**: 5.0.0 (2026-11-18). The `0.5.0`
  given as a reference for this initiative is far older than the version
  in use: installing it would be a downgrade, and it is not proposed.
  **Proposed target: 5.0.0** (or the latest 5.x when phase 2 starts).
- What 5.0 changes for Grafeno:
  - `Option<&str>`, `Option<String>`, `Option<bool>` and `Option<char>`
    are encoded differently; tables created before with them fail to open.
    **Grafeno's table types use none of them** (`grep` of every
    `TableDefinition`; the one custom `Value`, `Adj`, encodes no
    `Option`), so no table needs `Legacy<T>`. Phase 2 proves it with the
    layout fixtures (`crates/grafeno/tests/fixtures/layout-v*.redb.gz`).
  - API: `ReadOnlyTable::get`/`range` inherent methods removed (use
    `ReadableTable`), ranges borrow their table, `range`/`retain_in`/
    `extract_if` take a `KeyRange`, `compact()` takes a `CompactionMode`,
    `FileBackend::new` and `TableError` variants changed. Mechanical but
    wide (about 30 snapshot fields in `ReadTx`, every range).
  - `CompactionMode::Maximum` reclaims space inside pages (F7).
  - File locking uses byte-range locks only: on Linux, redb 4.2 or older
    must not open a file a 5.x process has open. Grafeno 0.5.x ships
    4.4; a mixed install on one machine must not share a file.
  - 4.5 and 4.6 (between): concurrency modes (`SingleWriter` with
    read-only processes), `try_begin_write`, `Database::close`,
    integrity checks on open, smaller new files, several repair and
    `check_integrity` fixes.
- Minimum Rust: 1.90 (raise `rust-version` from 1.88; CI already 1.99.1).
- The physical file format is unchanged (redb's file format 3 in both):
  files written by 4.4 open with 5.0, and files written by 5.0 open with
  4.4. Verified in phase 2 with the 0.5.3 build and this one, both ways,
  `check` clean each time, so the layout version stays as it is.

## 6. Target architecture

### 6.1 Versioned values (the envelope)

Every value of the record tables (`nodes`, `edges`, `hyperedges`,
`changes`, `replication_log`, `audit`, `state` values that are records)
may start with an envelope:

```text
byte 0      tag (< 0x20; a JSON object starts with `{` = 0x7B)
byte 1      encoding version
byte 2      codec: 0 none, 1 LZ4, 2 Zstandard, 3 Zstandard with dictionary
varint      original length (bounded: larger values are refused as corrupt)
[varint]    dictionary id (codec 3)
payload     the encoded record, compressed or not
4 bytes     CRC-32C of the decoded record (optional per table)
```

A value without the envelope is today's JSON. Readers decode both, so a
file can hold old and new records at once: new writes use the new
encoding, old ones are rewritten by the migration, in batches, or never.
Keys are **never** compressed (they are what ranges and lookups sort
by); key changes are separate and need a rebuild (6.4).

### 6.2 Adaptive compression

- On by default; per table and per value: a value under a threshold
  (proposed 128 bytes) is stored plain; a larger one is compressed and
  kept compressed only if it saves at least a margin (proposed 12%).
  Otherwise it is stored plain with the envelope (so the choice is
  recorded).
- Codecs: LZ4 for records read on hot paths (nodes, edges, hyperedges),
  Zstandard for large or cold ones (texts, change log, audit,
  replication log). Each decided by benchmark, not assumed.
- Dictionaries: per namespace and record kind, trained on a sample of
  its records (Zstandard dictionaries), stored in a `dictionaries` table
  with an id; a record names the dictionary that compressed it, so a new
  dictionary never makes old records unreadable. Training happens in the
  migration or in the background, not on the write path.
- Limits: a decoded size limit per value, refusal of corrupt or
  oversized envelopes (`check` reports them), a CPU budget measured per
  operation in the benchmarks.
- Configuration: `[storage] compression = "auto" | "off"`, per-table
  overrides for tests and benchmarks; `off` writes plain values, and both
  are always readable.

### 6.3 Records: a binary header, lazy properties

Following `docs/benchmarks.md` ("Record encodings"): a record becomes a
binary header (ids as 16 bytes, times as varints, labels as dictionary
ids or strings) followed by its properties, encoded once and decoded only
when read (`Node` keeps the raw properties until a property is asked
for). This answers F3 and F13 together; it is one encoding version of
the envelope, so it ships with 6.1 and needs no further layout change.
The logical `Node` and `Edge` types, the API and GQ do not change.

### 6.4 Index storage (key changes)

- **Full-text** (F2): one entry per term and node instead of per term,
  field and node, with frequencies, lengths and positions per field in
  the value as varints (positions delta-encoded), the key compact
  (namespace and index by numeric ids, see 6.5). Scoring stays BM25 per
  field with the same statistics; a rebuild writes the new entries and
  `check` compares results. Posting blocks (many nodes a key, delta and
  bit packing) save more but rewrite a block on every change: kept as a
  later option (phase 9b) only if the benchmarks justify the write cost.
- **Node text postings**: the repeated length leaves the posting (it is in
  `text_lengths`), and the value becomes a varint.
- **HNSW links** (F6): neighbors as dense per-index numbers (a `u32`
  assigned when a vector is inserted, a mapping table both ways) instead
  of 16-byte ids, varint lists; same graph, same neighbors, same order.
- **Vectors**: kept as `f32` (lossless). A byte-shuffled Zstandard
  variant is measured; adopted only if it saves more than it costs on
  searches that miss the page cache. `vectors_q8` stays an auxiliary
  copy, as today.

### 6.5 Names as numbers in keys (F4)

A `catalog` table maps namespace, label and index names to numeric ids
(`u32`), assigned in the write that first uses them, replicated as
logical effects (each member derives its own ids, so they never travel
in replication entries). Keys hold the id instead of the string.
This touches every table and every key: it is the most expensive change
and the last one (phase 12, P2), done only if the measured saving after
phases 5–10 still justifies it.

### 6.6 Namespace indexing policy

Kept in the namespace's metadata (`NamespaceMeta`, already JSON):

```json
{"indexing": {"mode": "automatic"}}
{"indexing": {"mode": "manual"}}
{"indexing": {"mode": "hybrid",
              "exclude": ["rawContent"],
              "labels": {"Document": {"properties": {"title": ["exact", "fulltext"], "createdAt": ["range"]}}}}}
```

- **automatic** (default for new namespaces): when a write gives a label
  a property it has never had, the policy decides from the value:

  | Value | Index |
  | --- | --- |
  | string up to 256 characters | exact (property index; also serves prefixes) |
  | longer string | full-text (one index per label, fields added as found) |
  | integer, float | exact and range (the same property index: values are ordered) |
  | ISO date string | exact and range (strings order as times) |
  | boolean | exact (low-cardinality: the planner already weighs it) |
  | list of scalars | none at first (needs list entries, phase 7) |
  | map | none (nested properties are not indexed) |
  | `@embedding` | vector index when a dimension is known (explicit or from the first vector) |
  | other numeric lists | **never** a vector index by inference |
  | null | nothing |

  Never a uniqueness constraint, never two indexes for one property, at
  most a configurable number of automatic indexes per label (proposed
  32), and never on structural or internal properties. Each decision is
  an ordinary `IndexDef` marked `origin: auto`, created through the
  existing background builder, so it is visible in `SHOW INDEXES`,
  replicated, exported and droppable like any other.
- **manual**: nothing automatic; indexes come from `CREATE INDEX` or from
  an `indexes` member of a write (`{"props": {…}, "indexes": {"title":
  ["exact", "fulltext"]}}`), which declares them as if `CREATE INDEX` had
  been run.
- **hybrid**: automatic, with exclusions and explicit rules.
- **Precedence** (first that applies wins): structural indexes (always);
  an explicit rule for the label and property; an exclusion; an explicit
  rule for the property in any label; the automatic decision; nothing.
- **Life cycle**: creating, building (in the background, with progress
  and `state`), dropping, rebuilding and changing the mode reuse today's
  index machinery. A new index is not used by queries until it is built
  (`state: online`), as today. Changing the mode never drops existing
  indexes nor builds new ones silently: it applies to properties seen
  from then on, and the migration (9) applies it to existing data on
  request.
- **Existing namespaces** (no policy stored) read as `automatic` for new
  properties after the upgrade; their existing data is indexed only by
  the migration's `--reindex-existing`. Because automatic indexing costs
  writes and space, phase 6 measures it before it is made the default
  for existing namespaces (question Q3).
- GQ and HTTP: `CREATE NAMESPACE x OPTIONS {indexing: {...}}`, `ALTER
  NAMESPACE x SET INDEXING {...}`, `PATCH /v1/namespaces/{ns}` with
  `indexing`, `SHOW INDEXES` with `origin`.

### 6.7 Cheaper indexing

- Entry diffs: on an update, compute the old and new entries of each
  index once and write only the difference (today a property change
  recomputes entries per index; to be confirmed and measured in phase 3).
- Tokenize a text once per field and reuse it for every full-text index
  of the node; reuse buffers across records of a transaction.
- Bulk paths (`import`, `IN TRANSACTIONS`, builders) sort entries before
  inserting them into redb (sequential inserts fill pages and fragment
  less: F1, F2).
- No asynchronous indexing that changes visibility: index entries stay
  in the record's transaction, except background builds of new indexes,
  which already exist and are not used until complete.

### 6.8 Compaction and maintenance

- Distinguished explicitly: compression (6.2), page reuse (automatic in
  redb), fragmentation inside pages, compaction (moving pages to shrink
  the file), index rebuilds.
- `grafeno vacuum` keeps its meaning (exclusive, server stopped) and gains
  `--maximum` (redb 5 `CompactionMode::Maximum`).
- Online: redb's `compact()` needs no open transactions; the server can
  run it in a maintenance window it owns: stop taking writes for the
  duration (requests wait in the admission queue, as they do now for a
  turn), wait for snapshots to end, compact, resume. Proposed as
  opt-in (`[maintenance] compact = "off" | "window"` with a window, a
  threshold of reclaimable space and a time limit), never by surprise.
- Reclaimable space is reported from redb's statistics: free pages and
  fragmented bytes (exact, `table_sizes`), as an estimate before running.

### 6.9 Observability

`GET /v1/storage` (owners) and `grafeno storage stats`, plus metrics:
per table and per namespace (where keys allow a range count) entries,
stored, metadata and fragmented bytes, free pages, file size, compression
ratio per table (from a sampled count of envelopes), encodings in use,
dictionary ids, pending migrations, index sizes by kind, last compaction
and what it gave back. Exact figures are marked exact; per-namespace
sizes of shared tables are estimates from samples and marked so.

### 6.10 The migration tool

`grafeno storage status | plan | apply | verify | resume` (name per Q2),
the logic in the engine (`grafeno::migrate`), the CLI and an admin route
(`POST /v1/storage/migrations`) calling it, scripts only wrapping the CLI.
Detailed in [section 9](#9-migration-of-existing-databases).

## 7. Decisions

| # | Decision | Why |
| --- | --- | --- |
| D1 | Target redb 5.0.0, not 0.5.0. | 0.5.0 is older than 4.4.0; 5.0 is the latest stable and brings `CompactionMode::Maximum`. |
| D2 | Envelope with a tag byte below `0x20` on values only. | Old JSON stays readable (F14): no forced rewrite, migrations in batches, a mixed file is valid. |
| D3 | Keys are never compressed; key shapes change only with a rebuild of the index they belong to. | Ordered keys are what ranges and seeks rely on. |
| D4 | Lossless only by default; vectors stay `f32`. | Retrieval quality and the original data are preserved; `vectors_q8` stays an auxiliary copy. |
| D5 | Records: binary header and lazy properties before general-purpose compression on hot paths. | Measured: decoding properties is half a read; compression alone would add CPU to every read. |
| D6 | Compression per value with a minimum size and gain, codecs by benchmark. | Small values grow when compressed; hot paths need cheap decoders. |
| D7 | The indexing policy produces ordinary `IndexDef`s marked by origin. | One index system: no second subsystem to replicate, export, check or plan. |
| D8 | Automatic indexing builds new indexes in the background and the planner ignores them until complete. | A write never waits for an index build; a query never reads half an index. |
| D9 | A physical change bumps the layout version when older builds would misread the file; logical formats (export, replication `Entry`) stay as they are. | Old builds refuse such files instead of misreading them; members of a cluster upgrade one at a time. A change older builds read correctly (redb 5, phase 2) keeps the layout, so a downgrade keeps working. |
| D10 | Migrations are recorded in a `migrations` table, idempotent and resumable by checkpoint. | Millions of records cannot be rewritten in one transaction, and an interruption must not restart from zero. |
| D11 | Mandatory changes (needed to open the file) run at open, as today; optional optimizations run only when an administrator asks. | No surprise rewrites at start. |
| D12 | Names as numeric ids in keys come last, and only if still worth it. | The widest change; the others may make it unnecessary. |
| D13 | Phase 5: Zstandard with a dictionary per namespace and kind of value, from the reference library (C, built like mimalloc); no LZ4. | Measured on the section 3 file: values of 170–500 bytes shrink by 2–10% alone with LZ4 and the pure-Rust encoder, 11–27% with Zstandard alone, 64–86% with a dictionary, which is also faster (1.4 µs to compress a node, 0.9 to decompress). The pure-Rust port of the reference library gives the same bytes at the same speed but is a prerelease (0.0.1); the format is the same, so either can replace the other without a migration. |
| D14 | A file becomes layout 20 with its first envelope or dictionary, not on opening; the tables of layout 20 are created then. A migration back to JSON over the whole file gives it its earlier layout back. | Earlier releases refuse layout 20 cleanly; a file never compressed stays readable by them (their backup refuses unknown tables, so none is created early). |
| D15 | No checksum per value. | redb checksums every page (`grafeno check`, `verify_file`); a damaged frame fails to decompress or has the wrong length; every value is decompressed and compared before it is written. Four bytes a value would be 10% of a compressed node. |
| D16 | The envelope's tag is `0x1E`, not `0x01`. | A hyperedge's binary record starts with its format, 1: with `0x01` a migration would have taken one for the other (found in phase 5; phase 4 was not released). |
| D18 | Compression on by default, though five reads that go through many records are 8–12% slower at p50 (above the 5% of phase 5's acceptance). | Measured with the suite (`docs/data/compression.csv` against `docs/data/redb5.csv`): stored bytes 13–45% lower on every dataset, writes and lookups within ±5%, scans and traversals of many records 8–12% slower from decompressing each one. `compression = "off"` keeps the earlier speed; lazy properties (D17) would take back decoding CPU. |
| D19 | Phase 6: a namespace made by this release stores the mode `default`, which follows the database's setting: automatic for a server (`[indexing] mode`), manual for the library (`OpenOptions::indexing`). A namespace of an earlier release has no policy and is manual (Q3). | The server is where automatic indexing is wanted; applications that embed the library keep the behavior they were written for, and their tests and plans do not change. |
| D20 | One full-text index per label and property (`auto_Label_property_text`), not one per label with fields added. | Adding a field to a full-text index rebuilds it over every node of the label, inside the write that brought the field. |
| D21 | No background builder for property indexes: a pair of label and property is decided before the record that brings it is written, so its index is filled from the records that already have the property, none in a namespace that was always automatic. Turning a namespace with data automatic records its pairs as decided (`existing`); the migration's `--indexing` builds their indexes, one per batch. | A write never waits for a build (D8) without a second way of filling indexes, and the planner never sees a partial index. |
| D22 | Only node properties are indexed automatically; not edge or hyperedge properties, nor `@embedding` (a vector index stays explicit), nor properties starting with `@` or `_`. A `CREATE INDEX` like an automatic one replaces it. | Nodes are what lookups start from; vector indexes need a dimension and parameters chosen on purpose. |
| D17 | 6.3 (binary header, lazy properties) moves after phase 5, to be decided on CPU. | Dictionaries took the size of records to 15–40%, the gain 6.3 was for; what is left is decoding CPU, measured with the latencies below. |

## 8. Phases and tasks

Priorities: **P0** needed for the initiative's goal, **P1** expected,
**P2** if measurements justify it, **P3** exploratory. Complexity: S
(days), M (one to two weeks), L (several weeks), XL. Every phase ends
with its tests and benchmarks green and this document updated.

### Phase 0 — Audit and roadmap (P0, S) — done in this proposal

- 0.1 Audit of storage, encodings, indexes, versions (sections 2, 4).
- 0.2 Baseline of one dataset (section 3).
- 0.3 Compatibility audit between versions: layouts 1–19, fixtures,
  replication `ENTRY_FORMAT`, export versions (section 2.1, 5).
- 0.4 This roadmap. **Acceptance**: approved (section 14).

### Phase 1 — Baseline benchmarks and observability (P0, M)

- 1.1 `examples/storage_bench_suite.rs`: datasets — the ICD-11 catalogue
  (real text, Spanish), a document workload (long texts, full-text
  indexes), structured rows (1 M nodes, property indexes), a dense graph
  and a hierarchy, hyperedges, a palace with memories, vectors (384
  dimensions). Each: load time, file and table sizes, after `vacuum`,
  write and read latency (p50/p95/p99), CPU per operation, search
  results stored for equivalence. CSV in `docs/data/storage_baseline.csv`.
- 1.2 `GET /v1/storage` and `grafeno storage stats` (6.9) from
  `table_sizes`'s logic, plus metrics.
- 1.3 Result fixtures: for each dataset, GQ queries, BM25 scores (to the
  last digit), vector search results and traversals saved, to compare
  every later phase against.
- Depends on: 0. **Acceptance**: the suite runs in under an hour, twice
  with results within ±10%, and its CSV and fixtures are committed.

### Phase 2 — redb 5 (P0, M)

- 2.1 Upgrade to 5.0 (`ReadableTable` methods, `KeyRange`, `compact(mode)`,
  errors), `rust-version` 1.90.
- 2.2 Prove every layout fixture (v1–v19) opens and upgrades; every table
  type free of `Option<str|String|bool|char>`.
- 2.3 Layout 20: a file opened by 5.0 is marked 20 so that redb-4 builds
  refuse it ("upgrade Grafeno"); the copy before upgrade keeps the 19 file.
- 2.4 `vacuum --maximum`.
- 2.5 Platforms: Linux musl x64/arm64, Windows x64/arm64, container, MSIX,
  `.deb`/`.rpm` builds; locking tested (two processes, read-only opens).
- Depends on: 1. Risk: R1, R2. **Rollback**: the kept copy of the 19
  file, opened by the previous release. **Acceptance**: every test and
  fixture green; phase-1 benchmarks within ±5% or better; `vacuum
  --maximum` reclaims the fragmented bytes `table_sizes` reports.

### Phase 3 — Existing indexing and concurrency (P0, M)

- 3.1 Measure CPU per write by part (records, adjacency, each index kind,
  change log, replication capture) with the phase-1 suite.
- 3.2 Index entry diffs on update (6.7); tokenize once per field.
- 3.3 Sorted inserts in bulk paths and builders.
- 3.4 The writer's throughput under mixed load (about 700 writes a second
  in `many_connections`, below the 1,600 measured with single memories):
  find where group commits spend their time.
- Depends on: 1. **Acceptance**: CPU per write and bulk load time lower
  than the baseline with the same results; no change in visibility or
  durability.

### Phase 4 — Versioned format and the migration framework (P0, L)

- 4.1 The envelope (6.1) in `store.rs`: readers of every record table
  accept both; writers keep writing JSON until phase 5 (a flag).
- 4.2 The `migrations` table: id, kind (mandatory/optional), scope
  (instance, namespace), version, state (`pending`, `in_progress`,
  `paused`, `completed`, `failed`, `rollback_required`), checkpoint
  (last key per table), counts, errors, verification result, times.
- 4.3 Detection of pending work at open (logged, `GET /v1/storage`,
  metrics): old layout, namespaces without a policy, records without the
  current encoding (sampled count), indexes needing a rebuild, reclaimable
  space.
- 4.4 The engine's migration runner: plans, batches through the bulk
  lane, persistent checkpoints, pause, resume, cancel, memory and batch
  limits, idempotent steps.
- 4.5 CLI and admin route (6.10), dry run, JSON reports.
- Depends on: 2. Risk: R3, R5. **Acceptance**: a migration interrupted at
  random points (kill tests) resumes and ends with the same data as an
  uninterrupted one; running it twice changes nothing.

### Phase 5 — Adaptive lossless compression (P0, L)

- 5.1 Codecs (LZ4, Zstandard with dictionaries) as dependencies, pure
  Rust or with C built on every platform (R4). *Done: Zstandard only
  (D13).*
- 5.2 Binary record header and lazy properties (6.3). *Moved (D17).*
- 5.3 Per-value adaptive choice (6.2), `[storage] compression`.
- 5.4 Dictionaries per namespace and record kind; their table; training
  in the migration and in the background.
- 5.5 Texts in a byte table (F10) with the envelope; the old `texts`
  table read until migrated.
- 5.6 Change log, audit and replication log values compressed.
- 5.7 `check` decodes and verifies envelopes (CRC, limits).
- Depends on: 4. **Acceptance**: every result fixture identical; size of
  records, texts and logs lower (measured, per dataset); reads by id and
  traversals not slower than the baseline by more than 5% at p50 (or the
  decision recorded); a corrupted envelope is detected, never misread.

### Phase 6 — Indexing policies per namespace (P0, L)

- 6.1 Policy in `NamespaceMeta`, GQ and HTTP (6.6), defaults.
- 6.2 Automatic decisions on writes (new label-property pairs), limits,
  `origin: auto` indexes built in the background.
- 6.3 Manual: `indexes` in node writes (HTTP, GQ `CREATE … OPTIONS`,
  ingestion, imports).
- 6.4 Hybrid: exclusions and rules, precedence (6.6) tested as a table.
- 6.5 `SHOW INDEXES` origin and state; statistics of use (queries that
  used each index, from the planner).
- 6.6 Measure: write cost and size with automatic indexing on each
  dataset, against manual.
- Depends on: 3 (cheap entries), 4 (policy migration). **Acceptance**:
  each mode behaves as specified; queries on automatically indexed
  properties use the index once built; write cost increase reported and
  approved (Q3).

### Phase 7 — Automatic indexing at bulk speed (P0, M)

- 7.1 Imports, ingestion and `IN TRANSACTIONS` decide policies per batch,
  not per record, and build new indexes after the load (one pass) rather
  than entry by entry.
- 7.2 List entries for lists of scalars (one entry per item), if phase 6
  shows the need.
- Depends on: 6. **Acceptance**: a bulk load with automatic indexing takes
  less than load + `CREATE INDEX` afterwards.

### Phase 8 — Specialized property encodings (P1, M)

- 8.1 Property names by dictionary id inside records (with 5.2).
- 8.2 Typed property values (integers as varints, times as micros) where
  JSON round-trips exactly (numbers keep integer/float distinction).
- Depends on: 5. **Acceptance**: fixtures identical; records smaller.

### Phase 9 — Full-text index storage (P1, L)

- 9.1 One entry per term and node, fields in the value, varint and delta
  positions (6.4); rebuilt per index, old and new readable until rebuilt.
- 9.2 Node text postings without the repeated length.
- 9.3 (P3) Posting blocks, only if 9.1 leaves the full-text index the
  largest structure and the write cost is acceptable.
- Depends on: 4, 1.3 (BM25 scores as fixtures). **Acceptance**: every
  search returns the same nodes, order and scores; index size measured
  down; insert and search times not worse beyond the recorded limits.

### Phase 10 — Adjacency and vector structures (P1, M)

- 10.1 HNSW links as dense per-index numbers (6.4).
- 10.2 Measure byte-shuffled Zstandard for vectors; adopt or record why not.
- 10.3 Adjacency: measure what remains after 6.5 (already compact).
- Depends on: 4. **Acceptance**: recall@10 and the neighbor lists
  identical; links smaller.

### Phase 11 — Compaction, manual and automatic (P1, M)

- 11.1 Reclaimable space reporting (exact free pages, fragmentation).
- 11.2 `vacuum --maximum`; online compaction in a window (6.8), opt-in.
- 11.3 Compaction as the last step of a migration, when asked.
- Depends on: 2. **Acceptance**: the file shrinks to what its tables
  take within a stated margin; an interrupted compaction leaves a valid
  file (crash tests); requests wait, never fail, during a window.

### Phase 12 — Names as numbers in keys (P2, XL)

- 12.1 The `catalog` table, ids per member (6.5).
- 12.2 Every table's keys; a full rewrite through the migration.
- Depends on: 5–10 measured. Go/no-go recorded here.

### Phase 13 — Whole validation (P1, M)

- The phase-1 suite on every configuration of section 11; equivalence of
  every fixture; crash and interruption tests of migrations and
  compactions; a migrated copy of every layout fixture.

### Phase 14 — Documentation, scripts and release (P1, M)

- Docs (`docs/install.md`, `instalacion.md`, server README in both
  languages, query language), the migration guide, scripts for Linux,
  Windows, the container, Compose and clusters wrapping the CLI, CHANGELOG
  sections with measured numbers, a release only after CI, Installers and
  a dispatched release build are green.

### Migration activities and where they live

| Activity (from the request) | Priority | Phase |
| --- | --- | --- |
| Compatibility audit between versions | P0 | 0.3 |
| Inventory of formats and structures | P0 | 0.1 (section 2.2) |
| Persistent migration registry | P0 | 4.2 |
| Versioned storage format | P0 | 2.3, 4.1 |
| Official migration tool | P0 | 4.4, 4.5 |
| Detection of pending optimizations | P0 | 4.3 |
| Indexing policy migration | P0 | 6.1, 9 (C) |
| Retroactive reindexing | P0 | 6, 7, 9 (C) |
| Retroactive compression | P0 | 5, 9 (D) |
| Specialized encodings migration | P1 | 8, 9, 10 |
| Batches and checkpoints | P0 | 4.4 |
| Compaction after migration | P1 | 11.3 |
| Dry run and resource estimates | P0 | 4.5 |
| Backup, recovery and rollback | P0 | 9 (J) |
| Integral validation of migrated data | P0 | 9 (K), 13 |
| Comparative reports | P1 | 4.5, 9 (L) |
| Scripts and documentation | P1 | 14 |
| Distributed deployments | P1 | 9 (M) |

### Dependencies

```text
0 → 1 → 2 → 4 → 5 → 8
     ↘ 3 ↗   ↘ 6 → 7
             ↘ 9, 10
         2 → 11
  5–10 measured → 12
  all → 13 → 14
```

## 9. Migration of existing databases

**A. The tool.** `grafeno storage status | plan | apply | verify |
resume` (Q2), `--namespace` or `--all-namespaces`, `--reindex-existing`,
`--indexing-mode`, `--compress-existing`, `--upgrade-encoding`,
`--compact`, `--verify`, `--batch-size`, `--max-memory`, `--dry-run`,
`--json`. The logic is the engine's; scripts call the CLI and never open
redb files themselves.

**B. Detection.** At open, Grafeno records what is pending (4.3) and says
so in the log, `GET /v1/storage`, `grafeno storage status` and metrics;
it runs nothing optional by itself. Mandatory changes (needed to open the
file with this build: the redb 5 layout mark) run as today's layout
upgrades, after the copy.

**C. Indexing policies.** Namespaces without a stored policy behave as
`automatic` for new properties (Q3); `--reindex-existing` scans each
namespace's records once, collects the label-property-type statistics,
plans the indexes the policy wants (reusing equivalent ones, never
duplicating, never touching `manual` or `hybrid` choices unless asked),
then builds them with the background builder in batches.

**D. Compression.** `--compress-existing` and `--upgrade-encoding`
rewrite records into the current envelope in key order, in bulk-lane
batches, after training the namespace's dictionaries on a sample. Values
already in the current encoding are skipped (idempotent).

**E. One pass.** `apply --all-namespaces --optimize` coordinates, in
order: compatibility check, state check (no other migration running,
`check` clean or reported), estimate, backup, mandatory upgrade, policy
statistics and re-encoding **in the same scan of each table**, index
builds and rebuilds, compaction (needs the window or a stopped server),
verification, report, registry update.

**F. Dry run.** `plan --dry-run` reads only: namespaces, counts, records
to re-encode (sampled), indexes to create or rebuild, estimated temporary
space (a rewritten table holds old and new pages until commit: about the
largest batch) and reclaimable space, whether a window or a stop is
needed, estimated time from the phase-1 rates; measured and estimated
figures are labelled.

**G. Registry.** The `migrations` table (4.2), per instance and
namespace, with engine, layout, policy, encoding and compression
versions.

**H. Batches.** Fixed-size batches through the bulk lane, a checkpoint
per batch in the same transaction as its writes (so a crash never loses
or repeats one), no snapshot held across commits (the lesson of
`IN TRANSACTIONS`), memory bounded by batch size. One writer: more
workers only help the read and encode side, and are measured before
being offered.

**I. Availability.** Re-encoding and index builds run online, between
interactive writes (bulk lane); readers see old or new encodings, both
valid. Index builds are invisible to the planner until complete.
Compaction needs no open transaction: a stopped server (`vacuum`) or the
window of 6.8. The plan says which steps need what.

**J. Backup and rollback.** Before anything that changes the layout, a
verified backup (`backup` then `check` of the copy, free space checked
first); the layout-upgrade copy for mandatory changes. A build that
cannot read the new layout refuses the file. Going back means restoring
the backup **and losing the writes after it**: the tool says so and asks
for confirmation; the alternative that keeps every write is an export
from the new build (the logical format does not change) imported into
the old one. No automatic rollback that could drop writes.

**K. Verification.** `verify` compares, per namespace: counts and ids of
nodes, edges and hyperedges (and participants), properties by a hash of
each record's logical JSON, adjacency against edges (`check`), index
entries against records, the result fixtures (GQ, BM25 scores, vector
results, traversals, palace walks), the change log sequence, rights and
namespace isolation. A migration is `completed` only after it passes.

**L. Report.** Before and after: versions, namespaces, records processed,
indexes created and rebuilt, records re-encoded, sizes per table and
file, space given back, compression ratio, time, peak memory, warnings,
verification; readable and JSON.

**M. Distribution.** The same CLI in every package (Linux, Windows,
container, Compose, MSIX); in a cluster, each member migrates its own file
(replication carries logical records, F15), one member at a time; a new
layout or encoding is written only after every member runs a build that
reads it (the cluster status reports members' versions; the leader refuses
to enable a new encoding before that).

**N. Not yet optimized.** An upgraded instance runs with old records and
writes new ones in the current encoding; `storage status` lists what is
pending, so "runs the latest build" and "fully optimized" stay distinct.

## 10. Testing

- **Equivalence**: the phase-1 fixtures after every phase: identical rows,
  order, BM25 scores, vector results (ids and order; scores to the last
  digit since vectors do not change), traversals, palace walks, change
  log.
- **Round trips**: property tests (proptest, already a dependency) that
  any record survives encode/decode in every codec and size; corrupt and
  truncated envelopes are refused.
- **Mixed files**: old and new encodings side by side, read and updated.
- **Layout fixtures**: every `layout-v*.redb.gz` opens, upgrades and
  migrates.
- **Crashes**: killing the process during migrations, compactions and
  bulk writes (the existing `crash.rs` approach), then `check` and the
  fixtures.
- **Clusters**: mixed-version members during a rolling upgrade, refusal
  of a new encoding before every member can read it.
- **Policies**: precedence as a table-driven test; no duplicate index;
  no index used before it is built.

## 11. Benchmarks

On E1-disk and E2 (Linux), same hardware, data and durability for every
comparison, each run twice:

| Configuration | Phase |
| --- | --- |
| today (redb 4.4, JSON, no compression) | 1 |
| redb 5 | 2 |
| + cheaper indexing | 3 |
| + adaptive compression and binary records | 5 |
| + automatic / manual / hybrid indexing | 6, 7 |
| + specialized encodings (properties, full text, links) | 8–10 |
| + compaction | 11 |
| everything | 13 |

Measured: operations and records indexed per second, load time,
latency p50/p95/p99 of reads, writes, GQ, full-text and vector search,
CPU per operation, peak memory, file size and size per table (after
`vacuum` too), compression ratio, bytes written per logical write (write
amplification, from redb's statistics and the file growth), recovery
time after a crash, search quality (recall@10, BM25 equality) and result
equivalence. No improvement target is set before the baseline exists.

## 12. Risks

| # | Risk | Mitigation |
| --- | --- | --- |
| R1 | redb 5 reads an existing table differently (encoding change). | No table uses the changed types (verified by type); every layout fixture opened in tests; copy before the layout mark. |
| R2 | Older Grafeno builds open a file redb 5 wrote. | Layout 20 refused by older builds; locking incompatibility documented (one build per file). |
| R3 | A migration interrupted halfway leaves mixed state. | Envelope makes mixed files valid; checkpoints in the batch's transaction; idempotent steps; kill tests. |
| R4 | A codec with C code complicates static musl, arm64 or Windows builds. | Prefer pure-Rust implementations where their speed is close; build every target in CI before adopting one. Phase 5: the reference Zstandard (C) builds through `cc` as mimalloc does, on every target of the Installers workflow (D13). |
| R5 | Compression makes reads slower on hot paths. | Binary header and lazy properties first; per-table codecs; thresholds; benchmarks gate each table. |
| R6 | Automatic indexing multiplies write cost and size. | Limits per label, measurement in phase 6, approval of the default for existing namespaces (Q3), `manual` available. |
| R7 | A new full-text layout changes scores. | BM25 fixtures to the last digit; both layouts readable until rebuilt; rebuild per index. |
| R8 | Online compaction blocks requests too long. | Opt-in window, time limit, abort if transactions do not drain; requests wait in the admission queue. |
| R9 | Dictionaries lost or changed make records unreadable. | Dictionaries immutable by id (a hash of their bytes), kept in the file and copied by backups and snapshots; a value naming a dictionary the file lacks is an error, and `check` reports values that do not decode. Each member trains its own: replication carries records, not their bytes. |
| R10 | A rollback silently drops writes. | Only explicit restores, with the loss stated; export/import as the lossless way back. |
| R11 | Cluster members disagree on encodings. | Encodings are per member (logical replication). A snapshot of a compressed member is layout 20, which a member of an earlier release refuses: during a rolling upgrade, compression stays off (`[storage] compression = "off"`) until every member runs this release. |

## 13. Acceptance criteria

The initiative is complete when, measured against the phase-1 baseline:

- redb is the only store, at the validated 5.x version; no other engine
  was added; BM25, HNSW and GQ are unchanged in behavior.
- New namespaces index automatically by default; every namespace can be
  `automatic`, `manual` or `hybrid`, with deterministic precedence.
- Compression is on by default, adaptive, lossless and versioned; old
  records stay readable.
- Storage is smaller and indexing cheaper on the benchmark datasets, with
  the numbers in `docs/benchmarks.md` and `docs/data/`, and no regression
  beyond the limits recorded per phase.
- Every result fixture (GQ, BM25 scores, vector results, traversals,
  palaces, memories, history, rights, isolation) is identical.
- An official tool migrates existing instances, per namespace or all,
  with dry run, registry, checkpoints, resume, verification, reports,
  backup and documented rollback; running it twice changes nothing; no
  database has to be dropped and recreated.
- Compaction recovers the free and fragmented space it reports.
- Every decision is recorded here and the docs explain each feature.

## 14. Questions for approval

- **Q1 — redb target.** 5.0.0 (latest stable) instead of the 0.5.0 given,
  which is older than the 4.4.0 in use. Approve 5.x?
- **Q2 — Tool name.** `grafeno migrate` already copies a Windows setup's
  database for the Store package. Proposed: `grafeno storage status |
  plan | apply | verify | resume` (and `grafeno storage stats`), keeping
  `grafeno migrate` as it is. Or move the Store copy elsewhere?
- **Q3 — Automatic indexing for existing namespaces.** New namespaces
  default to `automatic`. For existing ones: apply `automatic` to new
  properties right after the upgrade (writes get dearer at once), or keep
  their current behavior until the administrator runs the migration
  (proposed)?
- **Q4 — Codecs.** LZ4 and Zstandard need new dependencies (possibly C
  code built for every target). Accept them, preferring pure-Rust
  implementations when within 20% of the C ones?
- **Q5 — Online compaction.** Opt-in maintenance windows in which the
  server pauses writes for the compaction (proposed), or only offline
  `vacuum`?
- **Q6 — Phase 12.** Agree that names-as-numbers in keys is decided only
  after phases 5–10 are measured?
- **Q7 — Order.** The proposed order is 1 → 2 → 3 → 4 → 5 → 6 → 7, then
  9, 10, 8, 11 by measured gain; start with phase 1 once approved?

## Progress

| Date | Step | State | Notes |
| --- | --- | --- | --- |
| 2026-11-26 | 0.1 Audit | done | sections 2 and 4, against `8241baf` |
| 2026-11-26 | 0.2 First baseline | done | section 3: one synthetic catalogue; the full suite is phase 1 |
| 2026-11-26 | 0.3 Compatibility audit | done | sections 2.1, 2.4, 5 |
| 2026-11-26 | 0.4 Roadmap | done | approved with the proposed answers: redb 5.x (Q1), `grafeno storage` (Q2), automatic indexing of existing namespaces only through the migration (Q3), LZ4 and Zstandard preferring pure Rust (Q4), opt-in compaction windows (Q5), phase 12 after measuring (Q6), order as proposed (Q7) |
| 2026-11-26 | 1.1 Benchmark suite | done | `crates/grafeno-server/examples/storage_suite.rs`: six datasets (catalogue, documents, rows, graph, hyperedges, agent memory through MCP), sizes, `vacuum`, latencies; numbers in `docs/benchmarks.md` ("Storage baseline") and `docs/data/storage_baseline.csv`. Two full runs, 18 and 10 minutes: sizes identical, latencies not within ±10% (the first ran on a locked, throttled machine) |
| 2026-11-26 | 1.2 Storage statistics | done | `Grafeno::storage_usage`, `ReadTx::namespace_usage` (`crates/grafeno/src/usage.rs`), `GET /v1/storage` (owners, `?namespaces=true`), `grafeno storage stats [--namespaces] [--json]`. Not added to `/v1/metrics`: redb's page counts need a write transaction, too dear for every scrape |
| 2026-11-26 | 1.3 Result fixtures | done | `docs/data/storage_fixtures/*.json`: full-text results with BM25 scores, node text results, lookups, aggregates, traversals, hyperedges, recall sets, vector recall@10 (a lower bound); `COMPARE=` fails on any difference (numbers within a relative 1e-9, the precision JSON keeps) |
| 2026-11-26 | 1.1 Latency baseline on an idle machine | done | run 3 of `docs/data/storage_baseline.csv` (redb 4.4), every result matching the fixtures; CPU per operation is measured in phase 3 (3.1) |
| 2026-11-26 | 2.1 redb 5.0 | done | `redb = "5"`; the API changes were few: `ReadableTable` imported where ranges are read, `compact(CompactionMode)`, one typed range. `rust-version` 1.90 |
| 2026-11-26 | 2.2 Old layouts | done | the upgrade tests open and upgrade every fixture (layouts 1–19); no table uses the `Option` types redb 5 encodes differently; 678 tests green |
| 2026-11-26 | 2.3 Layout 20 | **not needed** | redb 5 keeps file format 3: a file written by this build was opened, written and checked by the 0.5.3 build (redb 4.4), and the other way round. Bumping the layout would only stop a downgrade that works; the layout stays 18/19 (decision D9 revised: a physical change bumps the layout only when older builds would misread it) |
| 2026-11-26 | 2.4 `vacuum --maximum` | done | `Grafeno::vacuum_with`; 1,028 → 403 MiB on the section 3 file (555 with `vacuum`) |
| 2026-11-26 | Phase 2 benchmarks | done | `docs/benchmarks.md` ("redb 5.0"), `docs/data/redb5.csv`: same bytes, same results, files grown more sparingly, times within noise |
| 2026-11-26 | 2.5 Platforms | done | CI (tests on Linux) and Installers (Linux musl x64/arm64, Windows x64/arm64, `.deb`, `.rpm`, the setup) green on `785d98a` |
| 2026-11-27 | 3.1 Write cost by part | done | `crates/grafeno/examples/write_cost.rs`: a full-text index made an unrelated property change 10 times dearer (123 µs against 12.5), one property index 1.8 times (`docs/benchmarks.md`, "What a write costs") |
| 2026-11-27 | 3.2 Entry diffs, tokenize once | done | unchanged indexes skipped (`unchanged_for` in `index.rs`), full-text fields analyzed once for entries and BM25 counts, index definitions read once per write transaction (`WriteTx::cached_index_defs`, forgotten by every change to a definition): 123 → 14 µs, create with full-text 142 → 89 µs |
| 2026-11-27 | 3.3 Sorted inserts | done | index builds insert in key order: 11.9 → 0.4 MiB fragmented for a full-text index of 20,000 nodes, 1.10 → 0.89 s. Bulk loads insert record by record (each record's entries in its own transaction), so they keep their order; their fragmentation is for compaction (phase 11) |
| 2026-11-27 | 3.4 Writer under mixed load | measured | the writer alone does 6,400–11,700 writes/s for the mix (7,000 in the Linux container); over HTTP each write takes about four times longer inside its group, sharing the CPUs with the requests. Fewer read threads changed nothing. Isolating the writer's thread (priority, its own core) is a follow-up, not in this phase |
| 2026-11-27 | Phase 3 checks | done | storage suite against the fixtures: identical; 682 tests green; `crates/grafeno/tests/index_upkeep.rs` |
| 2026-11-27 | 4.1 Envelope | done | `store.rs`: `[0x01][version 1][codec][length][payload]`, a first byte JSON never starts with; every reader of record values goes through `decode`, which accepts both; an unknown version or codec, or a wrong length, is an error. Writers keep JSON (phase 5 switches them) |
| 2026-11-27 | 4.2 Migration registry | done | `crates/grafeno/src/migrations.rs`: each migration in the `state` table (not replicated: it is physical, per member), its options, engine and layout, state, steps (`reindex`, `reencode`, `verify` per namespace) with estimate, count, checkpoint (last key), error and result, and digests of the namespaces before |
| 2026-11-27 | 4.3 Pending work | done | `Grafeno::pending_work`: migrations waiting, free pages over a quarter of the file, space lost inside pages over a quarter of what is stored; logged at server start, `pending` in `GET /v1/storage`, `grafeno storage status`. Not in `/v1/metrics` (it needs a write transaction, as in 1.2). Namespaces without a policy and records to re-encode wait for phases 5 and 6 |
| 2026-11-27 | 4.4 Runner | done | batches through `Grafeno::write` or the writer's bulk lane, the checkpoint saved in the batch's own transaction; pause (a stop flag, a batch limit), resume, idempotent steps; reindex before reencode (a rebuild rewrites records as JSON). Rebuilding indexes no longer writes the change log (it logged every record as updated). Cancel is not offered: a migration stopped stays `paused`, and the backup or `--encoding json` undoes one |
| 2026-11-27 | 4.5 Tool and route | done | `grafeno storage status|plan|apply|resume|verify` (`storage_cli.rs`): backup checked before anything changes, `--compact` (`vacuum --maximum`), reports with `--json`; `GET`/`POST /v1/storage/migrations` (owners, refused on a cluster) |
| 2026-11-27 | Phase 4 acceptance | done | `crates/grafeno/tests/migrations.rs` (dry run, interrupted and resumed, run twice, envelope and back, verification catching a change) and `crash.rs` (`migrations_survive_being_killed`: killed at random points six times, resumed, same digests). On the section 3 file, `apply --reindex-existing --encoding envelope --verify --compact`: 199,974 records rewritten and verified, 1,028 → 402.8 MiB, 165.9 → 16.2 MiB lost inside pages, 65.9 s (`docs/benchmarks.md`, "A storage migration"). Found on the way: redb grows a packed file while it is open and trims it on close, so the free pages counted while open are not what a compaction gives back; the tool and `pending_work` measure against the closed length |
| 2026-11-27 | 5.1 Codecs | done | measured with the section 3 file (`crates/grafeno/examples/compression.rs`, `docs/benchmarks.md` "Compression"): Zstandard with dictionaries (D13); LZ4 and the pure-Rust encoder measured and not used |
| 2026-11-27 | 5.2 Binary header | moved | D17 |
| 2026-11-27 | 5.3 Adaptive choice | done | `crates/grafeno/src/compress.rs`: with the namespace's dictionary from 32 bytes, without one from 1 KiB (Zstandard alone costs 7 µs a write for 28% on a 500-byte record), kept when it saves 12%, decompressed and compared before writing; `[storage] compression = "auto" \| "off"`, `OpenOptions::compression` |
| 2026-11-27 | 5.4 Dictionaries | done | `dictionaries` table, id = FNV-1a of the bytes, 64 KiB at most, trained on up to 4,000 evenly spread values of a namespace and kind with 1,000 or more; in a migration's `train` step, and by the server every `train_dictionaries_every_secs` (600) through the writer's bulk lane; written values use one only once the transaction storing it has committed |
| 2026-11-27 | 5.5 Texts | done | `text_values` (bytes, always in an envelope) for compressed texts, `texts` for the others; readers look in both, a text is in one only |
| 2026-11-27 | 5.6 Logs | done | change log entries per namespace, audit and replication log entries per file; the migration rewrites them too (a step for the file's logs when it covers every namespace) |
| 2026-11-27 | 5.7 `check` | done | nodes, edges, texts and change log entries that do not decode are problems, not errors; a text stored twice is one. No checksum per value (D15) |
| 2026-11-27 | Layout 20 | done | D14; the envelope tag became `0x1E` (D16); `tests/compression.rs` (compression off leaves no new table, compressed values read the same and search the same, back to JSON gives layout 18 back, an interrupted compression resumes, a backup restores with its dictionaries) |
| 2026-11-27 | Phase 5 on the section 3 file | done | `apply --encoding compressed --reindex-existing --verify --compact`: nodes 49.2 → 10.3 MiB, edges 28.4 → 11.0, changes 52.4 → 26.2; file 1,028 → 311.2 MiB in 66.7 s (403 with `vacuum --maximum` alone); every record's value as before |
| 2026-11-27 | Phase 5 suite | done | every result matches the fixtures; stored MiB: catalogue 341.8 → 277.3, documents 488.1 → 425.2, rows 162.2 → 95.0, graph 339.1 → 185.8, hyperedges 39.0 → 27.2, memory 21.4 → 13.9 (training and rewriting: 0.7–10 s). p50 within ±5% of redb 5's run except two hops up the hierarchy (+10.7%), node text search (+8.0%), a country's latest 20 (+11.7%), the unindexed count (+9.1%), two hops in the graph (+11.2%): D18. A first run made a Zstandard context per value and was tens of times slower on parallel scans; contexts are kept per thread now |
| 2026-11-27 | Phase 5 checks | done | 699 tests green; `docs/benchmarks.md` ("Compression") |
| 2026-11-27 | 6.1 Policy | done | `crates/grafeno/src/indexing.rs`: `IndexingPolicy` in `NamespaceMeta` (`mode`, `exclude`, `labels`, `properties`, `max_per_label`), D19; GQ `CREATE NAMESPACE … OPTIONS {indexing}`, `ALTER NAMESPACE … SET INDEXING`, HTTP `indexing` in `POST`/`PATCH /v1/namespaces`, `[indexing] mode` |
| 2026-11-27 | 6.2 Automatic decisions | done | each pair decided once, at its first value, kept as a state entry (replicated); `IndexDef::origin` `auto`, `auto_Label_property`, 32 a label; a dropped one is not made again; D20, D21, D22 |
| 2026-11-27 | 6.3 Manual | partly | `indexes` in node writes over HTTP (`POST`, `PUT /v1/{ns}/nodes`) and `WriteTx::declare_property_indexes`. Not in GQ `CREATE … OPTIONS`, ingestion or imports: bulk paths are phase 7 |
| 2026-11-27 | 6.4 Hybrid | done | precedence tested as a table (`indexing.rs`), rules and exclusions per label and for every label |
| 2026-11-27 | 6.5 `SHOW INDEXES` | done | `origin` and `uses` columns; `IndexInfo::uses` counts lookups, seeks and searches since the database was opened. `state` stays `online` for property indexes (they are never partial, D21) |
| 2026-11-27 | Policy migration | done | `grafeno storage apply --indexing automatic\|hybrid\|manual` (or `--indexing-policy <json>`): an `index` step per namespace sets the policy and creates, one per batch, the indexes it wants for the data there; `pending_work` lists namespaces of an earlier release |
| 2026-11-27 | 6.6 Measure | done | the storage suite with `INDEXING=automatic` (`docs/data/indexing.csv`) against compressed manual (`docs/data/compression.csv`), every result matching the fixtures: stored MiB catalogue 277.3 → 308.5, documents 425.2 → 427.5, rows 95.0 → 151.3, graph 185.8 → 188.2, hyperedges and memory unchanged; load s catalogue 53.6 → 59.1, rows 8.6 → 18.4, the others within 10%; one write 5–17% dearer; queries on properties nobody indexed: the unindexed count 136.2 → 0.10 ms, two hops in the graph 14.5 → 0.32 ms (`docs/benchmarks.md`, "Automatic indexing") |
| 2026-11-27 | Phase 6 checks | done | `tests/indexing.rs`, `grafeno-query/tests/indexing_policy.rs`; found on the way: `CREATE INDEX` after an automatic index of the same shape failed (now it replaces it) |
| 2026-11-27 | Q3 for new namespaces | approved | a server's new namespaces are automatic by default, for the write cost measured in 6.6; `[indexing] mode = "manual"` keeps the earlier behavior |
| | Phase 7 | next | automatic indexing at bulk speed |
| | Phases 6–14 | pending | |
