# Runs the Windows App Certification Kit (WACK, appcert.exe from the
# Windows SDK) on the MSIX package, as Partner Center's certification
# begins. For a disposable machine: WACK installs the package, so a copy is
# signed with an ephemeral certificate trusted for the run, then removed.
#
#   powershell -File packaging/windows/msix/wack.ps1 -Package <msix> [-Report wack-report.xml]
#
# Needs an administrator. Prints every test with its result and exits
# non-zero when the overall result is not PASS. WACK passing does not mean
# the Store will certify the package: its own review follows.
param(
    [Parameter(Mandatory)] [string]$Package,
    [string]$Report = 'wack-report.xml'
)
$ErrorActionPreference = 'Stop'
$appcert = "${env:ProgramFiles(x86)}\Windows Kits\10\App Certification Kit\appcert.exe"
if (-not (Test-Path $appcert)) { throw "WACK is not installed ($appcert): install the Windows SDK with the App Certification Kit" }
$publisher = 'CN=9C88685D-AD17-4F9E-8FAE-1760744D79C1'
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" | Sort-Object FullName | Select-Object -Last 1
$cert = New-SelfSignedCertificate -Type Custom -Subject $publisher -KeyUsage DigitalSignature `
    -KeyAlgorithm RSA -KeyLength 2048 -FriendlyName 'Grafeno WACK (ephemeral)' `
    -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddDays(1) `
    -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
$cer = Join-Path ([IO.Path]::GetTempPath()) 'grafeno-wack.cer'
Export-Certificate -Cert $cert -FilePath $cer | Out-Null
Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null
$copy = Join-Path ([IO.Path]::GetTempPath()) ("wack-" + (Split-Path -Leaf $Package))
try {
    Copy-Item $Package $copy -Force
    & $signtool.FullName sign /fd SHA256 /sha1 $cert.Thumbprint $copy | Out-Host
    if ($LASTEXITCODE -ne 0) { throw 'signtool failed' }
    Get-AppxPackage -Name 'GrafenoDB.GrafenoDB' | Remove-AppxPackage -ErrorAction SilentlyContinue
    $Report = [IO.Path]::GetFullPath($Report)
    if (Test-Path $Report) { Remove-Item $Report }
    & $appcert reset | Out-Host
    & $appcert test -appxpackagepath $copy -reportoutputpath $Report | Out-Host
    if (-not (Test-Path $Report)) { throw "WACK wrote no report (exit $LASTEXITCODE)" }
    [xml]$xml = Get-Content -Raw $Report
    $overall = $xml.REPORT.OVERALL_RESULT
    foreach ($test in $xml.SelectNodes('//TEST')) {
        $result = $test.RESULT.'#cdata-section'
        if (-not $result) { $result = $test.RESULT.InnerText }
        Write-Host ("{0,-10} {1}" -f $result, $test.NAME)
        if ($result -and $result -notmatch 'PASS') {
            foreach ($message in $test.SelectNodes('.//MESSAGE')) { Write-Host "           $($message.TEXT)" }
        }
    }
    Write-Host "WACK overall result: $overall (report: $Report)"
    if ($overall -ne 'PASS') { throw "WACK: $overall" }
} finally {
    Get-AppxPackage -Name 'GrafenoDB.GrafenoDB' | Remove-AppxPackage -ErrorAction SilentlyContinue
    Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object Thumbprint -eq $cert.Thumbprint | Remove-Item
    Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)" -ErrorAction SilentlyContinue
    Remove-Item $cer, $copy -ErrorAction SilentlyContinue
}
