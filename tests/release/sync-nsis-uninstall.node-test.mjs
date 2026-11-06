import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import test from 'node:test';

const compiler = process.env.NSIS_MAKENSIS ?? join(process.env.LOCALAPPDATA ?? '', 'tauri', 'NSIS', 'makensis.exe');
const skip = process.platform !== 'win32' || !existsSync(compiler);
if (process.env.RISUNEST_REQUIRE_NSIS === '1' && skip) throw new Error('NSIS compiler is required');
const installedExecutable = '$INSTDIR\\${MAINBINARYNAME}.exe';
// NSIS names its plug-in directory after the tick count, so NSIS processes that other test files
// start in the same TEMP at the same moment can share that directory and delete each other's files.
function runNsis(directory, executable, args, { env = process.env } = {}) {
  const temp = join(directory, 'nsis-temp');
  mkdirSync(temp, { recursive: true });
  return spawnSync(executable, args, { env: { ...env, TEMP: temp, TMP: temp }, timeout: 30000, windowsHide: true });
}
test('Sync removal hooks compile in the uninstaller context', { skip }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-sync-nsis-'));
  try {
    const script = join(directory, 'test.nsi');
    const hookSource = readFileSync('server/manager/install/windows.nsh', 'utf8');
    const displayBlock = hookSource.slice(hookSource.indexOf('    StrCpy $SyncDisplayDataPath $R9'), hookSource.indexOf('    DetailPrint "$(SyncResumeDataKept)"'));
    assert.ok(displayBlock.length > 0);
    const paths = [String.raw`C:\data 경로`, String.raw`\\?\C:\data 경로`, String.raw`\\?\UNC\server\share\data`, String.raw`\\server\share\data`];
    writeFileSync(script, '\ufeff' + String.raw`
Unicode true
RequestExecutionLevel user
OutFile "${join(directory, 'test.exe')}"
!include LogicLib.nsh
!include FileFunc.nsh
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\Korean.nlf"
!define MAINBINARYNAME "SyntheticSync"
!define PRODUCTNAME "Synthetic Sync"
!macro CheckIfAppIsRunning executable product
!macroend
Var UpdateMode
Var DeleteAppDataCheckboxState
!include "${resolve('server/manager/install/windows.nsh')}"
Section
${paths.map((path, index) => `StrCpy $R9 "${q(path)}"
${displayBlock}
FileOpen $0 "${q(join(directory, `display-${index}`))}" w
FileWriteUTF16LE $0 "$SyncDisplayDataPath$\\r$\\n$R9"
FileClose $0`).join('\n')}
SectionEnd
Section Uninstall
!insertmacro NSIS_HOOK_PREUNINSTALL
!insertmacro NSIS_HOOK_POSTUNINSTALL
SectionEnd
`);
    const result = spawnSync(compiler, ['/V2', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.equal(runNsis(directory, join(directory, 'test.exe'), ['/S']).status, 0);
    const displayed = [String.raw`C:\data 경로`, String.raw`C:\data 경로`, String.raw`\\server\share\data`, String.raw`\\server\share\data`];
    for (let index = 0; index < paths.length; index++) {
      assert.equal(readFileSync(join(directory, `display-${index}`), 'utf16le'), `${displayed[index]}\r\n${paths[index]}`);
    }
  } finally { rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});
import { readFileSync, mkdirSync, copyFileSync, symlinkSync, unlinkSync } from 'node:fs';

test('released archive application waits only for a transient sharing violation', { skip: process.platform !== 'win32' }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-archive-copy-'));
  try {
    const script = join(directory, 'copy.ps1');
    writeFileSync(script, String.raw`
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path (Get-Location) 'server/manager/install/windows.test.ps1'), [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'The installer harness could not be parsed.' }
$helper = $ast.Find({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Copy-ArchiveFile' }, $true)
if (!$helper) { throw 'The archive copy helper is missing.' }
Invoke-Expression $helper.Extent.Text
$copyStep = $ast.Find({ param($node) $node -is [Management.Automation.Language.ForEachStatementAst] -and $node.Condition.Extent.Text -eq 'Get-ChildItem $next -File' -and $node.Body.Extent.Text -match '\bCopy-(ArchiveFile|Item)\b' }, $true)
if (!$copyStep) { throw 'The archive application step is missing.' }
$script:attempts = 0
$script:sharingFailures = 0
$script:releaseOnFailure = $false
function Copy-Item {
  param([string]$LiteralPath, [string]$Destination, [switch]$Force, [string]$ErrorAction)
  $script:attempts++
  try { Microsoft.PowerShell.Management\Copy-Item @PSBoundParameters }
  catch {
    if ($_.Exception -is [IO.IOException] -and ($_.Exception.HResult -band 0xffff) -eq 32) {
      $script:sharingFailures++
      if ($script:releaseOnFailure) { $script:held.Dispose() }
    }
    throw
  }
}
$next = New-Item -ItemType Directory (Join-Path $PSScriptRoot 'next')
$syncInstall = New-Item -ItemType Directory (Join-Path $PSScriptRoot 'installed')
$source = Join-Path $next 'server.bin'
$destination = Join-Path $syncInstall 'server.bin'
[IO.File]::WriteAllText($source, 'replacement')
[IO.File]::WriteAllText($destination, 'original')
$script:held = [IO.File]::Open($destination, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
$script:releaseOnFailure = $true
try { Invoke-Expression $copyStep.Extent.Text }
finally { $script:held.Dispose() }
$replaced = [IO.File]::ReadAllText($destination)
$successAttempts = $script:attempts
$script:releaseOnFailure = $false
$script:held = [IO.File]::Open($destination, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
$before = $script:attempts
$timeoutCode = $null
try { Copy-ArchiveFile $source $destination -timeoutMilliseconds 0 }
catch { $timeoutCode = $_.Exception.HResult -band 0xffff }
finally { $script:held.Dispose() }
$timeoutAttempts = $script:attempts - $before
$before = $script:attempts
$missingFailed = $false
try { Copy-ArchiveFile (Join-Path $PSScriptRoot 'missing.bin') $destination }
catch { $missingFailed = $true }
@{
  replaced=$replaced; successAttempts=$successAttempts; sharingFailures=$script:sharingFailures
  timeoutCode=$timeoutCode; timeoutAttempts=$timeoutAttempts
  missingFailed=$missingFailed; missingAttempts=($script:attempts - $before)
} | ConvertTo-Json -Compress
`);
    const result = spawnSync('pwsh', ['-NoProfile', '-File', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.deepEqual(JSON.parse(result.stdout), {
      replaced: 'replacement', successAttempts: 2, sharingFailures: 2,
      timeoutCode: 32, timeoutAttempts: 1, missingFailed: true, missingAttempts: 1,
    });
  } finally { rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});

test('released uninstall validation selects exact previous sources and keeps the legacy baseline', { skip: process.platform !== 'win32' }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-frozen-selection-'));
  try {
    const source = readFileSync('server/manager/install/windows.test.ps1', 'utf8');
    const start = source.indexOf('                $frozenCases =');
    const end = source.indexOf('                foreach ($frozenCase in $frozenCases)', start);
    assert.ok(start >= 0 && end > start, 'frozen source selection must precede fixture execution');
    const selection = source.slice(start, end);
    const script = join(directory, 'select.ps1');
    writeFileSync(script, `
$ErrorActionPreference = 'Stop'
$directory = $PSScriptRoot
$released = 'previous-bundle'
$next = 'current-bundle'
$fixture = New-Item -ItemType Directory (Join-Path $directory 'legacy')
'{"version":"1.0.4"}' | Set-Content (Join-Path $fixture 'risunest-sync-bundle.json')
$LegacyReleasedBundle = Join-Path $directory 'legacy.zip'
Compress-Archive -Path (Join-Path $fixture '*') -DestinationPath $LegacyReleasedBundle
$result = @()
foreach ($releasedVersion in @('1.0.3', '1.0.4', '1.0.5')) {
  $nextVersion = "1.0.$([int]$releasedVersion.Split('.')[2] + 1)"
  $testRoot = New-Item -ItemType Directory (Join-Path $directory $releasedVersion)
${selection}
  $result += ,@($frozenCases | ForEach-Object { @{ version=$_.Version; source=$_.Ref; directory=$_.Directory.ToString() } })
}
ConvertTo-Json -Depth 8 -InputObject $result
`);
    const result = spawnSync('powershell.exe', ['-NoProfile', '-File', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    const [sameVersionRehearsal, legacyPrevious, resumablePrevious] = JSON.parse(result.stdout);
    assert.deepEqual(sameVersionRehearsal.map(item => [item.version, item.source]), [['1.0.4', 'sync-v1.0.4'], ['1.0.4', null]]);
    assert.deepEqual(legacyPrevious.map(item => [item.version, item.source]), [['1.0.4', 'sync-v1.0.4'], ['1.0.5', null]]);
    assert.deepEqual(resumablePrevious.map(item => [item.version, item.source]), [['1.0.5', 'sync-v1.0.5'], ['1.0.4', 'sync-v1.0.4'], ['1.0.6', null]]);
    assert.equal(resumablePrevious[0].directory, 'previous-bundle');
    assert.equal(resumablePrevious[2].directory, 'current-bundle');
    assert.equal(existsSync(join(resumablePrevious[1].directory, 'risunest-sync-bundle.json')), true);
  } finally { rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});

const q = value => value.replaceAll('$', '$$').replaceAll('"', '$\\"');
function buildManagerStub(directory) {
  const stubSource = join(directory, 'manager.rs');
  writeFileSync(stubSource, String.raw`
use std::{env, fs, io::Write};
fn main() {
 let initial=env::args().skip(1).collect::<Vec<_>>();
 if initial.first().is_some_and(|arg| arg=="hold-file") {
  use std::os::windows::fs::OpenOptionsExt;
  let handle=fs::OpenOptions::new().read(true).share_mode(3).open(&initial[1]).unwrap();
  fs::write(&initial[2], b"ready").unwrap();
  let deadline=std::time::Instant::now()+std::time::Duration::from_secs(30);
  while !std::path::Path::new(&initial[3]).exists() && std::time::Instant::now()<deadline {std::thread::sleep(std::time::Duration::from_millis(10));}
  drop(handle); fs::remove_file(&initial[2]).unwrap(); return;
 }
 let mut root=env::current_exe().unwrap().parent().unwrap().to_path_buf();
 let mut values=env::args().skip(1).collect::<Vec<_>>();
 let data=if values.first().is_some_and(|v| v=="--data-dir") {let data=std::path::PathBuf::from(values[1].clone());values.drain(0..2);data} else {root.join("local/RisuNestSyncData")};
 if values.first().is_some_and(|v| v=="--server") {root=std::path::PathBuf::from(&values[1]).parent().unwrap().to_owned();values.drain(0..2);}
 if values.iter().any(|v| v=="forget-removal-resumable") {return;}
 assert!(std::path::Path::new(data.to_str().unwrap().strip_prefix(r"\\?\").unwrap_or(data.to_str().unwrap())).starts_with(&root));
 let args=values.join(" ").trim().to_owned();
 if args=="installer removal-check normal delete" {println!("delete-data");}
 if args=="installer removal-check normal preserve" {println!("keep-data");}
 if args=="installer removal-check force delete" {println!("keep-data");}
 let mut log=fs::OpenOptions::new().create(true).append(true).open(root.join("calls")).unwrap();
 writeln!(log,"{args}: {}",data.display()).unwrap();
 if values.first().is_some_and(|v| v=="installer") && values.get(1).is_some_and(|v| v=="prepare") {
  assert_eq!(values.len(),3);
  let owner=values[2].parse::<u32>().unwrap();
  assert!(owner>0 && owner!=std::process::id());
  println!("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
 }
 if args=="installer preserve-files" {
  if fs::read(root.join("risunest-sync-bundle.json")).unwrap()==b"invalid" {println!("skipped");return;}
  let baseline=root.join("risunest-sync-bundle.previous.json");
  if !baseline.exists() {fs::copy(root.join("risunest-sync-bundle.json"), baseline).unwrap();}
 }
 if args=="installer delete-data" {
  if root.join("fail").exists() {std::process::exit(9);}
  if data.exists() {fs::remove_dir_all(&data).unwrap();}
 }
 if values.first().is_some_and(|v| v=="installer") && values.get(1).is_some_and(|v| v=="remove-files-resumable") && root.join("fail-remove").exists() {
  fs::remove_file(root.join("fail-remove")).unwrap();
  println!("removal-file-failed: synthetic");
  std::process::exit(7);
 }
 if args.starts_with("installer resume-removal ") && root.join("fail-resume").exists() {println!("synthetic-resume-failure");std::process::exit(7);}
 if args=="installer commit-removal" || args.starts_with("installer resume-removal ") {
  if args=="installer resume-removal delete" {
   if data.join("risunest-sync-instance.json").exists() {fs::remove_dir_all(&data).unwrap();}
   else {println!("keep-data");}
  }
  let retry=root.join(".risunest-sync-removal"); fs::create_dir_all(&retry).unwrap();
  fs::write(retry.join("complete"), "true").unwrap();
 }
 if args=="installer register" && root.join("fail-register").exists() {
  fs::remove_file(root.join("fail-register")).unwrap();
  println!("synthetic-register-failure");
  std::process::exit(7);
 }
 if values.len()==3 && values[0]=="installer" && values[1]=="obsolete-files" {
  fs::write(root.join("baseline-received"), fs::read(&values[2]).unwrap_or_else(|_| b"missing".to_vec())).unwrap();
 }
}
`);
  const binary = join(directory, 'manager.exe');
  const built = spawnSync('rustc', ['--edition=2021', stubSource, '-o', binary], { encoding: 'utf8', timeout: 60000, windowsHide: true });
  assert.equal(built.status, 0, built.stderr);
  return binary;
}
function waitForFixture(condition) {
  const deadline = Date.now() + 5000;
  while (!condition()) {
    assert.ok(Date.now() < deadline, 'synthetic file-lock boundary became ready');
    Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
  }
}
function holdDeletion(binary, path) {
  const signals = mkdtempSync(join(dirname(binary), 'lock-signals-'));
  const ready = join(signals, 'ready');
  const release = join(signals, 'release');
  const child = spawn(binary, ['hold-file', path, ready, release], { windowsHide: true, stdio: 'ignore' });
  const exited = new Promise(resolve => child.once('exit', resolve));
  waitForFixture(() => existsSync(ready));
  return async () => {
    writeFileSync(release, 'release');
    waitForFixture(() => !existsSync(ready));
    await exited;
    rmSync(signals, { recursive: true });
  };
}
function writeManagerHook(directory) {
  const hook = join(directory, 'hook.nsh');
  writeFileSync(hook, readFileSync(resolve('server/manager/install/windows.nsh'), 'utf8').replaceAll('$LOCALAPPDATA', '$INSTDIR\\local').replace(/!include [^\r\n]+windows-common.nsh"/, `!include "${resolve('src-tauri/windows-common.nsh')}"`).replace('Call un.SyncCommandError', 'Call un.SyncCommandError\nFileOpen $3 "$INSTDIR\\failure-message" w\nFileWriteUTF16LE $3 "$SyncFailureMessage"\nFileClose $3').replace('DetailPrint "$(SyncResumeDataKept)"', 'FileOpen $0 "$INSTDIR\\data-kept" w\nFileWriteUTF16LE $0 "$(SyncResumeDataKept)"\nFileClose $0'));
  return hook;
}

test('Sync NSIS preserves data by default, protects updates, and aborts before binary removal on cleanup failure', { skip }, async () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-sync-nsis-runtime-'));
  let completed = false;
  try {
    const binary = buildManagerStub(directory);
    const hook = writeManagerHook(directory);
    const script = join(directory, 'installer.nsi');
    writeFileSync(script, String.raw`
Unicode true
RequestExecutionLevel user
SilentInstall silent
SilentUnInstall silent
OutFile "${q(join(directory, 'installer.exe'))}"
!include LogicLib.nsh
!include FileFunc.nsh
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\Korean.nlf"
!define MAINBINARYNAME "SyntheticSync"
!define PRODUCTNAME "Synthetic Sync"
Var UpdateMode
Var DeleteAppDataCheckboxState
!macro CheckIfAppIsRunning executable product
  StrCmp "${'$'}{executable}" "${installedExecutable}" +3
  SetErrorLevel 9
  Abort "Process check must use the installed executable path"
!macroend
!include "${q(hook)}"
Function .onInit
ReadEnvStr $LANGUAGE RISUNEST_NSIS_TEST_LANGUAGE
FunctionEnd
Function .onInstFailed
FileOpen $0 "$INSTDIR\diagnostic" w
FileWriteUTF16LE $0 "$SyncFailureMessage"
FileClose $0
FunctionEnd
Function un.onUninstFailed
FileOpen $0 "$INSTDIR\diagnostic" w
FileWrite $0 "$SyncFailureMessage"
FileClose $0
FunctionEnd
Section
!insertmacro NSIS_HOOK_PREINSTALL
SetOutPath "$INSTDIR"
File /oname=risunest-sync-manager.exe "${q(binary)}"
WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd
Function un.onInit
ReadEnvStr $LANGUAGE RISUNEST_NSIS_TEST_LANGUAGE
${'$'}{GetParameters} $0
ClearErrors
${'$'}{GetOptions} $0 "/UPDATE" $1
${'$'}{IfNot} ${'$'}{Errors}
StrCpy $UpdateMode 1
${'$'}{EndIf}
; The template preselects deletion for /DELETEAPPDATA; /UNCHECKED stands for the user clearing it.
ClearErrors
${'$'}{GetOptions} $0 "/DELETEAPPDATA" $1
${'$'}{IfNot} ${'$'}{Errors}
StrCpy $DeleteAppDataCheckboxState 1
${'$'}{EndIf}
ClearErrors
${'$'}{GetOptions} $0 "/UNCHECKED" $1
${'$'}{IfNot} ${'$'}{Errors}
StrCpy $DeleteAppDataCheckboxState 0
${'$'}{EndIf}
FunctionEnd
Section Uninstall
!insertmacro NSIS_HOOK_PREUNINSTALL
Delete "$INSTDIR\risunest-sync-manager.exe"
!insertmacro NSIS_HOOK_POSTUNINSTALL
!insertmacro NSIS_HOOK_FINALUNINSTALL
SectionEnd
`);
    const compile = spawnSync(compiler, ['/V2', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(compile.status, 0, `${compile.stdout}\n${compile.stderr}`);
    for (const scenario of [
      { name: 'preserve', args: [], data: true, installed: false, code: 0 },
      { name: 'delete', args: ['/DELETEAPPDATA'], data: false, installed: false, code: 0 },
      { name: 'custom', custom: true, args: ['/DELETEAPPDATA'], data: false, installed: false, code: 0 },
      { name: 'update', args: ['/UPDATE', '/DELETEAPPDATA'], data: true, installed: false, code: 0 },
      { name: 'failed', args: ['/DELETEAPPDATA'], fail: true, data: true, installed: true, code: 1 },
      { name: 'forced-unregistered', args: ['/FORCE', '/DELETEAPPDATA'], unregistered: true, data: true, installed: false, code: 0 },
      { name: 'unchecked', args: ['/DELETEAPPDATA', '/UNCHECKED'], data: true, installed: false, code: 0 },
      { name: 'copy-failed', args: [], missingManager: true, data: true, installed: false, code: 1 },
      { name: 'retry', args: ['/DELETEAPPDATA'], failRemove: true, data: false, installed: true, code: 1 },
    ]) {
      const location = join(directory, scenario.name);
      const dataRoot = join(location, scenario.custom ? 'custom server data' : 'local/RisuNestSyncData');
      mkdirSync(dataRoot, { recursive: true });
      writeFileSync(join(dataRoot, 'synthetic'), 'fixture');
      if (!scenario.unregistered) writeFileSync(join(dataRoot, 'risunest-sync-instance.json'), '{}');
      if (scenario.fail) writeFileSync(join(location, 'fail'), 'fixture');
      if (scenario.failRemove) writeFileSync(join(location, 'fail-remove'), 'fixture');
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
      if (scenario.missingManager) rmSync(join(location, 'risunest-sync-manager.exe'));
      const uninstall = () => runNsis(directory, join(location, 'uninstall.exe'), ['/S', ...scenario.args, `_?=${location}`], { env: { ...process.env, RISUNEST_SYNC_UNINSTALL_DATA_DIR: scenario.custom ? dataRoot : '' } });
      const result = uninstall();
      assert.equal(result.status, scenario.code, `${scenario.name}: exit ${existsSync(join(location, "calls")) ? readFileSync(join(location, "calls"), "utf8") : "no manager calls"}\ndiagnostic: ${existsSync(join(location, 'diagnostic')) ? readFileSync(join(location, 'diagnostic'), 'utf8') : 'unavailable'}`);
      assert.equal(existsSync(join(location, 'risunest-sync-manager.exe')), scenario.installed, `${scenario.name}: manager remains on failure`);
      assert.equal(existsSync(join(dataRoot, 'synthetic')), scenario.data, `${scenario.name}: data`);
      if (scenario.missingManager) {
        const diagnostic = readFileSync(join(location, 'diagnostic'), 'utf8');
        assert.match(diagnostic, /removal-helper-copy-failed:/);
        assert.ok(diagnostic.includes(join(location, 'risunest-sync-manager.exe')));
        assert.equal(existsSync(join(location, 'calls')), false);
        continue;
      }
      const calls = readFileSync(join(location, 'calls'), 'utf8');
      if (scenario.name === 'update') {
        assert.match(calls, /^prepare-update:/);
        assert.doesNotMatch(calls, /uninstall|delete-data|forget-removal-resumable/);
      } else {
        assert.equal([...calls.matchAll(/^installer prepare ([1-9]\d*):/gm)].length, 1, `${scenario.name}: prepare receives the uninstaller owner PID`);
        assert.match(calls, /^uninstall lock-held(?: --force)?:/m);
        assert.ok(calls.indexOf('installer prepare ') < calls.indexOf('uninstall lock-held'), `${scenario.name}: acquire guard before cleanup`);
      }
      if (scenario.name === 'delete') {
        assert.match(calls, /^installer finish aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa:/m);
        assert.ok(calls.indexOf('installer finish') < calls.indexOf('installer delete-data'));
      }
      if (scenario.data && !scenario.installed && scenario.name !== 'update') {
        assert.match(calls, /^installer finish aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa:/m);
        assert.ok(calls.indexOf('installer finish') > calls.indexOf('installer remove-files-resumable'));
      }
      if (scenario.unregistered) {
        assert.match(calls, /installer removal-check force delete/);
        assert.match(calls, /uninstall lock-held --force/);
        assert.match(calls, /installer remove-files-resumable force/);
        assert.doesNotMatch(calls, /installer delete-data/);
      }
      if (scenario.name === 'unchecked') {
        assert.match(calls, /^installer removal-check normal preserve:/m);
        assert.doesNotMatch(calls, /installer delete-data/);
      }
      if (scenario.failRemove) {
        assert.match(readFileSync(join(location, 'diagnostic'), 'utf8'), /removal-file-failed: synthetic/);
        const retry = uninstall();
        assert.equal(retry.status, 0, `${scenario.name}: retry ${readFileSync(join(location, 'calls'), 'utf8')}`);
        assert.equal(existsSync(join(location, 'risunest-sync-manager.exe')), false, `${scenario.name}: retry removes the manager`);
        const retried = readFileSync(join(location, 'calls'), 'utf8').slice(calls.length);
        assert.match(retried, /^installer delete-data:/m);
        assert.match(retried, /^installer remove-files-resumable normal:/m);
      }
    }
    {
      const location = join(directory, 'linked-retry');
      const outside = join(directory, 'unrelated-folder');
      mkdirSync(location); mkdirSync(outside);
      writeFileSync(join(outside, 'receipt.json'), '{}');
      writeFileSync(join(outside, 'complete'), 'true');
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
      const retry = join(location, '.risunest-sync-removal');
      symlinkSync(outside, retry, 'junction');
      try {
        assert.equal(runNsis(directory, join(location, 'uninstall.exe'), ['/S', `_?=${location}`]).status, 1);
        assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 1);
        assert.equal(readFileSync(join(outside, 'receipt.json'), 'utf8'), '{}');
        assert.equal(existsSync(join(location, 'risunest-sync-manager.exe')), true);
      } finally { unlinkSync(retry); }
    }
    for (const boundary of ['helper-copy', 'receipt-staged', 'receipt', 'manager-deleted', 'final-files-deleted', 'registration-forgotten', 'complete', 'helper-cleaned', 'receipt-cleaned']) {
      const location = join(directory, `interrupted-${boundary}`);
      mkdirSync(join(location, 'local/RisuNestSyncData'), { recursive: true });
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
      const retry = join(location, '.risunest-sync-removal');
      mkdirSync(retry);
      writeFileSync(join(retry, 'data-path'), Buffer.from(join(location, 'local/RisuNestSyncData'), 'utf16le'));
      copyFileSync(binary, join(retry, 'manager.exe'));
      const prepared = !['helper-copy', 'receipt-staged'].includes(boundary);
      if (prepared) writeFileSync(join(retry, 'receipt.json'), '{}');
      if (boundary === 'receipt-staged') writeFileSync(join(retry, 'receipt.new'), '{');
      if (prepared && boundary !== 'receipt') rmSync(join(location, 'risunest-sync-manager.exe'));
      if (['complete', 'helper-cleaned', 'receipt-cleaned'].includes(boundary)) writeFileSync(join(retry, 'complete'), 'true');
      if (['helper-cleaned', 'receipt-cleaned'].includes(boundary)) rmSync(join(retry, 'manager.exe'));
      if (boundary === 'receipt-cleaned') rmSync(join(retry, 'receipt.json'));
      if (boundary === 'receipt') {
        const refused = runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]);
        assert.equal(refused.status, 1, 'unfinished removal refuses reinstall');
        assert.equal(readFileSync(join(retry, 'receipt.json'), 'utf8'), '{}');
      }
      if (boundary === 'receipt-cleaned') {
        writeFileSync(join(retry, 'receipt.json'), 'synthetic completed receipt');
        const releaseLock = holdDeletion(binary, join(retry, 'receipt.json'));
        try {
          const refused = runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]);
          assert.equal(refused.status, 1, 'locked completed residue refuses reinstall');
          assert.equal(readFileSync(join(retry, 'complete'), 'utf8'), 'true', 'cleanup refusal retains completion evidence');
        } finally { await releaseLock(); }
        const reinstall = runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]);
        assert.equal(reinstall.status, 0, 'completed residue permits reinstall');
        assert.equal(existsSync(retry), false, 'reinstall clears only completed retry residue');
        assert.equal(existsSync(join(location, 'risunest-sync-manager.exe')), true);
      }
      const result = runNsis(directory, join(location, 'uninstall.exe'), ['/S', `_?=${location}`]);
      assert.equal(result.status, 0, `${boundary}: ${existsSync(join(location, 'diagnostic')) ? readFileSync(join(location, 'diagnostic'), 'utf8') : result.error ?? ''}`);
      assert.equal(existsSync(join(location, 'risunest-sync-manager.exe')), false, boundary);
      assert.equal(existsSync(retry), false, `${boundary}: retry artifacts removed`);
      assert.equal(existsSync(join(location, 'local/RisuNestSyncData')), true, `${boundary}: preserved data`);
    }
    for (const [boundary, deleted] of ['pending', 'forgotten', 'complete'].flatMap(boundary => [false, true].map(deleted => [boundary, deleted]))) {
      const location = join(directory, `delete-resume-${boundary}-${deleted}`);
      const data = join(location, 'custom data 경로');
      mkdirSync(data, { recursive: true });
      writeFileSync(join(data, 'synthetic'), 'fixture');
      if (boundary === 'pending') writeFileSync(join(data, 'risunest-sync-instance.json'), '{}');
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
      const retry = join(location, '.risunest-sync-removal');
      mkdirSync(retry);
      writeFileSync(join(retry, 'data-path'), Buffer.from(`\\\\?\\${data}`, 'utf16le'));
      copyFileSync(binary, join(retry, 'manager.exe'));
      writeFileSync(join(retry, 'receipt.json'), '{}');
      rmSync(join(location, 'risunest-sync-manager.exe'));
      if (boundary === 'complete') writeFileSync(join(retry, 'complete'), 'true');
      if (deleted) rmSync(data, { recursive: true });
      const result = runNsis(directory, join(location, 'uninstall.exe'), ['/S', '/DELETEAPPDATA', `_?=${location}`]);
      assert.equal(result.status, 0, boundary);
      assert.equal(existsSync(data), !deleted && boundary !== 'pending', boundary);
      assert.equal(existsSync(join(location, 'data-kept')), !deleted && boundary !== 'pending');
      if (!deleted && boundary !== 'pending') {
        const message = readFileSync(join(location, 'data-kept'), 'utf16le');
        assert.ok(message === `Server data could not be deleted. Data folder: ${data}` || message === `서버 데이터를 삭제하지 못했습니다. 데이터 폴더: ${data}`, message);
      }
    }
    for (const [language, pendingMessage, resumeMessage] of [
      ['1033', 'A previous RisuNest Sync removal is incomplete. Run the removal again, then install.', 'RisuNest Sync removal could not be completed. Retry removal to finish.'],
      ['1042', '이전 RisuNest Sync 제거가 완료되지 않았습니다. 제거를 다시 실행한 후 설치해주세요.', 'RisuNest Sync 제거를 완료하지 못했습니다. 다시 시도해주세요.'],
    ]) {
      const location = join(directory, `resume-failure-${language}`);
      const data = join(location, 'custom data');
      const rawData = `\\\\?\\${data}`;
      const env = { ...process.env, RISUNEST_NSIS_TEST_LANGUAGE: language };
      mkdirSync(data, { recursive: true });
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`], { env }).status, 0);
      const retry = join(location, '.risunest-sync-removal');
      mkdirSync(retry);
      writeFileSync(join(retry, 'data-path'), Buffer.from(rawData, 'utf16le'));
      writeFileSync(join(retry, 'receipt.json'), '{}');
      copyFileSync(binary, join(retry, 'manager.exe'));
      writeFileSync(join(location, 'fail-resume'), 'fixture');
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`], { env }).status, 1);
      assert.ok(readFileSync(join(location, 'diagnostic'), 'utf16le').startsWith(pendingMessage));
      assert.equal(runNsis(directory, join(location, 'uninstall.exe'), ['/S', `_?=${location}`], { env }).status, 1);
      assert.ok(readFileSync(join(location, 'failure-message'), 'utf16le').startsWith(resumeMessage));
      assert.ok(readFileSync(join(location, 'calls'), 'utf8').includes(`installer resume-removal preserve: ${rawData}`));
      assert.equal(readFileSync(join(retry, 'data-path'), 'utf16le'), rawData);
    }
    for (const invalid of ['', 'relative', 'x'.repeat(20000)]) {
      const location = join(directory, `invalid-path-${invalid.length}`);
      mkdirSync(location);
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
      rmSync(join(location, 'risunest-sync-manager.exe'));
      const retry = join(location, '.risunest-sync-removal');
      mkdirSync(retry);
      writeFileSync(join(retry, 'complete'), 'true');
      writeFileSync(join(retry, 'data-path'), Buffer.from(invalid, 'utf16le'));
      assert.equal(runNsis(directory, join(location, 'uninstall.exe'), ['/S', '/DELETEAPPDATA', `_?=${location}`]).status, 1);
      assert.equal(existsSync(join(location, 'data-kept')), false, 'an invalid path never reports the default data directory');
      assert.equal(existsSync(join(location, 'uninstall.exe')), true);
    }
    for (const locked of ['complete', 'data-path']) {
      const location = join(directory, `locked-completed-${locked}`);
      const data = join(location, 'custom data');
      mkdirSync(data, { recursive: true });
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
      rmSync(join(location, 'risunest-sync-manager.exe'));
      const retry = join(location, '.risunest-sync-removal');
      mkdirSync(retry);
      writeFileSync(join(retry, 'complete'), 'true');
      writeFileSync(join(retry, 'data-path'), Buffer.from(data, 'utf16le'));
      const releaseLock = holdDeletion(binary, join(retry, locked));
      try {
        assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 1);
        assert.equal(readFileSync(join(retry, 'complete'), 'utf8'), 'true');
        assert.equal(readFileSync(join(retry, 'data-path'), 'utf16le'), data);
      } finally { await releaseLock(); }
      assert.equal(runNsis(directory, join(location, 'uninstall.exe'), ['/S', '/FORCE', '/DELETEAPPDATA', `_?=${location}`]).status, 0);
      assert.equal(existsSync(retry), false);
      assert.equal(existsSync(data), true);
      assert.ok(readFileSync(join(location, 'data-kept'), 'utf16le').includes(data));
    }
    completed = true;
  } finally {
    assert.equal(dirname(directory), resolve(tmpdir()));
    if (completed) rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    else console.error(`Retained synthetic Sync NSIS fixture: ${directory}`);
  }
});

test('Sync preinstall checks the resolved installation directory', { skip }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-sync-nsis-path-'));
  const q = value => value.replaceAll('$', '$$').replaceAll('"', '$\\"');
  const original = join(directory, 'original');
  const normalized = join(directory, 'normalized');
  const custom = join(directory, 'custom');
  try {
    const script = join(directory, 'installer.nsi');
    writeFileSync(script, String.raw`
Unicode true
RequestExecutionLevel user
SilentInstall silent
OutFile "${q(join(directory, 'installer.exe'))}"
!include LogicLib.nsh
!include FileFunc.nsh
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\Korean.nlf"
!define MAINBINARYNAME "SyntheticSync"
!define PRODUCTNAME "Synthetic Sync"
!define RISUNEST_SYNC_DEFAULT_INSTALL_DIR "${q(original)}"
!define RISUNEST_SYNC_INSTALL_DIR "${q(normalized)}"
!macro CheckIfAppIsRunning executable product
  FileOpen $0 "$EXEDIR\checked-path" w
  FileWrite $0 "${'$'}{executable}"
  FileClose $0
!macroend
!include "${q(resolve('server/manager/install/windows.nsh'))}"
Section
  !insertmacro NSIS_HOOK_PREINSTALL
SectionEnd
`);
    const compiled = spawnSync(compiler, ['/V2', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(compiled.status, 0, `${compiled.stdout}\n${compiled.stderr}`);
    for (const [requested, expected] of [[original, normalized], [custom, custom]]) {
      const result = runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${requested}`]);
      assert.equal(result.status, 0);
      assert.equal(readFileSync(join(directory, 'checked-path'), 'utf8'), join(expected, 'SyntheticSync.exe'));
    }
  } finally {
    assert.equal(dirname(directory), resolve(tmpdir()));
    rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});

const setupScript = (directory, hook, body) => String.raw`
Unicode true
RequestExecutionLevel user
SilentInstall silent
OutFile "${q(join(directory, 'installer.exe'))}"
!include LogicLib.nsh
!include FileFunc.nsh
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\Korean.nlf"
!define MAINBINARYNAME "SyntheticSync"
!define PRODUCTNAME "Synthetic Sync"
!macro CheckIfAppIsRunning executable product
!macroend
!include "${q(hook)}"
Section
${body}
SectionEnd
`;

test('Sync setup keeps the replaced bundle list until a retry removes obsolete files', { skip }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-sync-nsis-baseline-'));
  try {
    const binary = buildManagerStub(directory);
    const replacement = join(directory, 'replacement.json');
    writeFileSync(replacement, '{"version":"1.0.5","files":["risunest-sync-manager.exe"]}\n');
    const script = join(directory, 'installer.nsi');
    const hook = writeManagerHook(directory);
    writeFileSync(hook, readFileSync(hook, 'utf8').replaceAll('DetailPrint "$(SyncObsoleteSkipped)"', 'FileOpen $0 "$INSTDIR\\obsolete-skipped" w\nFileWriteUTF16LE $0 "$(SyncObsoleteSkipped)"\nFileClose $0'));
    writeFileSync(script, setupScript(directory, hook, String.raw`
!insertmacro NSIS_HOOK_PREINSTALL
SetOutPath "$INSTDIR"
File /oname=risunest-sync-manager.exe "${q(binary)}"
File /oname=risunest-sync-bundle.json "${q(replacement)}"
!insertmacro NSIS_HOOK_POSTINSTALL`));
    const compiled = spawnSync(compiler, ['/V2', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(compiled.status, 0, `${compiled.stdout}\n${compiled.stderr}`);
    const install = location => runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]);
    const location = join(directory, 'installed');
    const baseline = join(location, 'risunest-sync-bundle.previous.json');
    const previous = '{"version":"1.0.4","files":["risunest-sync-manager.exe","risunest-sync-retired.exe"]}\n';
    mkdirSync(location);
    writeFileSync(join(location, 'risunest-sync-bundle.json'), previous);
    copyFileSync(binary, join(location, 'risunest-sync-manager.exe'));
    writeFileSync(join(location, 'fail-register'), 'fixture');
    assert.equal(install(location).status, 1);
    assert.equal(readFileSync(join(location, 'risunest-sync-bundle.json'), 'utf8'), readFileSync(replacement, 'utf8'));
    assert.equal(readFileSync(baseline, 'utf8'), previous, 'a failed setup keeps the replaced bundle list');
    assert.equal(install(location).status, 0);
    assert.equal(readFileSync(join(location, 'baseline-received'), 'utf8'), previous, 'the retry removes files the first attempt replaced');
    assert.equal(existsSync(baseline), false);
    const fresh = join(directory, 'fresh');
    assert.equal(install(fresh).status, 0);
    assert.equal(existsSync(join(fresh, 'risunest-sync-bundle.previous.json')), false);
    assert.doesNotMatch(readFileSync(join(fresh, 'calls'), 'utf8'), /obsolete-files/);
    const corrupt = join(directory, 'corrupt');
    mkdirSync(corrupt);
    copyFileSync(binary, join(corrupt, 'risunest-sync-manager.exe'));
    writeFileSync(join(corrupt, 'risunest-sync-bundle.json'), 'invalid');
    writeFileSync(join(corrupt, 'risunest-sync-bundle.previous.json'), previous);
    assert.equal(install(corrupt).status, 0);
    assert.doesNotMatch(readFileSync(join(corrupt, 'calls'), 'utf8'), /obsolete-files/);
    assert.equal(readFileSync(join(corrupt, 'risunest-sync-bundle.previous.json'), 'utf8'), previous);
    const detail = readFileSync(join(corrupt, 'obsolete-skipped'), 'utf16le');
    assert.ok(detail === 'The previous bundle marker could not be read. Obsolete-file cleanup was skipped.' || detail === '이전 설치의 파일 목록을 읽을 수 없어 이전 파일 제거를 건너뛰었습니다.', detail);
  } finally {
    assert.equal(dirname(directory), resolve(tmpdir()));
    rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});

test('Sync setup compares against the version in the installed bundle marker', { skip }, () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-sync-nsis-version-'));
  try {
    const script = join(directory, 'installer.nsi');
    writeFileSync(script, setupScript(directory, writeManagerHook(directory), String.raw`
StrCpy $0 "registry"
!insertmacro NSIS_HOOK_INSTALLED_VERSION
FileOpen $1 "$INSTDIR\installed-version" w
FileWrite $1 $0
FileClose $1`));
    const compiled = spawnSync(compiler, ['/V2', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(compiled.status, 0, `${compiled.stdout}\n${compiled.stderr}`);
    const generated = {
      schema: 'risunest-sync-bundle/v1', product: 'sync', variant: 'managed', version: '1.0.7', protocolId: 'synthetic', storeFormatId: 'synthetic',
      files: ['risunest-sync-manager.exe'], vendor: [{ name: 'cloudflared', version: '2025.8.1', path: 'cloudflared.exe' }],
    };
    for (const [name, marker, expected] of [
      ['generated', `${JSON.stringify(generated, null, 2)}\n`, '1.0.7'],
      ['compact', JSON.stringify(generated), '1.0.7'],
      ['spaced', '{"schema": "version", "version" :\t"1.0.8"}', '1.0.8'],
      ['missing', undefined, 'registry'],
      ['unversioned', `${JSON.stringify({ ...generated, version: undefined }, null, 2)}\n`, 'registry'],
      ['numeric', '{"version": 107}', 'registry'],
      ['empty', '{"version": ""}', 'registry'],
    ]) {
      const location = join(directory, name);
      mkdirSync(location);
      if (marker !== undefined) writeFileSync(join(location, 'risunest-sync-bundle.json'), marker);
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
      assert.equal(readFileSync(join(location, 'installed-version'), 'utf8'), expected, name);
    }
  } finally {
    assert.equal(dirname(directory), resolve(tmpdir()));
    rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});

for (const installMode of ['currentUser', 'both']) {
test(`Sync template retains a discoverable retry when the uninstaller cannot be deleted (${installMode})`, { skip }, async () => {
  const directory = mkdtempSync(join(tmpdir(), 'risunest-sync-finalize-'));
  const location = join(directory, 'installed');
  const key = `Software\\RisuNestSynthetic\\${directory.split(/[\\/]/).at(-1)}`;
  try {
    const template = readFileSync(resolve('src-tauri/windows-installer.nsi'), 'utf8');
    const start = template.indexOf('  !ifdef RISUNEST_DEFER_UNINSTALL_TEARDOWN', template.indexOf('!insertmacro NSIS_HOOK_POSTUNINSTALL'));
    const teardown = template.slice(start, template.indexOf('  ; Auto close', start));
    assert.ok(start > 0 && teardown.includes('DeleteRegKey'));
    const finalHook = readFileSync(resolve('server/manager/install/windows.nsh'), 'utf8').match(/!macro NSIS_HOOK_FINALUNINSTALL\r?\n[\s\S]*?!macroend/)[0];
    const script = join(directory, 'installer.nsi');
    writeFileSync(script, String.raw`
Unicode true
RequestExecutionLevel user
SilentInstall silent
SilentUnInstall silent
OutFile "${q(join(directory, 'installer.exe'))}"
!include LogicLib.nsh
!define RISUNEST_DEFER_UNINSTALL_TEARDOWN
!define INSTALLMODE "${installMode}"
Var MultiUser.InstallMode
!define PRODUCTNAME "RisuNest Synthetic Retry"
!define UNINSTKEY "${key}"
${finalHook}
Section
SetOutPath "$INSTDIR"
WriteRegStr HKCU "${key}" "DisplayName" "RisuNest Synthetic Retry"
WriteRegStr HKCU "${key}" "UninstallString" '$\"$INSTDIR\uninstall.exe$\"'
WriteUninstaller "$INSTDIR\uninstall.exe"
SectionEnd
Section Uninstall
StrCpy $MultiUser.InstallMode "CurrentUser"
SetShellVarContext all
${teardown}
SectionEnd
`);
    const compile = spawnSync(compiler, ['/V2', script], { encoding: 'utf8', timeout: 30000, windowsHide: true });
    assert.equal(compile.status, 0, `${compile.stdout}\n${compile.stderr}`);
    assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
    const installed = join(location, 'uninstall.exe');
    const active = join(directory, 'active-uninstaller.exe');
    copyFileSync(installed, active);
    const retry = join(location, '.risunest-sync-removal');
    mkdirSync(retry); writeFileSync(join(retry, 'complete'), 'true');
    const binary = buildManagerStub(directory);
    const releaseLock = holdDeletion(binary, installed);
    try {
      assert.equal(runNsis(directory, active, ['/S', `_?=${location}`]).status, 1);
      assert.equal(existsSync(installed), true);
      assert.equal(readFileSync(join(retry, 'complete'), 'utf8'), 'true');
      const registration = spawnSync('reg.exe', ['query', `HKCU\\${key}`, '/v', 'UninstallString'], { encoding: 'utf8', windowsHide: true });
      assert.equal(registration.status, 0);
      assert.ok(registration.stdout.includes(installed), 'Apps registration retains the same retry executable');
    } finally { await releaseLock(); }
    assert.equal(runNsis(directory, active, ['/S', `_?=${location}`]).status, 0);
    assert.equal(existsSync(installed), false);
    assert.equal(existsSync(retry), false);
    assert.equal(spawnSync('reg.exe', ['query', `HKCU\\${key}`], { windowsHide: true }).status, 1);
    assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
    // The launched image and _?= may spell the same folder differently.
    const shortName = path => spawnSync('cmd.exe', ['/d', '/c', `for %I in ("${path}") do @echo %~sI`], { encoding: 'utf8', windowsHide: true, windowsVerbatimArguments: true }).stdout.trim();
    const inPlaceRuns = [
      [installed, location.toUpperCase()],
      [installed, `${directory}\\\\installed`],
      [`${location}\\.\\uninstall.exe`, location],
    ];
    const shortLocation = shortName(location);
    if (shortLocation && shortLocation.toLowerCase() !== location.toLowerCase()) {
      inPlaceRuns.push([installed, shortLocation], [shortName(installed), location]);
    }
    for (const [image, instdir] of inPlaceRuns) {
      const run = `${image} _?=${instdir}`;
      mkdirSync(retry); writeFileSync(join(retry, 'complete'), 'true');
      assert.equal(runNsis(directory, image, ['/S', `_?=${instdir}`]).status, 0, `in-place removal leaves its image for the caller (${run})`);
      assert.equal(existsSync(installed), true, run);
      assert.equal(existsSync(location), true, run);
      assert.equal(existsSync(retry), false, run);
      assert.equal(spawnSync('reg.exe', ['query', `HKCU\\${key}`], { windowsHide: true }).status, 1, run);
      assert.equal(runNsis(directory, join(directory, 'installer.exe'), ['/S', `/D=${location}`]).status, 0);
    }
    mkdirSync(retry); writeFileSync(join(retry, 'complete'), 'true');
    const registryDeletionDenied = deny => {
      const result = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', `
        $ErrorActionPreference = 'Stop'
        $key = $env:RISUNEST_SYNTHETIC_UNINSTALL_KEY
        if (!$key.StartsWith('Software\\RisuNestSynthetic\\risunest-sync-finalize-')) { throw 'not a synthetic registry key' }
        $rights = [System.Security.AccessControl.RegistryRights]::ReadPermissions -bor [System.Security.AccessControl.RegistryRights]::ChangePermissions
        $registryKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($key, [Microsoft.Win32.RegistryKeyPermissionCheck]::ReadWriteSubTree, $rights)
        try {
          $acl = $registryKey.GetAccessControl()
          $rule = [System.Security.AccessControl.RegistryAccessRule]::new([System.Security.Principal.WindowsIdentity]::GetCurrent().User, [System.Security.AccessControl.RegistryRights]::Delete, [System.Security.AccessControl.AccessControlType]::Deny)
          ${deny ? '$acl.AddAccessRule($rule)' : '$acl.RemoveAccessRuleSpecific($rule)'}
          $registryKey.SetAccessControl($acl)
        } finally { $registryKey.Dispose() }
      `], { encoding: 'utf8', windowsHide: true, env: { ...process.env, RISUNEST_SYNTHETIC_UNINSTALL_KEY: key } });
      assert.equal(result.status, 0, result.stderr);
    };
    registryDeletionDenied(true);
    try {
      assert.equal(runNsis(directory, active, ['/S', `_?=${location}`]).status, 1, 'access denied remains a removal failure');
      assert.equal(existsSync(installed), true);
      assert.equal(readFileSync(join(retry, 'complete'), 'utf8'), 'true');
    } finally { registryDeletionDenied(false); }
    assert.equal(spawnSync('reg.exe', ['delete', `HKCU\\${key}`, '/f'], { windowsHide: true }).status, 0);
    assert.equal(runNsis(directory, active, ['/S', `_?=${location}`]).status, 0, 'retry after registry removal finishes uninstall');
    assert.equal(existsSync(installed), false);
    assert.equal(existsSync(retry), false);
  } finally {
    spawnSync('reg.exe', ['delete', `HKCU\\${key}`, '/f'], { windowsHide: true });
    assert.equal(dirname(directory), resolve(tmpdir()));
    rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});
}
