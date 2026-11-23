# Installs the MSIX package on this machine and runs Grafeno from it through
# its whole life, then beside the setup's installation:
#
#   1. install; the command line; setup and service refused
#   2. the first package's files (in the package's private folder) copied
#      out to %USERPROFILE%\GrafenoDB on the first start, once
#   3. serving: the start message, a second server refused with the cause,
#      HTTP, doctor, shell, MCP, the Start menu entry
#   4. backup (server running and stopped) and restore
#   5. update to a newer package: settings and data kept
#   6. uninstall: the package goes, %USERPROFILE%\GrafenoDB stays;
#      reinstall finds it again
#   7. with the setup installed (its service on 7810): serving is refused
#      with the cause and writes nothing; `grafeno migrate` plans without
#      changing anything, then copies the service's database into new
#      settings on the next free port; the service is left as it was
#
# For a disposable Windows machine (the msix workflow's runner): it trusts
# an ephemeral certificate for the run, installs and removes the package
# and the setup, and writes C:\GrafenoMsixTest and %USERPROFILE%\GrafenoDB.
#
#   powershell -File packaging/windows/msix/test-install.ps1 -Package <msix> -Update <newer msix> -Setup <setup exe> -Engine <grafeno.exe>
#
# Needs an administrator and signtool.exe from the Windows SDK. -Engine is
# an unpackaged grafeno.exe of the same build (to write the first
# package's files as it did).
param(
    [Parameter(Mandatory)] [string]$Package,
    [Parameter(Mandatory)] [string]$Update,
    [Parameter(Mandatory)] [string]$Setup,
    [Parameter(Mandatory)] [string]$Engine
)
$ErrorActionPreference = 'Stop'
$publisher = 'CN=9C88685D-AD17-4F9E-8FAE-1760744D79C1'
$family = 'GrafenoDB.GrafenoDB_rwmas8f7748ap'
$alias = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\grafeno.exe'
$private = Join-Path $env:LOCALAPPDATA "Packages\$family\LocalCache\Local\Grafeno"
$home_ = Join-Path $env:USERPROFILE 'GrafenoDB'
$outside = 'C:\GrafenoMsixTest'
$url = 'http://127.0.0.1:7810'
$failed = 0
function Check([string]$what, [bool]$ok, [string]$detail = '') {
    if ($ok) { Write-Host "ok    $what" } else { Write-Host "FAIL  $what $detail"; $script:failed++ }
}
function Quote([string[]]$arguments) {
    ($arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
}
# Runs a program, returns its output and exit code.
function Run([string]$program, [string[]]$arguments) {
    $info = New-Object Diagnostics.ProcessStartInfo $program
    $info.Arguments = Quote $arguments
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($info)
    $out = $process.StandardOutput.ReadToEndAsync()
    $err = $process.StandardError.ReadToEndAsync()
    if (-not $process.WaitForExit(120000)) { $process.Kill(); throw "$program $($info.Arguments) did not finish" }
    [pscustomobject]@{ Code = $process.ExitCode; Out = $out.Result; Err = $err.Result }
}
function Grafeno([string[]]$arguments) { Run $alias $arguments }
function Health([string]$at) {
    try { Invoke-RestMethod "$at/health" | Out-Null; $true } catch { $false }
}
# `grafeno serve` from the package, stopped when its standard input closes
# (as on Ctrl+C); what it writes to stderr is collected.
function Start-Serve([string]$at = $url, [string[]]$extra = @()) {
    $info = New-Object Diagnostics.ProcessStartInfo $alias
    $info.Arguments = Quote (@('serve', '--stop-with-stdin') + $extra)
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($info)
    $script:serveErr = $process.StandardError.ReadToEndAsync()
    foreach ($i in 1..150) {
        if (Health $at) { return $process }
        if ($process.HasExited) { break }
        Start-Sleep -Milliseconds 200
    }
    throw "grafeno serve did not answer on $at$(if ($process.HasExited) { ': ' + $script:serveErr.Result })"
}
function Stop-Serve($process) {
    $process.StandardInput.Close()
    $stopped = $process.WaitForExit(30000)
    Check 'grafeno serve stops when asked' $stopped
    if ($stopped) { Check 'grafeno serve exits cleanly' ($process.ExitCode -eq 0) "(exit $($process.ExitCode))" }
}
function Add-Note([string]$at, [string]$token, [string]$text) {
    Invoke-RestMethod "$at/v1/msix/nodes" -Method Post -Headers @{ Authorization = "Bearer $token" } `
        -ContentType 'application/json' -Body (@{ labels = @('Note'); text = $text } | ConvertTo-Json) | Out-Null
}
function Count-Notes([string]$at, [string]$token) {
    $result = Invoke-RestMethod "$at/v1/msix/query" -Method Post -Headers @{ Authorization = "Bearer $token" } `
        -ContentType 'application/json' -Body '{"query":"MATCH (n:Note) RETURN count(n) AS n"}'
    [int]$result.rows[0][0]
}
function Token { (Grafeno @('config', 'token')).Out.Trim() }

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
New-Item -ItemType Directory -Force $outside | Out-Null

try {
    Write-Host '== 1. Install and the command line'
    Add-AppxPackage -Path $signed[$Package]
    $installed = Get-AppxPackage -Name 'GrafenoDB.GrafenoDB'
    Check 'installed' ($null -ne $installed)
    Check "package family $family" ($installed.PackageFamilyName -ceq $family) "($($installed.PackageFamilyName))"
    Check 'grafeno alias for PowerShell and CMD' (Test-Path $alias)
    $first = $installed.Version
    $semver = ($first -split '\.')[0..2] -join '.'
    $r = Grafeno @('--version')
    Check "grafeno --version is $semver" ($r.Code -eq 0 -and $r.Out.Trim() -eq "grafeno $semver") "($($r.Out.Trim()) $($r.Err.Trim()))"
    $r = Grafeno @('--help')
    Check 'grafeno --help' ($r.Code -eq 0 -and $r.Out -match 'serve' -and $r.Out -match 'migrate')
    $r = Grafeno @('config', 'path')
    Check 'settings in %USERPROFILE%\GrafenoDB' ($r.Out.Trim() -eq (Join-Path $home_ 'grafeno.toml')) "($($r.Out.Trim()))"
    $r = Grafeno @('setup', '--unattended')
    Check 'grafeno setup is refused (no service from the Store)' ($r.Code -ne 0 -and $r.Err -match 'not as a') "($($r.Err.Trim()))"
    $r = Grafeno @('service', 'install')
    Check 'grafeno service install is refused' ($r.Code -ne 0 -and $r.Err -match 'not as a')

    Write-Host '== 2. The first package''s files are copied out, once'
    # The first package (0.5.1, before this change) kept them in
    # %LOCALAPPDATA%\Grafeno, which Windows puts in the package's folder.
    $legacyToml = Join-Path $private 'grafeno.toml'
    $r = Run $Engine @('config', 'init', '--path', $legacyToml, '--db', (Join-Path $private 'data\grafeno.redb'), '--addr', '127.0.0.1:7899')
    Check 'the first package''s settings, as it wrote them' ($r.Code -eq 0) "($($r.Err.Trim()))"
    $legacyToken = (Run $Engine @('config', 'token', '--path', $legacyToml)).Out.Trim()
    $old = Start-Process $Engine -ArgumentList @('serve', '--config', "`"$legacyToml`"") -PassThru -WindowStyle Hidden
    foreach ($i in 1..100) { if (Health 'http://127.0.0.1:7899') { break }; Start-Sleep -Milliseconds 200 }
    Add-Note 'http://127.0.0.1:7899' $legacyToken 'kept by the first package'
    Stop-Process -Id $old.Id; $old.WaitForExit()
    Check 'nothing in %USERPROFILE%\GrafenoDB yet' (-not (Test-Path $home_))
    $serve = Start-Serve 'http://127.0.0.1:7899'
    Stop-Serve $serve
    Check 'the start says the files were copied' ($script:serveErr.Result -match 'Copied your settings and database') "($($script:serveErr.Result))"
    Check 'the copied settings keep the token' ((Token) -eq $legacyToken)
    Check 'the copied database is in %USERPROFILE%\GrafenoDB\data (not redirected)' (Test-Path (Join-Path $home_ 'data\grafeno.redb'))
    Check 'the old folder is left as it was, with a note' ((Test-Path (Join-Path $private 'data\grafeno.redb')) -and (Test-Path (Join-Path $private 'MOVED-TO-GRAFENODB-FOLDER.txt')))
    $serve = Start-Serve 'http://127.0.0.1:7899'
    Check 'the copy holds the first package''s data' ((Count-Notes 'http://127.0.0.1:7899' $legacyToken) -eq 1)
    Stop-Serve $serve
    Check 'copied once (no message the second time)' (-not ($script:serveErr.Result -match 'Copied your settings'))
    # The rest uses fresh default settings.
    Remove-Item -Recurse -Force $home_

    Write-Host '== 3. Serving'
    $serve = Start-Serve
    $token = Token
    Check 'a token was made' ($token.Length -eq 32)
    Add-Note $url $token 'written from the package'
    Check 'a node was written over HTTP' ((Count-Notes $url $token) -eq 1)
    Check 'the database is in %USERPROFILE%\GrafenoDB\data' (Test-Path (Join-Path $home_ 'data\grafeno.redb'))
    Check 'nothing written to C:\ProgramData\Grafeno' (-not (Test-Path 'C:\ProgramData\Grafeno'))
    $acl = (Get-Acl (Join-Path $home_ 'grafeno.toml')).Access | Where-Object { $_.IdentityReference -match 'Everyone|Users$|Authenticated' }
    Check 'the settings (with the token) are not open to other users' ($null -eq $acl)
    $second = Grafeno @('serve')
    Check 'a second server is refused, naming the cause' ($second.Code -ne 0 -and $second.Err -match 'port 7810 is in use' -and $second.Err -match 'another Grafeno' -and $second.Err -match 'not affected') "($($second.Err.Trim()))"
    $r = Grafeno @('doctor')
    Check 'grafeno doctor passes' ($r.Code -eq 0 -and $r.Out -match 'Microsoft Store package') "($($r.Out.Trim()) $($r.Err.Trim()))"
    $r = Grafeno @('shell', '-n', 'msix', '--json', '-c', 'MATCH (n:Note) RETURN count(n) AS n')
    Check 'grafeno shell reaches the server with the user''s settings' ($r.Code -eq 0 -and $r.Out -match '1') "($($r.Out.Trim()) $($r.Err.Trim()))"
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

    Write-Host '== 4. Backup and restore'
    $r = Grafeno @('backup', "$outside\running.redb")
    Check 'a backup from the running server' ($r.Code -eq 0 -and (Test-Path "$outside\running.redb")) "($($r.Err.Trim()))"
    Stop-Serve $serve
    Check 'the start message says what it serves and how to stop it' ($script:serveErr.Result -match 'Serving on http://127.0.0.1:7810' -and $script:serveErr.Result -match 'stay when GrafenoDB is uninstalled' -and $script:serveErr.Result -match 'Ctrl\+C') "($($script:serveErr.Result))"
    Check 'the start message shows no token' (-not $script:serveErr.Result.Contains($token))
    Check 'no grafeno process left' (-not (Get-Process grafeno -ErrorAction SilentlyContinue))
    $r = Grafeno @('backup', "$outside\stopped.redb")
    Check 'a backup with the server stopped (from the file)' ($r.Code -eq 0 -and (Test-Path "$outside\stopped.redb")) "($($r.Err.Trim()))"
    # Restore: the database aside, the backup restored in its place.
    $db = Join-Path $home_ 'data\grafeno.redb'
    Move-Item $db "$db.before-restore"
    $r = Grafeno @('restore', "$outside\running.redb", $db)
    Check 'grafeno restore checks and writes the backup' ($r.Code -eq 0 -and (Test-Path $db)) "($($r.Err.Trim()))"
    $r = Grafeno @('restore', "$outside\running.redb", $db)
    Check 'grafeno restore never replaces a file' ($r.Code -ne 0)
    $serve = Start-Serve
    Check 'the restored database serves its data' ((Count-Notes $url $token) -eq 1)

    # The Start menu entry, while nothing serves.
    Stop-Serve $serve
    Start-Process explorer.exe "shell:AppsFolder\$family!GrafenoDB"
    $up = $false
    foreach ($i in 1..100) { if (Health $url) { $up = $true; break }; Start-Sleep -Milliseconds 300 }
    Check 'the Start menu entry serves' $up
    Get-Process grafeno -ErrorAction SilentlyContinue | Stop-Process -Force
    foreach ($i in 1..50) { if (-not (Get-Process grafeno -ErrorAction SilentlyContinue)) { break }; Start-Sleep -Milliseconds 200 }

    Write-Host '== 5. Update'
    Add-AppxPackage -Path $signed[$Update]
    $updated = (Get-AppxPackage -Name 'GrafenoDB.GrafenoDB').Version
    Check "updated $first -> $updated" ($updated -ne $first)
    $serve = Start-Serve
    Check 'the update kept the settings (same token)' ((Token) -eq $token)
    Check 'the update kept the data' ((Count-Notes $url $token) -eq 1)
    Stop-Serve $serve

    Write-Host '== 6. Uninstall and reinstall'
    Remove-AppxPackage -Package (Get-AppxPackage -Name 'GrafenoDB.GrafenoDB').PackageFullName
    Check 'uninstalled' ($null -eq (Get-AppxPackage -Name 'GrafenoDB.GrafenoDB'))
    Check 'the alias is gone' (-not (Test-Path $alias))
    Check 'the package''s private folder is gone' (-not (Test-Path $private))
    Check '%USERPROFILE%\GrafenoDB (settings and database) stays' ((Test-Path (Join-Path $home_ 'grafeno.toml')) -and (Test-Path $db))
    Add-AppxPackage -Path $signed[$Package]
    $serve = Start-Serve
    Check 'a reinstall finds the same settings' ((Token) -eq $token)
    Check 'a reinstall finds the data' ((Count-Notes $url $token) -eq 1)
    Stop-Serve $serve
    # The setup scenario starts from a user with nothing yet.
    Move-Item $home_ "$outside\GrafenoDB-before-setup"

    Write-Host '== 7. Beside the setup, and grafeno migrate'
    Start-Process $Setup -Wait -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/PORT=7810', '/ACCOUNT=virtual', '/TASKS=service'
    $setupExe = 'C:\Program Files\Grafeno\grafeno.exe'
    foreach ($i in 1..100) { if (Health $url) { break }; Start-Sleep -Milliseconds 200 }
    Check 'the setup''s service serves on 7810' ((Get-Service Grafeno).Status -eq 'Running' -and (Health $url))
    $setupToken = (Run $setupExe @('config', 'token')).Out.Trim()
    Add-Note $url $setupToken 'kept by the service'
    $r = Grafeno @('serve')
    Check 'serving from the package is refused, naming the setup''s service' ($r.Code -ne 0 -and $r.Err -match 'service installed with the setup' -and $r.Err -match 'not affected') "($($r.Err.Trim()))"
    Check 'and wrote no settings' (-not (Test-Path $home_))
    $r = Grafeno @('migrate')
    Check 'grafeno migrate shows the plan' ($r.Code -eq 0 -and $r.Out -match 'nothing is changed' -and $r.Out -match 'running' -and $r.Out -match '7811') "($($r.Out.Trim()) $($r.Err.Trim()))"
    Check 'the plan changed nothing' (-not (Test-Path $home_))
    Check 'the plan shows no token' (-not ($r.Out + $r.Err).Contains($setupToken))
    $r = Grafeno @('migrate', '--apply')
    Check 'grafeno migrate --apply copies the service''s database' ($r.Code -eq 0 -and (Test-Path (Join-Path $home_ 'data\grafeno.redb'))) "($($r.Out.Trim()) $($r.Err.Trim()))"
    Check 'grafeno migrate --apply shows no token' (-not ($r.Out + $r.Err).Contains($setupToken))
    $serve = Start-Serve 'http://127.0.0.1:7811'
    $newToken = Token
    Check 'the copy serves on the next free port with a new token' ($newToken.Length -eq 32 -and $newToken -ne $setupToken)
    Check 'the copy holds the service''s data' ((Count-Notes 'http://127.0.0.1:7811' $newToken) -eq 1)
    Stop-Serve $serve
    $r = Grafeno @('migrate', '--apply')
    Check 'a second migrate --apply replaces nothing' ($r.Code -ne 0 -and $r.Err -match 'exists already') "($($r.Err.Trim()))"
    Check 'the setup''s service was left running' ((Get-Service Grafeno).Status -eq 'Running')
    Check 'the setup''s data is as it was' ((Count-Notes $url $setupToken) -eq 1)
} finally {
    Get-Process grafeno -ErrorAction SilentlyContinue | Where-Object { $_.Path -notlike 'C:\Program Files\Grafeno\*' } | Stop-Process -Force
    Get-AppxPackage -Name 'GrafenoDB.GrafenoDB' | Remove-AppxPackage -ErrorAction SilentlyContinue
    if (Test-Path 'C:\Program Files\Grafeno\unins000.exe') {
        Start-Process 'C:\Program Files\Grafeno\unins000.exe' -Wait -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/PURGE=1'
    }
    Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object Thumbprint -eq $cert.Thumbprint | Remove-Item
    Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)" -ErrorAction SilentlyContinue
    Remove-Item $cer, $signed.Values -ErrorAction SilentlyContinue
}
if ($failed -gt 0) { throw "$failed check(s) failed" }
Write-Host 'All install checks passed.'
