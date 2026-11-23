# Renders grafeno-mark.svg (packaging/windows) into the images the MSIX
# package and its Store listing need, in Assets/. The images are kept in the
# repository; run this again only when the mark changes.
#
#   powershell -File packaging/windows/msix/make-assets.ps1
#
# Needs Microsoft Edge (part of Windows), run headless: it draws the SVG as
# a browser does, at each size.
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$mark = (Resolve-Path (Join-Path $here '..\grafeno-mark.svg')).Path
$assets = Join-Path $here 'Assets'
New-Item -ItemType Directory -Force $assets | Out-Null
$edge = @("${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe", "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $edge) { throw 'Microsoft Edge is not installed' }
$svg = [Convert]::ToBase64String([IO.File]::ReadAllBytes($mark))
$work = Join-Path ([IO.Path]::GetTempPath()) "grafeno-assets-$PID"
New-Item -ItemType Directory -Force $work | Out-Null

# name, width, height, size of the mark (the rest is the mark's dark color)
$images = @(
    @('Square44x44Logo.png', 44, 44, 44),
    @('Square44x44Logo.targetsize-16_altform-unplated.png', 16, 16, 16),
    @('Square44x44Logo.targetsize-24_altform-unplated.png', 24, 24, 24),
    @('Square44x44Logo.targetsize-32_altform-unplated.png', 32, 32, 32),
    @('Square44x44Logo.targetsize-48_altform-unplated.png', 48, 48, 48),
    @('Square44x44Logo.targetsize-256_altform-unplated.png', 256, 256, 256),
    @('Square150x150Logo.png', 150, 150, 150),
    @('Square310x310Logo.png', 310, 310, 310),
    @('Wide310x150Logo.png', 310, 150, 150),
    @('StoreLogo.png', 50, 50, 50)
)
foreach ($image in $images) {
    $name, $width, $height, $size = $image
    $html = Join-Path $work 'mark.html'
    @"
<!doctype html><html><head><style>
html,body{margin:0;width:${width}px;height:${height}px;overflow:hidden;background:#0a1418}
body{display:flex;align-items:center;justify-content:center}
img{width:${size}px;height:${size}px;display:block}
</style></head><body><img src="data:image/svg+xml;base64,$svg"></body></html>
"@ | Set-Content -Encoding utf8 $html
    $out = Join-Path $assets $name
    if (Test-Path $out) { Remove-Item $out }
    # Edge writes harmless diagnostics to stderr: run it as its own process.
    Start-Process -FilePath $edge -Wait -WindowStyle Hidden -ArgumentList @(
        '--headless', '--disable-gpu', '--hide-scrollbars', '--force-device-scale-factor=1',
        "--user-data-dir=`"$work\profile`"", "--window-size=$width,$height", "--screenshot=`"$out`"",
        "file:///$($html -replace '\\','/')")
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path $out) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (-not (Test-Path $out)) { throw "Edge did not write $name" }
    Write-Host "$name ${width}x$height"
}
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
