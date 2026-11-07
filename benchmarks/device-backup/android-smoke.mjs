import assert from "node:assert/strict";
import { measureLegacyRestore } from "../legacy-restore/android-measure.mjs";
import { measureEntryCount } from "../legacy-restore/entry-count.mjs";
import { measurePluginValue } from './plugin-value-driver.mjs';
import { measurePluginSync, syntheticPluginSyncServer } from './plugin-sync-driver.mjs';
import { execFile, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";
import { pipeline } from "node:stream/promises";
import { cutDeviceNetwork } from "../../scripts/phase3AndroidSmoke.mjs";
import { REALM_BLOCKED_URL_PATTERNS } from "../../scripts/realmBlocklist.mjs";
import { androidTarget, runnerOptions } from "../support/androidTarget.mjs";

const packageName = "io.github.rsyumi.risunest";
const syntheticTitle = "RisuNest synthetic device backup smoke";
const port = 19369;
const execute = promisify(execFile);
const delay = (milliseconds) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));
const args = runnerOptions(process.argv.slice(2));
for (const name of Object.keys(args))
  assert.ok(
    ["adb", "aapt", "adb-port", "serial", "avd", "apk", "output", "health", "peer", "exchange", "legacy-mb", "legacy-encoding", "legacy-binary-bytes", "legacy-entries", "plugin-values", "sync-server"].includes(name),
    "Unsupported runner option",
  );
const android = androidTarget(args);
const { adb, serial, avd: avdName } = android;
const apk = args.apk && path.resolve(args.apk);
const output = path.resolve(
  args.output ?? ".tmp/device-webview-android/result.json",
);
const healthPath = path.resolve(
  args.health ?? ".tmp/device-webview-android/health-summary.json",
);
const peerPath = args.peer && path.resolve(args.peer);
const exchangePath = path.resolve(
  args.exchange ?? ".tmp/device-webview-android/exchange.json",
);
let phase = "identity";
let client;
let started = false;
let forwarding = false;
let validatedAvd = false;
let completed = false;
let importedArchive;
let state;
let pluginReport;
let peakNativeRssKiB = null;
let peakNativePssKiB = null;
const rendererHeap = {
  usedSize: null,
  totalSize: null,
  backingStorageSize: null,
  samples: 0,
};

async function run(command, timeout = 10_000, allowFailure = false) {
  try {
    return await execute(adb, android.args(command), {
      env: android.env,
      windowsHide: true,
      timeout,
      encoding: "utf8",
      maxBuffer: 2 * 1024 * 1024,
    });
  } catch (error) {
    if (allowFailure) return { stdout: "", stderr: "", failed: true };
    throw new Error(`Synthetic ADB ${command.slice(0, 3).join(' ')} failed (${error.code ?? 'unknown'}, ${error.signal ?? 'no signal'})`, { cause: error });
  }
}

async function launchSyntheticApp() {
  const deadline = Date.now() + 90_000;
  let pid = '', stableSince = 0, nextLaunch = 0, launches = 0;
  while (Date.now() < deadline) {
    const current = (await run(['shell', 'pidof', packageName], 5000, true)).stdout.trim();
    if (!/^\d+$/.test(current)) {
      pid = ''; stableSince = 0;
      if (Date.now() >= nextLaunch) {
        await run(['shell', 'am', 'start', '-n', `${packageName}/.MainActivity`]);
        launches++;
        nextLaunch = Date.now() + 5000;
      }
    } else if (current !== pid) {
      pid = current; stableSince = Date.now();
    } else if (Date.now() - stableSince >= 15_000) {
      // Android can kill a new process when the old task's 10-second destroy timeout expires.
      return { pid, launches };
    }
    await delay(250);
  }
  throw new Error('Synthetic Android process did not remain alive after data clearing');
}

function progress(next) {
  phase = next;
  console.log(JSON.stringify({ phase }));
}

async function fileHash(file) {
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(file)) digest.update(chunk);
  return digest.digest("hex");
}

function validateFingerprints(expected) {
  assert.ok(
    Array.isArray(expected) &&
      expected.length === 3 &&
      expected.every(
        (value) => typeof value === "string" && /^[a-f0-9]{64}$/.test(value),
      ),
    "Three synthetic section fingerprints are required",
  );
}

async function retainArchive() {
  validateFingerprints(state.expected);
  assert.match(
    state.backupPath,
    /^\/data\/(?:user\/0|data)\/io\.github\.rsyumi\.risunest\/files\/synthetic-device-backup\.risunest$/,
    "Archive must be the synthetic app's published private destination",
  );
  const archivePath = path.join(
    path.dirname(exchangePath),
    `android-${randomUUID()}.risunest`,
  );
  await mkdir(path.dirname(archivePath), { recursive: true });
  const child = spawn(
    adb,
    android.args(["exec-out", "run-as", packageName, "cat", state.backupPath]),
    {
      env: android.env,
      windowsHide: true,
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 60_000,
    },
  );
  await Promise.all([
    pipeline(child.stdout, createWriteStream(archivePath, { flags: "wx" })),
    new Promise((resolve, reject) => {
      child.on("error", reject);
      child.on("exit", (code) =>
        code === 0
          ? resolve()
          : reject(new Error("Synthetic archive extraction failed")),
      );
    }),
  ]).catch((error) => {
    child.kill();
    throw error;
  });
  const descriptor = {
    archivePath,
    expected: state.expected,
    sha256: await fileHash(archivePath),
  };
  assert.equal(
    (
      await run(["shell", "run-as", packageName, "sha256sum", state.backupPath])
    ).stdout
      .trim()
      .split(/\s/)[0],
    descriptor.sha256,
    "Extracted synthetic archive bytes changed",
  );
  await writeFile(exchangePath, JSON.stringify(descriptor, null, 2));
  await writeFile(
    path.join(path.dirname(exchangePath), "completed-control.json"),
    JSON.stringify(state, null, 2),
  );
  return descriptor;
}

async function startPeerRestore() {
  const peer = JSON.parse(await readFile(peerPath, "utf8"));
  validateFingerprints(peer.expected);
  assert.ok(
    path.isAbsolute(peer.archivePath) &&
      path.extname(peer.archivePath) === ".risunest",
    "Explicit peer synthetic archive required",
  );
  const sha256 = await fileHash(peer.archivePath);
  if (peer.sha256)
    assert.equal(sha256, peer.sha256, "Peer archive checksum changed");
  importedArchive = `/data/local/tmp/risunest-device-backup-${randomUUID()}.risunest`;
  await run(["push", peer.archivePath, importedArchive], 60_000);
  assert.equal(
    (await run(["shell", "sha256sum", importedArchive])).stdout
      .trim()
      .split(/\s/)[0],
    sha256,
    "Transferred peer bytes changed",
  );
  await client.evaluate(`globalThis.__deviceBackupSmoke.restoreControl({
    ...globalThis.__deviceBackupSmoke.state(), checks:[],
    maxNativeProgressBytes:0,statusTransitions:[],peakHeapBytes:0,
    captureMs:undefined,restoreMs:undefined,cancellationMs:undefined
  })`);
  await client.evaluate(
    `void globalThis.__deviceBackupSmoke.restoreArchive(${JSON.stringify({ archivePath: importedArchive, expected: peer.expected })})`,
    false,
  );
  return { sha256, expected: peer.expected };
}

async function connect() {
  const deadline = Date.now() + 30_000;
  let target;
  let discovery = {};
  while (Date.now() < deadline) {
    const targets = await fetch(`http://127.0.0.1:${port}/json/list`, {
      signal: AbortSignal.timeout(3000),
      headers: { connection: 'close' },
    })
      .then((response) => response.json())
      .then((entries) => {
        discovery = { targets: entries.map(({ type, title, url }) => ({ type, title, url })) };
        return entries;
      })
      .catch((error) => {
        discovery = { error: error.message, cause: error.cause?.code };
        return [];
      });
    target = targets.find(
      (entry) =>
        entry.type === "page" &&
        entry.title === syntheticTitle &&
        new URL(entry.url).origin === "http://tauri.localhost",
    );
    if (target) break;
    await delay(250);
  }
  assert.ok(
    target,
    `Separate synthetic APK title/origin not found; refusing page evaluation: ${JSON.stringify(discovery)}`,
  );
  const websocketUrl = new URL(target.webSocketDebuggerUrl);
  assert.ok(
    ["127.0.0.1", "localhost"].includes(websocketUrl.hostname),
    "Nonlocal CDP endpoint rejected",
  );
  assert.equal(Number(websocketUrl.port), port, "Unexpected CDP port rejected");
  const socket = new WebSocket(websocketUrl);
  await new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      socket.close();
      reject(new Error("CDP connection timed out"));
    }, 10_000);
    socket.onopen = () => {
      clearTimeout(timeout);
      resolve();
    };
    socket.onerror = () => {
      clearTimeout(timeout);
      reject(new Error("CDP connection failed"));
    };
  });
  let sequence = 0;
  const pending = new Map();
  socket.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    const request = pending.get(message.id);
    if (!request) return; // Never collect console events, request bodies, DOM text, or exception objects.
    clearTimeout(request.timeout);
    pending.delete(message.id);
    if (message.error) request.reject(new Error("CDP command failed"));
    else request.resolve(message.result);
  };
  const close = () => {
    for (const request of pending.values()) {
      clearTimeout(request.timeout);
      request.reject(new Error("CDP closed"));
    }
    pending.clear();
    socket.close();
  };
  socket.onclose = () => {
    for (const request of pending.values()) {
      clearTimeout(request.timeout);
      request.reject(new Error("CDP disconnected"));
    }
    pending.clear();
  };
  const call = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = ++sequence;
      const timeout = setTimeout(() => {
        pending.delete(id);
        reject(new Error("CDP command timed out"));
      }, args['plugin-values'] ? 120_000 : 15_000);
      pending.set(id, { resolve, reject, timeout });
      socket.send(JSON.stringify({ id, method, params }));
    });
  const evaluate = async (expression, awaitPromise = true) => {
    const response = await call("Runtime.evaluate", {
      expression,
      awaitPromise,
      returnByValue: true,
    });
    assert.ok(!response.exceptionDetails, "Synthetic page evaluation failed");
    return response.result.value;
  };
  try {
    await call("Network.enable");
    await call("Network.setBlockedURLs", {
      urls: [
        ...REALM_BLOCKED_URL_PATTERNS,
        "*update.rsyumi.workers.dev/translator/prompt-presets.json*",
      ],
    });
    await call("Page.enable");
    // Only identity is evaluated until the native application identity is established.
    assert.equal(
      await evaluate(
        "window.__TAURI_INTERNALS__?.invoke('plugin:app|identifier')",
      ),
      packageName,
    );
    assert.equal(await evaluate("document.title"), syntheticTitle);
    return { call, evaluate, close };
  } catch (error) {
    close();
    throw error;
  }
}

async function measureNativeMemory(pid) {
  const memory = await run(["shell", "dumpsys", "meminfo", pid], 10_000, true);
  const rss = /TOTAL RSS:\s*(\d+)/.exec(memory.stdout);
  const pss = /TOTAL PSS:\s*(\d+)/.exec(memory.stdout);
  if (rss) peakNativeRssKiB = Math.max(peakNativeRssKiB ?? 0, Number(rss[1]));
  if (pss) peakNativePssKiB = Math.max(peakNativePssKiB ?? 0, Number(pss[1]));
}

async function measureRendererHeap() {
  const usage = await client.call("Runtime.getHeapUsage").catch(() => null);
  if (!usage) return;
  rendererHeap.samples++;
  for (const name of ["usedSize", "totalSize", "backingStorageSize"]) {
    if (typeof usage[name] === "number" && Number.isFinite(usage[name])) {
      rendererHeap[name] = Math.max(rendererHeap[name] ?? 0, usage[name]);
    }
  }
}

function metrics(value) {
  if (!value) return null;
  return {
    stage: value.stage,
    failure:
      typeof value.failure === "string" ? value.failure.slice(0, 1024) : null,
    captureMs: value.captureMs ?? null,
    restoreMs: value.restoreMs ?? null,
    cancellationMs: value.cancellationMs ?? null,
    maxNativeProgressBytes: value.maxNativeProgressBytes,
    statusTransitions: value.statusTransitions,
    restartCount: value.restartCount,
    peakRendererHeapBytes: value.peakHeapBytes || null,
    checks: value.checks,
  };
}

async function main() {
  assert.ok(
    adb && apk,
    "A separate agent-built --apk is required",
  );
  android.assertAvd((await run(["emu", "avd", "name"])).stdout);
  validatedAvd = true;
  assert.equal(
    (await run(["shell", "getprop", "sys.boot_completed"])).stdout.trim(),
    "1",
  );
  const health = JSON.parse(await readFile(healthPath, "utf8"));
  assert.ok(
    health.Serial === serial &&
      health.Seconds >= 120 &&
      health.Samples >= 8 &&
      health.Failures === 0 &&
      health.Passed === true &&
      health.HashMatch === true &&
      health.Push === true &&
      health.Pull === true &&
      health.ConsolePing === true,
    "Documented sustained ADB health and transfer proof required",
  );
  assert.ok(
    Date.now() - (await stat(healthPath)).mtimeMs < 30 * 60_000,
    "Repeat stale ADB health proof",
  );
  const sha256 = await fileHash(apk);
  const { stdout: apkMetadata } = await execute(
    android.aapt,
    ["dump", "badging", apk],
    { windowsHide: true, timeout: 10_000, encoding: "utf8" },
  );
  assert.equal(
    /^package: name='([^']+)'/m.exec(apkMetadata)?.[1],
    packageName,
    "Unexpected synthetic APK package",
  );
  progress("cut-synthetic-guest-network");
  // The shared helper expects a synchronous adapter, but its network commands are small and bounded.
  const { spawnSync } = await import("node:child_process");
  await cutDeviceNetwork(serial, {
    run(target, command, { allowFailure = false } = {}) {
      assert.equal(target, serial);
      const result = spawnSync(adb, android.args(command, target), {
        env: android.env,
        windowsHide: true,
        timeout: 5000,
        encoding: "utf8",
      });
      if (!allowFailure)
        assert.equal(
          result.status,
          0,
          "Synthetic guest network command failed",
        );
      return result;
    },
    sleep: delay,
  });
  if (!peerPath) {
    progress("install-separate-synthetic-apk");
    // Only this verified disposable AVD may have the fixed Android JNI package cleared.
    await run(["shell", "am", "force-stop", packageName], 10_000, true);
    const installed = await run(["install", "-r", "-t", apk], 120_000);
    assert.match(installed.stdout, /Success/);
    assert.match(
      (await run(["shell", "pm", "clear", packageName])).stdout,
      /Success/,
    );
    // The startup notification prompt would cover and pause the WebView. Android 12 and older reject the grant.
    await run(
      ["shell", "pm", "grant", packageName, "android.permission.POST_NOTIFICATIONS"],
      10_000,
      true,
    );
    progress("launch-synthetic-harness");
    started = true;
  } else progress("connect-existing-synthetic-peer-fixture");
  let pid = peerPath ? undefined : (await launchSyntheticApp()).pid;
  const launchDeadline = Date.now() + 30_000;
  while (!pid && Date.now() < launchDeadline) {
    pid = (
      await run(["shell", "pidof", packageName], 5000, true)
    ).stdout.trim();
    if (/^\d+$/.test(pid)) break;
    await delay(250);
  }
  assert.match(pid, /^\d+$/, "Synthetic native app process unavailable");
  await run([
    "forward",
    `tcp:${port}`,
    `localabstract:webview_devtools_remote_${pid}`,
  ]);
  forwarding = true;
  client = await connect();
  const environment = await client.evaluate(
    "({userAgent:navigator.userAgent,devicePixelRatio,width:innerWidth,height:innerHeight})",
  );
  const bootDeadline = Date.now() + 30_000;
  while (Date.now() < bootDeadline) {
    state = await client.evaluate(
      "globalThis.__deviceBackupSmoke?.state() ?? null",
    );
    if (state) break;
    await delay(100);
  }
  assert.equal(
    state?.stage,
    peerPath ? "done" : "ready",
    "Synthetic harness state does not match the requested run",
  );
  assert.ok(!state.failure, "Synthetic bootstrap failed before start");
  if (args['plugin-values']) {
    assert.match(args['plugin-values'], /^(64|256|512)-(string|json)$/);
    assert.ok(args['sync-server'] && !peerPath, 'A synthetic server executable and a fresh app profile are required');
    const [size, kind] = args['plugin-values'].split('-');
    const directory = path.join(path.dirname(output), `plugin-${args['plugin-values']}`);
    await mkdir(directory, { recursive: true });
    let currentPid = pid;
    const sample = async () => {
      const usage = await client.call('Runtime.getHeapUsage');
      const status = await run(['shell', 'run-as', packageName, 'cat', `/proc/${currentPid}/status`]);
      const rss = /^VmRSS:\s*(\d+)\s+kB$/m.exec(status.stdout);
      const hwm = /^VmHWM:\s*(\d+)\s+kB$/m.exec(status.stdout);
      assert.ok(rss && hwm, 'Native memory sample unavailable');
      return { at: Date.now(), nativePid: Number(currentPid), usedBytes: usage.usedSize, totalBytes: usage.totalSize, backingStorageBytes: usage.backingStorageSize ?? 0,
        nativeWorkingSetBytes: Number(rss[1]) * 1024, nativePeakWorkingSetBytes: Number(hwm[1]) * 1024 };
    };
    const report = { platform: 'android-emulator', synthetic: true, success: false, apkSha256: sha256, environment, health, avd: avdName, serial, nativePids: [Number(pid)] };
    pluginReport = report;
    const save = async () => { await writeFile(output, JSON.stringify(report, null, 2)); };
    progress(`plugin-${args['plugin-values']}`);
    report.measurement = await measurePluginValue({ mebibytes: Number(size), kind, directory,
      evaluate: expression => client.evaluate(expression), memory: sample,
      save: async value => { report.measurement = value; await save(); },
    });
    const server = await syntheticPluginSyncServer(path.resolve(args['sync-server']), path.join(directory, 'sync'));
    try {
      await run(['reverse', `tcp:${server.port}`, `tcp:${server.port}`]);
      report.sync = [];
      for (const sender of [true, false]) {
        report.sync.push(await measurePluginSync({ config: server.configs[sender ? 0 : 1], sender,
          evaluate: expression => client.evaluate(expression), memory: sample,
          save: async value => { report.syncProgress = value; await save(); },
        }));
        if (sender) {
          report.senderIdentity = await client.evaluate("globalThis.__TAURI_INTERNALS__.invoke('server_sync_status').then(s=>({writerId:s.writerId,deviceId:s.deviceId}))");
          client.close();
          await run(['shell', 'am', 'force-stop', packageName]);
          assert.match((await run(['shell', 'pm', 'clear', packageName])).stdout, /Success/);
          await run(['shell', 'pm', 'grant', packageName, 'android.permission.POST_NOTIFICATIONS'], 10_000, true);
          const launched = await launchSyntheticApp();
          currentPid = launched.pid;
          report.receiverLaunches = launched.launches;
          report.nativePids.push(Number(currentPid));
          await run(['forward', `tcp:${port}`, `localabstract:webview_devtools_remote_${currentPid}`]);
          client = await connect();
          const deadline = Date.now() + 30_000;
          while (!(await client.evaluate('!!globalThis.__pluginValueMeasurement'))) { assert.ok(Date.now() < deadline); await delay(100); }
        }
      }
      report.receiverIdentity = await client.evaluate("globalThis.__TAURI_INTERNALS__.invoke('server_sync_status').then(s=>({writerId:s.writerId,deviceId:s.deviceId}))");
      assert.notEqual(report.senderIdentity.writerId, report.receiverIdentity.writerId);
      const descriptor = { kind, hash: report.measurement.state.hash, codeUnits: report.measurement.state.codeUnits };
      await client.evaluate(`void globalThis.__pluginValueMeasurement.expect(${JSON.stringify(descriptor)}); true`);
      report.peerReadMemory = [];
      let peer;
      const deadline = Date.now() + 30 * 60_000;
      do {
        peer = await client.evaluate('globalThis.__pluginValueMeasurement.state()');
        report.peerReadMemory.push(await sample());
        assert.ok(peer.phase !== 'failed' && Date.now() < deadline, 'Synced plugin verification failed');
        if (peer.busy) await delay(500);
      } while (peer.busy);
      assert.equal(peer.phase, 'verified');
      assert.equal(peer.hash, descriptor.hash);
      report.syncedHash = peer.hash;
      report.success = true;
      await save();
      completed = true;
      console.log(JSON.stringify({ platform: report.platform, success: true, output }));
    } finally {
      await run(['reverse', '--remove', `tcp:${server.port}`], 10_000, true);
      await server.stop();
    }
    return;
  }
  if (args['legacy-entries']) {
    assert.ok(!peerPath, 'Entry measurements require a fresh synthetic process');
    const count = Number(args['legacy-entries']);
    const directory = path.join(path.dirname(output), `entries-${count}-${randomUUID()}`);
    await mkdir(directory, { recursive: true });
    const report = { platform: 'android-emulator', synthetic: true, success: false, apkSha256: sha256, environment, health, avd: avdName, serial };
    const ownedSources = [];
    try {
      report.measurement = await measureEntryCount({ count, directory,
        evaluate: expression => client.evaluate(expression),
        prepareSource: async file => {
          const name = `synthetic-entry-${randomUUID()}.bin`;
          const temporary = `/data/local/tmp/${name}`;
          const native = `/data/user/0/${packageName}/files/${name}`;
          ownedSources.push(native);
          try {
            await run(['push', file, temporary], 120_000);
            await run(['shell', 'run-as', packageName, 'cp', temporary, native], 120_000);
          } finally { await run(['shell', 'rm', '-f', temporary], 10_000, true); }
          return native;
        },
        memory: async () => {
          const usage = await client.call('Runtime.getHeapUsage');
          const status = await run(['shell', 'run-as', packageName, 'cat', `/proc/${pid}/status`]);
          const rss = /^VmRSS:\s*(\d+)\s+kB$/m.exec(status.stdout);
          const hwm = /^VmHWM:\s*(\d+)\s+kB$/m.exec(status.stdout);
          assert.ok(rss && hwm, 'Native memory sample unavailable');
          return { usedBytes: usage.usedSize, nativeWorkingSetBytes: Number(rss[1]) * 1024, nativePeakWorkingSetBytes: Number(hwm[1]) * 1024 };
        },
        save: async measurement => { report.measurement = measurement; await writeFile(output, JSON.stringify(report, null, 2)); },
      });
      report.success = true;
      completed = true;
      await writeFile(output, JSON.stringify(report, null, 2));
      console.log(JSON.stringify({ success: true, output }));
    } finally {
      for (const file of ownedSources) await run(['shell', 'run-as', packageName, 'rm', '-f', file], 10_000, true);
    }
    return;
  }
  if (args['legacy-mb'] || args['legacy-encoding'] || args['legacy-binary-bytes']) {
    const measurement = await measureLegacyRestore({ args, client, run, pid, packageName, output,
      apkSha256: sha256, environment });
    completed = measurement.success;
    console.log(JSON.stringify(measurement));
    if (!completed) process.exitCode = 1;
    return;
  }
  const startedAt = Date.now();
  const peer = peerPath ? await startPeerRestore() : null;
  if (!peer)
    await client.evaluate("void globalThis.__deviceBackupSmoke.start()", false);
  let lastStage;
  let lastNativeTransition;
  let observedActiveStage = false;
  let stageDeadline = Date.now() + 120_000;
  let nextMemory = 0;
  while (true) {
    assert.ok(
      Date.now() < stageDeadline,
      "Synthetic device phase did not advance within 120 seconds",
    );
    const next = await client
      .evaluate(
        `document.title === ${JSON.stringify(syntheticTitle)} ? globalThis.__deviceBackupSmoke?.state() ?? null : null`,
      )
      .catch(() => null);
    if (next) {
      if (next.stage !== "done") observedActiveStage = true;
      if (peer && next.stage === "done" && !observedActiveStage) {
        await delay(100);
        continue;
      }
      state = next;
      assert.ok(!state.failure, `Synthetic harness failed: ${state.failure}`);
      if (state.stage !== lastStage) {
        lastStage = state.stage;
        stageDeadline = Date.now() + 120_000;
        progress(state.stage);
      }
      const nativeTransition = state.statusTransitions.at(-1);
      if (nativeTransition !== lastNativeTransition) {
        lastNativeTransition = nativeTransition;
        stageDeadline = Date.now() + 120_000;
      }
      if (state.stage === "done") break;
    }
    if (Date.now() >= nextMemory) {
      await measureNativeMemory(pid);
      await measureRendererHeap();
      nextMemory = Date.now() + 2000;
    }
    await delay(200);
  }
  assert.ok(
    state.statusTransitions.some((value) =>
      value.startsWith("export-portable-backup:"),
    ) &&
      state.statusTransitions.some((value) =>
        value.startsWith("restore-portable-backup:"),
      ),
    "Native file-job phases were not observed",
  );
  const storage = await client.evaluate(
    "navigator.storage.estimate().then(({usage,quota})=>({usage,quota}))",
  );
  const exchange = peer ? null : await retainArchive();
  if (peer)
    assert.deepEqual(
      state.expected,
      peer.expected,
      "Peer fingerprints changed",
    );
  const report = {
    platform: "android-emulator",
    synthetic: true,
    success: true,
    avd: avdName,
    serial,
    apkSha256: sha256,
    peerArchiveSha256: peer?.sha256 ?? null,
    exchange,
    health,
    environment,
    elapsedMs: Date.now() - startedAt,
    peakNativeRssKiB,
    peakNativePssKiB,
    rendererHeap,
    storage,
    ...metrics(state),
  };
  await mkdir(path.dirname(output), { recursive: true });
  await writeFile(output, JSON.stringify(report, null, 2));
  completed = true;
  console.log(JSON.stringify(report));
}

main()
  .catch(async (error) => {
    const report = {
      ...pluginReport,
      platform: "android-emulator",
      synthetic: true,
      success: false,
      phase,
      error: error.message,
      peakNativeRssKiB,
      peakNativePssKiB,
      rendererHeap,
      ...metrics(state),
    };
    await mkdir(path.dirname(output), { recursive: true });
    await writeFile(output, JSON.stringify(report, null, 2));
    console.error(JSON.stringify({ platform: report.platform, success: false, phase, error: report.error, output }));
    process.exitCode = 1;
  })
  .finally(async () => {
    client?.close();
    if (validatedAvd && started && !completed)
      await run(["shell", "am", "force-stop", packageName], 10_000, true);
    if (importedArchive)
      await run(["shell", "rm", importedArchive], 10_000, true);
    if (forwarding)
      await run(["forward", "--remove", `tcp:${port}`], 10_000, true);
  });
