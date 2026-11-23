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

## What the package is, and why

### What the setup does, and what a package cannot

The setup (Inno Setup, run as administrator) installs `grafeno.exe` in
Program Files, writes `C:\ProgramData\Grafeno\grafeno.toml` (with a fresh
token) and the database under it, restricts both folders with `icacls`,
creates the first administrator, registers the `Grafeno` Windows service
(a virtual account or LocalSystem, started with Windows), opens the
firewall when asked, and adds the folder to the system PATH. Upgrades stop
and remove the service first; uninstalling removes the program, the
service, the firewall rule and the PATH entry, and keeps the settings and
the data unless asked. On Windows there is one instance (`grafeno setup`
refuses others). The program starts child processes only for `icacls`,
`whoami` and the service tools.

Microsoft's documentation sets what an MSIX package for the Store can do:

- A packaged Windows service (`desktop6:Service`) needs the restricted
  capabilities `packagedServices` (and `localSystemServices` for
  LocalSystem); Microsoft says "the use of this capability won't be
  approved" in most Store submissions, and installing one needs an
  administrator.
- "Apps that require elevation for any part of their functionality won't
  be accepted into the Store."
- A packaged program cannot write in its install folder or to HKLM, and
  what it writes under `%LOCALAPPDATA%` and `%APPDATA%` goes (since Windows
  10 1903) to a per-user, per-package private folder, kept through
  updates and removed with the package. Writes elsewhere in the user's
  profile are not redirected.
- A console program gets a command through an execution alias
  (`uap5:AppExecutionAlias`, `desktop4:Subsystem="console"`, Windows 10
  1709 and later): `grafeno` in PowerShell and CMD, without touching PATH.

### The choice: Grafeno as the user's process

Three ways were weighed:

- **A packaged service**: not viable for the Store (the capabilities above,
  an administrator to install). Not done.
- **A manager app controlling an engine installed apart**: it would not
  put the engine in the Store, and would add an interface to maintain. Not
  done.
- **The engine as the user's own process** (chosen): the same
  `grafeno.exe`, with every command; no service, no elevation, no
  capability but `runFullTrust` (every packaged desktop program has it).

What changes in the program, only when it runs from a package (it has a
package identity, `crates/grafeno-server/src/packaged.rs`):

- The settings are `%LOCALAPPDATA%\Grafeno\grafeno.toml` and the database
  `%LOCALAPPDATA%\Grafeno\data\grafeno.redb`, instead of
  `C:\ProgramData\Grafeno`. On disk they are in
  `%LOCALAPPDATA%\Packages\GrafenoDB.GrafenoDB_rwmas8f7748ap\LocalCache\Local\Grafeno`.
- `grafeno serve` with no `--config`, `--db` or `--memory` serves those
  settings, and creates them (with a fresh token) the first time.
- `grafeno shell` with no connection options connects to that server.
- `grafeno setup` and `grafeno service install/uninstall` are refused, with
  what to do instead.
- `grafeno doctor` checks the user's settings and says where the files are
  on disk; it does not look for a service.

Nothing changes for a program that is not packaged.

### The package

`AppxManifest.xml` declares two applications on the same `grafeno.exe`:

- **GrafenoDB**, in the Start menu: it opens a console that runs
  `grafeno serve` (`uap10:Parameters`, hence Windows 10 2004, build 19041,
  as the minimum). Ctrl+C or closing the window stops it.
- **GrafenoCli**, with no Start menu entry: the `grafeno` alias, given the
  arguments typed.

No service, extension or capability is declared beyond these. Grafeno
listens on `127.0.0.1:7810` by default, so Windows' firewall does not ask;
listening on the network needs a firewall rule the user adds.

## Building

Tools: Rust (the version the workflows pin), the Windows SDK
(`makeappx.exe`, `signtool.exe`; GitHub's Windows runners have it).

```powershell
cargo build --release --locked -p grafeno-server
powershell -File packaging/windows/msix/build.ps1
powershell -File packaging/windows/msix/validate.ps1
```

`build.ps1` writes `dist\msix\GrafenoDB_<version>_x64.msix` and its
`.sha256`, unsigned. The version is `Cargo.toml`'s with a fourth number,
0 (the Store reserves it): `0.5.1` gives `0.5.1.0`. To change it, change
the workspace version as for any release; `-Version` overrides it for a
test.

`validate.ps1` checks the identity (name and publisher exactly), the
architecture, the version, that every image the manifest names is there,
that the package holds only the program, its images, the manifest and the
licenses (no settings, keys or certificates), that it declares no service
and no capability beyond `runFullTrust`, and that `grafeno.exe` is an x64
program printing that version.

The images in `Assets/` come from `packaging/windows/grafeno-mark.svg`;
`make-assets.ps1` renders them again (with Edge, headless) when the mark
changes.

## Testing

`test-install.ps1` installs the package and runs it through its life:
install, `grafeno --version`, `--help`, `config path`, setup and service
refused, `grafeno serve` creating its settings and database in the
package's private folder (nothing in `C:\ProgramData` or the real
`%LOCALAPPDATA%\Grafeno`), a node written over HTTP, `grafeno doctor`,
`grafeno shell`, `grafeno mcp` answering `initialize`, a clean stop with no
process left, the Start menu entry serving, a database made outside the package, an update to a newer
package keeping the settings and the data, uninstalling (the alias and the
private folder go, the outside database stays), and reinstalling (fresh
settings, no data).

It is for a disposable machine: it trusts an ephemeral self-signed
certificate (subject = the Publisher) for the run, signs copies of the
packages with it, and removes it at the end. The MSIX workflow runs it on
GitHub's Windows runner.

On a computer of your own, prefer the workflow: an unsigned package does
not install, and Smart App Control may block the unsigned `grafeno.exe`
validate.ps1 runs.

## The workflow

`.github/workflows/msix.yml` runs by hand (Actions → MSIX → Run workflow),
on tags `v*`, and on changes to these files. It builds `grafeno.exe`, the
package, checks it, builds a newer one for the update test, runs
`test-install.ps1`, and uploads the unsigned package as the artifact
`grafeno-msix-<version>`. It publishes nothing and does not touch the
release workflow; on a tag, it fails if the tag and `Cargo.toml` differ.

## Signing

Three different things:

1. **The unsigned package** (the artifact): what Partner Center takes.
   It does not install by double-clicking: Windows installs only signed
   packages.
2. **Signed for installing outside the Store**: needs a code-signing
   certificate a Windows machine trusts, whose subject is the manifest's
   Publisher. None is in this repository; the workflow's test certificate
   lives only for its run and is trusted only on that runner.
3. **Signed by Microsoft**: the Store signs the package it distributes.
   That signature covers that package only: it does not sign the setup,
   the zip or the MSI files on GitHub Releases.

## Publishing in the Microsoft Store

Automatic submission is not set up: it needs a Partner Center API client
(Microsoft Entra application) with access to the account, stored as a
secret, and is left for when that exists.

1. Run the MSIX workflow on the release's tag (or by hand on its commit)
   and download the artifact `grafeno-msix-<version>`.
2. Partner Center (partner.microsoft.com) → Apps and games → **GrafenoDB**
   → **Start update** (or **Start submission** the first time).
3. **Packages**: upload `GrafenoDB_<version>_x64.msix`. Partner Center
   checks that its identity matches the product; device family: Windows
   Desktop.
4. **Properties**: category (Developer tools), privacy policy URL (the
   engine sends nothing anywhere the user did not configure), support
   contact, and the restricted capability **runFullTrust** with its reason:
   "a desktop database server and command line packaged with the desktop
   bridge".
5. **Age ratings**: the questionnaire.
6. **Store listings** (at least English): description, what's new,
   screenshots (at least one, 1366×768 or larger; a terminal running
   `grafeno serve` and `grafeno shell`). Store logos are optional; render
   them from `packaging/windows/grafeno-mark.svg` as `make-assets.ps1`
   does if wanted.
7. **Pricing and availability**: free; markets (all, or chosen);
   visibility (public, or private while testing); release (as soon as
   certified, or a date).
8. **Submit to the Store.** Certification takes from hours to days.
   Package flighting and gradual rollout of updates, where Partner Center
   offers them for the account, are on the update's Packages page.

When certification fails, Partner Center gives a report with the policy
and the step that failed. Usual causes for this package: the
`runFullTrust` reason missing, the listing lacking what the program does,
or the program failing when the tester opens it (the Start menu entry
must serve). Fix it, build a package with a higher version (a new
Cargo.toml version for real changes) and resubmit.

## Known limits

- One server per user from the package (the default settings); others need
  `--config` or `--db` and another port.
- No service: Grafeno runs while its console (or the program that starts
  it) runs, and does not start with Windows.
- Uninstalling removes the settings and the database in the package's
  folder. To keep a database, back it up first (`grafeno backup`), or put
  it outside the package (`grafeno config init --db D:\Data\grafeno.redb
  --force`; that folder is the user's and stays).
- Other programs see the files at the on-disk path above, not at
  `%LOCALAPPDATA%\Grafeno`.
- x64 only for now.
