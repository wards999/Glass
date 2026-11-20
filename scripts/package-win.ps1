# Builds dist/Glass-win-<version>.zip: glass.exe + FFmpeg DLLs + test scripts.
# Run on Windows with the MSVC toolchain (inside a VS developer environment
# locally) after scripts/fetch-ffmpeg.ps1.
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
Copy-Item target/release/glass.exe $dir
# Only what glass.exe loads (checked with dumpbin /dependents): avcodec -> avutil, swresample.
foreach ($dll in 'avcodec-62.dll', 'avutil-60.dll', 'swresample-6.dll') {
    Copy-Item (Join-Path third_party/ffmpeg/bin $dll) $dir
}
Copy-Item third_party/ffmpeg/LICENSE.txt (Join-Path $dir 'FFmpeg-LICENSE.txt')
Copy-Item app-win/packaging/record-clips.cmd, app-win/packaging/host.cmd $dir
(Get-Content app-win/packaging/README.txt -Raw).Replace('__VERSION__', $Version) |
    Set-Content -Encoding utf8 (Join-Path $dir 'README.txt')

New-Item -ItemType Directory -Force dist | Out-Null
$zip = "dist/Glass-win-$Version.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $dir -DestinationPath $zip
Remove-Item -Recurse -Force $stage
Write-Host $zip
