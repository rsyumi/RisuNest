import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { REALM_BLOCKED_URL_PATTERNS } from "../../scripts/realmBlocklist.mjs";

// A separately installed package is mandatory. Never inspect the user's app.
const packageName = "io.github.rsyumi.risunest.syncservervalidation20260911";
const adb = process.env.ANDROID_HOME + "/platform-tools/adb.exe";
const root = fileURLToPath(new URL(".local/", import.meta.url));
const executable =
  process.env.CARGO_TARGET_DIR + "/debug/risunest-sync-server.exe";
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const run = (...args) =>
  execFileSync(adb, ["-P", "15037", "-s", "emulator-5554", ...args], {
    timeout: 15000,
    encoding: "utf8",
    windowsHide: true,
  }).trim();
if (run("emu", "avd", "name").split(/\s+/)[0] !== "risunest_vm_retest")
  throw new Error("Unsafe Android AVD");
if (!run("shell", "pm", "path", packageName).startsWith("package:"))
  throw new Error("Isolated validation package is not installed");
mkdirSync(root, { recursive: true });
const data = root + "runtime-server-" + Date.now();
const cli = (...args) =>
  execFileSync(executable, [...args, "--data-dir", data], {
    encoding: "utf8",
    windowsHide: true,
    timeout: 15000,
  });
cli("init");
const config = {
  ...JSON.parse(cli("device", "add")),
  endpoint: "http://127.0.0.1:19419",
};
const daemon = spawn(
  executable,
  ["serve", "--data-dir", data, "--listen", "127.0.0.1:19419"],
  {
    windowsHide: true,
    stdio: ["ignore", "ignore", "pipe"],
  },
);
daemon.stderr.resume();
let client;
async function connect() {
  const pid = run("shell", "pidof", packageName);
  if (!/^\d+$/.test(pid)) throw new Error("Isolated package is not running");
  if (
    run("shell", "cat", `/proc/${pid}/cmdline`).replaceAll("\0", "") !==
    packageName
  )
    throw new Error("Unexpected Android process identity");
  run("forward", "tcp:19420", `localabstract:webview_devtools_remote_${pid}`);
  const targets = await (
    await fetch("http://127.0.0.1:19420/json/list")
  ).json();
  const target = targets.find((item) => item.type === "page");
  if (!target) throw new Error("Isolated WebView unavailable");
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = reject;
  });
  let next = 0;
  const pending = new Map();
  ws.onmessage = ({ data }) => {
    const message = JSON.parse(data);
    const item = pending.get(message.id);
    if (!item) return;
    pending.delete(message.id);
    clearTimeout(item.timer);
    if (message.error) item.reject(new Error("CDP command failed"));
    else item.resolve(message.result);
  };
  const call = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = ++next;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error("CDP timeout"));
      }, 120000);
      pending.set(id, { resolve, reject, timer });
      ws.send(JSON.stringify({ id, method, params }));
    });
  const evaluate = async (expression) => {
    const result = await call("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (result.exceptionDetails) throw new Error("Synthetic evaluation failed");
    return result.result.value;
  };
  const close = () => {
    for (const item of pending.values()) clearTimeout(item.timer);
    pending.clear();
    ws.close();
  };
  await call("Network.enable");
  await call("Network.setBlockedURLs", {
    urls: [
      ...REALM_BLOCKED_URL_PATTERNS,
      "*update.rsyumi.workers.dev/translator/prompt-presets.json*",
    ],
  });
  // Tauri's compiled config identifier remains the JNI namespace. Android's
  // distinct package/process identity above is the actual storage isolation.
  const identity = await evaluate(
    "window.__TAURI_INTERNALS__.invoke('plugin:app|identifier')",
  );
  if (identity !== "io.github.rsyumi.risunest") {
    close();
    throw new Error("Unexpected Tauri identity");
  }
  return { call, evaluate, close };
}
const invoke = async (command, args = {}) =>
  client.evaluate(`(async () => {
  try { return { ok: true, value: await window.__TAURI_INTERNALS__.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)}) }; }
  catch (error) { return { ok: false, code: typeof error === 'object' ? error.code ?? error.kind : 'native-error' }; }
})()`);
async function checked(command, args) {
  const reply = await invoke(command, args);
  if (!reply.ok) throw new Error(`${command}: ${reply.code}`);
  return reply.value;
}
async function cycle() {
  const prepared = await checked("server_sync_prepare", { options: {} });
  if (prepared.kind === "report") return prepared.result;
  await checked("server_sync_activate", {
    preparationId: prepared.preparationId,
  });
  return checked("server_sync_publish", {
    preparationId: prepared.preparationId,
  });
}
try {
  run("reverse", "tcp:19419", "tcp:19419");
  await delay(1500);
  client = await connect();
  await checked("pds_open");
  if ((await checked("server_sync_status")).configured) {
    await checked("server_sync_unbind");
  }
  const bound = await checked("server_sync_bind", { config });
  if (!bound.configured) throw new Error("Registration not persisted");
  const initial = await cycle();
  client.close();
  run("shell", "am", "force-stop", packageName);
  run(
    "shell",
    "am",
    "start",
    "-n",
    `${packageName}/io.github.rsyumi.risunest.MainActivity`,
  );
  await delay(5000);
  client = await connect();
  await checked("pds_open");
  const restored = await checked("server_sync_status");
  if (!restored.configured) throw new Error("Registration lost on restart");
  const resumed = await cycle();
  if (resumed.phase !== "idle" || resumed.head.libraryId !== config.libraryId)
    throw new Error(
      "Restarted replica did not reach the synthetic server head",
    );
  const report = {
    packageName,
    avd: "risunest_vm_retest",
    registration: true,
    restart: true,
    initialPhase: initial.phase,
    resumedPhase: resumed.phase,
    credentialResolvedAfterRestart: true,
  };
  writeFileSync(root + "android-runtime.json", JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
} finally {
  client?.close();
  daemon.kill();
  run("reverse", "--remove", "tcp:19419");
  run("forward", "--remove", "tcp:19420");
}
