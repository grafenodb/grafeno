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
text, vector and graph retrieval to assemble context.

Grafeno is the engine alone, as a database server is: a server (HTTP API,
MCP for agents, clustering), a console (`grafeno shell`) and administration
commands, with no graphical interface. **Grafeno Studio**, the interface, is
a separate product installed on its own, and connects to engines through
their API like any other client. Grafeno also embeds as a Rust library.

## What you can do

| Capability | Purpose |
| --- | --- |
| **Connect knowledge** | Nodes with labels and properties, directed relationships and namespaces to organize data. |
| **Query the past** | Distinguish when a fact was valid from when the system knew it using `VALID AT` and `KNOWN AT`. |
| **Retrieve context** | Combine BM25 text search, HNSW vectors and graph expansion; pack context into a token budget. |
| **Maintain memory** | Resolve entities, review possible duplicates, record reversible merges and summarize communities without a model. |
| **Query** | GQ, Grafeno's declarative graph query language, from the console, the HTTP API, MCP or any client. |
| **Operate the database** | Accounts that sign in, roles with grants and denials down to labels, properties and records, API keys, backups, import and export, integrity checks and index rebuilds. |
| **Run a cluster** | Several servers kept in sync with Raft: a leader elected, failover, linearizable reads, large reads split across members, namespaces on several groups. |

## Install the engine

Windows (a setup `.exe` with the service, or winget), Linux on x86_64 and
aarch64 (signed APT and DNF repositories, `.deb`, `.rpm`, and a `curl`
one-liner that also suits AlmaLinux, Rocky and RHEL) and a `.tar.gz`: see
the [installation guide](docs/install.md). The engine runs as a service on
`127.0.0.1:7810`; its settings are in `grafeno.toml`. `grafeno setup` adds
instances (several databases on one machine, each with its own service),
and `grafeno doctor` checks them.
The engine's releases are tagged `v<version>`; what each one
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

```text
CREATE (a:Person {name: 'Ana'})
  -[:works_at {@valid_from: '2024-01-01'}]->
  (h:Organization {name: 'Hospital Central'})
RETURN a.name, h.name;
```

Query what was valid on a particular date:

```text
MATCH (p:Person)-[r:works_at]->(o:Organization)
VALID AT '2024-06-01'
RETURN p.name, o.name, r.@valid_from;
```

GQ also includes aggregates, indexes and algorithms for paths, centrality,
communities and similarity. See the [GQ reference](docs/query-language.md).

## An answer only the graph finds

Ana belongs to the Platform team, the team works on Atlas, and Atlas
decided on event-driven messaging. The decision's text names neither Ana
nor her team, so a search for the question alone cannot find it. Recall
starts from what the search finds and follows the facts:

```text
Ana ─member_of→ Platform team ─works_on→ Atlas ─decided→ Decision 42
```

```bash
cargo run -p grafeno-memory --example multi_hop
```

```text
What did Ana's team decide about service communication?

Text search alone:
1. Ana            0.0254  text #1
2. Mobile team    0.0250  text #2
3. Platform team  0.0246  text #3

With the graph (three hops):
1. Ana            0.0325  text #1, 2 hops from Mobile team, 1 hop from Platform team
2. Platform team  0.0319  text #3, 1 hop from Ana, 2 hops from Mobile team
3. Mobile team    0.0261  text #2, 2 hops from Ana, 2 hops from Platform team
4. Acme           0.0111  1 hop from Ana, 1 hop from Mobile team, 1 hop from Platform team
5. Atlas          0.0085  2 hops from Ana, 3 hops from Mobile team, 1 hop from Platform team
6. Pocket         0.0077  3 hops from Ana, 1 hop from Mobile team, 3 hops from Platform team
7. Decision 42    0.0032  3 hops from Ana, 2 hops from Platform team
8. Decision 17    0.0028  2 hops from Mobile team
```

Each result says why it came back. Decision 42 ranks above the Mobile
team's decision because Ana leads to it too. Everyone in the example is
also part of Acme, which has 200 more people: recall damps a node by how
many edges it has (`hubs`), so a node linked to everything does not rank
as relevant to everything. Without that damping, Acme's people fill the
results and both decisions drop out. The example is also a test
([tests/multi_hop.rs](crates/grafeno-memory/tests/multi_hop.rs)).

## Integrate applications and agents

| Access mode | Entry point |
| --- | --- |
| HTTP service, MCP server and shell | [Grafeno server](crates/grafeno-server/README.md) |
| A graphical interface | Grafeno Studio, installed apart, through the HTTP API |
| JavaScript and TypeScript applications | [`@grafenodb/client`](https://github.com/grafenodb/grafeno-sdk/tree/main/typescript), the TypeScript SDK (`npm install @grafenodb/client`) |
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
| `packaging` | Windows setup, Linux packages, the one-liner's setup, a cluster compose. |

## Development and tests

The [contribution guide](CONTRIBUTING.md) and its
[Spanish version](CONTRIBUTING.es.md) cover prerequisites and an alternative
that reduces temporary storage usage.

```bash
cargo test --workspace --locked
```

The [contribution workflow](.github/workflows/ci.yml) checks the engine and
the server, and runs the TypeScript SDK's tests (from
[grafeno-sdk](https://github.com/grafenodb/grafeno-sdk)) against the binary it builds. Building needs no
Node: only those tests use it.

## Design, performance and scope

- [Design and decisions](DESIGN.md): architecture, storage and temporal semantics.
- [Phases](docs/phases/README.md): each of Grafeno's phases in its own file, with its steps.
- [Performance](docs/performance.md) and [benchmarks](docs/benchmarks.md): methods, data and limits of project measurements.
- [Batched reads: contract, tests and reproducible measurements](docs/read-many.md).
- [Mental Palace](docs/mental-palace.md): memories arranged into rooms and found again by where they are, over MCP.
- [Graph expansion](docs/graph-expansion.md): batched neighborhoods (the endpoint the Studio's graph view uses).
- [Development history](docs/development-history.md): every phase built so far, in detail.

Embeddings are provided by the client. Context uses an estimated token count by
default; the Rust library accepts a model-specific counter. The engine supports
multiple readers and one writer per Raft group; a cluster replicates every
write to its members (phase 13).

## License

Grafeno is free software, licensed under either of the
[MIT license](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE), at
your option. Unless you say otherwise, a contribution you submit is
licensed the same way.

Created by [Seerendo](https://github.com/Seerendo). The visual identity depicts
three fused carbon rings from a graphene lattice.
