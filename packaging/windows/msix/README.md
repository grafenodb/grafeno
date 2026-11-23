# The Microsoft Store package (MSIX)

GrafenoDB in the Microsoft Store: the engine packaged as an MSIX, a channel
beside the setup (`packaging/windows/grafeno.iss`), winget, the zip and the
Linux packages, which do not change.

| | |
| --- | --- |
| Identity name | `GrafenoDB.GrafenoDB` |
| Publisher | `CN=9C88685D-AD17-4F9E-8FAE-1760744D79C1` |
| Publisher display name | `GrafenoDB` |
| Package family name (derived by Windows) | `GrafenoDB.GrafenoDB_rwmas8f7748ap` |
| Store ID | `9PH0179H0JL5` ([listing](https://apps.microsoft.com/detail/9PH0179H0JL5)) |

Files here: `AppxManifest.xml`, `Assets/` (rendered from
`../grafeno-mark.svg` by `make-assets.ps1`), `build.ps1`, `validate.ps1`,
`test-install.ps1`, `wack.ps1`, and `store/` (listing, privacy policy
draft, runFullTrust reason).

## Design

### What the setup does, and what a package cannot

The setup (run as administrator) installs `grafeno.exe` in Program Files,
writes `C:\ProgramData\Grafeno\grafeno.toml` (with a fresh token) and the
database under it, restricts both folders to SYSTEM, the administrators
and the service, registers the `Grafeno` Windows service (started with
Windows), opens the firewall when asked, and adds the folder to the system
PATH. Uninstalling keeps the settings and data unless asked.

Microsoft's documentation sets what a Store package can do:

- A packaged service (`desktop6:Service`) needs the restricted capabilities
  `packagedServices` (and `localSystemServices` for LocalSystem), which
  "in most cases" are not approved for the Store, and installing one needs
  an administrator.
- "Apps that require elevation for any part of their functionality won't
  be accepted into the Store."
- A packaged program cannot write in its install folder or to HKLM. What
  it writes under `%LOCALAPPDATA%` and `%APPDATA%` goes (Windows 10 1903
  and later) to a per-user, per-package private folder, kept through
  updates and **removed with the package**. Other folders of the user's
  profile are not redirected.
- A console program gets a command through an execution alias
  (`uap5:AppExecutionAlias`, `desktop4:Subsystem="console"`).

### Grafeno as the user's process

Weighed: a packaged service (not viable for the Store, above); a manager
app controlling an engine installed apart (it would not put the engine in
the Store, and adds an interface); **the engine as the user's own process**
(chosen): the same `grafeno.exe` with every command, no service, no
elevation, no capability but `runFullTrust`.

The manifest declares one application (the Store accepts no more; the
Windows App Certification Kit fails a package with two): **GrafenoDB**, in
the Start menu and as the `grafeno` alias. Started with no arguments from
the package (as the Start menu does), `grafeno` serves the user's
database; with arguments it is the usual command line. The minimum is
Windows 10 2004 (build 19041), the oldest version the package is meant to
be tested on. Grafeno listens on `127.0.0.1:7810`, so the firewall does
not ask.

### Where the data lives: `%USERPROFILE%\GrafenoDB`

Two ways were weighed for a database that must not be lost by surprise:

- **The package's private folder** (`%LOCALAPPDATA%\Grafeno`, redirected):
  kept through updates, but uninstalling deletes the database with no
  question asked. The first package did this.
- **A folder of the user's profile outside AppData** (chosen):
  `%USERPROFILE%\GrafenoDB`, which Windows does not redirect. It needs no
  administrator, belongs to the user (other standard users cannot read
  it), does not collide with the setup's `C:\ProgramData\Grafeno`, is not
  under Documents (often synced, which does not suit an open database
  file), and **stays when the package is uninstalled**, as the setup's
  data does. The install test checks all of this on a real installation.

Only when `grafeno.exe` runs from a package (it has a package identity,
`crates/grafeno-server/src/packaged.rs`):

- Settings: `%USERPROFILE%\GrafenoDB\grafeno.toml`; database:
  `%USERPROFILE%\GrafenoDB\data\grafeno.redb`.
- `grafeno` alone, or `grafeno serve` with no `--config`, `--db` or
  `--memory`, serves them,
  and creates them (with a fresh token) the first time. Before serving it
  prints what GrafenoDB is, the address, where the data is, that it stays
  on uninstall, how to back up, the CLI commands and how to stop. It never
  prints the token.
- **The first package's files** (in the private folder) are copied out on
  the first start: only when `%USERPROFILE%\GrafenoDB` has no settings
  yet; the database through a consistent backup, checked as it is
  restored; the settings keep their token; the old folder is left as it
  was, with `MOVED-TO-GRAFENODB-FOLDER.txt`; a file at the destination
  stops it (nothing is replaced); once only.
- `grafeno shell` with no connection options connects to that server;
  `grafeno backup <file>` alone backs up that database.
- `grafeno setup` and `grafeno service install/uninstall` are refused with
  what to do instead; `grafeno doctor` checks the user's settings.

Nothing changes for a program that is not packaged, except `grafeno
serve`'s explanation of a taken port and the new `grafeno migrate`.

### Conflicts

The setup's service and the package both default to port 7810. `grafeno
serve` checks the port before writing anything, and when it is taken says
by what: the Grafeno service installed with the setup (it asks the
service manager), another Grafeno server (it answers `/v1/about`), or
another program; that the data is not affected; and how to use another
port. A database file open in another process is refused with its own
message. Nothing is stopped or uninstalled.

### From the setup's installation: `grafeno migrate`

```powershell
grafeno migrate            # the plan; changes nothing
grafeno migrate --apply    # does it
```

It finds the setup's settings (`C:\ProgramData\Grafeno\grafeno.toml`), the
`Grafeno` service and whether it runs, the server answering with its
version and file layout (a newer layout than this program reads stops
it), the target (`%USERPROFILE%\GrafenoDB`), and a free port. With
`--apply`:

- the copy is consistent: a backup from the running service over HTTP
  (`GET /v1/backup`, while it keeps serving), or from the file when the
  service is stopped (never a raw copy of an open file);
- it is checked as it is restored into a new file; nothing existing is
  replaced, and an existing target stops it;
- new settings get a new token and port 7810, or the next free one when
  the service holds 7810; accounts, keys and roles come with the data;
- the service and its files are never stopped, changed or removed, and no
  token is printed. Remove the setup's installation yourself when the copy
  serves well.

Privileges: the setup's settings (with its token) are readable only by
SYSTEM and the administrators. Without them, start the service and give
its owner token with `--token` (an administrator gets it with the setup's
`grafeno config token`), or run `grafeno migrate --apply` once from an
elevated PowerShell. Ordinary use of the package needs no administrator.

Going back (package to setup) is not automated: `grafeno backup` here,
then `grafeno restore` into the setup's data folder as administrator.

### Backup and restore

- `grafeno backup <new file>`: from the running server (it keeps serving),
  or from the file when the server is stopped.
- To restore: stop the server, move
  `%USERPROFILE%\GrafenoDB\data\grafeno.redb` aside, then
  `grafeno restore <backup> %USERPROFILE%\GrafenoDB\data\grafeno.redb`
  (it checks every page and namespace and never replaces a file), and
  start the server.

### Updates

The package version is `Cargo.toml`'s with a fourth number 0 (the Store
reserves it): 0.5.1 → 0.5.1.0, 0.5.2 → 0.5.2.0, 1.0.0 → 1.0.0.0. Updates
keep the settings, the token and the database (tested). The database
layout follows the engine's rules (docs/install.md): a file stays layout
18 until it holds hyperedges, then 19, which engines before 0.5 refuse;
upgrades copy the file first. The Store does not install older versions
over newer ones; restoring an older backup is the way back.

## Building

Tools: Rust (the version the workflows pin) and the Windows SDK
(`makeappx.exe`, `signtool.exe`, `appcert.exe`), which GitHub's Windows
runners have.

```powershell
cargo build --release --locked -p grafeno-server
powershell -File packaging/windows/msix/build.ps1
powershell -File packaging/windows/msix/validate.ps1
```

`build.ps1` writes `dist\msix\GrafenoDB_<version>_x64.msix` and its
`.sha256`, unsigned; `-Version` overrides the version for a test.
`validate.ps1` checks the identity exactly, the architecture, the version
(fourth number 0), every image the manifest names, that the package holds
only the program, its images, the manifest and the licenses (no settings,
keys or certificates), no service and no capability but runFullTrust, and
that `grafeno.exe` is x64 and prints that version.

## Testing

`test-install.ps1` (in the workflow, on GitHub's Windows Server runner)
installs the package and checks, on the real installation:

1. install, the alias, `--version`, `--help`, `config path`, setup and
   service refused;
2. the first package's files copied out once (same token, data intact, old
   folder left with its note);
3. serving: the token, an HTTP write, the database in
   `%USERPROFILE%\GrafenoDB\data`, nothing in `C:\ProgramData`, the
   settings not open to other users, a second server refused with the
   cause, `doctor`, `shell`, `mcp` answering `initialize`;
4. backup while serving and while stopped, the start message (and no
   token in it), no process left, restore (and restore refusing to
   replace), the Start menu entry serving;
5. an update to a newer package keeping the token and the data;
6. uninstalling: the package, alias and private folder go,
   `%USERPROFILE%\GrafenoDB` stays; a reinstall finds it;
7. with the setup installed and its service on 7810: serving refused,
   naming the service, writing nothing; `grafeno migrate` planning without
   changes or tokens; `--apply` copying the service's data to new settings
   on 7811; a second `--apply` replacing nothing; the service still
   running with its data.

`wack.ps1` runs the Windows App Certification Kit on the package and
prints each test; the workflow runs it after the install test, keeps its
report as the artifact `wack-report-<version>` and shows its outcome in
the run's summary. It fails when a required test fails. Microsoft's
optional Desktop Bridge tests are "informational only and will not be
used to evaluate your app during Microsoft Store onboarding"; two show up
here and are reported, not failed:

- **Blocked executables** (optional; about Windows 10 S): `grafeno.exe`
  can start processes (`CreateProcessW`; the Rust standard library also
  names `cmd.exe`, to run batch files). Grafeno starts `icacls` and
  `whoami` to restrict its settings file, and nothing else; the package
  is not meant for Windows 10 S.
- **DPI awareness** (a warning): a console program draws no windows of
  its own.

The first runs also failed **Application count** (the package then had two
applications); it has one since, and that test passes.

### Windows 10 and 11 by hand

GitHub has no Windows 10 or 11 runner. On a disposable virtual machine of
each (Windows 10 2004 or later; Windows 11), as administrator:

1. Download the workflow's artifact, the setup and `grafeno.exe` (zip) of
   the same version.
2. `powershell -ExecutionPolicy Bypass -File test-install.ps1 -Package
   GrafenoDB_<v>_x64.msix -Update <a newer test package> -Setup
   <setup.exe> -Engine grafeno.exe` (a newer package: `build.ps1 -Version
   <x.y.z+1>.0`).
3. `wack.ps1 -Package GrafenoDB_<v>_x64.msix`.
4. By hand: open GrafenoDB from the Start menu and read its start message;
   close the window and check that no `grafeno` process is left.

With Smart App Control on (Windows 11), the test-signed package and the
unsigned `grafeno.exe` may be blocked; such a machine can check the
package as the Store signs it only after publication (or in a private
flight). Record each run here:

| Environment | Status |
| --- | --- |
| Windows Server (GitHub runner), CI | runs on every change (see the workflow) |
| Windows 11 x64 | pending |
| Windows 10 2004+ x64 | pending |
| WACK | in CI: every required test passes; Blocked executables (optional) fails, DPI awareness warns |
| Store certification | pending (first submission) |

## The workflow

`.github/workflows/msix.yml` runs by hand (Actions → MSIX → Run workflow),
on tags `v*`, and on changes to these files. It builds `grafeno.exe`, the
package and a newer one, checks them, builds the setup, runs
`test-install.ps1` and `wack.ps1`, and uploads the unsigned package
(`grafeno-msix-<version>`) and the WACK report. On a tag it fails if the
tag and `Cargo.toml` differ. It publishes nothing and does not change the
release workflow.

## Signing

1. **The unsigned package** (the artifact): what Partner Center takes. It
   does not install by double-clicking.
2. **Signed for installing outside the Store**: needs a code-signing
   certificate a machine trusts, whose subject is the Publisher. None is
   in this repository; the tests' certificate lives only for their run.
3. **Signed by Microsoft**: the Store signs the package it distributes.
   That signature covers that package only, not the setup, zip or MSI on
   GitHub Releases.

## Publishing in Partner Center

Automatic submission is not set up (it needs a Partner Center API client
with access to the account, stored as a secret).

1. Run the MSIX workflow on the release's tag (or by hand on its commit);
   check that the install test and WACK passed (the run's summary and
   report); download `grafeno-msix-<version>`.
2. Partner Center → Apps and games → **GrafenoDB** → **Start submission**
   (later, **Start update**).
3. **Pricing and availability**: free; markets; visibility (private while
   testing, then public); release as soon as certified or on a date.
4. **Properties**: category Developer tools; privacy policy URL (publish
   `store/privacy-policy-draft.md` after review); support contact; the
   restricted capability runFullTrust with the reason in
   `store/runfulltrust.md`.
5. **Age ratings**: the questionnaire (a developer tool; the app shares no
   user content).
6. **Packages**: upload `GrafenoDB_<version>_x64.msix`; Partner Center
   checks its identity against the product; device family Windows Desktop.
7. **Store listings**: English and Spanish from `store/listing.md`, with
   real screenshots (its list).
8. **Submission options**, notes for certification: "GrafenoDB is a
   database server with a console. Opening it from the Start menu starts
   the server on 127.0.0.1:7810 and prints how to use it; `grafeno --help`
   in PowerShell lists the commands."
9. **Submit to the Store.** Gradual rollout and package flights, where the
   account offers them, are on later updates' Packages page.

When certification fails, its report names the policy and the step. Usual
causes here: the runFullTrust reason, a listing that does not say what the
program does, or the program failing when the tester opens it. Fix, build
a package with a higher version and resubmit.

## Known limits

- One server per user from the package by default; others need
  `--config` or `--db` and another port.
- No service: Grafeno runs while its console (or the program that starts
  it) runs, and does not start with Windows.
- `%USERPROFILE%\GrafenoDB` stays after uninstalling; delete it to remove
  the data.
- x64 only.
