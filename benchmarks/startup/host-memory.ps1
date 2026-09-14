param([Parameter(Mandatory = $true)][int]$RootProcessId)
$ErrorActionPreference = 'Stop'
$rootProcess = Get-Process -Id $RootProcessId
$relations = @(Get-CimInstance Win32_Process -Property ProcessId, ParentProcessId | Select-Object ProcessId, ParentProcessId)
$owned = [System.Collections.Generic.HashSet[int]]::new()
[void]$owned.Add($RootProcessId)
do {
    $added = $false
    foreach ($relation in $relations) {
        if ($owned.Contains([int]$relation.ParentProcessId) -and $owned.Add([int]$relation.ProcessId)) { $added = $true }
    }
} while ($added)
$treeBytes = [long]0
$count = 0
foreach ($processId in $owned) {
    $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
    if ($process) { $treeBytes += $process.WorkingSet64; $count++ }
}
[ordered]@{
    nativeWorkingSetBytes = $rootProcess.WorkingSet64
    nativePeakWorkingSetBytes = $rootProcess.PeakWorkingSet64
    treeWorkingSetBytes = $treeBytes
    processCount = $count
} | ConvertTo-Json -Compress
