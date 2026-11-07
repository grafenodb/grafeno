# Grafeno

<p align="center">
  <img src="docs/assets/readme-banner-en.svg" width="1440" alt="Grafeno symbol: three carbon rings connected to a network of nodes.">
</p>

<p align="center">
  <strong>English</strong> · <a href="README.es.md">Español</a><br>
  <a href="#install-the-engine">Get started</a> ·
  <a href="docs/install.md">Installation</a> ·
  <a href="docs/query-language.md">GQ queries</a> ·
  <a href="CONTRIBUTING.md">Contribute</a>
</p>

Grafeno is a **Rust graph engine for agent memory and knowledge**. It connects
entities, documents and facts; preserves provenance and history; and combines
text, vector and graph retrieval to assemble context. Its first client is
[Grafito](https://github.com/Seerendo/grafito).

Grafeno is the engine alone, as PostgreSQL is: a server (HTTP API, MCP for
agents, clustering), a console (`grafeno shell`) and administration commands,
with no graphical interface. **Grafeno Studio**, the interface, is a separate
product installed on its own (as pgAdmin is), and connects to engines through
their API like any other client. Grafeno also embeds as a Rust library.

## What you can do

| Capability | Purpose |
| --- | --- |
| **Connect knowledge** | Nodes with labels and properties, directed relationships and namespaces to organize data. |
| **Query the past** | Distinguish when a fact was valid from when the system knew it using `VALID AT` and `KNOWN AT`. |
| **Retrieve context** | Combine BM25 text search, HNSW vectors and graph expansion; pack context into a token budget. |
| **Maintain memory** | Resolve entities, review possible duplicates, record reversible merges and summarize communities without a model. |
| **Query** | GQ, a Cypher-like language, from the console, the HTTP API, MCP or any client. |
| **Operate the database** | Accounts that sign in, roles with grants and denials down to labels, properties and records, API keys, backups, import and export, integrity checks and index rebuilds. |
| **Run a cluster** | Several servers kept in sync with Raft: a leader elected, failover, linearizable reads, large reads split across members, namespaces on several groups. |

## Install the engine

Windows (a setup `.exe` with the service, or winget), Linux on x86_64 and
aarch64 (signed APT and DNF repositories, `.deb`, `.rpm`, and a `curl`
one-liner that also suits AlmaLinux, Rocky and RHEL) and a `.tar.gz`: see
the [installation guide](docs/install.md). The engine runs as a service on
`127.0.0.1:7810`; its settings are in `grafeno.toml`. `grafeno setup` adds
instances, as PostgreSQL clusters are, and `grafeno doctor` checks them.
The engine's releases are tagged `engine-v<version>`; what each one
changes is in [CHANGELOG.md](CHANGELOG.md).

From source, with Rust installed:

```bash
cargo build --release -p grafeno-server
```

The binary is `target/release/grafeno`.

A `.redb` file can be open in only one process at a time: stop the service
before opening its file with the console, or connect the console to the
service instead (`grafeno shell --config …`).

## A query, relationships and history

With the `grafeno` binary installed, open a local shell:

```bash
grafeno shell --db ejemplo.redb -n demo
```

Create a person and a relationship with a starting date:

```cypher
CREATE (a:Person {name: 'Ana'})
  -[:works_at {@valid_from: '2024-01-01'}]->
  (h:Organization {name: 'Hospital Central'})
RETURN a.name, h.name;
```

Query what was valid on a particular date:

```cypher
MATCH (p:Person)-[r:works_at]->(o:Organization)
VALID AT '2024-06-01'
RETURN p.name, o.name, r.@valid_from;
```

GQ also includes aggregates, indexes and algorithms for paths, centrality,
communities and similarity. See the [GQ reference](docs/query-language.md).

## Integrate applications and agents

| Access mode | Entry point |
| --- | --- |
| HTTP service, MCP server and shell | [Grafeno server](crates/grafeno-server/README.md) |
| A graphical interface | Grafeno Studio, installed apart, through the HTTP API |
| JavaScript and TypeScript applications | [TypeScript client](clients/typescript/README.md) |
| Embedded Rust library | [`grafeno`](crates/grafeno/src/lib.rs) and [`grafeno-memory`](crates/grafeno-memory/src/lib.rs) |

To start MCP with its own database after installing the binary:

```bash
grafeno mcp --db agente.redb
```

To try the HTTP server with temporary in-memory data:

```bash
grafeno serve --memory
```

It listens on `127.0.0.1:7810` by default. Data in `--memory` is lost when
the process exits. For persistent, authenticated
configuration, follow the [server guide](docs/install.md#settings).

## Repository structure

| Directory | Responsibility |
| --- | --- |
| `crates/grafeno-core` | Nodes, edges, identifiers and time. |
| `crates/grafeno` | redb storage, transactions, indexes, traversals and algorithms. |
| `crates/grafeno-memory` | Hybrid retrieval, context, extraction and memory maintenance. |
| `crates/grafeno-query` | GQ parser, planning and execution. |
| `crates/grafeno-server` | HTTP API, MCP, shell and administration: the `grafeno` binary. |
| `crates/grafeno-raft`, `crates/grafeno-cluster` | Raft and the cluster. |
| `clients/typescript` | HTTP client for applications. |
| `packaging` | Windows setup, Linux packages, the one-liner's setup, a cluster compose. |

## Development and tests

The [contribution guide](CONTRIBUTING.md) and its
[Spanish version](CONTRIBUTING.es.md) cover prerequisites and an alternative
that reduces temporary storage usage.

```bash
cargo test --workspace --locked
```

The [contribution workflow](.github/workflows/ci.yml) checks the engine and
the server, and the TypeScript client against the real binary. Building needs
no Node: only the client's tests use it.

## Design, performance and scope

- [Design and decisions](DESIGN.md): architecture, storage and temporal semantics.
- [Phases](docs/phases/README.md): each of Grafeno's phases in its own file, with its steps.
- [Performance](docs/performance.md) and [benchmarks](docs/benchmarks.md): methods, data and limits of project measurements.
- [Batched reads: contract, tests and reproducible measurements](docs/read-many.md).
- [Graph expansion](docs/graph-expansion.md): batched neighborhoods (the endpoint the Studio's graph view uses).
- [Development history](docs/development-history.md): every phase built so far, in detail.

Embeddings are provided by the client. Context uses an estimated token count by
default; the Rust library accepts a model-specific counter. The engine supports
multiple readers and one writer per Raft group; a cluster replicates every
write to its members (phase 13).
The license remains an open decision in the design.

Created by [Seerendo](https://github.com/Seerendo). The visual identity depicts
three fused carbon rings from a graphene lattice.
