import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { createReadStream, createWriteStream } from "node:fs";
import { mkdir, readFile, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";
import { pipeline } from "node:stream/promises";
import { cutDeviceNetwork } from "../../scripts/phase3AndroidSmoke.mjs";
import { REALM_BLOCKED_URL_PATTERNS } from "../../scripts/realmBlocklist.mjs";

// This runner deliberately has its own fixed device guard. It must not relax or reuse
// the startup benchmark's distinct emulator-5580/risunest_startup_synthetic guard.
const serial = "emulator-5554";
const avdName = "risunest_vm_retest";
const packageName = "io.github.rsyumi.risunest";
const syntheticTitle = "RisuNest synthetic device backup smoke";
const port = 19369;
const execute = promisify(execFile);
const delay = (milliseconds) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));
const args = Object.fromEntries(
  process.argv.slice(2).map((argument) => {
    const equal = argument.indexOf("=");
    assert.ok(
      argument.startsWith("--") && equal > 2,
      "Use --name=value arguments",
    );
    return [argument.slice(2, equal), argument.slice(equal + 1)];
  }),
);
for (const name of Object.keys(args))
  assert.ok(
    ["adb", "apk", "output", "health", "peer", "exchange"].includes(name),
    "Unsupported runner option",
  );
const adb = args.adb;
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
    return await execute(adb, ["-s", serial, ...command], {
      windowsHide: true,
      timeout,
      encoding: "utf8",
      maxBuffer: 2 * 1024 * 1024,
    });
  } catch (error) {
    if (allowFailure) return { stdout: "", stderr: "", failed: true };
    throw new Error(`Synthetic ADB ${command[0]} failed`, { cause: error });
  }
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
      expected.length === 4 &&
      expected.every(
        (value) => typeof value === "string" && /^[a-f0-9]{64}$/.test(value),
      ),
    "Four synthetic section fingerprints are required",
  );
}

async function retainArchive() {
  validateFingerprints(state.expected);
  assert.match(
    state.backupPath,
    /^\/data\/(?:user\/0|data)\/io\.github\.rsyumi\.risunest\/native-file-jobs\/handoffs\/risunest-backup-[a-f0-9-]{36}\.risunest$/,
    "Archive must be the synthetic app's managed native handoff",
  );
  const archivePath = path.join(
    path.dirname(exchangePath),
    `android-${randomUUID()}.risunest`,
  );
  await mkdir(path.dirname(archivePath), { recursive: true });
  const child = spawn(
    adb,
    ["-s", serial, "exec-out", "run-as", packageName, "cat", state.backupPath],
    {
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
    maxIpcBytes:0,totalIpcBytes:0,peakHeapBytes:0,
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
  while (Date.now() < deadline) {
    const targets = await fetch(`http://127.0.0.1:${port}/json/list`, {
      signal: AbortSignal.timeout(3000),
    })
      .then((response) => response.json())
      .catch(() => []);
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
    "Separate synthetic APK title/origin not found; refusing page evaluation",
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
      }, 15_000);
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
    maxIpcBytes: value.maxIpcBytes,
    totalIpcBytes: value.totalIpcBytes,
    peakRendererHeapBytes: value.peakHeapBytes || null,
    checks: value.checks,
  };
}

async function main() {
  assert.ok(
    adb && apk,
    "Explicit --adb and separate agent-built --apk are required",
  );
  assert.equal(
    process.env.ANDROID_ADB_SERVER_PORT,
    "15037",
    "Owned private ADB server is required",
  );
  assert.equal(
    process.env.ADB_SERVER_SOCKET,
    "tcp:127.0.0.1:15037",
    "Unexpected ADB server socket",
  );
  assert.equal(
    (await run(["emu", "avd", "name"])).stdout.split("\n")[0].trim(),
    avdName,
    "Unsafe Android AVD",
  );
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
    path.resolve(path.dirname(adb), "../build-tools/36.0.0/aapt.exe"),
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
      const result = spawnSync(adb, ["-s", target, ...command], {
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
    progress("launch-synthetic-harness");
    await run(["shell", "am", "start", "-n", `${packageName}/.MainActivity`]);
    started = true;
  } else progress("connect-existing-synthetic-peer-fixture");
  let pid;
  const launchDeadline = Date.now() + 30_000;
  while (Date.now() < launchDeadline) {
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
  const startedAt = Date.now();
  const peer = peerPath ? await startPeerRestore() : null;
  if (!peer)
    await client.evaluate("void globalThis.__deviceBackupSmoke.start()", false);
  let lastStage;
  let observedActiveStage = false;
  let stageDeadline = Date.now() + 120_000;
  let nextMemory = 0;
  while (true) {
    assert.ok(
      Date.now() < stageDeadline,
      "Synthetic device stage exceeded 120 seconds",
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
    state.maxIpcBytes > 0 && state.maxIpcBytes <= 256 * 1024,
    "IPC bytes were not bounded",
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
    console.error(JSON.stringify(report));
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
