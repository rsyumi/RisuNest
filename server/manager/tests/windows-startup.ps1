param([Parameter(Mandatory)][string]$Manager,[Parameter(Mandatory)][string]$Server)
$ErrorActionPreference='Stop'
$testRoot=Join-Path $PSScriptRoot '../.test-output'
[void](New-Item -ItemType Directory -Force -Path $testRoot)
$dataRoot=Join-Path (Resolve-Path -LiteralPath $testRoot).Path ('scheduler-'+[guid]::NewGuid().ToString('N'))
$started=$false
function Invoke-Manager([string[]]$ManagerArguments) {
    $result=& $Manager --data-dir $dataRoot --server $Server @ManagerArguments
    if($LASTEXITCODE -ne 0){throw 'Synthetic task operation failed'}
    return $result
}
try {
    Invoke-Manager @('autostart','install') | Out-Null
    $registered=Invoke-Manager @('autostart','status') | ConvertFrom-Json
    if(!$registered.registered -or !$registered.enabled){throw 'Task was not registered'}
    if(!(Get-NetTCPConnection -State Listen -LocalPort 4319 -ErrorAction SilentlyContinue)) {
        Invoke-Manager @('start') | Out-Null
        $started=$true
        Invoke-Manager @('status') | Out-Null
    } else { Write-Output 'SKIP: task start and running-task removal, port 4319 is occupied' }
    Invoke-Manager @('autostart','remove') | Out-Null
    $removed=Invoke-Manager @('autostart','status') | ConvertFrom-Json
    if($removed.registered -or $removed.enabled){throw 'Task was not removed'}
    if($started) {
        Invoke-Manager @('status') | Out-Null
        Invoke-Manager @('stop') | Out-Null
        $started=$false
        Write-Output 'PASS: task starts daemon, registration removal preserves the live daemon, graceful stop'
    }
    Write-Output 'PASS: current-user task registration, status and removal in an isolated data directory'
} finally {
    if($started) { & $Manager --data-dir $dataRoot --server $Server stop | Out-Null }
    & $Manager --data-dir $dataRoot --server $Server autostart remove | Out-Null
    if($LASTEXITCODE -ne 0){Write-Warning "Synthetic task cleanup needs attention: $dataRoot"}
}
