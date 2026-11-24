# Owner test: install a real pinned llama.cpp build and (optionally) load a model.
#
# Run from any folder:
#   powershell -ExecutionPolicy Bypass -File C:\Lu\scripts\owner-tests\engine-and-model.ps1 -Engine vulkan
#   powershell -ExecutionPolicy Bypass -File C:\Lu\scripts\owner-tests\engine-and-model.ps1 -Engine rocm -Model "D:\models\Qwen3.8-27B-Q4_K_M.gguf"
#
# What it does:
#   1. Downloads the pinned llama.cpp b12651 build for -Engine (cpu 25 MB,
#      vulkan 39 MB, rocm 284 MB) from github.com/ggml-org/llama.cpp into a
#      new temporary folder, checks its SHA-256 against Lu's pin list,
#      extracts it and runs llama-server --version.
#      For -Engine rocm it first installs Lu's ROCm runtime pack (hipBLAS,
#      rocBLAS and their GPU kernels, 492 MB) from AMD's pinned ROCm 10.0
#      tarball: from -RocmSource if you give a local copy whose SHA-256
#      matches, otherwise downloaded from stable.repo.amd.com (2.28 GB).
#   2. With -Model, loads that GGUF (backend order: the chosen engine), asks
#      for a short completion and prints the load time and tokens per second.
#   3. Deletes the temporary folder.
# It does not touch LM Studio, your real Lu data or any model folder; the
# model file is only read.

param(
    [ValidateSet('cpu', 'vulkan', 'rocm')][string]$Engine = 'vulkan',
    [string]$Model = '',
    [string]$RocmSource = ''
)
$ErrorActionPreference = 'Stop'
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..\..'))
$dir = Join-Path ([IO.Path]::GetTempPath()) ("lu-engine-test-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $dir | Out-Null
try {
    $env:LU_TEST_ENGINE = $Engine
    $env:LU_TEST_ENGINE_DIR = $dir
    if ($Model) {
        if (-not (Test-Path -LiteralPath $Model -PathType Leaf)) { throw "no such model file: $Model" }
        $env:LU_TEST_GGUF = (Resolve-Path -LiteralPath $Model).Path
    } else {
        Remove-Item Env:LU_TEST_GGUF -ErrorAction SilentlyContinue
    }
    if ($RocmSource) {
        if (-not (Test-Path -LiteralPath $RocmSource -PathType Leaf)) { throw "no such file: $RocmSource" }
        $env:LU_TEST_ROCM_SOURCE = (Resolve-Path -LiteralPath $RocmSource).Path
    } else {
        Remove-Item Env:LU_TEST_ROCM_SOURCE -ErrorAction SilentlyContinue
    }
    cargo test -p lu-core --test engines -- --ignored real_engine_install_and_load --nocapture
    if ($LASTEXITCODE -ne 0) { Write-Host "RESULT: FAIL ($Engine)"; exit 1 }
    Write-Host "RESULT: PASS ($Engine$(if ($Model) { ', model loaded' } else { ', install only' }))"
} finally {
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
    Remove-Item Env:LU_TEST_ENGINE, Env:LU_TEST_ENGINE_DIR, Env:LU_TEST_GGUF, Env:LU_TEST_ROCM_SOURCE -ErrorAction SilentlyContinue
}
