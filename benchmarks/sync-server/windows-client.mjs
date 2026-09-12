import { REALM_BLOCKED_URL_PATTERNS } from "../../scripts/realmBlocklist.mjs";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
export async function connect() {
  execFileSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-NonInteractive",
      "-File",
      fileURLToPath(new URL("windows-profile.ps1", import.meta.url)),
      "-Action",
      "Check",
    ],
    { windowsHide: true, stdio: "pipe" },
  );
  const targets = await (
    await fetch("http://127.0.0.1:19421/json/list")
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
  // Both the compiled identifier and the CDP process must belong to this fixture.
  const identity = await evaluate(
    "window.__TAURI_INTERNALS__.invoke('plugin:app|identifier')",
  );
  if (identity !== "io.github.rsyumi.risunest.syncservervalidation20260911") {
    close();
    throw new Error("Unexpected Tauri identity");
  }
  return { call, evaluate, close };
}
