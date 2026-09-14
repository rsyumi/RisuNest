param([switch]$FullNative, [switch]$CheckAndroid, [ValidateRange(1, 64)][int]$NativeTestThreads = 1, [string]$NdkHome = $env:ANDROID_NDK_HOME)

$ErrorActionPreference = 'Stop'
$taskRepo = Split-Path -Parent $PSScriptRoot
$taskPreviousTarget = $env:CARGO_TARGET_DIR
$taskCompilerVariables = @('CC_wasm32_unknown_unknown', 'AR_wasm32_unknown_unknown', 'CC_aarch64_linux_android', 'AR_aarch64_linux_android', 'CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER')
$taskPreviousCompilers = @{}
foreach ($taskName in $taskCompilerVariables) { $taskPreviousCompilers[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process') }
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
    if (-not $NdkHome -and $IsWindows) {
        $taskSdk = $env:ANDROID_HOME
        if (-not $taskSdk) { $taskSdk = Join-Path $env:LOCALAPPDATA 'Android/Sdk' }
        $taskNdk = Get-ChildItem -LiteralPath (Join-Path $taskSdk 'ndk') -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -like '28.*' } | Sort-Object Name -Descending | Select-Object -First 1
        if ($taskNdk) { $NdkHome = $taskNdk.FullName }
    }
    if ($IsWindows -and $NdkHome) {
        $taskLlvm = Join-Path $NdkHome 'toolchains/llvm/prebuilt/windows-x86_64/bin'
        if (-not (Test-Path -LiteralPath (Join-Path $taskLlvm 'clang.exe'))) { throw 'NDK clang not found' }
        if (-not $env:CC_wasm32_unknown_unknown) { $env:CC_wasm32_unknown_unknown = Join-Path $taskLlvm 'clang.exe' }
        if (-not $env:AR_wasm32_unknown_unknown) { $env:AR_wasm32_unknown_unknown = Join-Path $taskLlvm 'llvm-ar.exe' }
        if ($CheckAndroid) {
            $env:CC_aarch64_linux_android = Join-Path $taskLlvm 'aarch64-linux-android24-clang.cmd'
            $env:AR_aarch64_linux_android = Join-Path $taskLlvm 'llvm-ar.exe'
            $env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = $env:CC_aarch64_linux_android
        }
    }
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
    $taskReverseVector = Join-Path $taskOutput 'wasm-vector.json'
    Invoke-CoreCheck 'node' @('tests/externalStorageWasm.mjs', (Join-Path $taskOutput 'risunest_external_storage_wasm.js'), $taskVectorPath, $taskReverseVector)
    Invoke-CoreCheck 'cargo' @('run', '--manifest-path', 'crates/external-storage-format/Cargo.toml', '--locked', '--example', 'golden', '--', $taskReverseVector)
    # Native must compile again AFTER standalone core/WASM builds. This ordering
    # catches dependency artifact collisions inside the shared target directory.
    $taskNative = @('test', '--manifest-path', 'src-tauri/Cargo.toml', '--locked', '--lib')
    if (-not $FullNative) { $taskNative += 'external_storage' }
    $taskNative += @('--', "--test-threads=$NativeTestThreads")
    Invoke-CoreCheck 'cargo' $taskNative
    if ($CheckAndroid) {
        Invoke-CoreCheck 'cargo' @('build', '--manifest-path', 'crates/external-storage-format/Cargo.toml', '--locked', '--target', 'aarch64-linux-android', '--example', 'golden')
    }
    Write-Output "External storage core checks passed. Synthetic artifacts: $taskOutput"
} finally {
    Pop-Location
    $env:CARGO_TARGET_DIR = $taskPreviousTarget
    foreach ($taskName in $taskCompilerVariables) { [Environment]::SetEnvironmentVariable($taskName, $taskPreviousCompilers[$taskName], 'Process') }
}
