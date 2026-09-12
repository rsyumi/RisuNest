import { execFileSync } from "node:child_process";
import { syntheticDaemon } from "./daemon.mjs";
import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { createServer } from "node:net";
import { REALM_BLOCKED_URL_PATTERNS } from "../../scripts/realmBlocklist.mjs";

// A separately installed package is mandatory. Never inspect the user's app.
const packageName = "io.github.rsyumi.risunest.syncservervalidation20260911";
const adb = process.env.ANDROID_HOME + "/platform-tools/adb.exe";
const root = fileURLToPath(new URL(".local/", import.meta.url));
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
const server = syntheticDaemon(data, 19419);
const cli = server.cli;
cli("init");
const config = {
  ...JSON.parse(cli("device", "add")),
  endpoint: "http://127.0.0.1:19419",
};
const verifyUnresponsiveStartup = process.argv.includes(
  "--unresponsive-startup",
);
const verifyUi = process.argv.includes("--ui") || verifyUnresponsiveStartup;
const uiConfig = verifyUi
  ? { ...JSON.parse(cli("device", "add")), endpoint: config.endpoint }
  : undefined;
const daemon = await server.start();
let client;
let reverseInstalled = false;
let forwardInstalled = false;
async function connect() {
  const pid = run("shell", "pidof", packageName);
  if (!/^\d+$/.test(pid)) throw new Error("Isolated package is not running");
  if (
    run("shell", "cat", `/proc/${pid}/cmdline`).replaceAll("\0", "") !==
    packageName
  )
    throw new Error("Unexpected Android process identity");
  run("forward", "tcp:19420", `localabstract:webview_devtools_remote_${pid}`);
  forwardInstalled = true;
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
async function waitForUi(expression) {
  const started = Date.now();
  while (!(await client.evaluate(expression))) {
    if (Date.now() - started > 60000)
      throw new Error(`Synthetic UI did not become ready: ${expression}`);
    await delay(250);
  }
}
async function exerciseUi() {
  // This separately installed profile contains only this harness's synthetic
  // state. Seed its local onboarding marker without contacting an account API.
  await client.evaluate(`(async()=>{
    const invoke=window.__TAURI_INTERNALS__.invoke;
    const root=await invoke('pds_read_root');
    await invoke('pds_commit',{commit:{expectedRevision:root.revision,rootMutations:[{type:'set',key:'didFirstSetup',value:true}]},assetAliases:[]});
    localStorage.setItem('tos4','true'); return true;
  })()`);
  await client.call("Page.reload");
  await waitForUi("!!document.querySelector('button:has(svg.lucide-list)')");
  await client.evaluate(
    "document.querySelector('button:has(svg.lucide-list)').click(); true",
  );
  await waitForUi(
    "!!document.querySelector('button:has(svg.lucide-settings)')",
  );
  await client.evaluate(
    "document.querySelector('button:has(svg.lucide-settings)').click(); true",
  );
  await waitForUi(
    "Array.from(document.querySelectorAll('button')).some(b=>b.innerText==='RisuNest')",
  );
  await client.evaluate(
    "Array.from(document.querySelectorAll('button')).find(b=>b.innerText==='RisuNest').click(); true",
  );
  await waitForUi(
    "Array.from(document.querySelectorAll('.server-sync button')).some(b=>['Disconnect','연결 해제'].includes(b.innerText)&&!b.disabled)",
  );
  await client.evaluate(
    "Array.from(document.querySelectorAll('.server-sync button')).find(b=>['Disconnect','연결 해제'].includes(b.innerText)).click(); true",
  );
  await waitForUi("document.querySelectorAll('.server-sync input').length===4");
  // Only synthetic credentials enter the form, never logs or screenshots.
  await client.evaluate(`(()=>{
    const values=${JSON.stringify([uiConfig.endpoint, uiConfig.libraryId, uiConfig.deviceId, uiConfig.token])};
    document.querySelectorAll('.server-sync input').forEach((input,index)=>{input.value=values[index]; input.dispatchEvent(new Event('input',{bubbles:true}));});
    document.querySelector('.server-sync form').requestSubmit(); return true;
  })()`);
  await waitForUi(
    "['Last successful sync','최근 동기화 성공'].some(text=>document.querySelector('.server-sync')?.innerText.includes(text))",
  );
  const layout = await client.evaluate(`(()=>{
    const panel=document.querySelector('.server-sync'); panel.scrollIntoView();
    const connection=panel.querySelector('.connection');
    return {viewport:innerWidth,panelWidth:panel.getBoundingClientRect().width,connectionWidth:connection.clientWidth,connectionScrollWidth:connection.scrollWidth,credentialInputs:panel.querySelectorAll('input').length};
  })()`);
  if (
    layout.credentialInputs !== 0 ||
    layout.connectionScrollWidth > layout.connectionWidth + 1
  )
    throw new Error(
      "Synthetic server settings overflow or exposed credential form",
    );
  const shot = await client.call("Page.captureScreenshot", { format: "png" });
  writeFileSync(
    root + "android-server-settings.png",
    Buffer.from(shot.data, "base64"),
  );
  return layout;
}
async function unresponsiveStartup() {
  const before = await checked("server_sync_status");
  const stopped = new Promise((resolve) => daemon.once("exit", resolve));
  daemon.kill();
  await stopped;
  const sockets = new Set();
  let receivedRequest = false;
  const stall = createServer((socket) => {
    sockets.add(socket);
    socket.on("data", () => {
      receivedRequest = true;
    });
    socket.on("close", () => sockets.delete(socket));
    socket.on("error", () => {});
    // Consume synthetic requests without returning headers or a body.
  });
  await new Promise((resolve, reject) => {
    stall.once("error", reject);
    stall.listen(19419, "127.0.0.1", resolve);
  });
  try {
    client.close();
    run("shell", "am", "force-stop", packageName);
    const started = Date.now();
    run(
      "shell",
      "am",
      "start",
      "-n",
      `${packageName}/io.github.rsyumi.risunest.MainActivity`,
    );
    await delay(5000);
    client = await connect();
    await waitForUi("!!document.querySelector('button:has(svg.lucide-list)')");
    await client.evaluate(
      "document.querySelector('button:has(svg.lucide-list)').click(); true",
    );
    await waitForUi(
      "!!document.querySelector('button:has(svg.lucide-settings)')",
    );
    await client.evaluate(
      "document.querySelector('button:has(svg.lucide-settings)').click(); true",
    );
    await waitForUi(
      "Array.from(document.querySelectorAll('button')).some(b=>b.innerText==='RisuNest')",
    );
    const localInteractionMs = Date.now() - started;
    if (localInteractionMs >= 30000)
      throw new Error("Startup waited for the stalled server");
    await delay(Math.max(0, 35000 - (Date.now() - started)));
    if (!receivedRequest)
      throw new Error("Startup never attempted its configured server");
    const after = await checked("server_sync_status");
    if (
      !after.configured ||
      JSON.stringify(after.head) !== JSON.stringify(before.head)
    )
      throw new Error("Stalled startup changed the confirmed server head");
    return {
      localInteractionMs,
      stalledForMs: Date.now() - started,
      requestObserved: true,
      headPreserved: true,
    };
  } finally {
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => stall.close(resolve));
  }
}
try {
  run("reverse", "tcp:19419", "tcp:19419");
  reverseInstalled = true;
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
  const verifiedBytes = await checked("server_sync_verified_bytes");
  if (typeof verifiedBytes !== "string" || !/^\d+$/.test(verifiedBytes))
    throw new Error("Native transfer progress is unavailable");
  const ui = verifyUi ? await exerciseUi() : undefined;
  const stalledStartup = verifyUnresponsiveStartup
    ? await unresponsiveStartup()
    : undefined;
  const report = {
    serverPlatform: server.platform,
    packageName,
    avd: "risunest_vm_retest",
    registration: true,
    restart: true,
    initialPhase: initial.phase,
    resumedPhase: resumed.phase,
    credentialResolvedAfterRestart: true,
    verifiedByteProgress: true,
    ui,
    stalledStartup,
  };
  writeFileSync(root + "android-runtime.json", JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
} finally {
  client?.close();
  // Cleanup must not replace the original failure during startup/connection.
  for (const cleanup of [
    () => reverseInstalled && run("reverse", "--remove", "tcp:19419"),
    () => forwardInstalled && run("forward", "--remove", "tcp:19420"),
    () => run("shell", "am", "force-stop", packageName),
  ]) {
    try {
      cleanup();
    } catch {
      console.error("Synthetic Android cleanup failed");
    }
  }
  await daemon.stop();
}
