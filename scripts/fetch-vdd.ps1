# Downloads the pinned Virtual Display Driver (MIT; signed by SignPath
# Foundation) into third_party/vdd: only the driver (INF, DLL, catalog) and
# its licence, not its control app. Bundled in the Windows zip's vdd folder
# and installed from Glass's Settings (ADR 0030). Same file locally and in CI.
$ErrorActionPreference = 'Stop'
$tag = '25.7.23'
$url = "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/$tag/VirtualDisplayDriver-x86.Driver.Only.zip"
$sha256 = 'E24210692B442B39AF763536330CE78B423F19342B7A7792C26DE3944E418B3A'
$license = "https://raw.githubusercontent.com/VirtualDrivers/Virtual-Display-Driver/$tag/LICENSE"

$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'third_party/vdd'
if (Test-Path (Join-Path $dest 'MttVDD.inf')) { Write-Host "Virtual Display Driver already at $dest"; exit 0 }

$tmp = Join-Path ([IO.Path]::GetTempPath()) "glass-vdd-$([guid]::NewGuid())"
New-Item -ItemType Directory -Force $tmp | Out-Null
$zip = Join-Path $tmp 'vdd.zip'
Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
$hash = (Get-FileHash $zip -Algorithm SHA256).Hash
if ($hash -ne $sha256) { throw "Virtual Display Driver checksum mismatch: $hash" }
Expand-Archive $zip -DestinationPath $tmp
New-Item -ItemType Directory -Force $dest | Out-Null
foreach ($f in 'MttVDD.inf', 'MttVDD.dll', 'mttvdd.cat') {
    Copy-Item (Join-Path $tmp "VirtualDisplayDriver/$f") $dest
}
Invoke-WebRequest -Uri $license -OutFile (Join-Path $dest 'LICENSE.txt') -UseBasicParsing
Remove-Item -Recurse -Force $tmp
Write-Host "Virtual Display Driver $tag -> $dest"
