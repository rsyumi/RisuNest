param([Parameter(Mandatory=$true)][ValidateSet('Start','Check','Restart','Stop')][string]$Action)
$ErrorActionPreference='Stop'
$taskOutput=Join-Path $PSScriptRoot '.local'
$manifest=Join-Path $taskOutput 'windows-process.json'
$binary=[IO.Path]::GetFullPath((Join-Path $env:CARGO_TARGET_DIR 'debug/RisuNestSyncValidation.exe'))
$identifier='io.github.rsyumi.risunest.syncservervalidation20260911'
New-Item -ItemType Directory -Path $taskOutput -Force | Out-Null
function Get-OwnedProcess {
  if(-not (Test-Path -LiteralPath $manifest)){throw 'No synthetic process record'}
  $record=Get-Content -LiteralPath $manifest | ConvertFrom-Json
  $actual=Get-Process -Id $record.pid -ErrorAction Stop
  if($actual.Path -ne $binary -or $actual.Path -ne [IO.Path]::GetFullPath($record.exe) -or $actual.StartTime.ToUniversalTime() -ne ([datetime]$record.start).ToUniversalTime()){throw 'Unexpected validation process'}
  return $actual
}
if($Action -in @('Restart','Stop')){
  $owned=Get-OwnedProcess
  Stop-Process -Id $owned.Id -ErrorAction Stop
  $owned.WaitForExit(10000) | Out-Null
  if($Action -eq 'Stop'){exit 0}
}
if($Action -in @('Start','Restart')){
  if(-not [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($binary)).Contains($identifier)){throw 'Binary does not have the isolated app identifier'}
  if(Get-NetTCPConnection -LocalPort 19421 -State Listen -ErrorAction SilentlyContinue){throw 'Validation CDP port is occupied'}
  $env:WEBVIEW2_USER_DATA_FOLDER=Join-Path $taskOutput 'windows-webview'
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=19421 --remote-debugging-address=127.0.0.1'
  $process=Start-Process -FilePath $binary -WorkingDirectory ([IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))) -WindowStyle Hidden -PassThru
  @{pid=$process.Id;start=$process.StartTime.ToString('o');exe=$binary} | ConvertTo-Json | Set-Content -LiteralPath $manifest
  $process.Id
  exit 0
}
if($Action -eq 'Check'){
  $owned=Get-OwnedProcess
  $listeners=Get-NetTCPConnection -LocalPort 19421 -State Listen -ErrorAction Stop
  foreach($listener in $listeners){
    $cursor=[int]$listener.OwningProcess
    $matched=$false
    for($i=0;$i -lt 12 -and $cursor -gt 0;$i++){
      if($cursor -eq $owned.Id){$matched=$true;break}
      $cursor=[int](Get-CimInstance Win32_Process -Filter "ProcessId=$cursor").ParentProcessId
    }
    if(-not $matched){throw 'CDP listener is not owned by the synthetic app'}
  }
  $owned.Id
}
