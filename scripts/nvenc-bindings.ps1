# Regenerates app-win/src/win/nvenc_sys.rs (the direct NVENC path's Rust
# bindings) from NVIDIA's nvEncodeAPI.h in nv-codec-headers (MIT), pinned.
# Only needed when moving to a newer NVENC API: the generated file is in the
# repo, so builds and CI don't need the header or bindgen.
# Needs: bindgen (cargo install bindgen-cli) and LLVM's libclang, run inside a
# VS developer environment.
$ErrorActionPreference = 'Stop'
$tag = 'n12.1.14.1'
$sha256 = 'DBDAA8BCBC2B9325BD24BC68B36170A8432411518040E8D4C8421608CF217425'
$url = "https://raw.githubusercontent.com/FFmpeg/nv-codec-headers/$tag/include/ffnvcodec/nvEncodeAPI.h"

$root = Split-Path -Parent $PSScriptRoot
$dir = Join-Path $root 'third_party/nvenc'
$h = Join-Path $dir 'nvEncodeAPI.h'
New-Item -ItemType Directory -Force $dir | Out-Null
if (-not (Test-Path $h)) { Invoke-WebRequest -Uri $url -OutFile $h -UseBasicParsing }
$hash = (Get-FileHash $h -Algorithm SHA256).Hash
if ($hash -ne $sha256) { throw "nvEncodeAPI.h checksum mismatch: $hash" }
if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }
$out = Join-Path $root 'app-win/src/win/nvenc_sys.rs'
bindgen $h `
    --allowlist-type 'NV_ENC.*|_NV_ENC.*|NVENC.*|PNVENC.*|NV_ENCODE.*|GUID|_GUID' `
    --allowlist-var 'NV_ENC.*|NVENC.*' `
    --no-layout-tests --default-enum-style consts --no-prepend-enum-name --no-doc-comments `
    -o $out -- --target=x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw 'bindgen failed' }
Write-Host "Regenerated $out from nv-codec-headers $tag. Put back the licence header at its top."
