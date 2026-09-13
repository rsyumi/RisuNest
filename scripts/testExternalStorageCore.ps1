param([switch]$FullNative, [switch]$CheckAndroid, [ValidateRange(1, 64)][int]$NativeTestThreads = 1)

$ErrorActionPreference = 'Stop'
$taskRepo = Split-Path -Parent $PSScriptRoot
$taskPreviousTarget = $env:CARGO_TARGET_DIR
# All checkout/worktree builds use the main checkout's shared cache.
$taskSafeRepo = $taskRepo.Replace('\', '/')
$taskCommonGit = & git -c "safe.directory=$taskSafeRepo" -C $taskRepo rev-parse --path-format=absolute --git-common-dir
if ($LASTEXITCODE -ne 0 -or (Split-Path -Leaf $taskCommonGit) -ne '.git') {
    throw 'Cannot locate the main checkout shared Cargo target'
}
$env:CARGO_TARGET_DIR = Join-Path (Split-Path -Parent $taskCommonGit) 'src-tauri/target'
$taskOutput = Join-Path $taskRepo ('.tmp/external-storage-core-' + [Guid]::NewGuid().ToString('N'))

function Invoke-CoreCheck([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Program failed with exit code $LASTEXITCODE"
    }
}

Push-Location $taskRepo
try {
    New-Item -ItemType Directory -Path $taskOutput | Out-Null
    Invoke-CoreCheck 'cargo' @('test', '--manifest-path', 'crates/external-storage-format/Cargo.toml', '--locked')
    Invoke-CoreCheck 'cargo' @('build', '--manifest-path', 'crates/external-storage-wasm/Cargo.toml', '--locked', '--target', 'wasm32-unknown-unknown')
    $taskWasm = Join-Path $env:CARGO_TARGET_DIR 'wasm32-unknown-unknown/debug/risunest_external_storage_wasm.wasm'
    Invoke-CoreCheck 'wasm-bindgen' @($taskWasm, '--target', 'nodejs', '--out-dir', $taskOutput)
    [IO.File]::WriteAllText((Join-Path $taskOutput 'package.json'), '{"type":"commonjs"}')
    $taskVector = & cargo run --manifest-path crates/external-storage-format/Cargo.toml --locked --example golden
    if ($LASTEXITCODE -ne 0) { throw 'Native synthetic vector generation failed' }
    $taskVectorPath = Join-Path $taskOutput 'native-vector.json'
    [IO.File]::WriteAllText($taskVectorPath, ($taskVector -join "`n"))
    Invoke-CoreCheck 'node' @('tests/externalStorageWasm.mjs', (Join-Path $taskOutput 'risunest_external_storage_wasm.js'), $taskVectorPath)
    # Native must compile again AFTER standalone core/WASM builds. This ordering
    # catches dependency artifact collisions inside the shared target directory.
    $taskNative = @('test', '--manifest-path', 'src-tauri/Cargo.toml', '--locked', '--lib')
    if (-not $FullNative) { $taskNative += 'external_storage' }
    $taskNative += @('--', "--test-threads=$NativeTestThreads")
    Invoke-CoreCheck 'cargo' $taskNative
    if ($CheckAndroid) {
        Invoke-CoreCheck 'cargo' @('check', '--manifest-path', 'crates/external-storage-format/Cargo.toml', '--locked', '--target', 'aarch64-linux-android')
    }
    Write-Output "External storage core checks passed. Synthetic artifacts: $taskOutput"
} finally {
    Pop-Location
    $env:CARGO_TARGET_DIR = $taskPreviousTarget
}
