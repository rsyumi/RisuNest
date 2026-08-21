[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string] $ApkPath,

    [string] $SdkRoot,
    [string] $NdkRoot
)

Set-StrictMode -Version Latest

$isWindowsHost = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows
)
$isLinuxHost = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Linux
)

if (-not $isWindowsHost -and -not $isLinuxHost) {
    throw 'Android alignment checking is supported only on Windows and Linux.'
}

if (-not (Test-Path -LiteralPath $ApkPath -PathType Leaf)) {
    throw "APK file was not found: $ApkPath"
}
$resolvedApkPath = (Resolve-Path -LiteralPath $ApkPath).Path

$sdkCandidate = if ($SdkRoot) {
    $SdkRoot
}
elseif ($env:ANDROID_SDK_ROOT) {
    $env:ANDROID_SDK_ROOT
}
elseif ($env:ANDROID_HOME) {
    $env:ANDROID_HOME
}
else {
    $null
}

if (-not $sdkCandidate) {
    throw 'SDK root was not provided and ANDROID_SDK_ROOT or ANDROID_HOME is not set.'
}
if (-not (Test-Path -LiteralPath $sdkCandidate -PathType Container)) {
    throw "SDK root directory was not found: $sdkCandidate"
}
$resolvedSdkRoot = (Resolve-Path -LiteralPath $sdkCandidate).Path

$ndkCandidate = if ($NdkRoot) {
    $NdkRoot
}
elseif ($env:ANDROID_NDK_HOME) {
    $env:ANDROID_NDK_HOME
}
elseif ($env:NDK_HOME) {
    $env:NDK_HOME
}
else {
    Join-Path $resolvedSdkRoot 'ndk/28.2.13676358'
}

if (-not (Test-Path -LiteralPath $ndkCandidate -PathType Container)) {
    throw "NDK root directory was not found: $ndkCandidate"
}
$resolvedNdkRoot = (Resolve-Path -LiteralPath $ndkCandidate).Path

$executableSuffix = if ($isWindowsHost) { '.exe' } else { '' }
$hostTag = if ($isWindowsHost) { 'windows-x86_64' } else { 'linux-x86_64' }
$zipAlignPath = Join-Path $resolvedSdkRoot "build-tools/36.0.0/zipalign$executableSuffix"
$objdumpPath = Join-Path $resolvedNdkRoot "toolchains/llvm/prebuilt/$hostTag/bin/llvm-objdump$executableSuffix"

if (-not (Test-Path -LiteralPath $zipAlignPath -PathType Leaf)) {
    throw "zipalign was not found: $zipAlignPath"
}
if (-not (Test-Path -LiteralPath $objdumpPath -PathType Leaf)) {
    throw "llvm-objdump was not found: $objdumpPath"
}

& $zipAlignPath -c -P 16 -v 4 $resolvedApkPath
$zipAlignExitCode = $LASTEXITCODE
if ($zipAlignExitCode -ne 0) {
    throw "zipalign verification failed for $resolvedApkPath with exit code $zipAlignExitCode"
}

if ($PSVersionTable.PSEdition -eq 'Desktop') {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
}

$tempDirectory = $null
$tempPrefix = 'risu-android-alignment-'
$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())

try {
    $tempDirectory = Join-Path $tempRoot ($tempPrefix + [Guid]::NewGuid().ToString('N'))
    [void] [IO.Directory]::CreateDirectory($tempDirectory)
    [System.IO.Compression.ZipFile]::ExtractToDirectory($resolvedApkPath, $tempDirectory)

    $arm64LibraryDirectory = Join-Path $tempDirectory 'lib/arm64-v8a'
    if (-not [IO.Directory]::Exists($arm64LibraryDirectory)) {
        throw "APK contains no lib/arm64-v8a directory: $resolvedApkPath"
    }

    $libraries = @(Get-ChildItem -LiteralPath $arm64LibraryDirectory -File -Filter '*.so' |
        Sort-Object FullName)
    if ($libraries.Count -eq 0) {
        throw "APK contains no arm64 native libraries: $resolvedApkPath"
    }

    foreach ($library in $libraries) {
        $objdumpOutput = @(& $objdumpPath -p $library.FullName)
        $objdumpExitCode = $LASTEXITCODE
        if ($objdumpExitCode -ne 0) {
            throw "llvm-objdump failed for $($library.FullName) with exit code $objdumpExitCode"
        }

        $loadLines = @($objdumpOutput | Where-Object { [string] $_ -match '^\s*LOAD\b' })
        if ($loadLines.Count -eq 0) {
            throw "No LOAD segments found in $($library.FullName)"
        }

        foreach ($loadLine in $loadLines) {
            $loadText = [string] $loadLine
            if ($loadText -notmatch '\balign\s+2\*\*(\d+)\s*$') {
                throw "Could not parse LOAD alignment in $($library.FullName): $loadText"
            }

            $alignmentExponent = [int] $Matches[1]
            Write-Output "ELF $($library.Name): $($loadText.Trim())"
            if ($alignmentExponent -lt 14) {
                throw "LOAD alignment below 2**14 in $($library.FullName): $($loadText.Trim())"
            }
        }
    }

    Write-Output "APK: $resolvedApkPath"
    Write-Output "Checked arm64 libraries: $($libraries.Count)"
}
finally {
    if ($tempDirectory -and [IO.Directory]::Exists($tempDirectory)) {
        $tempFullPath = [IO.Path]::GetFullPath($tempDirectory)
        $tempParent = [IO.Directory]::GetParent($tempFullPath).FullName
        $tempLeaf = [IO.Path]::GetFileName($tempFullPath)
        $pathComparison = if ($isWindowsHost) {
            [StringComparison]::OrdinalIgnoreCase
        }
        else {
            [StringComparison]::Ordinal
        }

        if (-not [string]::Equals(
                $tempParent.TrimEnd([IO.Path]::DirectorySeparatorChar),
                $tempRoot.TrimEnd([IO.Path]::DirectorySeparatorChar),
                $pathComparison
            ) -or -not $tempLeaf.StartsWith($tempPrefix, [StringComparison]::Ordinal)) {
            throw "Refusing to remove unexpected temp path: $tempFullPath"
        }

        [IO.Directory]::Delete($tempFullPath, $true)
    }
}
