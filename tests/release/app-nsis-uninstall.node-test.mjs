import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import test from "node:test";

const compiler = process.env.NSIS_MAKENSIS ?? join(process.env.LOCALAPPDATA ?? "", "tauri", "NSIS", "makensis.exe");
const skip = process.platform !== "win32" ? "NSIS execution requires Windows" : !existsSync(compiler) ? "NSIS compiler is unavailable" : false;
if (process.env.RISUNEST_REQUIRE_NSIS === "1" && skip) throw new Error(skip);
const quote = (value) => value.replaceAll("$", "$$").replaceAll('"', '$\\"');
const installedExecutable = "$INSTDIR\\${MAINBINARYNAME}.exe";
const bundleId = "synthetic.risunest.nsis";

test("both installers use the template pinned to their Tauri CLI and automatic Korean/English languages", () => {
  const template = readFileSync(resolve("src-tauri/windows-installer.nsi"), "utf8");
  const pinned = template.match(/Based on @tauri-apps\/cli (\d+\.\d+\.\d+)/)?.[1];
  assert.ok(pinned);
  for (const [manifest, configuration] of [
    ["package.json", "src-tauri/tauri.conf.json"],
    ["server/manager/gui/package.json", "server/manager/gui/src-tauri/tauri.bundle.conf.json"],
  ]) {
    assert.equal(JSON.parse(readFileSync(resolve(manifest), "utf8")).devDependencies["@tauri-apps/cli"], pinned);
    const nsis = JSON.parse(readFileSync(resolve(configuration), "utf8")).bundle.windows.nsis;
    assert.equal(resolve(dirname(configuration), nsis.template), resolve("src-tauri/windows-installer.nsi"));
    assert.deepEqual(nsis.languages, ["English", "Korean"]);
    assert.equal(nsis.displayLanguageSelector, false);
  }
});

test("the template normalizes an installation folder that no longer exists", { skip }, () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-nsis-path-"));
  try {
    const template = readFileSync(resolve("src-tauri/windows-installer.nsi"), "utf8");
    const normalize = template.match(/^Function NormalizePath\r?\n[\s\S]*?^FunctionEnd\r?$/m)?.[0];
    assert.ok(normalize);
    const existing = join(root, "existing");
    const missing = join(root, "missing");
    mkdirSync(existing);
    const cases = [
      [`${existing}\\.`, existing],
      [`${missing}\\.`, missing],
      [`${missing}\\\\.`, missing],
      [`${root.replaceAll("\\", "/")}/missing/nested/../.`, missing],
    ];
    const script = join(root, "normalize.nsi");
    writeFileSync(script, String.raw`Unicode true
RequestExecutionLevel user
SilentInstall silent
OutFile "${quote(join(root, "normalize.exe"))}"
!include LogicLib.nsh
${normalize}
Section
  FileOpen $9 "$EXEDIR\results" w
${cases.map(([input]) => `  StrCpy $0 "${quote(input)}"\n  Call NormalizePath\n  FileWrite $9 "$0|"`).join("\n")}
  FileClose $9
SectionEnd
`);
    const compiled = spawnSync(compiler, ["/V2", script], { encoding: "utf8", windowsHide: true, timeout: 30000 });
    assert.equal(compiled.status, 0, `${compiled.stdout}\n${compiled.stderr}`);
    assert.equal(spawnSync(join(root, "normalize.exe"), ["/S"], { timeout: 30000, windowsHide: true }).status, 0);
    assert.deepEqual(readFileSync(join(root, "results"), "utf8").split("|").slice(0, -1), cases.map(([, expected]) => expected));
    assert.equal(existsSync(missing), false);
  } finally { rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});

test("destination checks distinguish an absent drive from a missing directory", { skip }, () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-nsis-volume-"));
  try {
    const drive = [..."ZYXWVUTSRQPONMLKJIHGFED"].find(letter => !existsSync(`${letter}:\\`));
    assert.ok(drive, "an unused drive letter is required");
    const include = resolve("src-tauri/windows-common.nsh");
    const missingFolder = join(root, "missing", "nested");
    const script = join(root, "volume.nsi");
    writeFileSync(script, String.raw`Unicode true
RequestExecutionLevel user
SilentInstall silent
OutFile "${quote(join(root, "volume.exe"))}"
!include LogicLib.nsh
!include "${quote(include)}"
Section
  FileOpen $9 "$EXEDIR\results" w
  StrCpy $INSTDIR "${quote(missingFolder)}"
  !insertmacro CheckInstallVolume
  FileWrite $9 "$1|"
  StrCpy $INSTDIR "${drive}:\missing\folder"
  !insertmacro CheckInstallVolume
  FileWrite $9 "$1|"
  FileClose $9
SectionEnd
`);
    const compiled = spawnSync(compiler, ["/V2", script], { encoding: "utf8", windowsHide: true, timeout: 30000 });
    assert.equal(compiled.status, 0, `${compiled.stdout}\n${compiled.stderr}`);
    assert.equal(spawnSync(join(root, "volume.exe"), ["/S"], { timeout: 30000, windowsHide: true }).status, 0);
    const [present, absent] = readFileSync(join(root, "results"), "utf8").split("|");
    assert.notEqual(present, "-1");
    assert.equal(absent, "-1");
    assert.equal(existsSync(missingFolder), false);
  } finally { rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});

test("missing or misfiled Korean installer strings fail compilation", { skip }, () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-nsis-language-"));
  try {
    const common = readFileSync(resolve("src-tauri/windows-common.nsh"), "utf8");
    for (const [name, source] of [
      ["missing", common.replace(/^LangString InstallPermission 1042 .*\r?\n/m, "")],
      ["misfiled", common.replace("LangString InstallPermission 1042", "LangString InstallPermission ${LANG_KOREAN}")],
    ]) {
      const include = join(root, `${name}.nsh`);
      writeFileSync(include, source);
      const script = join(root, `${name}.nsi`);
      writeFileSync(script, String.raw`Unicode true
OutFile "${quote(join(root, `${name}.exe`))}"
!include LogicLib.nsh
!include "${quote(include)}"
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\Korean.nlf"
Section
DetailPrint "$(InstallPermission)"
SectionEnd
`);
      const result = spawnSync(compiler, ["/V2", script], { encoding: "utf8", windowsHide: true, timeout: 30000 });
      assert.notEqual(result.status, 0, `${name}: incomplete localization compiled`);
      assert.match(`${result.stdout}\n${result.stderr}`, /6040|7025/);
    }
  } finally { rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 }); }
});

test("NSIS executes app cleanup only on explicit removal and retains retry capability on failure", { skip }, async () => {
  let running;
  const root = mkdtempSync(join(tmpdir(), "risunest-nsis-contract-"));
  const compile = (name, source) => {
    const script = join(root, `${name}.nsi`);
    writeFileSync(script, source);
    const result = spawnSync(compiler, ["/V2", script], { encoding: "utf8", timeout: 30_000, windowsHide: true });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  };
  try {
    const native = {
      x64: { target: "x86_64-pc-windows-msvc", machine: 0x8664 },
      arm64: { target: "aarch64-pc-windows-msvc", machine: 0xaa64 },
    }[process.arch];
    assert.ok(native, `Unsupported Windows architecture: ${process.arch}`);
    const cleanup = join(root, "cleanup.exe");
    const compiled = spawnSync("rustc", [
      "--edition=2021", "--target", native.target,
      resolve("tests/release/fixtures/nsis-cleanup.rs"), "-o", cleanup,
    ], { encoding: "utf8", timeout: 60_000, windowsHide: true });
    assert.equal(compiled.status, 0, `${compiled.error ?? ""}\n${compiled.stdout}\n${compiled.stderr}`);
    const executable = readFileSync(cleanup);
    assert.equal(executable.readUInt16LE(executable.readUInt32LE(0x3c) + 4), native.machine);
    compile("installer", String.raw`
Unicode true
RequestExecutionLevel user
SilentInstall silent
SilentUnInstall silent
OutFile "${quote(join(root, "installer.exe"))}"
!include LogicLib.nsh
!include FileFunc.nsh
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\English.nlf"
LoadLanguageFile "${'$'}{NSISDIR}\Contrib\Language files\Korean.nlf"
!define MAINBINARYNAME "SyntheticRisuNest"
!define PRODUCTNAME "Synthetic RisuNest"
!define BUNDLEID "${bundleId}"
Var DeleteAppDataCheckboxState
Var UpdateMode
!macro CheckIfAppIsRunning executablePath productName
  StrCmp "${"$"}{executablePath}" "${installedExecutable}" +3
  SetErrorLevel 9
  Abort "Process check must use the installed executable path"
  FileOpen $0 "$INSTDIR\process-checked" w
  FileWrite $0 "checked"
  FileClose $0
  FileOpen $0 "$INSTDIR\events" a
  FileSeek $0 0 END
  FileWrite $0 "process-checked$\n"
  FileClose $0
!macroend
!include "${quote(resolve("src-tauri/windows.nsh"))}"
Function un.onUninstFailed
  FileOpen $0 "$INSTDIR\diagnostic" w
  FileWrite $0 "$InstallFailureMessage"
  FileClose $0
FunctionEnd
Section
  SetOutPath "$INSTDIR"
  !insertmacro NSIS_HOOK_PREINSTALL
  !insertmacro CheckIfAppIsRunning "${installedExecutable}" "${"$"}{PRODUCTNAME}"
  File /oname=SyntheticRisuNest.exe "${quote(join(root, "cleanup.exe"))}"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  FileOpen $0 "$INSTDIR\registered" w
  FileWrite $0 "registered"
  FileClose $0
SectionEnd
Function un.onInit
  ${"$"}{GetParameters} $0
  ClearErrors
  ${"$"}{GetOptions} $0 "/DELETE_DATA" $1
  ${"$"}{IfNot} ${"$"}{Errors}
    StrCpy $DeleteAppDataCheckboxState 1
  ${"$"}{EndIf}
  ClearErrors
  ${"$"}{GetOptions} $0 "/UPDATE" $1
  ${"$"}{IfNot} ${"$"}{Errors}
    StrCpy $UpdateMode 1
  ${"$"}{EndIf}
FunctionEnd
Section Uninstall
  !insertmacro NSIS_HOOK_PREUNINSTALL
  !insertmacro CheckIfAppIsRunning "${installedExecutable}" "${"$"}{PRODUCTNAME}"
  Delete "$INSTDIR\SyntheticRisuNest.exe"
  Delete "$INSTDIR\registered"
SectionEnd
`);
    for (const scenario of [
      { name: "default", args: [], data: true, called: false, installed: false, status: 0 },
      { name: "explicit", args: ["/DELETE_DATA"], data: false, called: true, installed: false, status: 0 },
      { name: "update", args: ["/UPDATE", "/DELETE_DATA"], data: true, called: false, installed: false, status: 0 },
      { name: "failed", args: ["/DELETE_DATA"], fail: true, data: true, called: true, installed: true, status: 1 },
      { name: "missing", args: ["/DELETE_DATA"], missing: true, data: true, called: false, installed: true, status: 1 },
    ]) {
      const location = join(root, scenario.name);
      mkdirSync(location);
      const install = spawnSync(join(root, "installer.exe"), ["/S", `/D=${location}`], { timeout: 30_000, windowsHide: true });
      assert.equal(install.status, 0, scenario.name);
      // Only the uninstaller's own process check may let cleanup run.
      rmSync(join(location, "process-checked"));
      writeFileSync(join(location, "synthetic-data"), "fixture");
      if (scenario.fail) writeFileSync(join(location, "fail-cleanup"), "fixture");
      if (scenario.missing) rmSync(join(location, "SyntheticRisuNest.exe"));
      const uninstall = spawnSync(join(location, "uninstall.exe"), ["/S", ...scenario.args, `_?=${location}`], { timeout: 30_000, windowsHide: true });
      assert.equal(uninstall.status, scenario.status, `${scenario.name}: exit`);
      assert.equal(existsSync(join(location, "synthetic-data")), scenario.data, `${scenario.name}: data`);
      assert.equal(existsSync(join(location, "cleanup-called")), scenario.called, `${scenario.name}: cleanup invocation`);
      assert.equal(existsSync(join(location, "registered")), scenario.installed, `${scenario.name}: registration`);
      if (!scenario.missing) assert.equal(existsSync(join(location, "SyntheticRisuNest.exe")), scenario.installed, `${scenario.name}: binary`);
      if (scenario.missing || scenario.fail) {
        const diagnostic = readFileSync(join(location, "diagnostic"), "utf8");
        assert.match(diagnostic, scenario.missing ? /app-cleanup-launch-failed/ : /app-cleanup-exit:7/);
        assert.doesNotMatch(diagnostic, scenario.missing ? /app-cleanup-exit:/ : /app-cleanup-launch-failed/);
      }
      if (scenario.fail) {
        rmSync(join(location, "fail-cleanup"));
        const retry = spawnSync(join(location, "uninstall.exe"), ["/S", "/DELETE_DATA", `_?=${location}`], { timeout: 30_000, windowsHide: true });
        assert.equal(retry.status, 0, "retry succeeds");
        assert.equal(existsSync(join(location, "synthetic-data")), false);
        assert.equal(existsSync(join(location, "registered")), false);
      }
    }

    // A running instance is asked to close before the process check, and both
    // the installer and the uninstaller wait until it has exited.
    const location = join(root, "running");
    mkdirSync(location);
    assert.equal(spawnSync(join(root, "installer.exe"), ["/S", `/D=${location}`], { timeout: 30_000, windowsHide: true }).status, 0);
    for (const [name, program, args] of [
      ["install", join(root, "installer.exe"), ["/S", `/D=${location}`]],
      ["uninstall", join(location, "uninstall.exe"), ["/S", `_?=${location}`]],
    ]) {
      for (const file of ["events", "ready"]) rmSync(join(location, file), { force: true });
      running = spawn(join(location, "SyntheticRisuNest.exe"), ["--serve-exit-request", bundleId], { stdio: "ignore", windowsHide: true });
      const deadline = Date.now() + 10_000;
      while (!existsSync(join(location, "ready"))) {
        assert.ok(running.exitCode === null && Date.now() < deadline, `${name}: synthetic app did not start`);
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      const result = spawnSync(program, args, { timeout: 90_000, windowsHide: true });
      assert.equal(result.status, 0, `${name}: exit`);
      // The installer returns only once the instance has exited.
      const exit = running.exitCode ?? (await once(running, "exit", { signal: AbortSignal.timeout(10_000) }))[0];
      assert.equal(exit, 0, `${name}: synthetic app exit`);
      running = undefined;
      assert.equal(readFileSync(join(location, "events"), "utf8"), "exit-requested\nexited\nprocess-checked\n", name);
    }
    assert.equal(existsSync(join(location, "registered")), false);
  } finally {
    running?.kill();
    assert.equal(dirname(root), resolve(tmpdir()));
    rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});
