# Phase 6: Studio and installers

[All phases](README.md) · [← Phase 5: Shell and query language](phase-05-shell-and-query-language.md) · [Phase 7: Memory →](phase-07-memory.md)

**Status:** done.

Grafeno Studio embedded in the binary; Windows installer and service; `.deb`, `.rpm`, `install.sh` and a systemd service ([docs/install.md](../install.md)).

## Studio and distribution (phase 6)

- **Grafeno Studio** (`studio/`): React, Vite and TypeScript, with
  its own canvas graph engine on d3-force (phase 9.3) and the TypeScript
  client for every call.
  Views: overview, explorer (create and edit nodes, labels, typed
  properties, text, importance and facts in place; bulk archive and
  delete), graph (create nodes, connect them by clicking, inspect and end
  facts, "valid at" time travel), scripts (a SQL-editor-style GQ workspace
  on CodeMirror: tabs kept in the browser, highlighting and completion, run
  at cursor or all statements, errors marked on their line, results as a
  table, JSON or graph), search, memory and administration, plus a command
  palette and light and dark themes. Scripts are split into statements in
  the browser; each runs as its own `POST /query`, so each is its own
  transaction.
- **Logo**: a fragment of graphene, three fused six-carbon rings
  (`docs/assets/logo.svg`, `studio/public/favicon.svg`). The Windows icon
  (`packaging/windows/grafeno.ico`) is embedded in `grafeno.exe` by
  `build.rs` and used by the installer; since 15.2 it is rendered from the
  Grafeno site's mark (`packaging/windows/grafeno-mark.svg`, the rings on a
  dark graphene lattice) by `packaging/windows/make-icon.sh`, at 16 to 256
  pixels.
- The build is embedded in the binary (`include_dir`, when `studio/dist`
  exists at compile time; otherwise `/studio/` explains how to build it) and
  served at `/studio/` without authentication; the API it calls still needs
  the token, which the Studio asks for. It uses relative paths and hash
  routing, so the same build can be hosted elsewhere with `--cors-origin`.
- **Settings file** (`grafeno.toml`): `db`, `addr`, `token`, `studio`,
  `cors_origins`. `grafeno config init` writes it with a random token; the
  installers call it. `grafeno serve --config` and `grafeno shell --config`
  read it; flags and environment variables take precedence.
- **Services**: on Windows, `grafeno service install|uninstall` registers
  the `Grafeno` service with the Service Control Manager (`windows-service`;
  stops on the SCM's stop request, logs to `grafeno.log`). On Linux, a
  systemd unit runs `grafeno serve --config` as the `grafeno` user, with
  sandboxing (`systemd-analyze security`: 1.3); the server stops gracefully
  on SIGTERM.
- **Packages**: a static musl binary for every Linux distribution, packed
  as `.tar.gz`, `.deb` (`cargo-deb`) and `.rpm` (`cargo-generate-rpm`) by
  `packaging/linux/Dockerfile`; an Inno Setup installer for Windows
  (`packaging/windows/grafeno.iss`); `install.sh` downloads a release and
  checks its SHA-256. `.github/workflows/release.yml` builds them on tags.
- Uninstalling never deletes data or settings.
