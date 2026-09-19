$ErrorActionPreference = 'Stop'
Push-Location (Join-Path $PSScriptRoot '../src-tauri/gen/android')
try {
    $gradle = if ($IsWindows) { './gradlew.bat' } else { './gradlew' }
    & $gradle :app:testArmDebugUnitTest :app:testLowMemorySafCopy :tauri-plugin-barcode-scanner:testDebugUnitTest --no-daemon
    if ($LASTEXITCODE -ne 0) { throw "Android tests failed with exit code $LASTEXITCODE" }
} finally {
    Pop-Location
}
