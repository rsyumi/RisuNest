param(
    [Parameter(Mandatory = $true)][string]$TestBinary,
    [ValidateSet('buffered', 'streaming')][string]$Mode = 'streaming',
    [ValidateRange(1, 100)][int]$Trial = 1
)
$ErrorActionPreference = 'Stop'
$outputDirectory = Join-Path (Get-Location) '.tmp/media-memory'
New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
$logPath = Join-Path $outputDirectory "$Mode-$Trial-out.log"
$errorPath = Join-Path $outputDirectory "$Mode-$Trial-error.log"
$env:RISUNEST_MEDIA_PROBE_MODE = $Mode
$process = Start-Process -FilePath (Resolve-Path -LiteralPath $TestBinary) -ArgumentList @(
    'native_media::streaming::tests::desktop_probe::compare_file_display',
    '--ignored', '--exact', '--nocapture'
) -WindowStyle Hidden -PassThru -RedirectStandardOutput $logPath -RedirectStandardError $errorPath
$watch = [Diagnostics.Stopwatch]::StartNew()
$samples = [Collections.Generic.List[object]]::new()
$known = [Collections.Generic.HashSet[int]]::new()
$known.Add($process.Id) | Out-Null
$handles = @{}
$handles[$process.Id] = $process
$nextDiscovery = 0
while (-not $process.HasExited) {
    if ($watch.ElapsedMilliseconds -ge $nextDiscovery) {
        $children = Get-CimInstance Win32_Process -Filter "Name = 'msedgewebview2.exe'" | Select-Object ProcessId, ParentProcessId
        do {
            $added = $false
            foreach ($child in $children) {
                if ($known.Contains([int]$child.ParentProcessId) -and $known.Add([int]$child.ProcessId)) {
                    $handles[[int]$child.ProcessId] = Get-Process -Id $child.ProcessId -ErrorAction SilentlyContinue
                    $added = $true
                }
            }
        } while ($added)
        # Keep slow process discovery away from the timed image request at 2s.
        $nextDiscovery = if ($nextDiscovery -eq 0) { 1000 } elseif ($nextDiscovery -eq 1000) { 5000 } else { 15000 }
    }
    $nativePrivate = 0L
    $totalPrivate = 0L
    $count = 0
    foreach ($processId in $known) {
        $item = $handles[$processId]
        if ($null -ne $item -and -not $item.HasExited) {
            $item.Refresh()
            if ($processId -eq $process.Id) { $nativePrivate = $item.PrivateMemorySize64 }
            $totalPrivate += $item.PrivateMemorySize64
            $count++
        }
    }
    $samples.Add([pscustomobject]@{ms=$watch.ElapsedMilliseconds;nativePrivate=$nativePrivate;totalPrivate=$totalPrivate;processes=$count})
    Start-Sleep -Milliseconds 20
    $process.Refresh()
}
$process.WaitForExit()
$samples | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $outputDirectory "$Mode-$Trial-samples.json")
$result = Get-Content -LiteralPath $logPath | Where-Object { $_ -match '^MEDIA_PROBE_' }
$summary = [pscustomobject]@{
    mode=$Mode;trial=$Trial;exit=$process.ExitCode;samples=$samples.Count
    nativePeak=($samples.nativePrivate | Measure-Object -Maximum).Maximum
    totalPrivatePeak=($samples.totalPrivate | Measure-Object -Maximum).Maximum
    maxProcesses=($samples.processes | Measure-Object -Maximum).Maximum
    result=$result
}
$summary | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $outputDirectory "$Mode-$Trial-summary.json")
$summary | ConvertTo-Json
if ($process.ExitCode -ne 0) { throw 'Synthetic media probe failed; inspect its isolated test logs.' }
