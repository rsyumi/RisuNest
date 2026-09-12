import { connect } from "./windows-client.mjs";
import { execFileSync } from "node:child_process";
import { syntheticDaemon } from "./daemon.mjs";
import { mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL(".local/", import.meta.url));
mkdirSync(root, { recursive: true });
const profile = (action) =>
  execFileSync(
    "powershell.exe",
    [
      "-NoProfile",
      "-NonInteractive",
      "-File",
      fileURLToPath(new URL("windows-profile.ps1", import.meta.url)),
      "-Action",
      action,
    ],
    { windowsHide: true, encoding: "utf8" },
  );
const data = root + "windows-server-" + Date.now();
const server = syntheticDaemon(data, 19422);
const cli = server.cli;
cli("init");
const config = {
  ...JSON.parse(cli("device", "add")),
  endpoint: "http://127.0.0.1:19422",
};
const daemon = await server.start();
let client;
let started = false;
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function waitFor(expression) {
  const start = Date.now();
  while (!(await client.evaluate(expression))) {
    if (Date.now() - start > 60000)
      throw new Error("Synthetic Windows UI timeout");
    await delay(250);
  }
}
async function settings() {
  await waitFor("!!document.querySelector('button:has(svg.lucide-list)')");
  await client.evaluate(
    "document.querySelector('button:has(svg.lucide-list)').click(); true",
  );
  await waitFor("!!document.querySelector('button:has(svg.lucide-settings)')");
  await client.evaluate(
    "document.querySelector('button:has(svg.lucide-settings)').click(); true",
  );
  await waitFor(
    "Array.from(document.querySelectorAll('button')).some(b=>b.innerText==='RisuNest')",
  );
  await client.evaluate(
    "Array.from(document.querySelectorAll('button')).find(b=>b.innerText==='RisuNest').click(); true",
  );
  await waitFor("!!document.querySelector('.server-sync')");
}
const completed =
  "['Last successful sync','최근 동기화 성공'].some(text=>document.querySelector('.server-sync')?.innerText.includes(text))";
try {
  profile("Start");
  started = true;
  await delay(5000);
  client = await connect();
  await client.evaluate(
    `(async()=>{const invoke=window.__TAURI_INTERNALS__.invoke; await invoke('pds_open'); const root=await invoke('pds_read_root'); await invoke('pds_commit',{commit:{expectedRevision:root.revision,rootMutations:[{type:'set',key:'didFirstSetup',value:true}]},assetAliases:[]});localStorage.setItem('tos4','true');return true;})()`,
  );
  await client.call("Page.reload");
  await delay(3000);
  await client.call("Emulation.setDeviceMetricsOverride", {
    width: 1100,
    height: 800,
    deviceScaleFactor: 1,
    mobile: false,
  });
  await settings();
  const disconnected =
    "Array.from(document.querySelectorAll('.server-sync button')).find(b=>['Disconnect','연결 해제'].includes(b.innerText))";
  if (await client.evaluate(`!!(${disconnected})`)) {
    await waitFor(`!(${disconnected})?.disabled`);
    await client.evaluate(`(${disconnected}).click();true`);
  }
  await waitFor("document.querySelectorAll('.server-sync input').length===4");
  await client.evaluate(
    `(()=>{const values=${JSON.stringify([config.endpoint, config.libraryId, config.deviceId, config.token])};document.querySelectorAll('.server-sync input').forEach((input,i)=>{input.value=values[i];input.dispatchEvent(new Event('input',{bubbles:true}));});document.querySelector('.server-sync form').requestSubmit();return true;})()`,
  );
  await waitFor(completed);
  const before = await client.evaluate(
    "window.__TAURI_INTERNALS__.invoke('server_sync_status')",
  );
  if (
    !before.configured ||
    before.head.libraryId !== config.libraryId ||
    before.dirtyRecords !== 0
  )
    throw new Error("Windows form did not synchronize");
  await client.evaluate(
    "document.querySelector('.server-sync').scrollIntoView();true",
  );
  if (
    await client.evaluate(
      "document.querySelectorAll('.server-sync input').length",
    )
  )
    throw new Error("Credentials still visible");
  const shot = await client.call("Page.captureScreenshot", { format: "png" });
  writeFileSync(
    root + "windows-server-settings.png",
    Buffer.from(shot.data, "base64"),
  );
  client.close();
  profile("Restart");
  await delay(6000);
  client = await connect();
  await settings();
  await waitFor(
    "Array.from(document.querySelectorAll('.server-sync button')).some(b=>['Sync now','지금 동기화'].includes(b.innerText)&&!b.disabled)",
  );
  await client.evaluate(
    "Array.from(document.querySelectorAll('.server-sync button')).find(b=>['Sync now','지금 동기화'].includes(b.innerText)).click();true",
  );
  await waitFor(completed);
  const after = await client.evaluate(
    "window.__TAURI_INTERNALS__.invoke('server_sync_status')",
  );
  if (
    !after.configured ||
    after.head.libraryId !== config.libraryId ||
    after.dirtyRecords !== 0
  )
    throw new Error("Windows restart failed to synchronize");
  const report = {
    serverPlatform: server.platform,
    registrationThroughForm: true,
    synchronized: true,
    restart: true,
    credentialRecovered: true,
    confirmedSeq: after.head.seq,
  };
  writeFileSync(root + "windows-runtime.json", JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report));
} finally {
  client?.close();
  try {
    await daemon.stop();
  } finally {
    if (started) profile("Stop");
  }
}
