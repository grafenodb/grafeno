# Server CPU per request under concurrent users

An outside benchmark of Grafeno 0.5.1 in containers (a catalogue of
about 110,000 entities, against another search engine) saw every
authenticated request cost more CPU as concurrent users grew: from 0.16 ms
to 3.8 ms for `GET /v1/namespaces` between 1 and 16 users, while
`GET /health` stayed flat, and throughput stalled at 1.4 to 1.6 times its
single-user value while the server used 10 cores.

These tools measure it, in containers only.

- `crates/grafeno-server/examples/http_load.rs`: a closed-loop HTTP load
  (each user sends its next request when the previous answer arrives) for
  four cases: `health`, `namespaces`, `return1` (GQ `RETURN 1`) and `code`
  (an indexed `MATCH` by code), and `setup`, which loads `mms` nodes with
  an index on `code`.
- `Dockerfile`: the engine and that tool, built for musl (as the release)
  or glibc, with the system allocator or mimalloc.
- `run.sh <load image> <server image>...`: starts each server image in a
  container, loads it, runs each case with each number of users from
  another container, and reads the server's CPU from its cgroup. CSV out.
- `check.sh <load image> <server image>`: fails when the CPU per request
  with 16 users is twice or more that with 1 user. The Installers workflow
  runs it on the image it builds.

## What it found (2026-11-23)

Docker Desktop, 16 CPUs; 100,000 nodes; 10 s per case; the server's CPU
per request in ms ([data](../../docs/data/http_allocator.csv)):

| Case | Users | Published 0.5.1 image (musl) | musl + mimalloc | glibc |
| --- | ---: | ---: | ---: | ---: |
| `GET /health` | 1 / 16 / 64 | 0.31 / 0.39 / 0.37 | 0.29 / 0.32 / 0.30 | 0.31 / 0.33 / 0.31 |
| `GET /v1/namespaces` | 1 / 16 / 64 | 0.57 / **2.45** / 1.76 | 0.54 / 0.62 / 0.55 | 0.51 / 0.58 / 0.54 |
| GQ `RETURN 1` | 1 / 16 / 64 | 0.41 / **1.32** / 1.09 | 0.36 / 0.41 / 0.38 | 0.36 / 0.41 / 0.38 |
| GQ `MATCH` by code | 1 / 16 / 64 | 0.43 / **1.53** / 1.26 | 0.42 / 0.45 / 0.46 | 0.40 / 0.46 / 0.42 |

With 16 users the published image used 4 to 5.5 cores for what mimalloc
does with 1.2 to 1.9, at the same or a higher rate (`RETURN 1`: 3,982
against 4,573 requests per second; the load tool, one container, is the
limit here, not the server).

The cause is musl's allocator. The Linux release files (the static
binaries, the `.deb`, the `.rpm` and the container image) are built
against musl, whose allocator serializes allocations across threads: the
cost grows with every thread that allocates at once, and a request that
reads a snapshot and writes JSON allocates a great deal, where `/health`
hardly does. glibc's allocator and mimalloc do not have the problem, and
musl with mimalloc matches glibc. The Windows build was never affected
(its allocator is the system's); the project's earlier benchmarks ran
there, which is why this was not seen.

The Linux builds now use mimalloc (`--features mimalloc` in
`packaging/linux/build-packages.sh`); other builds keep the system's
allocator.

## Running it

```sh
docker build -f tools/bench-http/Dockerfile -t grafeno-bench:musl .
docker build -f tools/bench-http/Dockerfile --build-arg FEATURES=mimalloc -t grafeno-bench:musl-mimalloc .
docker build -f tools/bench-http/Dockerfile --build-arg TARGET=x86_64-unknown-linux-gnu -t grafeno-bench:gnu .
sh tools/bench-http/run.sh grafeno-bench:musl ghcr.io/grafenodb/grafeno:0.5.1 grafeno-bench:musl-mimalloc grafeno-bench:gnu
```

`NODES`, `SECONDS_EACH`, `CASES` and `USERS` change what runs (defaults
100000, 10, all four cases, 1 and 16 users).
