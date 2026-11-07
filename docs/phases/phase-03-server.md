# Phase 3: Server

[All phases](README.md) · [← Phase 2: Retrieval](phase-02-retrieval.md) · [Phase 4: Foundations and validation →](phase-04-foundations-and-validation.md)

**Status:** done.

HTTP API with atomic batches, MCP server, TypeScript client, Docker image.

## Server (phase 3)

One binary, `grafeno`, with two modes. The endpoints, options and MCP setup
are documented in [crates/grafeno-server/README.md](../../crates/grafeno-server/README.md).

- **HTTP** (`grafeno serve`, axum): JSON under `/v1/{namespace}/…`, an
  optional static bearer token, and engine calls on tokio's blocking pool
  (redb is synchronous). `POST /batch` applies many operations in one
  transaction, with `$name` references between them, so a client can sync a
  note, its chunks and its links atomically. Clients may choose their own
  node ids (`PUT /nodes/{id}`, `upsert_node`), which makes syncing idempotent.
- **MCP** (`grafeno mcp`): newline-delimited JSON-RPC over stdio, written by
  hand (a few hundred lines; no SDK dependency). Seven tools shaped for
  language models: `remember`, `recall`, `context`, `relate`, `update`,
  `forget` and `get_node`. Tool results use names and ISO dates rather than
  raw records, and unnamed memories are named by the start of their text.
- **Docker**: a two-stage image (Debian trixie), port 7810, data in `/data`.

A database file is locked by the process that opens it, so the HTTP and MCP
modes cannot share a file. An MCP mode that forwards to an HTTP server
(`grafeno mcp --url`) would remove that limit.
