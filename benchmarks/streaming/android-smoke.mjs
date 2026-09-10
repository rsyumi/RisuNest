import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { createHash } from "node:crypto";
import path from "node:path";
import { cutDeviceNetwork } from "../../scripts/phase3AndroidSmoke.mjs";
import { REALM_BLOCKED_URL_PATTERNS } from "../../scripts/realmBlocklist.mjs";

const options = Object.fromEntries(
  process.argv.slice(2).map((arg) => arg.replace(/^--/, "").split("=")),
);
const adb = options.adb;
assert.ok(
  !options.device || options.device === "api35",
  "Unknown synthetic device",
);
const serial = options.device === "api35" ? "emulator-5556" : "emulator-5554";
const avd =
  options.device === "api35"
    ? "risunest_buffer_api35_synthetic"
    : "risunest_vm_retest";
const packageName = "io.github.rsyumi.risunest";
const port = 19367;
const apk = path.resolve(
  options.apk ??
    "src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk",
);
const output = path.resolve(
  options.output ?? "benchmarks/streaming/android-result.local.json",
);
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function run(target, args, { allowFailure = false } = {}) {
  const result = spawnSync(adb, ["-s", target, ...args], {
    encoding: "utf8",
    windowsHide: true,
    timeout: args[0] === "install" ? 120000 : 10000,
  });
  if (!allowFailure && result.status !== 0)
    throw new Error(`ADB failed: ${args[0]}`);
  return result;
}

async function connect(onProgress) {
  const deadline = Date.now() + 30000;
  let target;
  while (Date.now() < deadline) {
    const targets = await fetch(`http://127.0.0.1:${port}/json/list`, {
      signal: AbortSignal.timeout(3000),
    })
      .then((res) => res.json())
      .catch(() => []);
    target = targets.find(
      (entry) =>
        entry.type === "page" && entry.url.startsWith("http://tauri.localhost"),
    );
    if (target) break;
    await delay(250);
  }
  assert.ok(target, "Synthetic APK WebView unavailable");
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("CDP connection timeout")),
      10000,
    );
    ws.onopen = () => {
      clearTimeout(timer);
      resolve();
    };
    ws.onerror = () => {
      clearTimeout(timer);
      reject(new Error("CDP connection error"));
    };
  });
  let sequence = 0;
  const pending = new Map();
  ws.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    if (
      message.method === "Runtime.bindingCalled" &&
      message.params.name === "__streamingSmokeProgress" &&
      /^(recent|collapsed|off|split|cadence|persistence)[a-z0-9-]{0,90}$/.test(
        message.params.payload,
      )
    ) {
      onProgress(message.params.payload);
      return;
    }
    const entry = pending.get(message.id);
    if (!entry) return; // Do not collect console, network bodies or unsolicited events.
    clearTimeout(entry.timer);
    pending.delete(message.id);
    if (message.error) entry.reject(new Error("CDP command failed"));
    else entry.resolve(message.result);
  };
  const call = (method, params = {}, timeout = 30000) =>
    new Promise((resolve, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error("CDP command timeout"));
      }, timeout);
      pending.set(id, { resolve, reject, timer });
      ws.send(JSON.stringify({ id, method, params }));
    });
  const evaluate = async (expression, timeout) => {
    const result = await call(
      "Runtime.evaluate",
      { expression, awaitPromise: true, returnByValue: true },
      timeout,
    );
    assert.ok(!result.exceptionDetails, "Synthetic evaluation failed");
    return result.result.value;
  };
  const close = () => {
    for (const entry of pending.values()) {
      clearTimeout(entry.timer);
      entry.reject(new Error("CDP closed"));
    }
    pending.clear();
    ws.close();
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
    const identity = await evaluate(
      `window.__TAURI_INTERNALS__?.invoke('plugin:app|identifier')`,
    );
    assert.equal(identity, packageName);
    return { call, evaluate, close };
  } catch (error) {
    close();
    throw error;
  }
}

async function main() {
  assert.ok(adb, "--adb path required");
  const profile = options.profile ?? "smoke";
  assert.ok(
    ["smoke", "stress", "persistence-spike", "persistence"].includes(profile),
    "Unknown profile",
  );
  assert.equal(
    run(serial, ["emu", "avd", "name"]).stdout.split("\n")[0].trim(),
    avd,
  );
  assert.equal(
    run(serial, ["shell", "getprop", "sys.boot_completed"]).stdout.trim(),
    "1",
  );
  console.log(JSON.stringify({ phase: "offline-install", avd }));
  await cutDeviceNetwork(serial, { run, sleep: delay });
  run(serial, ["install", "-r", apk]);
  if (options["fresh-install"] === "true") {
    // Both serial and synthetic AVD identity were checked above. Never clear another profile.
    assert.match(
      run(serial, ["shell", "pm", "clear", packageName]).stdout,
      /Success/,
    );
  }
  let client;
  try {
    run(serial, ["shell", "am", "force-stop", packageName]);
    run(serial, ["shell", "am", "start", "-n", `${packageName}/.MainActivity`]);
    await delay(3000);
    const pid = run(serial, ["shell", "pidof", packageName]).stdout.trim();
    assert.match(pid, /^\d+$/);
    run(serial, [
      "forward",
      `tcp:${port}`,
      `localabstract:webview_devtools_remote_${pid}`,
    ]);
    let phase = "fixture";
    client = await connect((next) => {
      phase = next;
      console.log(JSON.stringify({ phase }));
    });
    const deadline = Date.now() + 30000;
    while (
      !(await client.evaluate(
        'window.__streamingSmoke?.marker === "synthetic-v1"',
      ))
    ) {
      assert.ok(
        Date.now() < deadline,
        "APK lacks the opt-in synthetic fixture",
      );
      await delay(250);
    }
    console.log(JSON.stringify({ phase: "suite" }));
    const environment = await client.evaluate(`({
            userAgent: navigator.userAgent, devicePixelRatio, width: innerWidth, height: innerHeight,
            visible: document.visibilityState === 'visible',
            hardwareConcurrency: navigator.hardwareConcurrency,
            binaryCommitSupported: typeof window.RisuNestCommit?.postMessage === 'function',
            longTaskSupported: PerformanceObserver.supportedEntryTypes.includes('longtask')
        })`);
    assert.equal(environment.visible, true);
    await client.call("Runtime.addBinding", {
      name: "__streamingSmokeProgress",
    });
    const result = await client
      .evaluate(
        `window.__streamingSmoke.run(${JSON.stringify(profile)})`,
        profile === "persistence" ? 300000 : 60000,
      )
      .catch(() => ({
        passed: false,
        phase,
        assertion: "evaluation-failed-or-timeout",
        cases: [],
      }));
    if (profile === "persistence" && result.passed) {
      try {
        await client.evaluate("delete window.__streamingSmoke");
        await client.call("Page.reload");
        const deadline = Date.now() + 30000;
        while (
          !(await client.evaluate(
            "!!window.__streamingSmoke?.checkPersistenceReload",
          ))
        ) {
          assert.ok(Date.now() < deadline, "Reloaded fixture unavailable");
          await delay(250);
        }
        result.reload = await client.evaluate(
          "window.__streamingSmoke.checkPersistenceReload()",
        );
        result.passed = result.reload.passed;
        client.close();
        run(serial, ["shell", "am", "force-stop", packageName]);
        run(serial, [
          "shell",
          "am",
          "start",
          "-n",
          `${packageName}/.MainActivity`,
        ]);
        await delay(3000);
        const restartedPid = run(serial, [
          "shell",
          "pidof",
          packageName,
        ]).stdout.trim();
        assert.match(restartedPid, /^\d+$/);
        run(serial, [
          "forward",
          `tcp:${port}`,
          `localabstract:webview_devtools_remote_${restartedPid}`,
        ]);
        client = await connect(() => {});
        const restartDeadline = Date.now() + 30000;
        while (
          !(await client.evaluate(
            "!!window.__streamingSmoke?.checkPersistenceReload",
          ))
        ) {
          assert.ok(
            Date.now() < restartDeadline,
            "Restarted fixture unavailable",
          );
          await delay(250);
        }
        result.restart = await client.evaluate(
          "window.__streamingSmoke.checkPersistenceReload()",
        );
        result.passed = result.restart.passed;
      } catch {
        result.passed = false;
        result.assertion = "reload-or-restart-failed";
      }
    }
    const report = {
      timestamp: new Date().toISOString(),
      synthetic: true,
      platform: "android-emulator",
      build: "debug-agent",
      profile,
      avd,
      serial,
      androidApi: Number(
        run(serial, ["shell", "getprop", "ro.build.version.sdk"]).stdout.trim(),
      ),
      apkSha256: createHash("sha256")
        .update(await readFile(apk))
        .digest("hex"),
      environment,
      ...result,
    };
    await mkdir(path.dirname(output), { recursive: true });
    await writeFile(output, JSON.stringify(report, null, 2) + "\n");
    console.log(JSON.stringify(report));
    assert.equal(result.passed, true, "Android streaming acceptance failed");
  } finally {
    client?.close();
    run(serial, ["shell", "am", "force-stop", packageName], {
      allowFailure: true,
    });
    run(serial, ["forward", "--remove", `tcp:${port}`], { allowFailure: true });
  }
}

main().catch((error) => {
  console.error(error.message);
  process.exitCode = 1;
});
