# Builds the MSIX package of the Grafeno engine (README.md beside this file).
#
#   cargo build --release --locked -p grafeno-server
#   powershell -File packaging/windows/msix/build.ps1
#
# Writes dist\msix\GrafenoDB_<version>_x64.msix, unsigned: the package to
# upload to Partner Center, which signs it. An unsigned package does not
# install by itself; for a local test, sign a copy with a certificate whose
# subject is the manifest's Publisher (validate.ps1 -Install does).
#
#   -Version 0.5.1.0   the package version (default: Cargo.toml's, with .0)
#   -Binary <path>     grafeno.exe (default: target\release\grafeno.exe)
#   -Out <folder>      where the package goes (default: dist\msix)
#
# Needs makeappx.exe from the Windows SDK (on GitHub's Windows runners).
param(
    [string]$Version,
    [string]$Binary,
    [string]$Out
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = (Resolve-Path (Join-Path $here '..\..\..')).Path
if (-not $Binary) { $Binary = Join-Path $root 'target\release\grafeno.exe' }
if (-not $Out) { $Out = Join-Path $root 'dist\msix' }

if (-not $Version) {
    $line = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"' | Select-Object -First 1
    if (-not $line) { throw 'no version in Cargo.toml' }
    $Version = "$($line.Matches[0].Groups[1].Value).0"
}
if ($Version -notmatch '^\d+\.\d+\.\d+\.\d+$') { throw "an MSIX version has four numbers: $Version" }
if (-not (Test-Path $Binary)) { throw "no $Binary (cargo build --release --locked -p grafeno-server)" }

$makeappx = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" -ErrorAction SilentlyContinue |
    Sort-Object FullName | Select-Object -Last 1
if (-not $makeappx) { throw 'makeappx.exe not found: install the Windows SDK' }

# The package's files: the program, its images, the manifest, the licenses.
$stage = Join-Path $Out 'stage'
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force (Join-Path $stage 'Assets') | Out-Null
Copy-Item $Binary (Join-Path $stage 'grafeno.exe')
Copy-Item (Join-Path $here 'Assets\*.png') (Join-Path $stage 'Assets')
Copy-Item (Join-Path $root 'LICENSE-MIT') (Join-Path $stage 'LICENSE-MIT.txt')
Copy-Item (Join-Path $root 'LICENSE-APACHE') (Join-Path $stage 'LICENSE-APACHE.txt')
$manifest = (Get-Content -Raw (Join-Path $here 'AppxManifest.xml')).Replace('@VERSION@', $Version)
[IO.File]::WriteAllText((Join-Path $stage 'AppxManifest.xml'), $manifest, (New-Object Text.UTF8Encoding $false))

$package = Join-Path $Out "GrafenoDB_${Version}_x64.msix"
if (Test-Path $package) { Remove-Item $package }
Write-Host "makeappx: $($makeappx.FullName)"
# makeappx checks the manifest against its schema as it packs.
& $makeappx.FullName pack /d $stage /p $package /o
if ($LASTEXITCODE -ne 0) { throw "makeappx failed ($LASTEXITCODE)" }
Remove-Item -Recurse -Force $stage
$hash = (Get-FileHash -Algorithm SHA256 $package).Hash.ToLower()
"$hash  $(Split-Path -Leaf $package)" | Set-Content -Encoding ascii "$package.sha256"
Write-Host "Built $package ($Version), sha256 $hash"
