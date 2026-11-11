# Contributing to Grafeno

**English** | [Español](CONTRIBUTING.es.md)

This guide prepares the engine for development and verification. Run commands from the repository root unless a different
directory is specified. The graphical interface, Grafeno Studio, lives in its
own repository ([Grafeno Studio](https://github.com/grafenodb/grafeno-studio)) since Grafeno 15.

## Requirements

- Stable Rust through rustup. The workspace declares Rust 1.88 as its minimum;
  CI uses stable Rust and does not yet verify the minimum supported version.
- On Windows: Visual Studio Build Tools with the C++ tools and the MSVC Rust
  toolchain.
- Node.js 24 and npm only to run the TypeScript SDK's tests against the
  engine; the engine builds without them.
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

## Check the SDKs against your engine

The SDKs live in their own repository, [grafeno-sdk](https://github.com/grafenodb/grafeno-sdk). A change to
the HTTP API should keep them working: check out that repository next to
this one, and its tests use the engine built here:

```bash
git clone https://github.com/grafenodb/grafeno-sdk.git ../grafeno-sdk
```

```bash
npm ci --prefix ../grafeno-sdk/typescript
npm test --prefix ../grafeno-sdk/typescript
```

The tests start the real server from `target/debug/grafeno` (`grafeno.exe`
on Windows), so they require the server build above (or `GRAFENO_BIN`).
Use Cargo commands with `--locked` to detect unexpected dependency changes.

## Build the installers

See [the installers](docs/install.md#building-the-installers) for the Windows
setup, the Linux packages and the one-liner, and how each is tested in clean
machines.

## Contribution checks

The [contribution checks](.github/workflows/ci.yml) workflow runs on pull
requests, pushes to `main`, and manual dispatch. Pushes to a branch with an
open PR therefore do not duplicate runs. It checks separately:

- Rust engine and server tests.
- The TypeScript SDK (from grafeno-sdk) built and tested against the server
  just built, and its contract with the latest engine release.

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
