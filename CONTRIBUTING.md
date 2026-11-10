# Contributing to Grafeno

**English** | [Español](CONTRIBUTING.es.md)

This guide prepares the engine and its TypeScript client for development and
verification. Run commands from the repository root unless a different
directory is specified. The graphical interface, Grafeno Studio, lives in its
own repository ([Grafeno Studio](https://github.com/grafenodb/grafeno-studio)) since Grafeno 15.

## Requirements

- Stable Rust through rustup. The workspace declares Rust 1.88 as its minimum;
  CI uses stable Rust and does not yet verify the minimum supported version.
- On Windows: Visual Studio Build Tools with the C++ tools and the MSVC Rust
  toolchain.
- Node.js 24 and npm only for the TypeScript client's build and tests; the
  engine builds without them.
- To build the installers: Docker (the Linux `.tar.gz`, `.deb` and `.rpm`),
  and Inno Setup 6 on Windows (the setup `.exe`).

Check the tools before starting:

```bash
rustc --version
cargo --version
```

## Check the engine

```bash
cargo test --workspace --locked
cargo build --locked -p grafeno-server
```

When temporary storage is limited, on Linux you can reduce build artifacts
with this variant (also used by CI):

```bash
CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
  cargo test --workspace --locked -j 2
```

It disables debug information and incremental compilation while preserving
tests and debug assertions. It does not require changes to `Cargo.toml`.

## Check the client

```bash
npm ci --prefix clients/typescript
npm run build --prefix clients/typescript
npm test --prefix clients/typescript
```

Client tests start the real server from `target/debug/grafeno`
(`grafeno.exe` on Windows), so they require the server build above. Set
`GRAFENO_BIN` to use a different binary. Keep the lockfile; use `npm ci` and
Cargo commands with `--locked` to detect unexpected dependency changes.

## Build the installers

See [the installers](docs/install.md#building-the-installers) for the Windows
setup, the Linux packages and the one-liner, and how each is tested in clean
machines.

## Contribution checks

The [contribution checks](.github/workflows/ci.yml) workflow runs on pull
requests, pushes to `main`, and manual dispatch. Pushes to a branch with an
open PR therefore do not duplicate runs. It checks separately:

- The TypeScript client's types and build.
- Rust engine and server tests, plus the client against the real server.

These checks do not replace tests of the installers on clean machines. Work on
a branch and open a focused PR. Describe the problem, resulting behavior and
checks performed, including any you could not run. Preserve the project's
technical names and commands.

## License

Grafeno is licensed under either of the MIT license (`LICENSE-MIT`) or the
Apache License 2.0 (`LICENSE-APACHE`), at your option. Unless you state
otherwise, any contribution you submit for inclusion is licensed the same
way, without any additional terms or conditions.

Documentation and sources describe Grafeno on its own terms: no other
company, product, client application or game by name (operating systems
and Grafeno's dependencies are fine). `crates/grafeno/tests/names.rs`
checks it.
