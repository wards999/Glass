# Downloads the pinned FFmpeg dev build (headers, import libs, DLLs) used by
# app-win into third_party/ffmpeg. Same file locally and in CI.
$ErrorActionPreference = 'Stop'
$url = 'https://github.com/FFmpeg-Builds/FFmpeg-Builds/releases/download/autobuild-2026-11-19-12-39/ffmpeg-n8.1.3-36-ge22a4510c9-win64-lgpl-shared-8.1.zip'
$sha256 = 'AE947C6F7F87D19CF3DE80E448409419A9E81A18987D55EAD133C55C2DB29E9C'

$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'third_party/ffmpeg'
if (Test-Path (Join-Path $dest 'lib/avcodec.lib')) { Write-Host "FFmpeg already at $dest"; exit 0 }

$tmp = Join-Path ([IO.Path]::GetTempPath()) "glass-ffmpeg-$([guid]::NewGuid())"
New-Item -ItemType Directory -Force $tmp | Out-Null
$zip = Join-Path $tmp 'ffmpeg.zip'
Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
if ($hash -ne $sha256) { throw "FFmpeg checksum mismatch: $hash" }
Expand-Archive $zip -DestinationPath $tmp
New-Item -ItemType Directory -Force (Split-Path $dest) | Out-Null
Move-Item (Get-ChildItem $tmp -Directory | Select-Object -First 1).FullName $dest
Remove-Item -Recurse -Force $tmp
Write-Host "FFmpeg 8.1 -> $dest"
