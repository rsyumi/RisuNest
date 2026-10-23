import { execFileSync, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { copyFile, lstat, mkdir, readFile, rename, writeFile } from "node:fs/promises";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { REALM_BLOCKED_URL_PATTERNS } from "../../scripts/realmBlocklist.mjs";
import { sharedCargoTarget } from "../../scripts/testNative.mjs";
import { measureEntryCount } from "../legacy-restore/entry-count.mjs";
import { measurePluginValue } from './plugin-value-driver.mjs';
import { measurePluginSync, syntheticPluginSyncServer } from './plugin-sync-driver.mjs';

const root = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../..",
);
const target = sharedCargoTarget(
  execFileSync("git", ["rev-parse", "--git-common-dir"], { cwd: root, encoding: "utf8" }),
  process.env.CARGO_TARGET_DIR,
  root,
);
const title = "RisuNest synthetic device backup smoke";
const timeoutMs = 120_000;
const delay = (milliseconds) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));
const blockedUrls = [
  ...REALM_BLOCKED_URL_PATTERNS,
  "*://update.rsyumi.workers.dev/translator/prompt-presets.json*",
];

class Cdp {
  nextId = 1;
  pending = new Map();

  async connect(url) {
    this.socket = new WebSocket(url);
    await Promise.race([
      once(this.socket, "open"),
      once(this.socket, "error").then(() => {
        throw new Error("synthetic-cdp-connect-failed");
      }),
      delay(10_000).then(() => {
        throw new Error("synthetic-cdp-connect-timeout");
      }),
    ]);
    this.socket.addEventListener("message", (event) => {
      const message = JSON.parse(String(event.data));
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      clearTimeout(pending.timeout);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
    });
    this.socket.addEventListener("close", () => {
      for (const pending of this.pending.values()) {
        clearTimeout(pending.timeout);
        pending.reject(new Error("synthetic-cdp-closed"));
      }
      this.pending.clear();
    });
  }

  call(method, params = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`synthetic-cdp-timeout:${method}`));
      }, 10_000);
      this.pending.set(id, { resolve, reject, timeout });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  close() {
    this.socket?.close();
  }
}

async function command(executable, args, options = {}) {
  const child = spawn(executable, args, {
    cwd: root,
    env: process.env,
    windowsHide: true,
    stdio: ["ignore", "pipe", "pipe"],
    ...options,
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const [code] = await once(child, "exit");
  return { code, stdout, stderr };
}

async function stopOwnedApp(app) {
  if (!app || app.exitCode !== null) return;
  await command("taskkill.exe", ["/PID", String(app.pid), "/T"]);
  const gracefulDeadline = Date.now() + 10_000;
  while (app.exitCode === null && Date.now() < gracefulDeadline) await delay(100);
  if (app.exitCode === null) {
    await command("taskkill.exe", ["/PID", String(app.pid), "/T", "/F"]);
    const forcedDeadline = Date.now() + 10_000;
    while (app.exitCode === null && Date.now() < forcedDeadline) await delay(100);
  }
  if (app.exitCode === null) throw new Error("Synthetic measurement process did not stop");
}

async function evaluate(page, expression) {
  const result = await page.call("Runtime.evaluate", {
    expression,
    awaitPromise: true,
    returnByValue: true,
    userGesture: true,
  });
  if (result.exceptionDetails) {
    throw new Error(
      result.exceptionDetails.exception?.description ??
        "synthetic-evaluation-failed",
    );
  }
  return result.result.value;
}

async function absent(candidate) {
  try {
    await lstat(candidate);
    return false;
  } catch (error) {
    if (error.code === "ENOENT") return true;
    throw error;
  }
}

async function sourceHashes() {
  const files = [
    "benchmarks/device-backup/main.ts",
    "benchmarks/device-backup/vite.config.ts",
    "benchmarks/device-backup/windows-smoke.mjs",
    "src/ts/storage/nativeFileJobs.ts",
    "src/ts/storage/deviceBackup/entry.ts",
    "src-tauri/src/native_file_jobs/portable.rs",
    "src-tauri/src/device_backup/mod.rs",
    "src-tauri/src/local_backup.rs",
    "src-tauri/src/native_file_jobs/legacy_backup.rs",
    "src-tauri/src/native_file_jobs/legacy_backup/account_encryption.rs",
    "src-tauri/src/native_file_jobs/restore/historical.rs",
    "src-tauri/src/native_file_jobs/restore_receipts.rs",
    "src-tauri/src/asset_repository/import_payloads.rs",
    "src-tauri/src/native_file_jobs/restore.rs",
    "benchmarks/legacy-restore/entry-count.mjs",
    "benchmarks/device-backup/plugin-values.ts",
    "benchmarks/device-backup/plugin-performance.ts",
    "benchmarks/device-backup/plugin-value-driver.mjs",
    "benchmarks/device-backup/plugin-sync-driver.mjs",
    "src/ts/plugins/pluginDeviceTransfer.ts",
    "src/ts/plugins/pluginSafeClass.ts",
    "src/ts/plugins/pluginDeviceKeyspace.ts",
    "src/ts/plugins/localPluginStorageValue.ts",
    "src-tauri/src/persistent_store/device_store/plugin_values.rs",
    "src-tauri/src/persistent_store/device_store/plugin_values/text_value.rs",
    "src-tauri/src/persistent_store/device_store/sections/large_plugin.rs",
  ];
  return Object.fromEntries(
    await Promise.all(
      files.map(async (file) => [
        file,
        createHash("sha256")
          .update(await readFile(path.join(root, file)))
          .digest("hex"),
      ]),
    ),
  );
}

async function memory(page, browser, appPid, includeProcesses) {
  const heap = await page.call("Runtime.getHeapUsage");
  const value = {
    at: Date.now(),
    appPid,
    usedBytes: heap.usedSize,
    totalBytes: heap.totalSize,
    backingStorageBytes: heap.backingStorageSize ?? 0,
    embedderHeapUsedBytes: heap.embedderHeapUsedSize ?? 0,
  };
  if (includeProcesses) {
    const information = await browser.call("SystemInfo.getProcessInfo");
    const pids = [
      ...new Set([appPid, ...information.processInfo.map((entry) => entry.id)]),
    ].filter(Number.isSafeInteger);
    const result = await command("powershell.exe", [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      `Get-Process | Where-Object { $_.Id -in @(${pids.join(",")}) } | ForEach-Object { [pscustomobject]@{ pid=$_.Id; workingSetBytes=$_.WorkingSet64; peakWorkingSetBytes=$_.PeakWorkingSet64; privateBytes=$_.PrivateMemorySize64 } } | ConvertTo-Json -Compress`,
    ]);
    if (result.code !== 0) throw new Error("synthetic-memory-query-failed");
    const rows = result.stdout.trim() ? JSON.parse(result.stdout) : [];
    value.processes = Array.isArray(rows) ? rows : [rows];
    value.workingSetBytes = value.processes.reduce(
      (sum, row) => sum + row.workingSetBytes,
      0,
    );
    value.privateBytes = value.processes.reduce(
      (sum, row) => sum + row.privateBytes,
      0,
    );
    const native = value.processes.find((row) => row.pid === appPid);
    if (!native || !Number.isFinite(native.workingSetBytes) || !Number.isFinite(native.peakWorkingSetBytes))
      throw new Error("synthetic-native-memory-sample-missing");
    value.nativeWorkingSetBytes = native?.workingSetBytes;
    value.nativePeakWorkingSetBytes = native?.peakWorkingSetBytes;
    value.nativePrivateBytes = native?.privateBytes;
  }
  return value;
}

function within(candidate, parent) {
  const relative = path.relative(parent, candidate);
  return (
    relative !== "" && !relative.startsWith("..") && !path.isAbsolute(relative)
  );
}

async function reuseRun() {
  if (process.argv.length === 2) return null;
  if (process.argv.length === 3 && ['--plugin-performance', '--plugin-performance-json'].includes(process.argv[2])) return null;
  if (process.argv.length === 3 && /^--plugin-performance(?:-json)?-built=[0-9a-f-]{36}$/.test(process.argv[2])) return null;
  if (process.argv.length === 3 && /^--legacy-entries=(all|20000|50000|100000|200001)$/.test(process.argv[2])) return null;
  if (process.argv.length === 3 && /^--plugin-values=(all|json|64-string|64-json|256-string|256-json|512-string|512-json)$/.test(process.argv[2])) return null;
  const values = Object.fromEntries(
    process.argv.slice(2).map((argument) => {
      const separator = argument.indexOf("=");
      if (separator < 0) throw new Error("synthetic-runner-invalid-argument");
      return [argument.slice(0, separator), argument.slice(separator + 1)];
    }),
  );
  if (
    Object.keys(values).length !== 2 ||
    !values["--reuse"] ||
    (!values["--peer"] &&
      !values["--verify-fault"] &&
      values["--fault"] !== "activating-database")
  )
    throw new Error("synthetic-runner-invalid-argument");
  const reportPath = path.resolve(values["--reuse"]);
  const directory = path.dirname(reportPath);
  if (
    path.basename(reportPath) !== "result.json" ||
    path.dirname(directory) !== path.join(root, ".tmp") ||
    !/^device-windows-smoke-[0-9a-f-]{36}$/.test(path.basename(directory))
  )
    throw new Error("synthetic-prior-report-outside-owned-profile");
  const previous = JSON.parse(await readFile(reportPath, "utf8"));
  if (
    !previous.passed ||
    !previous.identityVerified ||
    !previous.profileName ||
    previous.state?.stage !== "done" ||
    previous.directory !== directory ||
    path.basename(directory) !== `device-windows-smoke-${previous.runId}` ||
    previous.identifier !==
      `io.github.rsyumi.risunest.devicebackupsynthetic.s${previous.runId.replaceAll("-", "")}`
  )
    throw new Error("synthetic-prior-run-not-complete");
  const executable = await readFile(path.join(directory, "RisuNest.exe"));
  if (
    createHash("sha256").update(executable).digest("hex") !==
    previous.executableSha256
  )
    throw new Error("synthetic-prior-executable-changed");
  let recoveryReport;
  if (values["--verify-fault"]) {
    const faultReportPath = path.resolve(values["--verify-fault"]);
    if (path.dirname(faultReportPath) !== directory)
      throw new Error("synthetic-fault-report-outside-profile");
    recoveryReport = JSON.parse(await readFile(faultReportPath, "utf8"));
    if (
      recoveryReport.identifier !== previous.identifier ||
      recoveryReport.executableSha256 !== previous.executableSha256 ||
      recoveryReport.failure !==
        "synthetic-fault-restore:synthetic-process-kill-point-reached" ||
      recoveryReport.killPoint?.state?.fault?.reached !== true ||
      !recoveryReport.state.checks.includes(
        "native-startup-recovery-completed",
      )
    )
      throw new Error("synthetic-fault-report-not-control-only-failure");
    recoveryReport.path = faultReportPath;
  }
  const descriptorPath = path.resolve(
    values["--peer"] ?? path.join(directory, "exchange.json"),
  );
  if (!within(descriptorPath, path.join(root, ".tmp")))
    throw new Error("synthetic-peer-descriptor-outside-fixtures");
  const peer = JSON.parse(await readFile(descriptorPath, "utf8"));
  if (
    !path.isAbsolute(peer.archivePath) ||
    !within(peer.archivePath, path.join(root, ".tmp")) ||
    !peer.archivePath.endsWith(".risunest") ||
    !Array.isArray(peer.expected) ||
    peer.expected.length !== 3 ||
    peer.expected.some((fingerprint) => !/^[0-9a-f]{64}$/.test(fingerprint))
  )
    throw new Error("synthetic-peer-descriptor-invalid");
  const archive = await readFile(peer.archivePath);
  const sha256 = createHash("sha256").update(archive).digest("hex");
  if (peer.sha256 && peer.sha256.toLowerCase() !== sha256)
    throw new Error("synthetic-peer-archive-changed");
  return {
    previous,
    recoveryReport,
    faultPoint: values["--fault"] ?? recoveryReport?.killPoint.point,
    peer: {
      archivePath: peer.archivePath,
      expected: peer.expected,
      sha256,
      bytes: archive.length,
    },
    reportPath,
  };
}

async function run() {
  if (process.platform !== "win32") throw new Error("Windows is required");
  const reuse = await reuseRun();
  const entryCountArgument = process.argv.find(argument => argument.startsWith('--legacy-entries='));
  const pluginValueArgument = process.argv.find(argument => argument.startsWith('--plugin-values='));
  const preparedPerformance = process.argv.find(argument => /^--plugin-performance(?:-json)?-built=/.test(argument))?.split('=')[1];
  const jsonPerformanceOnly = process.argv.includes('--plugin-performance-json') || process.argv.some(argument => argument.startsWith('--plugin-performance-json-built='));
  if (entryCountArgument && reuse) throw new Error('Entry measurements require a fresh synthetic profile');
  const runId = reuse?.previous.runId ?? preparedPerformance ?? randomUUID();
  const directory =
    reuse?.previous.directory ??
    path.join(root, ".tmp", `device-windows-smoke-${runId}`);
  const identifier = `io.github.rsyumi.risunest.devicebackupsynthetic.s${runId.replaceAll("-", "")}`;
  const profileName = `smoke-${runId.replaceAll("-", "").slice(0, 24)}`;
  if (!reuse) await mkdir(directory, { recursive: true });
  const reservation = net.createServer(socket => socket.destroy());
  reservation.listen(reuse?.previous.port ?? 0, "127.0.0.1");
  await once(reservation, "listening");
  const port = reservation.address().port;
  const report = {
    runId,
    identifier,
    profileName,
    directory,
    port,
    startedAt: new Date().toISOString(),
    stages: [],
    memory: [],
    environment: { windows: os.release(), cpu: os.cpus()[0]?.model, node: process.version },
    gitHead: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim(),
    productDiffSha256: createHash('sha256').update(execFileSync('git', ['diff', 'HEAD', '--', 'src', 'src-tauri', 'crates'], { cwd: root, maxBuffer: 16 * 1024 * 1024, stdio: ['ignore', 'pipe', 'ignore'] })).digest('hex'),
    sourceHashes: reuse?.previous.sourceHashes ?? (await sourceHashes()),
    ...(reuse
      ? {
          mode: reuse.faultPoint ?? "cross-import",
          peer: reuse.peer,
          baselineReport: reuse.reportPath,
          ...(reuse.recoveryReport
            ? {
                priorFaultReport: reuse.recoveryReport.path,
                killPoint: reuse.recoveryReport.killPoint,
                resumedFaultVerification: true,
              }
            : {}),
        }
      : {}),
  };
  const resultPath = path.join(
    directory,
    reuse ? `${reuse.faultPoint ?? "cross"}-${Date.now()}.json` : "result.json",
  );
  let app;
  let page;
  let browser;
  let pageOrigin;
  let appOutput = "";
  try {
    const knownFolder = await command("powershell.exe", [
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      "[Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)",
    ]);
    if (knownFolder.code !== 0 || !path.isAbsolute(knownFolder.stdout.trim())) {
      throw new Error("synthetic-known-folder-unavailable");
    }
    const profilesRoot = path.join(knownFolder.stdout.trim(), 'RisuNestProfiles-agent');
    const profileDirectory = path.join(profilesRoot, profileName);
    const nativeRoot = path.join(profileDirectory, 'data');
    const webview = path.join(profileDirectory, 'webview');
    if (!reuse && !preparedPerformance && !(await absent(profileDirectory)))
      throw new Error("synthetic-native-profile-already-exists");
    if (reuse && nativeRoot !== reuse.previous.nativeRoot)
      throw new Error("synthetic-prior-native-profile-mismatch");
    report.nativeRoot = nativeRoot;
    const executable = path.join(directory, "RisuNest.exe");
    if (preparedPerformance) {
      const config = JSON.parse(await readFile(path.join(directory, 'tauri.json'), 'utf8'));
      if (config.identifier !== identifier || !config.plugins?.risunest?.agent || config.app.windows[0].title !== title)
        throw new Error('synthetic-prepared-performance-identity-mismatch');
      // The embedded WebView endpoint is immutable in a previously built agent.
      report.port = Number(/--remote-debugging-port=(\d+)/.exec(config.app.windows[0].additionalBrowserArgs)[1]);
      report.preparedExecutable = true;
      report.executableSha256 = createHash('sha256').update(await readFile(executable)).digest('hex');
    } else if (!reuse) {
      const config = JSON.parse(
        await readFile(path.join(root, "src-tauri/tauri.conf.json"), "utf8"),
      );
      const frontend = path.join(directory, "frontend");
      config.identifier = identifier;
      config.plugins = { ...config.plugins, risunest: { ...config.plugins?.risunest, agent: true } };
      config.bundle.active = false;
      config.build = {
        ...config.build,
        beforeBuildCommand: `node node_modules/vite/bin/vite.js build --mode agent --config benchmarks/device-backup/vite.config.ts --outDir .tmp/device-windows-smoke-${runId}/frontend`,
        // FrontendDist tries URL parsing first. A namespaced absolute Windows
        // path is unambiguously a directory, so Tauri embeds the harness assets.
        frontendDist: path.toNamespacedPath(frontend),
      };
      config.app.windows = [
        {
          ...config.app.windows[0],
          label: "main",
          title,
          visible: false,
          skipTaskbar: true,
          dataDirectory: webview,
          additionalBrowserArgs: `--remote-debugging-port=${port} --remote-allow-origins=* --enable-precise-memory-info --disable-background-timer-throttling`,
        },
      ];
      const configPath = path.join(directory, "tauri.json");
      await writeFile(configPath, JSON.stringify(config, null, 2));
      process.stdout.write(`synthetic-windows-build ${runId}\n`);
      const buildStarted = Date.now();
      const build = await command(
        process.execPath,
        [
          path.join(root, "node_modules/@tauri-apps/cli/tauri.js"),
          "build",
          "--debug",
          "--no-bundle",
          "--config",
          configPath,
          '--',
          '--locked',
        ],
        {
          env: { ...process.env, CARGO_TARGET_DIR: target, RISUNEST_SMOKE_ROOT: nativeRoot },
        },
      );
      report.buildMs = Date.now() - buildStarted;
      await writeFile(
        path.join(directory, "build.log"),
        build.stdout + build.stderr,
      );
      if (build.code !== 0)
        throw new Error(`synthetic-build-failed:${build.code}`);
      if (
        JSON.stringify(await sourceHashes()) !==
        JSON.stringify(report.sourceHashes)
      )
        throw new Error("synthetic-source-changed-during-build");
      await copyFile(path.join(target, "debug/RisuNest.exe"), executable);
      report.executableSha256 = createHash("sha256")
        .update(await readFile(executable))
        .digest("hex");
    } else {
      report.executableSha256 = reuse.previous.executableSha256;
    }
    await new Promise(resolve => reservation.close(resolve));
    if (!reuse && !preparedPerformance && !(await absent(profileDirectory)))
      throw new Error("synthetic-native-profile-created-before-launch");
    async function launch() {
      app = spawn(executable, [`--profile=${profileName}`], {
        cwd: directory,
        windowsHide: true,
        stdio: ["ignore", "pipe", "pipe"],
      });
      app.stdout.on("data", (chunk) => {
        appOutput += chunk;
      });
      app.stderr.on("data", (chunk) => {
        appOutput += chunk;
      });
      const launchDeadline = Date.now() + timeoutMs;
      let endpoints;
      let endpointResponded = false;
      while (Date.now() < launchDeadline) {
        if (app.exitCode !== null)
          throw new Error(`synthetic-app-exited:${app.exitCode}`);
        try {
          const signal = AbortSignal.timeout(2_000);
          const targets = await fetch(`http://127.0.0.1:${report.port}/json/list`, {
            signal,
          }).then((response) => response.json());
          endpointResponded = true;
          const match = targets.find(
            (item) => item.type === "page" && item.title === title,
          );
          if (match) {
            const version = await fetch(
              `http://127.0.0.1:${report.port}/json/version`,
              {
                signal,
              },
            ).then((response) => response.json());
            endpoints = {
              page: match.webSocketDebuggerUrl,
              browser: version.webSocketDebuggerUrl,
            };
            break;
          }
        } catch {}
        await delay(100);
      }
      if (!endpoints)
        throw new Error(
          endpointResponded
            ? "synthetic-cdp-title-timeout"
            : "synthetic-cdp-endpoint-timeout",
        );
      page = new Cdp();
      browser = new Cdp();
      await Promise.all([
        page.connect(endpoints.page),
        browser.connect(endpoints.browser),
      ]);
      await page.call("Network.enable");
      await page.call("Network.setBlockedURLs", { urls: blockedUrls });
      await page.call("Runtime.enable");
      const identity = await evaluate(
        page,
        `(async()=>{if(document.title!==${JSON.stringify(title)})throw new Error('wrong-synthetic-title');return {identifier:await globalThis.__TAURI_INTERNALS__.invoke('plugin:app|identifier'),nativeRoot:(await globalThis.__TAURI_INTERNALS__.invoke('app_paths_roots')).data,origin:location.origin}})()`,
      );
      if (
        identity.identifier !== identifier ||
        path.resolve(identity.nativeRoot) !== path.resolve(nativeRoot)
      ) {
        throw new Error("synthetic-native-identity-mismatch");
      }
      report.identityVerified = true;
      pageOrigin = identity.origin;
      report.ownedAppPid = app.pid;
      report.environment.webview = await browser.call("Browser.getVersion");
      report.environment.originStorage = await evaluate(
        page,
        "navigator.storage.estimate()",
      );
      process.stdout.write(`synthetic-windows-identity-verified ${runId}\n`);
    }
    await launch();
    if (process.argv.includes('--plugin-performance') || jsonPerformanceOnly || preparedPerformance) {
      const readyDeadline = Date.now() + timeoutMs;
      while (!(await evaluate(page, 'typeof globalThis.__pluginPerformance?.start === "function"'))) {
        if (Date.now() >= readyDeadline) throw new Error('synthetic-plugin-performance-entry-timeout');
        await delay(100);
      }
      await evaluate(page, `globalThis.__pluginPerformance.start(${jsonPerformanceOnly ? '{binary:false}' : ''});true`);
      const deadline = Date.now() + 600_000;
      while (Date.now() < deadline) {
        const state = await evaluate(page, 'globalThis.__pluginPerformance.state()');
        report.pluginPerformance = state;
        if (state.failure) throw new Error(state.failure);
        if (jsonPerformanceOnly && state.results.some(row => row.name === 'ArrayBuffer-1MiB/correctness')) {
          // jsonCases() has returned (including its final correctness checks).
          // Stop this disposable process before repeating the binary workload.
          report.pluginPerformance = { ...state, phase: 'done', scope: 'json-only',
            stoppedBeforeRemainingBinaryWorkload: true,
            results: state.results.filter(row => /^(small-json|array-1000|array-50000|flat-json-8MiB)\//.test(row.name)) };
          if (report.pluginPerformance.results.length !== 12) throw new Error('synthetic-plugin-json-results-incomplete');
          report.passed = true;
          return;
        }
        if (state.phase === 'done') { report.passed = true; return; }
        await delay(500);
      }
      throw new Error('synthetic-plugin-performance-timeout');
    }
    if (pluginValueArgument) {
      const selected = pluginValueArgument.split('=')[1];
      const cases = selected === 'all' ? [64, 256, 512].flatMap(size => ['string', 'json'].map(kind => `${size}-${kind}`))
        : selected === 'json' ? [64, 256, 512].map(size => `${size}-json`) : [selected];
      report.pluginValues = [];
      async function retainProfile(suffix) {
        try { await browser.call('Browser.close'); } catch {}
        page.close(); browser.close();
        await stopOwnedApp(app);
        if (path.dirname(profileDirectory) !== profilesRoot || path.basename(profileDirectory) !== profileName
          || !within(nativeRoot, profileDirectory) || !within(webview, profileDirectory)) throw new Error('Synthetic profile boundary mismatch');
        const retainedProfile = `${profileDirectory}.plugin-${suffix}`;
        await rename(profileDirectory, retainedProfile);
        return path.join(retainedProfile, 'data');
      }
      for (const name of cases) {
        const [size, kind] = name.split('-');
        const measurementDirectory = path.join(directory, `plugin-${name}`);
        await mkdir(measurementDirectory);
        const measured = await measurePluginValue({
          mebibytes: Number(size), kind, directory: measurementDirectory,
          evaluate: expression => evaluate(page, expression),
          memory: () => memory(page, browser, app.pid, true),
          save: async value => { report.pluginValue = value; await writeFile(resultPath, JSON.stringify(report, null, 2)); },
        });
        report.pluginValues.push(measured);
        const server = await syntheticPluginSyncServer(process.env.RISUNEST_PLUGIN_SYNC_SERVER ?? path.join(target, 'release', 'risunest-sync-server.exe'), path.join(measurementDirectory, 'sync'));
        try {
          measured.sync = [];
          for (const sender of [true, false]) {
            const synchronized = await measurePluginSync({
              config: server.configs[sender ? 0 : 1], sender,
              evaluate: expression => evaluate(page, expression), memory: () => memory(page, browser, app.pid, true),
              save: async value => { measured.syncProgress = value; await writeFile(resultPath, JSON.stringify(report, null, 2)); },
            });
            measured.sync.push(synchronized);
            if (sender) {
              measured.retainedNativeRoot = await retainProfile(`${name}-sender`);
              await launch();
            }
          }
          const descriptor = { kind, hash: measured.state.hash, codeUnits: measured.state.codeUnits };
          await evaluate(page, `void globalThis.__pluginValueMeasurement.expect(${JSON.stringify(descriptor)}); true`);
          measured.peerReadMemory = [];
          let peer;
          const deadline = Date.now() + 30 * 60_000;
          do {
            peer = await evaluate(page, 'globalThis.__pluginValueMeasurement.state()');
            measured.peerReadMemory.push(await memory(page, browser, app.pid, true));
            if (peer.phase === 'failed' || Date.now() > deadline) throw new Error('Synthetic synced plugin verification failed');
            if (peer.busy) await delay(500);
          } while (peer.busy);
          if (peer.phase !== 'verified') throw new Error('Synthetic synced plugin verification incomplete');
          measured.syncedHash = peer.hash;
          measured.retainedPeerRoot = await retainProfile(`${name}-receiver`);
          await writeFile(path.join(measurementDirectory, 'result.json'), JSON.stringify(measured, null, 2));
          await writeFile(resultPath, JSON.stringify(report, null, 2));
        } finally { await server.stop(); }
        if (name !== cases.at(-1)) {
          await launch();
        }
      }
      report.passed = report.pluginValues.every(result => result.phase === 'verified');
      return;
    }
    if (entryCountArgument) {
      const selected = entryCountArgument.split('=')[1];
      const counts = selected === 'all' ? [20_000, 50_000, 100_000, 200_001] : [Number(selected)];
      report.entryCounts = [];
      for (const count of counts) {
        const measurementDirectory = path.join(directory, `entries-${count}`);
        await mkdir(measurementDirectory);
        report.entryCount = await measureEntryCount({
          count, directory: measurementDirectory,
          evaluate: expression => evaluate(page, expression),
          memory: () => memory(page, browser, app.pid, true),
          save: async result => {
            report.entryCount = result;
            await writeFile(resultPath, JSON.stringify(report, null, 2));
          },
        });
        report.entryCounts.push(report.entryCount);
        if (count !== counts.at(-1)) {
          try { await browser.call('Browser.close'); } catch {}
          page.close(); browser.close();
          await stopOwnedApp(app);
          if (path.dirname(profileDirectory) !== profilesRoot || path.basename(profileDirectory) !== profileName
            || !within(nativeRoot, profileDirectory) || !within(webview, profileDirectory)) throw new Error('Synthetic profile boundary mismatch');
          const retainedProfile = `${profileDirectory}.entries-${count}`;
          await rename(profileDirectory, retainedProfile);
          report.entryCount.retainedNativeRoot = path.join(retainedProfile, 'data');
          await launch();
        }
      }
      report.passed = report.entryCounts.every(result => result.phase === 'verified');
      return;
    }
    let stage = "ready";
    let stageStarted = Date.now();
    let started = !!reuse?.recoveryReport;
    let operationObserved = !reuse || !!reuse.recoveryReport;
    let killed = !!reuse?.recoveryReport;
    let controlRehydrationPending = false;
    let lastProcessSample = 0;
    while (Date.now() - stageStarted < timeoutMs) {
      if (app.exitCode !== null)
        throw new Error(`synthetic-app-exited:${app.exitCode}`);
      try {
        const state = await evaluate(
          page,
          `(()=>{if(document.title!==${JSON.stringify(title)}){if(${controlRehydrationPending}&&location.origin===${JSON.stringify(pageOrigin)}&&document.title===''&&document.readyState==='loading')return null;throw new Error('wrong-synthetic-title:'+JSON.stringify({title:document.title,state:document.readyState,origin:location.origin}))}return globalThis.__deviceBackupSmoke?.state()??null})()`,
        );
        if (!state) {
          await delay(100);
          continue;
        }
        report.state = state;
        if (
          reuse?.faultPoint &&
          killed &&
          !report.verificationControlRehydrated &&
          state.stage === "ready" &&
          state.failure === undefined &&
          state.revision === 0 &&
          state.restartCount === 0 &&
          Array.isArray(state.checks) &&
          state.checks.length === 0 &&
          Array.isArray(state.statusTransitions) &&
          state.statusTransitions.length === 0 &&
          state.fault === undefined &&
          state.jobId === undefined &&
          state.backupPath === undefined &&
          state.expected === undefined
        ) {
          const witness = report.killPoint;
          const killedTransition =
            "restore-portable-backup:running:activating-database";
          if (
            witness?.point !== reuse.faultPoint ||
            witness.state?.stage !== "fault-restore" ||
            typeof witness.state?.jobId !== "string" ||
            witness.state.jobId.length === 0 ||
            witness.state?.fault?.point !== reuse.faultPoint ||
            witness.state?.fault?.reached !== true ||
            witness.state?.statusTransitions?.at(-1) !== killedTransition ||
            !Number.isInteger(witness.pid) ||
            !Number.isInteger(witness.restartedPid) ||
            !Number.isFinite(witness.killedAt) ||
            !Number.isFinite(witness.restartedAt) ||
            witness.killedAt > witness.restartedAt ||
            witness.pid === witness.restartedPid
          )
            throw new Error("synthetic-missing-control-kill-witness-invalid");
          const completion = await evaluate(
            page,
            `(async()=>{try{await globalThis.__deviceBackupSmoke.restoreControl(${JSON.stringify(witness.state)});return 'unexpected-success'}catch(error){return error instanceof Error?error.message:String(error)}})()`,
          );
          if (completion !== "Synthetic fixture cannot be rehydrated while active")
            throw new Error("synthetic-missing-control-startup-not-complete");
          report.startupEntryCompletedWithMissingControl = true;
          report.verificationStartState = state;
          report.verificationControlRehydrated = true;
          controlRehydrationPending = true;
          await evaluate(
            page,
            `localStorage.setItem('risunest-synthetic-device-smoke-fault',${JSON.stringify(JSON.stringify(witness.state))});sessionStorage.setItem('risunest-synthetic-device-smoke',${JSON.stringify(JSON.stringify(witness.state))});setTimeout(()=>location.reload(),0);true`,
          );
          continue;
        }
        if (
          reuse?.faultPoint &&
          killed &&
          state.failure === "synthetic-process-kill-point-reached" &&
          !report.verificationControlRehydrated
        ) {
          const acknowledgement = "native-startup-recovery-completed";
          const coldRecoveryEvidence = reuse.recoveryReport?.state ?? state;
          const previousAcknowledgements = report.killPoint.state.checks.filter(
            (check) => check === acknowledgement,
          ).length;
          const coldAcknowledgements = coldRecoveryEvidence.checks.filter(
            (check) => check === acknowledgement,
          ).length;
          if (coldAcknowledgements <= previousAcknowledgements)
            throw new Error("synthetic-cold-recovery-not-acknowledged");
          const control = {
            ...report.killPoint.state,
            stage: "fault-restore",
            failure: undefined,
          };
          if (control.fault?.reached !== true)
            throw new Error("synthetic-external-kill-witness-missing");
          report.coldRecoveryState = coldRecoveryEvidence;
          report.verificationStartState = state;
          report.verificationControlRehydrated = true;
          report.productStartupRecoveryCompletedBeforeControlRehydration = true;
          controlRehydrationPending = true;
          await evaluate(
            page,
            `localStorage.setItem('risunest-synthetic-device-smoke-fault',${JSON.stringify(JSON.stringify(control))});sessionStorage.setItem('risunest-synthetic-device-smoke',${JSON.stringify(JSON.stringify(control))});setTimeout(()=>location.reload(),0);true`,
          );
          continue;
        }
        if (controlRehydrationPending) {
          if (state.failure === "synthetic-process-kill-point-reached") {
            await delay(100);
            continue;
          }
          controlRehydrationPending = false;
        }
        if (state.failure)
          throw new Error(`synthetic-${state.stage}:${state.failure}`);
        if (reuse && !started) {
          if (!["ready", "done", "fault-recovered"].includes(state.stage))
            throw new Error("synthetic-prior-profile-has-active-job");
          await evaluate(
            page,
            `globalThis.__deviceBackupSmoke.restoreControl(${JSON.stringify(reuse.previous.state)})`,
          );
          const restored = await evaluate(
            page,
            "globalThis.__deviceBackupSmoke.state()",
          );
          if (restored.stage !== "done")
            throw new Error("synthetic-prior-control-not-done");
          started = true;
          await evaluate(
            page,
            `void globalThis.__deviceBackupSmoke.restoreArchive(${JSON.stringify(reuse.peer)},${JSON.stringify(reuse.faultPoint) ?? "undefined"}); true`,
          );
          continue;
        }
        if (reuse && !operationObserved) {
          if (!["restore", "fault-restore"].includes(state.stage)) {
            await delay(100);
            continue;
          }
          operationObserved = true;
        }
        if (reuse?.faultPoint && state.fault?.reached && !killed) {
          if (
            state.stage !== "fault-restore" ||
            state.fault.point !== reuse.faultPoint
          )
            throw new Error("synthetic-fault-point-mismatch");
          report.killPoint = {
            point: reuse.faultPoint,
            reachedAt: Date.now(),
            pid: app.pid,
            state,
          };
          page.close();
          browser.close();
          const stopped = await command("taskkill.exe", [
            "/PID",
            String(app.pid),
            "/T",
            "/F",
          ]);
          if (stopped.code !== 0)
            throw new Error("synthetic-fault-process-kill-failed");
          killed = true;
          report.killPoint.killedAt = Date.now();
          await launch();
          report.killPoint.restartedAt = Date.now();
          report.killPoint.restartedPid = app.pid;
          stage = "fault-recovery";
          stageStarted = Date.now();
          continue;
        }
        if (state.stage !== stage) {
          report.stages.push({ stage, elapsedMs: Date.now() - stageStarted });
          stage = state.stage;
          stageStarted = Date.now();
          process.stdout.write(`synthetic-windows-stage ${stage}\n`);
        }
        const includeProcesses = Date.now() - lastProcessSample >= 2_000;
        const sample = await memory(page, browser, app.pid, includeProcesses);
        report.memory.push({ stage, ...sample });
        if (includeProcesses) lastProcessSample = Date.now();
        if (!started) {
          if (stage !== "ready")
            throw new Error("synthetic-profile-was-not-fresh");
          started = true;
          await evaluate(
            page,
            `void globalThis.__deviceBackupSmoke.start(); true`,
          );
        }
        if (
          (stage === "done" && !reuse?.faultPoint) ||
          (stage === "fault-recovered" && reuse?.faultPoint && killed)
        ) {
          if (!reuse) {
            const relativeArchive = path.relative(nativeRoot, state.backupPath);
            if (
              relativeArchive.startsWith("..") ||
              path.isAbsolute(relativeArchive) ||
              !state.backupPath.endsWith(".risunest")
            )
              throw new Error("synthetic-archive-outside-native-profile");
            const archivePath = path.join(directory, "export.risunest");
            await copyFile(state.backupPath, archivePath);
            const archive = await readFile(archivePath);
            report.archive = {
              archivePath,
              bytes: archive.length,
              sha256: createHash("sha256").update(archive).digest("hex"),
              expected: state.expected,
            };
            await writeFile(
              path.join(directory, "exchange.json"),
              JSON.stringify(
                {
                  ...report.archive,
                  sourcePlatform: "windows",
                  sourceReport: path.join(directory, "result.json"),
                },
                null,
                2,
              ),
            );
          }
          if (report.killPoint && !reuse?.recoveryReport)
            report.killPoint.postKillVerificationMs =
              Date.now() - report.killPoint.killedAt;
          if (reuse?.faultPoint) {
            if (
              state.fault?.outcome !== "old" &&
              state.fault?.outcome !== "committed"
            )
              throw new Error("synthetic-fault-recovery-outcome-missing");
            report.faultRecoveryOutcome = state.fault.outcome;
          }
          report.passed = true;
          break;
        }
      } catch (error) {
        if (
          !/Cannot find context|Execution context was destroyed|Cannot find default execution context|Inspected target navigated/i.test(
            error.message,
          )
        )
          throw error;
      }
      await delay(250);
    }
    if (!report.passed) throw new Error(`synthetic-stage-timeout:${stage}`);
  } catch (error) {
    report.passed = false;
    report.failure = error.message;
    process.exitCode = 1;
  } finally {
    if (reservation.listening) reservation.close();
    if (report.passed && browser) {
      report.gracefulBrowserCloseRequested = true;
      try {
        await browser.call("Browser.close");
        report.gracefulBrowserCloseResponded = true;
      } catch {
        report.gracefulBrowserCloseResponded = false;
      }
    }
    page?.close();
    browser?.close();
    await stopOwnedApp(app);
    report.completedAt = new Date().toISOString();
    await writeFile(resultPath, JSON.stringify(report, null, 2));
    // This executable and native identifier were generated for this run only.
    await writeFile(
      reuse
        ? resultPath.replace(/\.json$/, ".app.log")
        : path.join(directory, "app.log"),
      appOutput,
    );
    process.stdout.write(
      JSON.stringify({
        passed: report.passed,
        failure: report.failure,
        stage: report.state?.stage,
        result: resultPath,
      }) + "\n",
    );
  }
}

await run();
