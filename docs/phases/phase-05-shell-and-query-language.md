# Phase 5: Shell and query language

[All phases](README.md) · [← Phase 4: Foundations and validation](phase-04-foundations-and-validation.md) · [Phase 6: Studio and installers →](phase-06-studio-and-installers.md)

**Status:** done.

`grafeno shell` (a console on a file or a server) and GQ, a declarative graph query language with time travel ([docs/query-language.md](../query-language.md)).

## Query language and shell (phase 5)

- **GQ** (`grafeno-query`): a hand-written lexer and recursive-descent
  parser (errors carry line and column), and an executor on top of the
  engine API. A pattern starts from its most selective node (a bound
  variable, then `@id`, then labels with properties, then labels) and
  extends both ways through `neighbor_ids`, which reads adjacency entries
  only; edge and node records are read when the pattern needs them.
  `VALID AT` / `KNOWN AT` become the traversal's `EdgeFilter`.
- Writing statements take the write lock, then a read snapshot (which is
  then the latest state), match on the snapshot and write through the
  transaction; any error aborts all of it.
- Grafeno-specific parts: pseudo-properties (`@id`, `@text`, `@embedding`,
  `@valid_from`, …), `RETRACT`, and `SET r.@valid_to` to invalidate a fact.
- **Shell** (`grafeno shell`): a line editor (rustyline) with history,
  statements ending with `;`, backslash meta-commands, table or JSON
  output, and script mode. The same code drives a file (through the
  library) or a server (through the HTTP API, with `ureq`).
- GQ is also the HTTP API's `POST /v1/{ns}/query` and the MCP `query`
  tool, which is read-only so agents write through the dedicated tools.
