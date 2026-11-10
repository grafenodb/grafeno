# Changes to the Grafeno engine

The engine (`grafeno`: the server, the shell, the administration commands
and its installers) has its own versions and releases, tagged
`v<version>`; Grafeno Studio and Grafeno Desktop have theirs, in their own
repository. A version's section here is its release's notes;
the release workflow refuses a tag whose version has no section.

What the engine speaks to clients is versioned apart from it: the HTTP
API version (`GET /v1/about`, now 1) changes only when a change would
break a client of the previous one, and the database files' layout
version (now 17) is upgraded in place, with a copy kept first.

## Unreleased

- **License**: Grafeno is free software under the MIT license or the
  Apache License 2.0, at the user's choice (`LICENSE-MIT`,
  `LICENSE-APACHE`). The packages, the Windows setup (a license page),
  the archives and the winget manifest carry it.
- **Changed**: the text-similarity functions answer only to their `text.*`
  names; the five compatibility aliases outside that prefix are gone, and a
  query that uses one fails with "unknown function". Their replacements:
  the Levenshtein distance is `text.levenshtein`, the Jaro-Winkler
  similarity `text.jaroWinkler`, the Sørensen-Dice similarity
  `text.dice(a, b, 2)`, the phonetic key `text.soundex`, and the
  normalized Levenshtein similarity is closest to
  `text.similarity(a, b, 'levenshtein')`.

## 0.1.0 (2026-11-07)

The first release: everything built in phases 1 to 15 (the story, step by
step, is in [docs/development-history.md](https://github.com/grafenodb/grafeno/blob/main/docs/development-history.md)).

- **The graph**: namespaces of nodes and edges with properties, facts with
  valid and record time, full-text (BM25) and vector (HNSW, int8 copies)
  search, hybrid recall and context for prompts, memory upkeep (importance,
  decay, consolidation, summaries).
- **Queries**: GQ, a declarative graph query language with `EXPLAIN`, batches,
  traversals, graph algorithms, a cost-based planner, batched and parallel
  execution.
- **Knowledge upkeep**: entity resolution and record linkage, extraction of
  mentions, cardinality constraints, community summaries, a change log with
  streams, webhooks and NDJSON ingestion.
- **Access**: accounts with passwords and sessions, API keys, roles with
  grants and denials down to labels, properties and records, an audit log.
- **Clusters**: Raft replication, failover, linearizable reads, queries
  split across members, namespaces on several Raft groups.
- **Interfaces**: an HTTP API (version 1, `GET /v1/about` with 42
  capabilities), an MCP server, an interactive shell, a TypeScript client.
- **Installation**: a guided Windows setup (a virtual service account, the
  first administrator, the Event Log, winget manifests), `.deb` and `.rpm`
  packages for x86_64 and aarch64 with AppArmor and SELinux policies,
  signed APT and DNF repositories, a `curl` one-liner, `grafeno setup` for
  instances, `grafeno doctor`, a copy of the database before a layout
  upgrade and `grafeno instances rollback`.
- **For Grafeno Desktop**: `grafeno serve --stop-with-stdin` and `grafeno
  restore`, so the desktop app opens local files with the installed engine.

Database files: layout 17 (files of every earlier layout open and are
upgraded). HTTP API: version 1.
