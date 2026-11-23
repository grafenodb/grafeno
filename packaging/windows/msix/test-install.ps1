# Installs the MSIX package on this machine and runs Grafeno from it through
# its whole life: install, serve, CLI, MCP, update (the data stays),
# uninstall (what stays and what goes), reinstall. For a disposable Windows
# machine (the msix workflow's runner): it trusts an ephemeral certificate
# for the duration, installs and removes the package, and writes
# C:\GrafenoMsixTest.
#
#   powershell -File packaging/windows/msix/test-install.ps1 -Package <msix> -Update <newer msix>
#
# Needs an administrator (to trust the test certificate) and signtool.exe
# from the Windows SDK. The certificate is made here, never saved to disk
# beyond this run, and removed at the end.
param(
    [Parameter(Mandatory)] [string]$Package,
    [Parameter(Mandatory)] [string]$Update
)
$ErrorActionPreference = 'Stop'
$publisher = 'CN=9C88685D-AD17-4F9E-8FAE-1760744D79C1'
$family = 'GrafenoDB.GrafenoDB_rwmas8f7748ap'
$alias = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\grafeno.exe'
$private = Join-Path $env:LOCALAPPDATA "Packages\$family\LocalCache\Local\Grafeno"
$outside = 'C:\GrafenoMsixTest'
$url = 'http://127.0.0.1:7810'
$failed = 0
function Check([string]$what, [bool]$ok, [string]$detail = '') {
    if ($ok) { Write-Host "ok    $what" } else { Write-Host "FAIL  $what $detail"; $script:failed++ }
}
# Runs the installed `grafeno` (through its alias) and returns its output
# and exit code.
function Grafeno([string[]]$arguments) {
    $info = New-Object Diagnostics.ProcessStartInfo $alias
    $info.Arguments = ($arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($info)
    $out = $process.StandardOutput.ReadToEndAsync()
    $err = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit(60000)) { $process.Kill(); throw "grafeno $($info.Arguments) did not finish" }
    [pscustomobject]@{ Code = $process.ExitCode; Out = $out.Result; Err = $err.Result }
}
# `grafeno serve` from the package with its own settings, stopped when its
# standard input closes (as on Ctrl+C).
function Start-Serve {
    $info = New-Object Diagnostics.ProcessStartInfo $alias
    $info.Arguments = 'serve --stop-with-stdin'
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $process = [Diagnostics.Process]::Start($info)
    foreach ($i in 1..100) {
        try { Invoke-RestMethod "$url/health" | Out-Null; return $process } catch { Start-Sleep -Milliseconds 200 }
    }
    throw 'grafeno serve did not answer on 7810'
}
function Stop-Serve($process) {
    $process.StandardInput.Close()
    $stopped = $process.WaitForExit(30000)
    Check 'grafeno serve stops when asked' $stopped
    if ($stopped) { Check 'grafeno serve exits cleanly' ($process.ExitCode -eq 0) "(exit $($process.ExitCode))" }
}
function Count-Notes([string]$token) {
    $result = Invoke-RestMethod "$url/v1/msix/query" -Method Post -Headers @{ Authorization = "Bearer $token" } `
        -ContentType 'application/json' -Body '{"query":"MATCH (n:Note) RETURN count(n) AS n"}'
    [int]$result.rows[0][0]
}

# Sign copies of both packages with an ephemeral certificate whose subject
# is the manifest's Publisher, and trust it on this machine for the run.
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
$cert = New-SelfSignedCertificate -Type Custom -Subject $publisher -KeyUsage DigitalSignature `
    -KeyAlgorithm RSA -KeyLength 2048 -FriendlyName 'Grafeno MSIX test (ephemeral)' `
    -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddDays(1) `
    -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
$cer = Join-Path ([IO.Path]::GetTempPath()) 'grafeno-msix-test.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null
$signed = @{}
foreach ($source in @($Package, $Update)) {
    $copy = Join-Path ([IO.Path]::GetTempPath()) ("signed-" + (Split-Path -Leaf $source))
    Copy-Item $source $copy -Force
    & $signtool.FullName sign /fd SHA256 /sha1 $cert.Thumbprint $copy | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "signtool failed on $copy" }
    $signed[$source] = $copy
}

try {
    # Install.
    Add-AppxPackage -Path $signed[$Package]
    $installed = Get-AppxPackage -Name 'GrafenoDB.GrafenoDB'
    Check 'installed' ($null -ne $installed)
    Check "package family $family" ($installed.PackageFamilyName -ceq $family) "($($installed.PackageFamilyName))"
    Check 'grafeno alias for PowerShell and CMD' (Test-Path $alias)
    $first = $installed.Version

    # The command line.
    $r = Grafeno @('--version')
    $semver = ($first -split '\.')[0..2] -join '.'
    Check "grafeno --version is $semver" ($r.Code -eq 0 -and $r.Out.Trim() -eq "grafeno $semver") "($($r.Out.Trim()) $($r.Err.Trim()))"
    $r = Grafeno @('--help')
    Check 'grafeno --help' ($r.Code -eq 0 -and $r.Out -match 'serve')
    $r = Grafeno @('config', 'path')
    Check 'settings in the user''s folder' ($r.Out.Trim() -eq (Join-Path $env:LOCALAPPDATA 'Grafeno\grafeno.toml')) "($($r.Out.Trim()))"
    $r = Grafeno @('setup', '--unattended')
    Check 'grafeno setup is refused (no service from the Store)' ($r.Code -ne 0 -and $r.Err -match 'not as a') "($($r.Err.Trim()))"
    $r = Grafeno @('service', 'install')
    Check 'grafeno service install is refused' ($r.Code -ne 0 -and $r.Err -match 'not as a')

    # Serve: the first run creates the settings, with a token, and the data.
    $serve = Start-Serve
    $token = (Grafeno @('config', 'token')).Out.Trim()
    Check 'a token was made' ($token.Length -eq 32)
    Invoke-RestMethod "$url/v1/msix/nodes" -Method Post -Headers @{ Authorization = "Bearer $token" } `
        -ContentType 'application/json' -Body '{"labels":["Note"],"text":"written from the package"}' | Out-Null
    Check 'a node was written over HTTP' ((Count-Notes $token) -eq 1)
    Check 'the database is in the package''s private folder' (Test-Path (Join-Path $private 'data\grafeno.redb')) "($private)"
    Check 'nothing written to the real %LOCALAPPDATA%\Grafeno' (-not (Test-Path (Join-Path $env:LOCALAPPDATA 'Grafeno')))
    Check 'nothing written to C:\ProgramData\Grafeno' (-not (Test-Path 'C:\ProgramData\Grafeno'))
    $r = Grafeno @('doctor')
    Check 'grafeno doctor passes' ($r.Code -eq 0 -and $r.Out -match 'Microsoft Store package') "($($r.Out.Trim()) $($r.Err.Trim()))"
    $r = Grafeno @('shell', '-n', 'msix', '--json', '-c', 'MATCH (n:Note) RETURN count(n) AS n')
    Check 'grafeno shell reaches the server with the user''s settings' ($r.Code -eq 0 -and $r.Out -match '1') "($($r.Out.Trim()) $($r.Err.Trim()))"

    # MCP, through the running server.
    $info = New-Object Diagnostics.ProcessStartInfo $alias
    $info.Arguments = "mcp --url $url --token $token"
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $mcp = [Diagnostics.Process]::Start($info)
    $mcp.StandardInput.WriteLine('{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"msix-test","version":"1"}}}')
    $mcp.StandardInput.Flush()
    $line = $mcp.StandardOutput.ReadLineAsync()
    $answered = $line.Wait(30000)
    $mcp.StandardInput.Close()
    if (-not $mcp.WaitForExit(15000)) { $mcp.Kill() }
    Check 'grafeno mcp answers initialize' ($answered -and $line.Result -match '"serverInfo"') "($(if ($answered) { $line.Result }))"

    Stop-Serve $serve
    Check 'no grafeno process left' (-not (Get-Process grafeno -ErrorAction SilentlyContinue))

    # A database put outside the package's folders, on purpose.
    New-Item -ItemType Directory -Force $outside | Out-Null
    $r = Grafeno @('shell', '--db', "$outside\kept.redb", '-c', "CREATE (:Kept {name: 'outside'})")
    Check 'a database outside the package can be made' ($r.Code -eq 0 -and (Test-Path "$outside\kept.redb")) "($($r.Err.Trim()))"

    # Update: the newer package over the installed one keeps the data.
    Add-AppxPackage -Path $signed[$Update]
    $updated = (Get-AppxPackage -Name 'GrafenoDB.GrafenoDB').Version
    Check "updated $first -> $updated" ($updated -ne $first)
    $serve = Start-Serve
    $again = (Grafeno @('config', 'token')).Out.Trim()
    Check 'the update kept the settings (same token)' ($again -eq $token)
    Check 'the update kept the data' ((Count-Notes $token) -eq 1)
    Stop-Serve $serve

    # Uninstall: the package and its private folder go; outside stays.
    Remove-AppxPackage -Package (Get-AppxPackage -Name 'GrafenoDB.GrafenoDB').PackageFullName
    Check 'uninstalled' ($null -eq (Get-AppxPackage -Name 'GrafenoDB.GrafenoDB'))
    Check 'the alias is gone' (-not (Test-Path $alias))
    Check 'the private folder (settings and data) is removed with the package' (-not (Test-Path $private))
    Check 'the database outside the package stays' (Test-Path "$outside\kept.redb")

    # Reinstall: a fresh start.
    Add-AppxPackage -Path $signed[$Package]
    $serve = Start-Serve
    $fresh = (Grafeno @('config', 'token')).Out.Trim()
    Check 'a reinstall makes new settings' ($fresh.Length -eq 32 -and $fresh -ne $token)
    Check 'a reinstall starts with no data' ((Count-Notes $fresh) -eq 0)
    Stop-Serve $serve
} finally {
    Get-Process grafeno -ErrorAction SilentlyContinue | Stop-Process -Force
    Get-AppxPackage -Name 'GrafenoDB.GrafenoDB' | Remove-AppxPackage -ErrorAction SilentlyContinue
    Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object Thumbprint -eq $cert.Thumbprint | Remove-Item
    Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)" -ErrorAction SilentlyContinue
    Remove-Item $cer, $signed.Values -ErrorAction SilentlyContinue
}
if ($failed -gt 0) { throw "$failed check(s) failed" }
Write-Host 'All install checks passed.'
