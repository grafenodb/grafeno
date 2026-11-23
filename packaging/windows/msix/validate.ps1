# Checks an MSIX package of the Grafeno engine before it goes to Partner
# Center: its identity is GrafenoDB's exactly, its version the release's,
# the files only those it should hold, no service and no capability beyond
# runFullTrust, and grafeno.exe the x64 program of that version.
#
#   powershell -File packaging/windows/msix/validate.ps1 [-Package <msix>] [-Version 0.5.1.0]
#
# Exits non-zero on the first failed check, after printing every result.
param(
    [string]$Package,
    [string]$Version
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = (Resolve-Path (Join-Path $here '..\..\..')).Path
if (-not $Version) {
    $line = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"' | Select-Object -First 1
    $Version = "$($line.Matches[0].Groups[1].Value).0"
}
if (-not $Package) { $Package = Join-Path $root "dist\msix\GrafenoDB_${Version}_x64.msix" }
if (-not (Test-Path $Package)) { throw "no package $Package" }

$expected = @{
    Name = 'GrafenoDB.GrafenoDB'
    Publisher = 'CN=9C88685D-AD17-4F9E-8FAE-1760744D79C1'
    PublisherDisplayName = 'GrafenoDB'
    DisplayName = 'GrafenoDB'
    Architecture = 'x64'
}
$failed = 0
function Check([string]$what, [bool]$ok, [string]$detail = '') {
    if ($ok) { Write-Host "ok    $what" } else { Write-Host "FAIL  $what $detail"; $script:failed++ }
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$work = Join-Path ([IO.Path]::GetTempPath()) "grafeno-msix-$PID"
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
[IO.Compression.ZipFile]::ExtractToDirectory((Resolve-Path $Package).Path, $work)
try {
    [xml]$manifest = Get-Content -Raw (Join-Path $work 'AppxManifest.xml')
    $ns = New-Object Xml.XmlNamespaceManager $manifest.NameTable
    $ns.AddNamespace('m', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10')
    $ns.AddNamespace('uap', 'http://schemas.microsoft.com/appx/manifest/uap/windows10')
    $ns.AddNamespace('uap5', 'http://schemas.microsoft.com/appx/manifest/uap/windows10/5')
    $identity = $manifest.SelectSingleNode('/m:Package/m:Identity', $ns)
    Check 'identity name' ($identity.Name -ceq $expected.Name) "($($identity.Name))"
    Check 'identity publisher' ($identity.Publisher -ceq $expected.Publisher) "($($identity.Publisher))"
    Check 'architecture x64' ($identity.ProcessorArchitecture -ceq $expected.Architecture) "($($identity.ProcessorArchitecture))"
    Check "version $Version" ($identity.Version -ceq $Version) "($($identity.Version))"
    Check 'last version number 0 (the Store keeps it)' ($identity.Version -match '\.0$') "($($identity.Version))"
    $properties = $manifest.SelectSingleNode('/m:Package/m:Properties', $ns)
    Check 'publisher display name' ($properties.PublisherDisplayName -ceq $expected.PublisherDisplayName)
    Check 'display name' ($properties.DisplayName -ceq $expected.DisplayName)

    # Capabilities: runFullTrust and nothing else; no service.
    $capabilities = @($manifest.SelectNodes('/m:Package/m:Capabilities/*', $ns) | ForEach-Object { $_.Name })
    Check 'capabilities: runFullTrust only' (($capabilities -join ',') -eq 'runFullTrust') "($($capabilities -join ', '))"
    $text = Get-Content -Raw (Join-Path $work 'AppxManifest.xml')
    Check 'no packaged service' (-not ($text -match 'windows\.service|packagedServices|localSystemServices'))

    # The command-line alias and the Start menu entry.
    $alias = $manifest.SelectSingleNode('//uap5:ExecutionAlias', $ns)
    Check 'execution alias grafeno.exe' ($alias -and $alias.Alias -ceq 'grafeno.exe')

    # Every image the manifest names is in the package.
    $images = [regex]::Matches($text, 'Assets\\[A-Za-z0-9._-]+\.png') | ForEach-Object { $_.Value } | Sort-Object -Unique
    foreach ($image in $images) {
        Check "image $image" (Test-Path (Join-Path $work $image))
    }

    # Only the files it should hold (no settings, keys or certificates).
    $allowed = '^(AppxManifest\.xml|AppxBlockMap\.xml|\[Content_Types\]\.xml|AppxSignature\.p7x|grafeno\.exe|LICENSE-MIT\.txt|LICENSE-APACHE\.txt|Assets\\[A-Za-z0-9._-]+\.png)$'
    $files = Get-ChildItem -Recurse -File $work | ForEach-Object { $_.FullName.Substring($work.Length + 1) }
    $unexpected = @($files | Where-Object { $_ -notmatch $allowed })
    Check 'only expected files' ($unexpected.Count -eq 0) "($($unexpected -join ', '))"
    $signed = Test-Path (Join-Path $work 'AppxSignature.p7x')
    Write-Host "note  package $(if ($signed) { 'is signed' } else { 'is unsigned (as Partner Center takes it)' })"

    # grafeno.exe: an x64 program of this version.
    $exe = Join-Path $work 'grafeno.exe'
    $bytes = [IO.File]::ReadAllBytes($exe)
    $pe = [BitConverter]::ToInt32($bytes, 0x3C)
    $machine = [BitConverter]::ToUInt16($bytes, $pe + 4)
    Check 'grafeno.exe is x64' ($machine -eq 0x8664) ('(machine 0x{0:x})' -f $machine)
    $printed = (& $exe --version) -join ' '
    $semver = ($Version -split '\.')[0..2] -join '.'
    Check "grafeno --version says $semver" ($printed -eq "grafeno $semver") "($printed)"
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
if ($failed -gt 0) { throw "$failed check(s) failed" }
Write-Host "All checks passed for $Package"
