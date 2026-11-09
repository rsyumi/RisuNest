param(
    [string]$ReleasedBundle,
    [string]$LegacyReleasedBundle,
    [string]$ManagerBinary,
    [string]$BackgroundBinary
)
$ErrorActionPreference = "Stop"
if ($ManagerBinary) { $ManagerBinary = [IO.Path]::GetFullPath($ManagerBinary) }
if ($BackgroundBinary) { $BackgroundBinary = [IO.Path]::GetFullPath($BackgroundBinary) }
$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$testRoot = Join-Path $tempRoot ("risunest-nsis-guard-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $testRoot | Out-Null

function Copy-ArchiveFile([string]$source, [string]$destination, [int]$timeoutMilliseconds = 30000, [int]$pollMilliseconds = 50) {
    $elapsed = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        try {
            Copy-Item -LiteralPath $source -Destination $destination -Force -ErrorAction Stop
            return
        } catch [IO.IOException] {
            if (($_.Exception.HResult -band 0xffff) -ne 32 -or $elapsed.ElapsedMilliseconds -ge $timeoutMilliseconds) { throw }
            Start-Sleep -Milliseconds $pollMilliseconds
        }
    }
}

try {
    $fakeSource = @'
use std::{env, fs::OpenOptions, io::Write};

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 3 && args[0] == "synthetic-profile-check" {
        assert_eq!(env::var("LOCALAPPDATA").unwrap(), args[1]);
        assert_eq!(env::var("APPDATA").unwrap(), args[2]);
        return;
    }
    if let Ok(log) = env::var("RISUNEST_NSIS_TEST_LOG") {
        let mut file = OpenOptions::new().create(true).append(true).open(log).unwrap();
        writeln!(file, "{}", args.join("|")).unwrap();
    }
    if args.len() == 3 && args[0] == "installer" && args[1] == "prepare" {
        let owner = args[2].parse::<u32>().unwrap();
        assert!(owner > 0 && owner != std::process::id());
        println!("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        return;
    }
    if args.len() == 3 && args[0] == "installer" && args[1] == "finish" {
        std::process::exit(if args[2] == "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" { 0 } else { 41 });
    }
    if args.first().map(String::as_str) == Some("autostart")
        && env::var("RISUNEST_NSIS_FAIL_AUTOSTART").as_deref() == Ok("1")
    {
        std::process::exit(42);
    }
}
'@
    $fakeSourcePath = Join-Path $testRoot "fake-manager.rs"
    $fakeSource | Set-Content -Encoding utf8 $fakeSourcePath
    $fake = Join-Path $testRoot "risunest-sync-manager.exe"
    & rustc --edition=2021 $fakeSourcePath -o $fake
    if ($LASTEXITCODE -ne 0) {
        throw "rustc failed with exit code $LASTEXITCODE."
    }

    $installer = Join-Path $testRoot "guard-test.exe"
    $hook = (Resolve-Path (Join-Path $PSScriptRoot "windows.nsh")).Path
    $defaultInstallDir = Join-Path $testRoot "default-install"
    $installDir = Join-Path $testRoot "installed"
    $payload = Join-Path $testRoot "hook-output.txt"
    "installed" | Set-Content -Encoding utf8 $payload
    $nsi = Join-Path $testRoot "guard-test.nsi"
    @"
Unicode true
Name "RisuNest guard test"
OutFile "$installer"
InstallDir "`$TEMP\RisuNestGuardTest"
RequestExecutionLevel user
SilentInstall silent
!include LogicLib.nsh
!include FileFunc.nsh
LoadLanguageFile "`${NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "`${NSISDIR}\Contrib\Language files\Korean.nlf"
Var DeleteAppDataCheckboxState
Var UpdateMode
!define RISUNEST_SYNC_DEFAULT_INSTALL_DIR "$defaultInstallDir"
!define RISUNEST_SYNC_INSTALL_DIR "$installDir"
!macro CheckIfAppIsRunning APP PRODUCT
!macroend
!include "$hook"
Section
  SetOutPath `$INSTDIR
  !insertmacro NSIS_HOOK_PREINSTALL
  File /oname=hook-output.txt "$payload"
  CreateShortcut "`$INSTDIR\RisuNest Sync.lnk" "`$INSTDIR\hook-output.txt"
  !insertmacro NSIS_HOOK_POSTINSTALL
SectionEnd
Section "Uninstall"
  !insertmacro NSIS_HOOK_PREUNINSTALL
  !insertmacro NSIS_HOOK_POSTUNINSTALL
SectionEnd
"@ | Set-Content -Encoding utf8 $nsi

    $makeNsis = Join-Path $env:LOCALAPPDATA "tauri/NSIS/makensis.exe"
    if (!(Test-Path $makeNsis)) {
        throw "Tauri NSIS toolchain is unavailable."
    }
    & $makeNsis /V2 $nsi
    if ($LASTEXITCODE -ne 0) {
        throw "makensis failed with exit code $LASTEXITCODE."
    }

    New-Item -ItemType Directory -Path $installDir | Out-Null
    Copy-Item $fake (Join-Path $installDir "risunest-sync-manager.exe")
    $log = Join-Path $testRoot "commands.log"
    $env:RISUNEST_NSIS_TEST_LOG = $log
    $process = Start-Process -FilePath $installer -ArgumentList @("/S", "/D=$defaultInstallDir") -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -ne 0) {
        throw "NSIS success harness exited with $($process.ExitCode)."
    }
    if (!(Test-Path (Join-Path $installDir "hook-output.txt"))) {
        throw "NSIS copied files to the output directory selected before the install hook changed `$INSTDIR."
    }
    if (Test-Path (Join-Path $defaultInstallDir "hook-output.txt")) {
        throw "NSIS left installed files in the superseded install directory."
    }
    $shortcutPath = Join-Path $installDir "RisuNest Sync.lnk"
    $shortcut = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcutPath)
    if ($shortcut.TargetPath -ne (Join-Path $installDir "hook-output.txt") -or !(Test-Path -LiteralPath $shortcut.TargetPath)) {
        throw "NSIS created a shortcut whose target does not match the installed file location."
    }
    $expected = "installer|finish|aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
    if ((Get-Content $log) -notcontains $expected) {
        throw "NSIS did not pass the exact UUID nonce to installer finish."
    }

    $env:RISUNEST_NSIS_FAIL_AUTOSTART = "1"
    $process = Start-Process -FilePath $installer -ArgumentList @("/S", "/D=$defaultInstallDir") -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -eq 0) {
        throw "A quiet postinstall failure returned success."
    }
    Remove-Item Env:RISUNEST_NSIS_FAIL_AUTOSTART
    Write-Output "NSIS runtime guard tests passed."

    $repo = (Resolve-Path (Join-Path $PSScriptRoot '../../..')).Path
    $template = Join-Path $repo 'src-tauri/windows-installer.nsi'
    $templateSource = [IO.File]::ReadAllText($template)
    $pinned = [regex]::Match($templateSource, '@tauri-apps/cli (\d+\.\d+\.\d+)').Groups[1].Value
    foreach ($package in @('package.json', 'server/manager/gui/package.json')) {
        $version = (Get-Content (Join-Path $repo $package) -Raw | ConvertFrom-Json).devDependencies.'@tauri-apps/cli'
        if ($version -ne $pinned) { throw "Re-sync the NSIS template for Tauri CLI $version (template: $pinned)." }
    }
    $sharedTarget = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'src-tauri/target' }
    $fixtureName = 'risunest-nsis-' + [guid]::NewGuid().ToString('N')
    $fixtureBinary = Join-Path $sharedTarget "release/$fixtureName.exe"
    $project = Join-Path $testRoot 'tauri-fixture'
    New-Item -ItemType Directory $project | Out-Null
    Copy-Item $fake $fixtureBinary
    @"
[package]
name = "$fixtureName"
version = "1.0.4"
edition = "2021"
[workspace]
[[bin]]
name = "$fixtureName"
path = "main.rs"
"@ | Set-Content (Join-Path $project 'Cargo.toml')
    'fn main() {}' | Set-Content (Join-Path $project 'main.rs')
    $product = "RisuNest Synthetic $fixtureName"
    $publisher = 'RisuNest Synthetic Tests'
    $config = @{
        productName = $product; version = '1.0.4'; identifier = "test.risunest.$fixtureName"
        build = @{ frontendDist = '.' }
        bundle = @{
            active = $true; targets = @('nsis'); publisher = $publisher; icon = @()
            windows = @{
                webviewInstallMode = @{ type = 'skip' }
                nsis = @{ template = $template; installerHooks = (Join-Path $repo 'src-tauri/windows.nsh'); languages = @('English', 'Korean'); displayLanguageSelector = $false }
            }
        }
    }
    $config | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $project 'tauri.conf.json')
    Push-Location $project
    try {
        $priorTarget = $env:CARGO_TARGET_DIR
        $env:CARGO_TARGET_DIR = $sharedTarget
        & node (Join-Path $repo 'node_modules/@tauri-apps/cli/tauri.js') bundle --ci --no-sign --no-binary-patching
        if ($LASTEXITCODE -ne 0) { throw 'Tauri template generation failed.' }
    } finally {
        $env:CARGO_TARGET_DIR = $priorTarget
        Pop-Location
    }
    # Tauri bundles for the host architecture, so the fixtures follow it on x64 and ARM64 runners.
    $bundleRoot = Join-Path $sharedTarget 'release/bundle/nsis'
    $fixtureInstallers = @(Get-ChildItem -LiteralPath $bundleRoot -Filter "${product}_1.0.4_*-setup.exe")
    if ($fixtureInstallers.Count -ne 1) { throw "Expected one generated installer for $product, found $($fixtureInstallers.Count)." }
    $fixtureInstaller = $fixtureInstallers[0].FullName
    $arch = [regex]::Match($fixtureInstallers[0].Name, '_1\.0\.4_(x64|arm64)-setup\.exe$').Groups[1].Value
    if (!$arch) { throw "Unexpected generated installer name: $($fixtureInstallers[0].Name)." }
    $generatedRoot = Join-Path $sharedTarget "release/nsis/$arch"
    $generated = [IO.File]::ReadAllText((Join-Path $generatedRoot 'installer.nsi'))
    $installed = Join-Path $testRoot 'manual upgrade with spaces'
    $uninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$product"
    $productKey = "HKCU:\Software\$publisher\$product"
    function Set-Architecture([string]$source, [string]$value) {
        $define = "!define ARCH `"$arch`""
        if (!$source.Contains($define)) { throw 'The generated template does not define its package architecture.' }
        return $source.Replace($define, "!define ARCH `"$value`"")
    }
    function Run-Installer([string]$executable, [string[]]$arguments, [int]$expected, [int]$timeoutSeconds = 30) {
        $elapsed = [Diagnostics.Stopwatch]::StartNew()
        $process = Start-Process -FilePath $executable -ArgumentList $arguments -PassThru -WindowStyle Hidden
        if (!$process.WaitForExit($timeoutSeconds * 1000)) {
            $process.Kill()
            throw "Synthetic NSIS invocation exceeded $timeoutSeconds seconds: $executable $arguments."
        }
        if ($timeoutSeconds -gt 30) {
            Write-Output "Released-bundle NSIS invocation completed in $($elapsed.ElapsedMilliseconds) ms with exit $($process.ExitCode): $executable $arguments"
        }
        if ($process.ExitCode -ne $expected) {
            $failureFile = Join-Path $testRoot 'failure.txt'
            $reason = if (Test-Path $failureFile) { Get-Content $failureFile -Raw -Encoding Unicode } else { 'No failure details.' }
            throw "Synthetic installer returned $($process.ExitCode), expected $expected; previous=$previous; arguments=$arguments; executable=$executable. $reason"
        }
    }
    function Compile-Template([string]$name, [string]$source) {
        $output = Join-Path $testRoot "$name.exe"
        $source = $source.Replace('!define OUTFILE "nsis-output.exe"', "!define OUTFILE `"$output`"")
        $file = Join-Path $generatedRoot "$fixtureName-$name.nsi"
        $source | Set-Content -Encoding utf8 $file
        try {
            & $makeNsis /V2 $file | Out-Host
            if ($LASTEXITCODE -ne 0) { throw "Template compilation failed: $name." }
        } finally { Remove-Item -LiteralPath $file }
        return $output
    }
    try {
        Run-Installer $fixtureInstaller @('/S', '/NS', "/D=$installed") 0
        $keep = Join-Path $installed 'user-file.txt'
        'synthetic preserved file' | Set-Content $keep
        foreach ($previous in @('1.0.3', '1.0.4')) {
            Set-ItemProperty $uninstallKey DisplayVersion $previous
            Run-Installer $fixtureInstaller @('/S', '/NS') 0
            if (!(Test-Path $keep)) { throw 'In-place upgrade removed an unowned file.' }
            if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne '1.0.4') { throw 'Upgrade did not publish the new version.' }
            if ((Get-Item $productKey).GetValue('') -ne $installed) { throw 'Upgrade relocated the installation.' }
        }
        $relocated = Join-Path $testRoot 'refused relocation'
        Run-Installer $fixtureInstaller @('/S', '/NS', "/D=$relocated") 1
        if (Test-Path $relocated) { throw 'Silent relocation changed the destination.' }
        foreach ($previous in @('9.0.0', '', 'invalid-version')) {
            Set-ItemProperty $uninstallKey DisplayVersion $previous
            Run-Installer $fixtureInstaller @('/S', '/NS') 1
            if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne $previous) { throw 'A refused version changed the installation.' }
        }
        $confirmationLog = Join-Path $testRoot 'confirmation.txt'
        $confirmBoundary = @"
      FileOpen `$2 "$confirmationLog" w
      FileWrite `$2 "confirm|`$ExistingVersionRelation"
      FileClose `$2
      ReadEnvStr `$2 RISUNEST_NSIS_CONFIRM
      StrCmp `$2 "yes" version_confirmed
"@
        # Keep onInit's real invocation mode and decisions, substituting only dialog answers.
        # Switch to silent after initialization so the remaining wizard requires no operator.
        $modeSource = $generated.Replace('MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 "$InstallFailureMessage" IDYES version_confirmed', $confirmBoundary)
        $modeSource = $modeSource.Replace('MessageBox MB_OK|MB_ICONSTOP "$InstallFailureMessage"', 'DetailPrint "$InstallFailureMessage"')
        $modeSource = [regex]::Replace($modeSource, 'FunctionEnd\r?\n\r?\nFunction InitializationFailure', "  SetSilent silent`nFunctionEnd`n`nFunction InitializationFailure")
        if (!$modeSource.Contains('SetSilent silent')) { throw 'Invocation-mode fixture did not insert its unattended continuation.' }
        $modeInstaller = Compile-Template 'invocation-modes' $modeSource
        Set-ItemProperty $uninstallKey DisplayVersion '1.0.3'
        Run-Installer $modeInstaller @('/P', '/NS', "/D=$relocated") 1
        if (Test-Path $relocated) { throw 'Passive relocation changed the destination.' }
        if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne '1.0.3') { throw 'Passive relocation changed the installed version.' }
        foreach ($previous in @('1.0.3', '1.0.4', '9.0.0', 'invalid-version')) {
            $refused = $previous -in @('9.0.0', 'invalid-version')
            foreach ($mode in @('passive', 'interactive')) {
                Set-ItemProperty $uninstallKey DisplayVersion $previous
                Remove-Item $confirmationLog -ErrorAction SilentlyContinue
                $env:RISUNEST_NSIS_CONFIRM = 'no'
                $modeArgs = if ($mode -eq 'passive') { @('/P', '/NS') } else { @('/NS') }
                Run-Installer $modeInstaller $modeArgs ([int]$refused)
                if ((Test-Path $confirmationLog) -ne ($mode -eq 'interactive' -and $refused)) { throw 'Unexpected version confirmation.' }
                if ($refused) {
                    if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne $previous) { throw 'Refusal changed the installation.' }
                    if ($mode -eq 'interactive') {
                        $relation = if ($previous -eq '9.0.0') { '-1' } else { 'unknown' }
                        if ((Get-Content $confirmationLog -Raw) -ne "confirm|$relation") { throw 'Wrong confirmation kind.' }
                        $env:RISUNEST_NSIS_CONFIRM = 'yes'
                        Run-Installer $modeInstaller $modeArgs 0
                    }
                }
            }
        }
        Remove-Item Env:RISUNEST_NSIS_CONFIRM
        Set-ItemProperty $uninstallKey DisplayVersion '1.0.3'
        $otherArch = Compile-Template 'wrong-architecture' (Set-Architecture $generated $(if ($arch -eq 'x64') { 'arm64' } else { 'x64' }))
        Run-Installer $otherArch @('/S', '/NS') 1
        if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne '1.0.3') { throw 'Architecture rejection occurred after changing the installation.' }
        # Substitute only native-architecture detection to exercise ARM64-host decisions on x64 CI.
        $armHost = $generated.Replace('${If} ${IsNativeAMD64}', '${If} 0 = 1').Replace('${ElseIf} ${IsNativeARM64}', '${ElseIf} 1 = 1')
        $x64OnArm = Compile-Template 'x64-on-arm64' (Set-Architecture $armHost 'x64')
        Run-Installer $x64OnArm @('/S', '/NS') 1
        # x64 builds installed on ARM64 before the architecture check keep receiving x64 updates.
        Run-Installer $x64OnArm @('/S', '/NS', '/UPDATE') 0
        if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne '1.0.4') { throw 'An x64 update of an existing ARM64 installation was refused.' }
        $armOnArm = Compile-Template 'arm64-on-arm64' (Set-Architecture $armHost 'arm64')
        Run-Installer $armOnArm @('/S', '/NS') 0
        if (!(Test-Path $keep)) { throw 'Architecture migration removed unowned data.' }
        # Reinstalling a registered installation whose folder was deleted recreates it in place.
        $deleted = Join-Path $testRoot 'deleted installation'
        Move-Item -LiteralPath $installed -Destination $deleted
        try {
            Run-Installer $fixtureInstaller @('/S', '/NS', "/D=$relocated") 1
            Run-Installer $modeInstaller @('/P', '/NS', "/D=$relocated") 1
            if (Test-Path $relocated) { throw 'A missing installation folder allowed relocation.' }
            Run-Installer $fixtureInstaller @('/S', '/NS') 0
            if (!(Test-Path (Join-Path $installed "$fixtureName.exe"))) { throw 'Reinstalling did not recreate the missing installation folder.' }
            if ((Get-Item $productKey).GetValue('') -ne $installed) { throw 'Reinstalling relocated the missing installation folder.' }
        } finally {
            if (Test-Path -LiteralPath $installed) { Remove-Item -LiteralPath $installed -Recurse -Force }
            Move-Item -LiteralPath $deleted -Destination $installed
        }
        $missingDrive = ([char[]]'ZYXWVUTSRQPONMLKJIHGFED' | Where-Object { !(Test-Path "$($_):\") } | Select-Object -First 1)
        if (!$missingDrive) { throw 'An unused drive is required for the missing-drive fixture.' }
        $unavailable = "$($missingDrive):\synthetic-missing-install"
        $savedLocation = (Get-ItemProperty $uninstallKey).InstallLocation
        $savedUninstall = (Get-ItemProperty $uninstallKey).UninstallString
        try {
            Set-Item $productKey $unavailable
            Set-ItemProperty $uninstallKey InstallLocation ('"' + $unavailable + '"')
            Set-ItemProperty $uninstallKey UninstallString ('"' + $unavailable + '\uninstall.exe"')
            Run-Installer $fixtureInstaller @('/S', '/NS') 1
            Run-Installer $fixtureInstaller @('/P', '/NS') 1
            $directoryLog = Join-Path $testRoot 'missing-drive-directory.txt'
            $directoryProbe = @"
Function MissingDriveGUIInit
  System::Call 'user32::GetWindowLongW(p `$HWNDPARENT, i -20) i .r0'
  IntOp `$0 `$0 | 0x08080080
  System::Call 'user32::SetWindowLongW(p `$HWNDPARENT, i -20, i r0)'
  System::Call 'user32::SetLayeredWindowAttributes(p `$HWNDPARENT, i 0, i 0, i 2)'
  System::Call 'user32::SetWindowPos(p `$HWNDPARENT, p 0, i -32000, i -32000, i 0, i 0, i 5)'
FunctionEnd
"@
            $directorySource = "!define MUI_CUSTOMFUNCTION_GUIINIT MissingDriveGUIInit`n" + $generated + "`n" + $directoryProbe
            $directorySource = $directorySource.Replace('Function SkipIfPassive', "Function SkipIfPassive`n  Abort")
            $directorySource = $directorySource.Replace('Function SkipExistingDirectory', @"
Function SkipExistingDirectory
  FileOpen `$0 "$directoryLog" w
  FileWrite `$0 "`$INSTDIR|`$ExistingNsis"
  FileClose `$0
  SetErrorLevel 0
  Quit
"@)
            $directoryInstaller = Compile-Template 'missing-drive-directory' $directorySource
            Run-Installer $directoryInstaller @('/NS') 0
            $expectedDefault = Join-Path $env:LOCALAPPDATA $product
            if ([IO.File]::ReadAllText($directoryLog) -ne "$expectedDefault|") { throw 'Missing-drive recovery did not reach folder selection with the default location.' }
            if ((Get-Item $productKey).GetValue('') -ne $unavailable) { throw 'Missing-drive refusal modified the registration.' }
        } finally {
            Set-Item $productKey $installed
            Set-ItemProperty $uninstallKey InstallLocation $savedLocation
            Set-ItemProperty $uninstallKey UninstallString $savedUninstall
        }
        $permissions = Get-Acl -LiteralPath $installed
        $denied = Get-Acl -LiteralPath $installed
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $denyCreate = [Security.AccessControl.FileSystemAccessRule]::new($sid, [Security.AccessControl.FileSystemRights]::CreateFiles, [Security.AccessControl.AccessControlType]::Deny)
        $denied.AddAccessRule($denyCreate)
        try {
            Set-Acl -LiteralPath $installed -AclObject $denied
            Run-Installer $fixtureInstaller @('/S', '/NS') 1
            if (!(Test-Path (Join-Path $installed "$fixtureName.exe"))) { throw 'Permission failure removed the old executable.' }
        } finally { Set-Acl -LiteralPath $installed -AclObject $permissions }
        Run-Installer (Join-Path $installed 'uninstall.exe') @('/S', "_?=$installed") 0
        if (Test-Path (Join-Path $installed "$fixtureName.exe")) { throw 'The updated installation could not be removed.' }
        if (!(Test-Path $keep)) { throw 'Removal deleted an unowned file.' }
        Run-Installer $x64OnArm @('/S', '/NS', '/UPDATE', "/D=$installed") 1
        if (Test-Path $uninstallKey) { throw 'An x64 update without an existing installation was accepted on ARM64.' }
        foreach ($displayLanguage in @(1042, 1033, 1041)) {
            $languageLog = Join-Path $testRoot "language-$displayLanguage.txt"
            $probe = @"
  FileOpen `$0 "$languageLog" a
  FileSeek `$0 0 END
  FileWriteUTF16LE `$0 "`$LANGUAGE|`$(InstallReinstall)`$\r`$\n"
  FileClose `$0
"@
            # NSIS reselects its language table after onInit, using the same exact/primary/fallback match as startup.
            $localized = $generated.Replace('Function .onInit', "Function .onInit`n  StrCpy `$LANGUAGE $displayLanguage").Replace('Function un.onInit', "Function un.onInit`n  StrCpy `$LANGUAGE $displayLanguage")
            $localized = $localized.Replace('Section Install', "Section Install`n$probe").Replace('Section Uninstall', "Section Uninstall`n$probe")
            $languageInstaller = Compile-Template "language-$displayLanguage" $localized
            Run-Installer $languageInstaller @('/S', '/NS', "/D=$installed") 0
            Run-Installer (Join-Path $installed 'uninstall.exe') @('/S', "_?=$installed") 0
            $expectedText = if ($displayLanguage -eq 1042) { '1042|다시 설치' } else { '1033|Reinstall' }
            $observed = @(Get-Content -LiteralPath $languageLog -Encoding Unicode)
            if ($observed.Count -ne 2 -or $observed[0] -ne $expectedText -or $observed[1] -ne $expectedText) {
                throw "Installer/uninstaller display language mismatch for $displayLanguage`: $observed"
            }
        }
        Write-Output 'Generated Tauri NSIS upgrade, architecture, relocation, version and removal tests passed.'

        if ($ReleasedBundle) {
            # The real manager allows 45 seconds to prepare, 100 for server health, and 30 to finish.
            # Keep process startup and file-copy overhead outside those product budgets.
            $releasedInstallerTimeoutSeconds = 240
            if (!(Test-Path -LiteralPath $ManagerBinary) -or !(Test-Path -LiteralPath $BackgroundBinary)) {
                throw 'Released-bundle verification requires both newly built manager executables.'
            }
            $released = Join-Path $testRoot 'released'
            Expand-Archive -LiteralPath $ReleasedBundle -DestinationPath $released
            $releasedVersion = [string](Get-Content (Join-Path $released 'risunest-sync-bundle.json') -Raw | ConvertFrom-Json).version
            if ($releasedVersion -notmatch '^(\d+)\.(\d+)\.(\d+)$') { throw "Unexpected released Sync version: $releasedVersion." }
            $nextVersion = "$($Matches[1]).$($Matches[2]).$([int]$Matches[3] + 1)"
            $syncInstall = Join-Path $testRoot 'released Sync installation'
            Copy-Item $released $syncInstall -Recurse
            Copy-Item $fake (Join-Path $syncInstall 'uninstall.exe')
            $local = Join-Path $testRoot 'synthetic-local'
            $roaming = Join-Path $testRoot 'synthetic-roaming'
            $data = Join-Path $local 'RisuNestSyncData'
            $oldManager = Join-Path $syncInstall 'risunest-sync-manager.exe'
            $server = Join-Path $syncInstall 'risunest-sync-server.exe'
            $priorLocal = $env:LOCALAPPDATA
            $priorRoaming = $env:APPDATA
            $env:LOCALAPPDATA = $local
            $env:APPDATA = $roaming
            function Invoke-Manager([string[]]$arguments) {
                $output = & $oldManager --data-dir $data --server $server @arguments 2>&1
                if ($LASTEXITCODE -ne 0) { throw "Synthetic manager failed ($arguments): $output" }
            }
            try {
                & $fake synthetic-profile-check $local $roaming
                if ($LASTEXITCODE -ne 0) { throw 'Child process did not inherit the synthetic profile roots.' }
                & $server init --data-dir $data | Out-Null
                if ($LASTEXITCODE -ne 0) { throw 'Released server initialization failed.' }
                $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
                $listener.Start()
                $port = $listener.LocalEndpoint.Port
                $listener.Stop()
                & $server network configure --data-dir $data --listen "127.0.0.1:$port" | Out-Null
                if ($LASTEXITCODE -ne 0) { throw 'Released server listener configuration failed.' }
                Invoke-Manager @('update', 'policy', 'off')
                Invoke-Manager @('start')
                $before = (Get-FileHash (Join-Path $data 'risunest-sync-instance.json')).Hash
                New-Item -Path $productKey -Force | Out-Null
                Set-Item -Path $productKey -Value $syncInstall
                New-Item -Path $uninstallKey -Force | Out-Null
                Set-ItemProperty $uninstallKey UninstallString "`"$syncInstall\uninstall.exe`""
                Set-ItemProperty $uninstallKey DisplayVersion $releasedVersion

                $next = Join-Path $project 'next-bundle'
                Copy-Item $released $next -Recurse
                Copy-Item $ManagerBinary (Join-Path $next 'risunest-sync-manager.exe')
                Copy-Item $BackgroundBinary (Join-Path $next 'risunest-sync-manager-background.exe')
                $markerPath = Join-Path $next 'risunest-sync-bundle.json'
                $marker = Get-Content $markerPath -Raw | ConvertFrom-Json
                $marker.files += "$fixtureName.exe"
                $marker | ConvertTo-Json -Depth 10 | Set-Content $markerPath
                $syncHook = [IO.File]::ReadAllText((Join-Path $repo 'server/manager/install/windows.nsh'))
                $common = Join-Path $repo 'src-tauri/windows-common.nsh'
                $syncHook = $syncHook.Replace('!include "${__FILEDIR__}\..\..\..\src-tauri\windows-common.nsh"', "!include `"$common`"")
                $syncHook = $syncHook.Replace('$LOCALAPPDATA', $local)
                $detailLog = Join-Path $testRoot 'sync-details.txt'
                foreach ($message in @('SyncResumeDataKept', 'SyncObsoleteSkipped')) {
                    $detail = 'DetailPrint "$(' + $message + ')"'
                    $capture = "FileOpen `$0 `"$detailLog`" a`nFileSeek `$0 0 END`nFileWriteUTF16LE `$0 `"`$($message)`$\r`$\n`"`nFileClose `$0"
                    $syncHook = $syncHook.Replace($detail, "$detail`n$capture")
                }
                $syncHook = "!define SYNC_INSTALLER_MANAGER `"$ManagerBinary`"`n!define SYNC_INSTALLER_BACKGROUND `"$BackgroundBinary`"`n" + $syncHook
                $syncHookPath = Join-Path $project 'sync-hooks.nsh'
                $syncHook | Set-Content -Encoding utf8 $syncHookPath
                $config.version = $nextVersion
                $config.bundle.windows.nsis.installerHooks = $syncHookPath
                $config.bundle.windows.nsis.compression = 'zlib'
                $resources = @{}
                foreach ($file in Get-ChildItem $next -File) { $resources[$file.FullName] = $file.Name }
                $config.bundle.resources = $resources
                $config | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $project 'tauri.conf.json')
                Push-Location $project
                try {
                    $env:CARGO_TARGET_DIR = $sharedTarget
                    & node (Join-Path $repo 'node_modules/@tauri-apps/cli/tauri.js') bundle --ci --no-sign --no-binary-patching
                    if ($LASTEXITCODE -ne 0) { throw 'Sync template generation failed.' }
                } finally { $env:CARGO_TARGET_DIR = $priorTarget; Pop-Location }
                $syncGenerated = [IO.File]::ReadAllText((Join-Path $generatedRoot 'installer.nsi'))
                $syncGenerated = $syncGenerated.Replace('SetCompressor /SOLID "lzma"', 'SetCompressor zlib')
                $failureFile = Join-Path $testRoot 'failure.txt'
                $syncGenerated += @"

Function un.onUninstFailed
  FileOpen `$0 "$failureFile" w
  FileWriteUTF16LE `$0 "`$SyncFailureMessage"
  FileClose `$0
FunctionEnd
"@
                $syncFixture = Compile-Template 'released-upgrade' $syncGenerated
                # Archive updates leave DisplayVersion behind, so setup compares against the installed bundle marker.
                $installedMarker = Join-Path $syncInstall 'risunest-sync-bundle.json'
                $markerText = [IO.File]::ReadAllText($installedMarker)
                [IO.File]::WriteAllText($installedMarker, ([regex]'"version":\s*"[^"]*"').Replace($markerText, '"version": "9.0.0"', 1))
                try {
                    Run-Installer $syncFixture @('/S', '/NS') 1 $releasedInstallerTimeoutSeconds
                } finally { [IO.File]::WriteAllText($installedMarker, $markerText) }
                if ((Get-ItemProperty $uninstallKey).DisplayVersion -ne $releasedVersion) { throw 'A refused downgrade changed the installation.' }
                Run-Installer $syncFixture @('/S', '/NS') 0 $releasedInstallerTimeoutSeconds
                if ((Get-FileHash (Join-Path $data 'risunest-sync-instance.json')).Hash -ne $before) { throw 'Upgrade changed instance ownership.' }
                Invoke-Manager @('status')
                Invoke-Manager @('uninstall', '--dry-run')
                Run-Installer (Join-Path $syncInstall 'uninstall.exe') @('/S', "_?=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                if (Test-Path $oldManager) { throw 'Updated released bundle could not be removed normally.' }
                if (!(Test-Path (Join-Path $data 'metadata.sqlite'))) { throw 'Normal removal deleted server data.' }
                Write-Output "Released Sync $releasedVersion running-server upgrade and normal removal passed."
                Run-Installer $syncFixture @('/S', '/NS', "/D=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                Invoke-Manager @('installer', 'forget-removal')
                $unowned = Join-Path $syncInstall 'user-owned.txt'
                'synthetic unowned file' | Set-Content $unowned
                Invoke-Manager @('uninstall', '--dry-run', '--force', '--delete-data')
                Run-Installer (Join-Path $syncInstall 'uninstall.exe') @('/S', '/FORCE', '/DELETEAPPDATA', "_?=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                if (Test-Path $oldManager) { throw 'Forced removal left an owned manager.' }
                if (!(Test-Path $unowned) -or !(Test-Path (Join-Path $data 'metadata.sqlite'))) { throw 'Forced removal deleted unowned files or unregistered data.' }
                Write-Output 'Generated Sync forced removal preserves unowned files and unregistered data.'
                Remove-Item -LiteralPath $unowned
                # Data deletion runs right after the installer guard is released, and the manager alone
                # decides which GUI profiles go. The template's own profile folder must survive.
                Run-Installer $syncFixture @('/S', '/NS', "/D=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                $appProfile = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) "test.risunest.$fixtureName"
                New-Item -ItemType Directory -Path $appProfile | Out-Null
                'synthetic profile' | Set-Content (Join-Path $appProfile 'keep.txt')
                Run-Installer (Join-Path $syncInstall 'uninstall.exe') @('/S', '/DELETEAPPDATA', "_?=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                if (Test-Path $oldManager) { throw 'Removal with data deletion left program files.' }
                if (Test-Path $data) { throw 'Removal with data deletion kept the registered server data.' }
                if (!(Test-Path (Join-Path $appProfile 'keep.txt'))) { throw 'Sync removal deleted a profile folder the manager did not choose.' }
                Write-Output 'Generated Sync removal deletes registered data and leaves profile decisions to the manager.'
                foreach ($removalMode in @('normal', 'force')) {
                    if (!(Test-Path $data)) {
                        $seedServer = Join-Path $released 'risunest-sync-server.exe'
                        & $seedServer init --data-dir $data | Out-Null
                        if ($LASTEXITCODE -ne 0) { throw 'Synthetic data initialization failed.' }
                        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
                        $listener.Start()
                        $port = $listener.LocalEndpoint.Port
                        $listener.Stop()
                        & $seedServer network configure --data-dir $data --listen "127.0.0.1:$port" | Out-Null
                        if ($LASTEXITCODE -ne 0) { throw 'Synthetic listener configuration failed.' }
                        & $ManagerBinary --data-dir $data --server $server update policy off | Out-Null
                        if ($LASTEXITCODE -ne 0) { throw 'Synthetic update policy configuration failed.' }
                    }
                    Run-Installer $syncFixture @('/S', '/NS', "/D=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                    Invoke-Manager @('uninstall', 'lock-held')
                    if ($removalMode -eq 'force') { Invoke-Manager @('installer', 'forget-removal') }
                    $syntheticData = [IO.Path]::GetFullPath($data)
                    if (!$syntheticData.StartsWith($testRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
                        throw 'Refusing to remove data outside the synthetic test profile.'
                    }
                    Remove-Item -LiteralPath $syntheticData -Recurse -Force
                    $removeArgs = if ($removalMode -eq 'force') { @('/S', '/FORCE', "_?=$syncInstall") } else { @('/S', "_?=$syncInstall") }
                    Run-Installer (Join-Path $syncInstall 'uninstall.exe') $removeArgs 0 $releasedInstallerTimeoutSeconds
                    if (Test-Path $oldManager) { throw "$removalMode removal without a data directory left program files." }
                    if (Test-Path $data) { throw "$removalMode removal recreated the absent data directory." }
                }
                Write-Output 'Normal and forced removal do not recreate an absent data directory.'

                foreach ($alreadyDeleted in @($false, $true)) {
                    foreach ($resumeState in @('pending', 'forgotten', 'complete')) {
                        Run-Installer $syncFixture @('/S', '/NS', "/D=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                        Invoke-Manager @('uninstall', 'lock-held')
                        Invoke-Manager @('installer', 'forget-removal')
                        $resumeData = Join-Path $testRoot 'custom data 경로'
                        if (!(Test-Path (Join-Path $resumeData 'metadata.sqlite'))) {
                            & $server init --data-dir $resumeData | Out-Null
                            if ($LASTEXITCODE -ne 0) { throw 'Could not initialize custom synthetic data.' }
                        }
                        & $ManagerBinary --data-dir $resumeData --server $server installer register
                        if ($LASTEXITCODE -ne 0) { throw 'Could not register custom synthetic data.' }
                        if ($alreadyDeleted) {
                            & $ManagerBinary --data-dir $resumeData --server $server installer delete-data
                            if ($LASTEXITCODE -ne 0) { throw 'Could not delete registered synthetic data before retry.' }
                        }
                        & $ManagerBinary --data-dir $resumeData --server $server installer remove-files-resumable normal
                        if ($LASTEXITCODE -ne 0) { throw 'Could not create synthetic pending removal.' }
                        if ($resumeState -ne 'pending') {
                            & $ManagerBinary --data-dir $resumeData --server $server installer forget-removal-resumable
                            if ($LASTEXITCODE -ne 0) { throw 'Could not create synthetic forgotten removal.' }
                        }
                        if ($resumeState -eq 'complete') {
                            & $ManagerBinary --data-dir $resumeData --server $server installer commit-removal
                            if ($LASTEXITCODE -ne 0) { throw 'Could not create synthetic completed removal.' }
                        }
                        Remove-Item -LiteralPath $detailLog -ErrorAction SilentlyContinue
                        Run-Installer (Join-Path $syncInstall 'uninstall.exe') @('/S', '/DELETEAPPDATA', "_?=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                        $kept = !$alreadyDeleted -and $resumeState -ne 'pending'
                        if ((Test-Path $resumeData) -ne $kept) { throw "Resumed $resumeState removal ignored the data ownership state." }
                        $detail = if (Test-Path $detailLog) { Get-Content -LiteralPath $detailLog -Raw -Encoding Unicode } else { '' }
                        if (($detail -match 'Server data could not be deleted|서버 데이터를 삭제하지 못했습니다') -ne $kept) { throw 'Resumed removal reported the wrong data deletion outcome.' }
                        if ($kept) {
                            if (!$detail.Contains($resumeData) -or $detail.Contains('\\?\') -or ($detail -notmatch 'Server data could not be deleted|서버 데이터를 삭제하지 못했습니다')) { throw 'Resumed removal did not report kept custom data and its path.' }
                        }
                    }
                }
                Run-Installer $syncFixture @('/S', '/NS', "/D=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                'invalid' | Set-Content -LiteralPath $installedMarker
                Remove-Item -LiteralPath $detailLog -ErrorAction SilentlyContinue
                Run-Installer $syncFixture @('/S', '/NS') 0 $releasedInstallerTimeoutSeconds
                $skipDetail = Get-Content -LiteralPath $detailLog -Raw -Encoding Unicode
                if ($skipDetail -notmatch 'Obsolete-file cleanup was skipped|이전 파일 제거를 건너뛰었습니다') { throw 'Corrupt marker repair did not report skipped obsolete-file cleanup.' }
                Run-Installer (Join-Path $syncInstall 'uninstall.exe') @('/S', "_?=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                Write-Output 'Resumed removal honors registered data deletion, reports kept data after forgetting, and repair skips corrupt previous markers.'

                # Archive updates retain the installed uninstaller. Build that frozen script,
                # then apply the archive file set without replacing uninstall.exe.
                # Frozen removal contracts start at 1.0.4, the first release whose uninstaller runs
                # `installer remove-files`. A same-version 1.0.4 rehearsal starts from an earlier release.
                $frozenCases = @()
                if ([version]$releasedVersion -ge [version]'1.0.4') { $frozenCases += @{ Version = $releasedVersion; Directory = $released; Ref = "sync-v$releasedVersion" } }
                if ($releasedVersion -ne '1.0.4') {
                    if (!$LegacyReleasedBundle) { throw 'The fixed Sync 1.0.4 compatibility archive is required alongside the previous release.' }
                    $legacy = Join-Path $testRoot 'legacy-1.0.4'
                    Expand-Archive -LiteralPath $LegacyReleasedBundle -DestinationPath $legacy
                    if ((Get-Content (Join-Path $legacy 'risunest-sync-bundle.json') -Raw | ConvertFrom-Json).version -ne '1.0.4') { throw 'The legacy compatibility archive must be Sync 1.0.4.' }
                    $frozenCases += @{ Version = '1.0.4'; Directory = $legacy; Ref = 'sync-v1.0.4' }
                }
                # The current hook exercises the resumable retained-uninstaller contract before its next release.
                $frozenCases += @{ Version = $nextVersion; Directory = $next; Ref = $null }
                foreach ($frozenCase in $frozenCases) {
                    $frozenVersion = $frozenCase.Version
                    $frozenBundle = $frozenCase.Directory
                    $frozenName = if ($frozenCase.Ref) { "frozen-$frozenVersion" } else { 'current-resumable' }
                    function Read-FrozenSource([string]$path) {
                        if (!$frozenCase.Ref) { return [IO.File]::ReadAllText((Join-Path $repo $path)) }
                        $source = (& git -C $repo show "$($frozenCase.Ref):$path") -join "`n"
                        if ($LASTEXITCODE -ne 0) { throw "The $($frozenCase.Ref) source is required: $path" }
                        return $source
                    }
                    $releasedHook = Read-FrozenSource 'server/manager/install/windows.nsh'
                    $resumable = $releasedHook -match 'installer remove-files-resumable\b'
                    if (!$resumable -and $releasedHook -notmatch 'installer remove-files ') { throw 'Unknown frozen removal command contract.' }
                    # The current CLI's language files renamed choowHowToInstall; released templates still name it.
                    $releasedTemplate = (Read-FrozenSource 'src-tauri/windows-installer.nsi').Replace('$(choowHowToInstall)', '$(chooseHowToInstall)')
                    $releasedCommon = Read-FrozenSource 'src-tauri/windows-common.nsh'
                    $releasedCommonPath = Join-Path $project 'released-common.nsh'
                    $releasedCommon | Set-Content -Encoding utf8 $releasedCommonPath
                    $releasedHook = $releasedHook.Replace('!include "${__FILEDIR__}\..\..\..\src-tauri\windows-common.nsh"', "!include `"$releasedCommonPath`"").Replace('$LOCALAPPDATA', $local)
                    $releasedHook = "!define SYNC_INSTALLER_MANAGER `"$(Join-Path $frozenBundle 'risunest-sync-manager.exe')`"`n!define SYNC_INSTALLER_BACKGROUND `"$(Join-Path $frozenBundle 'risunest-sync-manager-background.exe')`"`n" + $releasedHook
                    $releasedHookPath = Join-Path $project 'released-hooks.nsh'
                    $releasedTemplatePath = Join-Path $project 'released-template.nsi'
                    $releasedHook | Set-Content -Encoding utf8 $releasedHookPath
                    $releasedTemplate | Set-Content -Encoding utf8 $releasedTemplatePath
                    $config.version = $frozenVersion
                    $config.bundle.windows.nsis.installerHooks = $releasedHookPath
                    $config.bundle.windows.nsis.template = $releasedTemplatePath
                    $resources = @{}
                    foreach ($file in Get-ChildItem $frozenBundle -File) { $resources[$file.FullName] = $file.Name }
                    $config.bundle.resources = $resources
                    $config | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $project 'tauri.conf.json')
                    Push-Location $project
                    try {
                        $env:CARGO_TARGET_DIR = $sharedTarget
                        & node (Join-Path $repo 'node_modules/@tauri-apps/cli/tauri.js') bundle --ci --no-sign --no-binary-patching
                        if ($LASTEXITCODE -ne 0) { throw 'Frozen Sync installer generation failed.' }
                    } finally { $env:CARGO_TARGET_DIR = $priorTarget; Pop-Location }
                    $frozenSource = [IO.File]::ReadAllText((Join-Path $generatedRoot 'installer.nsi'))
                    $frozenFixture = Compile-Template $frozenName $frozenSource
                    $releasedManager = Join-Path $frozenBundle 'risunest-sync-manager.exe'
                    if (!$resumable) {
                        foreach ($shape in @(@('remove-files-resumable', 'normal'), @('forget-removal-resumable'))) {
                            & $releasedManager --data-dir $data --server $server installer @shape 2>&1 | Out-Null
                            if ($LASTEXITCODE -eq 0) { throw "The legacy manager accepted a new command shape: $shape" }
                        }
                    }
                    foreach ($retryMode in @('normal', 'force')) {
                        Run-Installer $frozenFixture @('/S', '/NS', "/D=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                        Invoke-Manager @('uninstall', 'lock-held')
                        $frozenUninstaller = Join-Path $syncInstall 'uninstall.exe'
                        $frozenHash = (Get-FileHash $frozenUninstaller).Hash
                        $instanceName = (Get-Content (Join-Path $data 'risunest-sync-instance.json') -Raw | ConvertFrom-Json).instance
                        $instanceRecord = Join-Path $local ".risunest-sync-instances/$instanceName.json"
                        foreach ($file in Get-ChildItem $next -File) { Copy-ArchiveFile $file.FullName (Join-Path $syncInstall $file.Name) }
                        if ((Get-FileHash $frozenUninstaller).Hash -ne $frozenHash) { throw 'Archive application replaced the frozen uninstaller.' }
                        # A final-file lock leaves the released entry points available for retry.
                        $heldMarker = [IO.File]::Open($oldManager, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
                        try {
                            Run-Installer $frozenUninstaller @('/S', "_?=$syncInstall") 1 $releasedInstallerTimeoutSeconds
                            if (!(Test-Path $oldManager)) { throw 'Released retry lost its manager before the final-file failure.' }
                            $retry = Join-Path $syncInstall '.risunest-sync-removal'
                            if ($resumable) {
                                if (!(Test-Path (Join-Path $retry 'receipt.json')) -or !(Test-Path (Join-Path $retry 'manager.exe'))) { throw 'A resumable frozen command lost its retry entry points.' }
                            } elseif (Test-Path $retry) { throw 'A legacy frozen command created resumable artifacts.' }
                        } finally { $heldMarker.Dispose() }
                        $retryArgs = if ($retryMode -eq 'force') { @('/S', '/FORCE', '/DELETEAPPDATA') } else { @('/S') }
                        Run-Installer $frozenUninstaller $retryArgs 0 $releasedInstallerTimeoutSeconds
                        $removedDeadline = [DateTime]::UtcNow.AddSeconds(30)
                        while (((Test-Path $syncInstall) -or (Test-Path $instanceRecord)) -and [DateTime]::UtcNow -lt $removedDeadline) { Start-Sleep -Milliseconds 50 }
                        if (Test-Path $syncInstall) { throw "Frozen $retryMode removal left the installation directory." }
                        if (Test-Path $instanceRecord) { throw "Frozen $retryMode removal left its registration." }
                        if ((Test-Path $data) -ne ($retryMode -eq 'normal')) { throw 'Frozen removal did not honor its data choice.' }
                        Run-Installer $syncFixture @('/S', '/NS', "/D=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                        Run-Installer (Join-Path $syncInstall 'uninstall.exe') @('/S', "_?=$syncInstall") 0 $releasedInstallerTimeoutSeconds
                    }
                    Write-Output "$frozenName uninstallers remove archive-updated installs, retry normally and with force, and permit reinstall (resumable=$resumable)."
                }
            } finally {
                if (Test-Path $oldManager) {
                    & $ManagerBinary --data-dir $data --server $server uninstall lock-held | Out-Null
                    & $ManagerBinary --data-dir $data --server $server installer forget-removal | Out-Null
                }
                $env:LOCALAPPDATA = $priorLocal
                $env:APPDATA = $priorRoaming
                if ($appProfile -and (Test-Path -LiteralPath $appProfile)) { Remove-Item -LiteralPath $appProfile -Recurse -Force }
            }
        }
    } finally {
        foreach ($key in @($uninstallKey, $productKey)) {
            if (Test-Path -LiteralPath $key) { Remove-Item -LiteralPath $key -Recurse }
        }
        Remove-Item -LiteralPath $fixtureBinary -ErrorAction SilentlyContinue
        Get-ChildItem -LiteralPath $bundleRoot -Filter "${product}_*-setup.exe" -ErrorAction SilentlyContinue | Remove-Item -ErrorAction SilentlyContinue
    }
}
finally {
    Remove-Item Env:RISUNEST_NSIS_TEST_LOG -ErrorAction SilentlyContinue
    Remove-Item Env:RISUNEST_NSIS_FAIL_AUTOSTART -ErrorAction SilentlyContinue
    Remove-Item Env:RISUNEST_NSIS_CONFIRM -ErrorAction SilentlyContinue
    $resolved = [IO.Path]::GetFullPath($testRoot)
    if (!$resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to remove a test directory outside the system temp directory."
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force -ErrorAction SilentlyContinue
}
