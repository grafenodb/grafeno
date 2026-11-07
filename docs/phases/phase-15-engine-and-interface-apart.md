# Phase 15: Engine and interface apart

[All phases](README.md) · [← Phase 14: Users, roles and permissions (RBAC)](phase-14-users-roles-and-permissions.md) · [Phase 16: Integrations →](phase-16-integrations.md)

**Status:** done, in five steps.

The engine's installers install only the engine (Windows `.exe`, Linux `.deb`, `.rpm` and a `curl` one-liner), as a professional installation in the way PostgreSQL's is, with no Docker needed; and the interface becomes a separate desktop product with installers of its own, talking to engines only through their public API.

Steps:

- [15.1 The engine alone (done)](#151-the-engine-alone-done)
- [15.2 A professional installation of the engine (done)](#152-a-professional-installation-of-the-engine-done)
- [15.3 The public API as the only contract (done)](#153-the-public-api-as-the-only-contract-done)
- [15.4 The desktop application without an engine (done)](#154-the-desktop-application-without-an-engine-done)
- [15.5 Two products, two releases (done)](#155-two-products-two-releases-done)

**Where it starts from.** The engine and the interface are tied both ways.
The `grafeno` binary embeds Grafeno Studio (the `studio` feature,
`include_dir` of `studio/dist`) and serves it at `/studio/`; its Windows
installer adds Studio shortcuts and opens it after installing, and its
`.deb` and `.rpm` add a `grafeno-studio.desktop` entry. Grafeno Desktop
(9.4, Tauri) links the engine crates (`grafeno`, `grafeno-server`) and runs
the engine inside its own process to open database files, reaching its
HTTP router without a socket. So installing the server brings a web
interface, and installing the desktop app brings a whole engine.

**The goal.** Two products with their own installers, versions and
releases:

- **Grafeno**, the engine: the `grafeno` binary (server, HTTP API, MCP,
  shell, administration commands) and nothing else, for servers and
  machines without a screen. Installed natively, as a database server is
  (PostgreSQL is the model): Windows setup `.exe` (with the service), Linux
  `.deb` and `.rpm` from signed package repositories (with the systemd
  unit), a `curl … | sh` one-liner, and a `.tar.gz` for offline machines.
  No Docker image is published (the installers make it unnecessary); a
  test cluster's compose builds its own.
- **Grafeno Desktop**, the interface: the Studio as a desktop application,
  Windows `.exe` (and `.msi`), Linux `.deb`, `.rpm` (and `.AppImage`). It
  contains no engine: it talks to engines (on the same machine or others,
  one server or a cluster) only through their public HTTP API, as any
  other client does.

Phases 13 and 14 add views to the Studio as it is (cluster status, sign-in,
users and roles); this phase moves them with it. Each step ends with tests,
documentation and its own commit.

## 15.1 The engine alone (done)



- The Studio leaves the engine: no `studio` feature, no `include_dir`, no
  `/studio/` route (it answers with a short note pointing to Grafeno
  Desktop), no Node or Vite step in the engine's build.
- The engine's packages install only the engine: the binary, `grafeno.toml`,
  the service (Windows) or systemd unit (Linux), the icon of the binary and
  the documentation. No shortcuts to a web interface, no `.desktop` entry,
  no dependency on a graphical stack (no WebKit or GTK in the `.deb` and
  `.rpm`).
- The binary's size before and after is measured; the release notes say
  where the interface went.
- Done when: the engine builds and passes every test without Node installed,
  and installing its packages on a clean Windows and in clean Debian and
  Fedora containers brings no graphical files or dependencies.

As built:

- The Studio, the desktop app and a copy of the TypeScript client moved to
  their own repository, **Grafeno Studio** ([grafenodb/grafeno-studio](https://github.com/grafenodb/grafeno-studio)), with their own
  checks; their history up to here stays in this one (14.5, `87dc149`). The
  desktop app came as it was, to be rebuilt there to talk to engines only
  over HTTP. The TypeScript client stays here as the engine's client
  library (the API's contract, 15.3), as node-postgres is next to
  PostgreSQL; no installer of the engine carries it.
- The engine serves no interface: no `studio` feature, no `include_dir`, no
  `build.rs` step but the Windows icon; `/` and `/studio/` answer a short
  page saying the engine has no interface and Grafeno Studio is installed
  on its own. `studio` in `grafeno.toml` and `--no-studio` are still
  accepted (and ignored), so older settings and scripts keep working.
- The packages install the engine only: the binary, the settings, the
  service (or systemd unit), the setup script and the README. The `.deb`
  and `.rpm` lost the `grafeno-studio.desktop` entry and the `hicolor`
  icons (`packaging/linux/icons` is gone) and depend on nothing graphical;
  the Windows setup lost the Studio shortcut and "Open Grafeno Studio". No
  Docker image is offered any more (the installers make it unnecessary): the
  Dockerfile that remains builds the test cluster of `packaging/compose`.
  The Linux packages build without a Node stage; CI and
  the release workflow build and test the engine with Rust alone (Node
  only for the client's tests).
- The binary (Windows, release) went from 27 877 888 to 26 907 136 bytes
  (0.93 MiB, 3.5% smaller).
- Tests: `http.rs` (the note at `/` and `/studio/…`, CORS), `config.rs`
  (older settings with `studio` load), the whole workspace with no Node.

## 15.2 A professional installation of the engine (done)



Today: `install.sh` (`curl -fsSL …/install.sh | sh`) downloads a release
archive, checks its SHA-256 and installs the binary, and with `--service`
creates the `grafeno` user, `/etc/grafeno/grafeno.toml` and the systemd
service; the `.deb` and `.rpm` do the same from a downloaded file; the
Windows setup (Inno Setup) installs the binary, the configuration and the
service. Missing, next to how PostgreSQL installs: package repositories, a
setup step that initializes an instance, several instances on one machine,
upgrades that protect the data, a guided Windows setup, and signatures.

- Linux, from the package manager: signed APT and DNF/YUM repositories
  (`apt install grafeno`, `dnf install grafeno`), so updates come with the
  system's; packages for x86_64 and aarch64, for current Debian, Ubuntu,
  Fedora, RHEL and their derivatives, with SELinux and AppArmor policies
  where the distribution expects them.
- The one-liner, kept and widened: `curl -fsSL https://… | sh` detects the
  distribution and architecture, and either adds the repository and
  installs the package (the default where one exists) or installs the
  binary from the archive, verifying its checksum and signature. Options
  for a version, a prefix, installing without root (into the user's home,
  with a systemd user service), and unattended use (all choices as flags or
  environment variables).
- Initializing an instance, apart from installing the program, as `initdb`
  and `postgresql-setup` are: `grafeno setup` creates the data directory
  with its permissions, writes `/etc/grafeno/<instance>.toml` (address,
  port, durability, memory, TLS), creates the first administrator (a
  password, with phase 14) and enables and starts the service. Interactive
  by default; every answer as a flag or an answers file for automation.
- Several instances on one machine (as PostgreSQL clusters): a templated
  unit (`grafeno@<name>.service`), each instance with its own data
  directory, configuration, port and logs; `grafeno instances` lists them.
- Upgrades that protect the data: the package's upgrade backs up each
  instance (phase 4's `backup`) before a new version opens it and upgrades its
  layout, keeps the backup until the first clean start, and can roll back
  to the previous version with it. Removing the package keeps the data;
  purging (`apt purge`, an explicit `--purge`) removes it.
- Windows, a guided setup as PostgreSQL's installers have: install folder,
  data folder, port and address, the account the service runs as, the first
  administrator's password, a firewall rule, `grafeno` on the `PATH`; the
  same answers as parameters for a silent install (`/VERYSILENT /DATADIR=…
  /PORT=…`); upgrades in place that keep data and settings; uninstalling
  keeps the data unless asked; a `winget` package.
- Operations a server administrator expects: logs in journald and the Event
  Log (and a file where configured), log rotation, `grafeno doctor` (checks
  permissions, ports, the service, free disk space and the layout version),
  and the paths of everything documented in one table.
- Trust: release archives and packages signed (the repository keys and a
  `minisign` key published), the Windows setup and binary signed with a
  code-signing certificate where one exists, checksums published with
  every release.
- Done when: in clean machines (Debian, Ubuntu, Fedora, RHEL-compatible, on
  x86_64 and aarch64, and Windows 10 and 11), with no Docker, the one-liner,
  the repositories, the packages and the Windows setup each install,
  initialize, upgrade (with the backup taken) and remove the engine,
  interactively and unattended, and `grafeno doctor` passes after each.

As built (in two parts: the installers, then the rest):

- The one-liner (`install.sh`): as root it reads `/etc/os-release` and
  `uname -m` (x86_64 or aarch64), adds Grafeno's signed repository (DNF on
  RHEL, AlmaLinux, Rocky, Oracle Linux, CentOS Stream, Fedora and Amazon
  Linux; APT on Debian, Ubuntu and their derivatives) and installs the
  package from it, so updates come with the system's; when the repository
  cannot be reached, or with `--no-repo`, it installs the release's
  package file. Elsewhere, without root or with `--binary`, the static
  binary; `--service` adds the system service as root and a systemd user
  service otherwise (settings and data in the home folder). Every download
  is checked against its published SHA-256, and against its minisign
  signature when a key is known (a wrong key stops it). Options as flags
  or environment variables (`--version`, `--binary`, `--no-repo`,
  `--prefix`, `--service`, `--no-start`, mirrors).
- Packages for x86_64 and aarch64 (`.deb` amd64 and arm64, `.rpm` x86_64
  and aarch64, `.tar.gz`), built by one Dockerfile: aarch64 is
  cross-compiled with Zig (`cargo zigbuild`); releases also carry them
  under names without the version, which the latest-release URL needs.
  Sizes of 0.1.0: the `.deb` 6.1 MB (amd64) and 5.2 MB (arm64), the `.rpm`
  6.7 MB (x86_64, with the SELinux module) and 6.1 MB (aarch64), the
  `.tar.gz` 9.2 and 8.4 MB; the Windows setup 7.8 MB.
- Signed APT and DNF repositories (`packaging/repo/build-repo.sh`): signed
  `.rpm` files and `repomd.xml`, an APT `Release` signed as `InRelease`
  and `Release.gpg`, the public key and `grafeno.repo` beside them; new
  packages join the old, so earlier versions stay installable (and
  downgrades work). The release workflow publishes them on the `gh-pages`
  branch for GitHub Pages, and signs every release file with minisign, the
  repositories with GnuPG, and the Windows program and setup with a
  code-signing certificate, each when its secret is set.
- `grafeno setup` initializes an instance as `initdb` and
  `postgresql-setup` do: the data folder (0750, the `grafeno` user's), the
  settings (a fresh token; address, port, network, durability, memory;
  0640 root:grafeno), the database and its first administrator (a
  superuser with a password; or none, with `--no-admin`), and the service,
  enabled and started. Interactive by default; every answer a flag or a
  line of an answers file (`--answers`, `--unattended`). A database already
  in the folder is used as it is. A data folder outside `/var/lib/grafeno`
  is added to the unit's `ReadWritePaths` (a drop-in), to AppArmor's
  `local/grafeno` and to SELinux's labels. TLS is not among the questions:
  the engine serves plain HTTP (behind a proxy for TLS), as before.
- Several instances on one machine: `default` is `grafeno.toml` and
  `grafeno.service` (what the packages make on a first install, so
  existing installs are the default instance); any other name is
  `<name>.toml` and the templated `grafeno@<name>.service`, with its data
  in `/var/lib/grafeno/<name>` and its logs in journald. `grafeno
  instances` lists them (address, service state, database), `grafeno
  instances drop <name> [--purge]` removes one. On Windows there is one
  instance, the `Grafeno` service.
- Upgrades protect the data: the engine itself, before upgrading an older
  file's layout, copies the file as it is to `grafeno.layout-<old>.redb`
  beside it (`Grafeno::copy_before_upgrade`, on by default in
  `OpenOptions`; `copy_before_upgrade = false` in the settings), on every
  system and for every way of installing; one copy per database, the next
  upgrade's replacing it; no room for the copy means no upgrade and the
  file untouched. `grafeno instances rollback <name>` stops the service and
  puts the copy back (the newer file set aside, not deleted) for the
  previous version, installed next. Package upgrades restart every running
  instance. Removing the packages keeps the data; purging the `.deb`
  removes `/etc/grafeno` and `/var/lib/grafeno` (Debian's rule; the
  earlier "kept even on purge" is gone); with the `.rpm`, `grafeno
  instances drop --purge`.
- `grafeno doctor` checks each instance (or one): the settings and whether
  they are open to every user, the data folder and whether the service's
  user can write it, the database's layout (from the server when it runs:
  `/health` now gives `layout`; else from the file, read without upgrading
  it: `Grafeno::file_layout`), whether the running server is the installed
  version, free disk space against the database's size (the copy before
  an upgrade needs as much), copies left by upgrades, the service, and the
  port. It fails (exit 1) on a broken check; warnings and notes do not.
- Confinement: an AppArmor profile (`/etc/apparmor.d/grafeno`, enforced;
  selected by the units with `AppArmorProfile=-grafeno`, so `grafeno` run
  by hand is not confined) in the `.deb`, and an SELinux policy module in
  the `.rpm` (`grafeno_t` entered from systemd, `grafeno_etc_t`,
  `grafeno_var_lib_t`, `grafeno_port_t` for 7810-7819). The SELinux domain
  is permissive: no enforcing SELinux machine was available to prove it,
  so it logs what it would deny and denies nothing.
- Windows: the guided setup also asks the account the service runs as
  (`NT SERVICE\Grafeno`, a virtual account given Modify on Grafeno's two
  folders and nothing else, by default; or LocalSystem) and the first
  administrator (name and password, handed to `grafeno users create` on
  its standard input from a file deleted right after); upgrades keep the
  account; uninstalling asks whether to remove the settings and the data
  (no by default; `/PURGE=1` unattended). The service writes to the Event
  Log (source `Grafeno`, registered by the setup; `grafeno.exe` carries a
  one-message table so Event Viewer shows the text) and to `grafeno.log`,
  rotated at 10 MiB. `grafeno setup` on Windows makes the settings, the
  administrator and the service in one step (for the bare zip). winget
  manifests (`packaging/windows/winget/make-manifests.py`, validated by
  `winget validate`) come with each release. The installers' icon is the
  Grafeno site's mark (`grafeno-mark.svg`), rendered to `grafeno.ico` by
  `packaging/windows/make-icon.sh`.
- Tested on clean machines: `packaging/linux/test-install.sh --repo
  --systemd --arm64` passes in AlmaLinux 9 and 8, Rocky 9, Fedora 42,
  Debian 12 and Ubuntu 24.04 containers, from the release's files and
  from the repositories, and in AlmaLinux 9 and Debian 12 on aarch64
  (emulated): checksums and signatures, the package managers checking the
  repositories' signatures, the default instance answering, `grafeno
  doctor`, nothing graphical, and purging the `.deb`. With systemd
  (AlmaLinux 9): a second instance with an administrator who signs in; an
  instance with its data in `/srv` (the drop-in written), dropped with its
  data; a layout-16 database copied before its upgrade, then rolled back;
  a package upgrade restarting the instances; removal keeping the data.
  The `installers` workflow repeats it, installs the `.deb` on an Ubuntu
  runner with AppArmor enforced (the service and an instance with its data
  elsewhere confined, working, nothing denied), and on a Windows runner
  installs (the virtual account, an administrator who signs in, events in
  the Event Log, `grafeno doctor`), upgrades, uninstalls keeping the data,
  reinstalls over it and uninstalls with `/PURGE=1`. It found two bugs of
  the setup before any release: the uninstaller read a wizard page that
  does not exist there (it stopped before removing the service), and the
  data folder was read only from double-quoted TOML strings, while TOML
  writes Windows paths between single quotes.
- Not done, and why: the repositories, minisign and code signing need
  keys and a certificate only the project's owner can make (the workflow
  uses them once the secrets exist; `install.sh` checks minisign once its
  `MINISIGN_KEY` is set), GitHub Pages must be turned on for `gh-pages`,
  and the winget manifests must be submitted to `microsoft/winget-pkgs`
  after a release (it will also ask for a license). Several instances on
  Windows (one service each) and an enforcing SELinux policy are left for
  later.

## 15.3 The public API as the only contract (done)



- Everything the interface does goes through the documented HTTP API:
  whatever it reaches today in-process or through app-only commands
  (opening files, import and export, the database's own settings) gets an
  endpoint, or stays in the interface as a file it reads and sends.
- Versioning: the engine reports its version, its API version and its
  capabilities (`GET /v1/about`: authentication modes, cluster, features
  added by later phases); the interface states the API versions it speaks,
  checks them on connecting, and turns off what an older engine lacks
  instead of failing.
- The TypeScript client becomes the published contract (its own version,
  generated types where possible), and a contract test runs the interface's
  client against the real engine binary of each supported version.
- Done when: the interface has no call outside the public API, and the
  contract tests pass against the current engine and the previous release.

As built:

- `GET /v1/about` (public): the product, the engine's version, the API
  version and every version it answers to (`api: {version: 1, supported:
  [1]}`), the layout, the capabilities, how callers authenticate (`token`,
  `accounts`, `open`) and, on a cluster, the member and its group. Every
  response under `/v1` carries `x-grafeno-api: 1`. The rule: the API
  version changes only for a change that would break a client of the
  previous one; additions keep it and add a capability. The capabilities
  are 42 names (`crates/grafeno-server/src/about.rs`), one per feature a
  client may have to check (`audit`, `query.split`, `cluster.groups`, …),
  each documented in the server's README.
- `GET /v1/settings` (superusers): what the server runs with (its file,
  flags and defaults resolved), without the token or the cluster secret,
  which were the database's settings the interface could not see.
- The API is the routes the server answers, all of them in the README's
  tables: `tests/contract.rs` reads the router's `.route(...)` calls and
  the tables and fails on any difference (it found ten routes missing from
  the tables, the cluster's internal ones among them, and five rows that
  named routes in a shape the router did not have; the internal routes now
  have a table of their own).
- `grafeno restore BACKUP NEW`: recovering a backup into a new file
  (pages checked, layout upgraded, namespaces verified), the part of the
  desktop app's restore that needs the engine, now a command of the
  installed binary.
- The TypeScript client is the published contract: version 0.2.0 of its
  own (`CLIENT_VERSION`), `API_VERSIONS = [1]`, `about()`, `connect()`
  (fails with `incompatible` when no API version is shared), `supports()`
  and `settings()`. An engine from before `/v1/about` answers 404 there:
  the client tells it from `/health` (`legacy`, with the capabilities of
  that time), so one client serves both. The types stay written by hand:
  generating them would need a schema of every route, which is more than
  the contract test buys.
- Contract tests: `clients/typescript/test/contract.test.ts` runs the
  client against every engine binary in `GRAFENO_CONTRACT_BINS`, using
  what each says it supports (nodes, edges, text search, GQ, traversals,
  recall, export and import, backups, accounts and sessions, settings),
  plus an engine of an unknown API version (refused), a legacy one (told
  from `/health`) and one with newer capabilities (unknown names ignored).
  Run against this engine and the one built from the commit before 15.3
  (which has no `/v1/about`): both pass, the older as legacy. The CI runs it
  against the engine it builds and the latest published release (none yet:
  it says so and runs the current one only).
- Grafeno Studio (its repository): checks each engine with `connect()`, refuses
  an incompatible one with a clear message, hides the views an engine
  lacks (memory, cluster, access), and backs up through `GET /v1/backup`
  for local and remote databases alike (the desktop app only writes the
  file). What remains outside the API is the desktop app's local files:
  opening one and restoring a backup still use the engine linked into the
  app, which 15.4 replaces by starting the installed engine (`grafeno
  serve` on a loopback port, `grafeno restore`).

## 15.4 The desktop application without an engine (done)



- Grafeno Desktop stops linking the engine crates. Its sources (the Studio
  and the native shell) live in one place, `apps/desktop`, with no build
  dependency on the engine's workspace.
- Connections: saved servers (an address, a sign-in from phase 14 or a key),
  several open at once, clusters by their members (phase 13), the local
  service found on its default address.
- Local databases, without embedding the engine: when the user opens a
  file, the app asks an installed engine to serve it, by starting the
  `grafeno` binary found on the machine as a child process on a loopback
  port with a token for that session, and stopping it on close. If no
  engine is installed, the app says so and links to the engine's installer.
- Tauri or Electron: Tauri is what 9.4 built (the system web view, a Rust
  shell, installers of a few megabytes); Electron bundles Chromium (one
  rendering engine everywhere, installers of about a hundred megabytes).
  The choice is measured (installer size, memory, start time, rendering of
  the graph view on WebKitGTK) and recorded here; Tauri is kept unless the
  measurements say otherwise.
- Done when: the app, built with no engine crate, opens a local file through
  an installed engine, connects to a remote server and to a cluster, and
  passes its end-to-end tests against the engine binary.

As built (the app in the Grafeno Studio repository, `desktop/`; the engine's part here):

- The engine: `grafeno serve --stop-with-stdin` stops cleanly, as on
  Ctrl+C, when its standard input closes, which is what happens when the
  program that started it closes it or ends, a crash included; a test runs
  the real binary, closes its stdin, and finds the file closed cleanly
  (`verify_file`: clean). With 15.3's `grafeno restore`, it is all the app
  needs of the engine besides its HTTP API.
- The app links no engine crate (its own `Cargo.toml`, version 0.2.0, with
  no path into this repository); its sources live in the Grafeno Studio repository, beside the
  Studio, rather than in `apps/desktop` here, since the two products were
  split into two repositories in 15.1.
- Local files: the app finds the installed engine (`GRAFENO_BIN`, the
  `PATH`, then where the installers put it, checked with `grafeno
  --version`) and starts it per window as `grafeno serve --db FILE --addr
  127.0.0.1:0 --token <256 random bits> --stop-with-stdin`, waiting for its
  `listening on` line; a file another program holds is refused with a
  clear message. The Studio calls the session's internal address
  (`http://grafeno.local/session/<id>/…`), which the app sends to the
  engine with the token: the Studio never sees the port or the token.
  Closing the file or the window, or the app ending, closes the engine's
  stdin. Restoring runs `grafeno restore`. With no engine installed, the
  start screen says so and links to the installers.
- Connections: each window has its own (File → New window); saved servers
  with a token or key, or a sign-in; the local service, found by asking
  `127.0.0.1:7810/v1/about`; a cluster by its members, separated by commas
  (the client tries them in turn). Menus act on the focused window.
- Tests against the engine binary: a file served on a loopback port, the
  token required, a backup through the API recovered by `grafeno restore`,
  a held file refused, closing stopping the engine cleanly (the file
  reopens with its data), no engine left after a session ends, and a file
  the engine cannot open refused with its reason; they pass. The Linux
  WebDriver test of the window follows the new paths; it was not run here
  (it needs Linux with a display), and the Grafeno Studio repository's workflow runs the
  native tests with an engine built from this repository.
- Tauri or Electron, measured on the laptop of E1 (Windows 11, WebView2),
  with the same Studio build (0.9 MB):

  | | Tauri (Grafeno Desktop 0.2.0) | Electron 33.2.1 (a minimal shell) |
  | --- | --- | --- |
  | Installer | 3.1 MB (NSIS), 4.4 MB (MSI) | the runtime alone 269 MB, 110 MB as a zip |
  | Memory after 8 s, the window and its child processes | 376 MB working set, 355 MB private (the app 32 MB, WebView2 the rest) | 330 MB working set, 250 MB private |
  | Until the window has its title | 0.85 s (a debug build) | 0.48 s |

  Windows' application control (Smart App Control) refused to run the
  unsigned release build, so Tauri's memory and start were measured on the
  debug build: its Rust part is unoptimized and runs a console, which
  makes both numbers worse than a release build's, and WebView2, most of
  the memory, is the same in both. Memory is about the same because
  WebView2 is Chromium too; the difference is the installer, 35 times
  smaller, and a web runtime that Windows updates. Rendering on WebKitGTK
  (Linux) was not measured: no Linux desktop was at hand. Tauri is kept.

## 15.5 Two products, two releases (done)



- Separate versions and change logs; separate release workflows, started by
  their own tags (`engine-v…`, `desktop-v…`), each building its installers
  on Windows and Linux, signing them where certificates exist, and
  publishing checksums.
- Install tests per product in clean machines (Windows, Debian, Fedora):
  the engine package brings no graphical files; the desktop package
  installs no service and no engine; installing, upgrading and removing
  either leaves the other untouched, and data stays where it was.
- Documentation split: `docs/install.md` for the engine (every way of
  installing it, initializing instances, upgrading, the paths); the desktop
  app's own guide, with the engine versions it supports.
- Done when: a tagged release of each product builds and passes its install
  tests on its own.

As built (the products already live in two repositories since 15.1):

- The engine (this repository): version 0.1.0, `CHANGELOG.md` with a
  section per version (its release's notes), releases started by
  `engine-v<version>` tags; the workflow refuses a tag that does not name
  the version in `Cargo.toml` or has no section in the change log. The
  one-liner asks for `engine-v<version>` (`--version 0.1.0`), and the
  winget manifests point at those tags.
- Grafeno Studio and Grafeno Desktop (the Grafeno Studio repository): one product, version
  0.2.0 (`desktop/Cargo.toml`, `desktop/tauri.conf.json` and
  `studio/package.json` must agree), its own `CHANGELOG.md` saying which
  engines each version works with (API version 1, engine 0.1.0 or later),
  releases started by `desktop-v<version>` tags: the Windows installers
  (`.exe`, `.msi`, for the current user), the Linux packages (`.deb`,
  `.rpm`, `.AppImage`, built on the oldest supported runner) and the
  Studio's web build as a `.zip`, with checksums, minisign signatures and
  Windows code signing when their secrets exist.
- Install tests per product: the engine's `installers` workflow (15.2)
  shows its packages bring nothing graphical; the Grafeno Studio repository's `installers`
  workflow builds the desktop packages and the engine's from their
  repositories and installs them side by side. `desktop/packaging/
  test-install.sh` passes in Debian 12 and Fedora 42 containers: the
  desktop package alone brings no service, no engine binary, no
  `grafeno` user and no dependency on the engine; with the engine beside
  it and an instance made, removing the desktop leaves the engine and its
  files byte for byte, reinstalling and upgrading the desktop too, and
  removing the engine leaves the desktop and the data. On Windows the
  workflow installs the engine (its service, data in its own folder) and
  the desktop (`/S`, for the user), checks that the desktop brings no
  service and no `grafeno.exe`, that removing it leaves the service
  running with its data, and that removing the engine leaves the desktop
  and the data; it runs on that repository's
  pushes, and was not run on this laptop, where installing the engine's
  setup would replace a real service.
- The Linux packages of the desktop app, built in a container on Debian
  12 (`desktop/packaging/Dockerfile`): 6.1 MB as a `.deb` or an `.rpm`.
- Documentation: `docs/install.md` is the engine's (every way of
  installing it, instances, upgrades, the paths, and how the two products'
  releases relate); the desktop app's guide is its own `README.md`, with
  the engines it works with.
- Not done, and why: no release of either product has been tagged yet
  (the first `engine-v0.1.0` and `desktop-v0.2.0` are the owner's to
  push), so "a tagged release builds" is shown by the same jobs running on
  pushes, not by a published release.
