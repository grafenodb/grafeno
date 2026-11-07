"""Writes the winget manifests of a release (GrafenoDB.Grafeno), for a pull
request to microsoft/winget-pkgs:

    python packaging/windows/winget/make-manifests.py VERSION SETUP.exe URL OUT

VERSION is the release's (0.1.0), SETUP.exe the setup built for it (its
SHA-256 goes in the manifest), URL where the release serves it, OUT the
folder to write manifests/g/GrafenoDB/Grafeno/VERSION/ into. Then:

    winget validate --manifest OUT/manifests/g/GrafenoDB/Grafeno/VERSION
    wingetcreate submit OUT/manifests/g/GrafenoDB/Grafeno/VERSION   (or a PR by hand)
"""

import hashlib
import pathlib
import sys

SCHEMA = "1.6.0"
ID = "GrafenoDB.Grafeno"


def main() -> None:
    if len(sys.argv) != 5:
        sys.exit(__doc__)
    version, setup, url, out = sys.argv[1:]
    sha = hashlib.sha256(pathlib.Path(setup).read_bytes()).hexdigest().upper()
    folder = pathlib.Path(out) / "manifests" / "g" / "GrafenoDB" / "Grafeno" / version
    folder.mkdir(parents=True, exist_ok=True)
    header = "# Created by packaging/windows/winget/make-manifests.py\n"
    files = {
        f"{ID}.yaml": f"""{header}# yaml-language-server: $schema=https://aka.ms/winget-manifest.version.{SCHEMA}.schema.json
PackageIdentifier: {ID}
PackageVersion: {version}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: {SCHEMA}
""",
        f"{ID}.installer.yaml": f"""{header}# yaml-language-server: $schema=https://aka.ms/winget-manifest.installer.{SCHEMA}.schema.json
PackageIdentifier: {ID}
PackageVersion: {version}
InstallerType: inno
Scope: machine
ElevationRequirement: elevatesSelf
UpgradeBehavior: install
InstallModes:
  - interactive
  - silent
  - silentWithProgress
InstallerSwitches:
  Custom: /TASKS="service,path"
Commands:
  - grafeno
Installers:
  - Architecture: x64
    InstallerUrl: {url}
    InstallerSha256: {sha}
ManifestType: installer
ManifestVersion: {SCHEMA}
""",
        f"{ID}.locale.en-US.yaml": f"""{header}# yaml-language-server: $schema=https://aka.ms/winget-manifest.defaultLocale.{SCHEMA}.schema.json
PackageIdentifier: {ID}
PackageVersion: {version}
PackageLocale: en-US
Publisher: GrafenoDB
PublisherUrl: https://github.com/grafenodb
PackageName: Grafeno
PackageUrl: https://github.com/grafenodb/grafeno
License: Unspecified
ShortDescription: A graph engine for agent memory and knowledge, run as a Windows service.
Description: |-
  Grafeno is a graph database engine for agent memory and knowledge: a property graph with
  namespaces, facts with valid and record time, full-text and vector search, the GQ query
  language and an MCP server. It installs as the Grafeno service with its HTTP API on
  127.0.0.1:7810; Grafeno Studio is installed apart.
Moniker: grafeno
Tags:
  - database
  - graph
  - graph-database
  - knowledge-graph
ReleaseNotesUrl: https://github.com/grafenodb/grafeno/releases/tag/v{version}
ManifestType: defaultLocale
ManifestVersion: {SCHEMA}
""",
    }
    for name, text in files.items():
        (folder / name).write_text(text, encoding="utf-8", newline="\n")
    print(folder)


if __name__ == "__main__":
    main()
