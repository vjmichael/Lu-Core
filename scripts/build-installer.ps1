# Build Lu's Windows installer (NSIS).
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1
#
# 1. Builds the installed Lu (release, its own data folder %LOCALAPPDATA%\Lu).
# 2. Gathers the pinned llama.cpp engines (CPU, Vulkan, ROCm) from GitHub,
#    checking each archive's SHA-256 against crates\lu-core\pins.
# 3. Gathers the pinned ROCm runtime pack files from an installed copy (a Lu
#    data folder that already set it up), checking every file's SHA-256.
# 4. Runs makensis on installer\lu.nsi.
#
# Output: target\installer\Lu-Setup-<version>.exe
# Nothing is installed on this PC. Downloads are cached in target\installer\cache.
param(
    # Where to take the ROCm pack files from (a Lu data folder's engines\runtime).
    [string]$PackFrom = "$env:LOCALAPPDATA\Lu-Dev\engines\runtime",
    [string]$Makensis = "${env:ProgramFiles(x86)}\NSIS\makensis.exe"
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path crates\lu-desktop\Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$out = Join-Path $root "target\installer"
$stage = Join-Path $out "stage"
$cache = Join-Path $out "cache"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force "$stage\bundled\packs", $cache | Out-Null

function Get-Sha([string]$path) { (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant() }

Write-Host "1/4  Building Lu $version (installed identity)..."
# Its own build folder: never collides with a running development build.
cargo build --release --locked -p lu-desktop --features "installed,custom-protocol" --target-dir "$out\build"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
Copy-Item "$out\build\release\lu.exe" "$stage\lu.exe"
Copy-Item installer\THIRD_PARTY.txt "$stage\THIRD_PARTY.txt"

Write-Host "2/4  Engines (pinned llama.cpp builds)..."
$pins = (Get-Content crates\lu-core\pins\llama-engines.json -Raw | ConvertFrom-Json).builds | Where-Object platform -eq "windows-x64"
foreach ($p in $pins) {
    $file = Join-Path $cache $p.asset
    if (-not (Test-Path $file) -or (Get-Item $file).Length -ne $p.bytes) {
        $url = "https://github.com/ggml-org/llama.cpp/releases/download/$($p.version)/$($p.asset)"
        Write-Host "     downloading $($p.asset) ($([math]::Round($p.bytes / 1MB)) MB)"
        Invoke-WebRequest -Uri $url -OutFile $file -UseBasicParsing
    }
    if ((Get-Item $file).Length -ne $p.bytes -or (Get-Sha $file) -ne $p.sha256) {
        Remove-Item $file -Force
        throw "$($p.asset) does not match its pin; it was deleted"
    }
    Copy-Item $file (Join-Path "$stage\bundled" $p.asset)
    Write-Host "     $($p.variant): $($p.asset) ok"
}

Write-Host "3/4  ROCm runtime pack..."
$packs = (Get-Content crates\lu-core\pins\rocm-runtime.json -Raw | ConvertFrom-Json).packs | Where-Object platform -eq "windows-x64"
foreach ($pack in $packs) {
    $src = Join-Path $PackFrom $pack.name
    if (-not (Test-Path $src)) { throw "No installed copy of $($pack.name) at $src. Set it up once in a Lu (Models > Engines > ROCm), or pass -PackFrom." }
    $dst = Join-Path "$stage\bundled\packs" $pack.name
    $n = 0
    foreach ($f in $pack.files) {
        $rel = $f.path -replace '/', '\'
        $from = Join-Path $src $rel
        if (-not (Test-Path -LiteralPath $from)) { throw "$($pack.name) lacks $($f.path)" }
        if ((Get-Item -LiteralPath $from).Length -ne $f.bytes -or (Get-Sha $from) -ne $f.sha256) { throw "$($pack.name): $($f.path) does not match its pin" }
        $to = Join-Path $dst $rel
        New-Item -ItemType Directory -Force (Split-Path -Parent $to) | Out-Null
        Copy-Item -LiteralPath $from -Destination $to
        $n++
    }
    Write-Host "     $($pack.name): $n files ok"
}

Write-Host "4/4  Installer..."
if (-not (Test-Path $Makensis)) { throw "makensis not found at $Makensis (install NSIS 3)" }
$setup = Join-Path $out "Lu-Setup-$version.exe"
& $Makensis /V2 "/DSTAGE=$stage" "/DVERSION=$version" "/DOUTFILE=$setup" installer\lu.nsi
if ($LASTEXITCODE -ne 0) { throw "makensis failed" }
$size = [math]::Round((Get-Item $setup).Length / 1MB)
Write-Host "Done: $setup ($size MB), SHA-256 $(Get-Sha $setup)"
