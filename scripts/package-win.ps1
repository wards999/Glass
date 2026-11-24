# Builds dist/Glass-win-<version>.zip: Glass.exe (the app), glass-cli.exe (command
# line), FFmpeg DLLs, the virtual display driver (vdd), test scripts.
# Run on Windows with the MSVC toolchain (inside a VS developer environment
# locally) after scripts/fetch-ffmpeg.ps1 and scripts/fetch-vdd.ps1.
param([string]$Version = (git describe --tags --always --dirty))
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$env:GLASS_VERSION = $Version
cargo build -p glass-win --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = Join-Path ([IO.Path]::GetTempPath()) "glass-win-$([guid]::NewGuid())"
$dir = Join-Path $stage "Glass-win-$Version"
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item target/release/Glass.exe, target/release/glass-cli.exe $dir
# Only what the programs load (checked with dumpbin /dependents): avcodec -> avutil, swresample.
foreach ($dll in 'avcodec-62.dll', 'avutil-60.dll', 'swresample-6.dll') {
    Copy-Item (Join-Path third_party/ffmpeg/bin $dll) $dir
}
Copy-Item third_party/ffmpeg/LICENSE.txt (Join-Path $dir 'FFmpeg-LICENSE.txt')
# The virtual display driver (scripts/fetch-vdd.ps1), installed from Settings.
$vdd = Join-Path $dir 'vdd'
New-Item -ItemType Directory -Force $vdd | Out-Null
foreach ($f in 'MttVDD.inf', 'MttVDD.dll', 'mttvdd.cat', 'LICENSE.txt') {
    Copy-Item (Join-Path third_party/vdd $f) $vdd
}
Copy-Item app-win/packaging/record-clips.cmd, app-win/packaging/host.cmd, app-win/packaging/connect.cmd $dir
(Get-Content app-win/packaging/README.txt -Raw).Replace('__VERSION__', $Version) |
    Set-Content -Encoding utf8 (Join-Path $dir 'README.txt')

New-Item -ItemType Directory -Force dist | Out-Null
$zip = "dist/Glass-win-$Version.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $dir -DestinationPath $zip
Remove-Item -Recurse -Force $stage
Write-Host $zip
