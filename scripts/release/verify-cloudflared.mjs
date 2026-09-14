import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs, requireArg } from "./common.mjs";

function observeExit(child) {
  if (child.exitCode !== null || child.signalCode !== null)
    return Promise.resolve({ code: child.exitCode, signal: child.signalCode });
  return new Promise((resolveExit) => {
    child.once("exit", (code, signal) => resolveExit({ code, signal }));
    child.once("error", (error) => resolveExit({ error }));
  });
}

async function waitForExit(exit, timeout = 10000) {
  let timer;
  try {
    return await Promise.race([
      exit,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error("cloudflared did not exit.")), timeout);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function runTunnel(binary, origin, token) {
  const child = spawn(binary, ["tunnel", "--no-autoupdate", "--logformat", "json", "--url", origin], {
    windowsHide: true,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const exit = observeExit(child);
  let spawnError;
  child.once("error", (error) => { spawnError = error; });
  let text = "";
  const append = (chunk) => { text = `${text}${chunk}`.slice(-131072); };
  child.stdout.on("data", append);
  child.stderr.on("data", append);
  try {
    const deadline = Date.now() + 60000;
    let publicUrl;
    while (Date.now() < deadline) {
      publicUrl = /https:\/\/[a-z0-9-]+\.trycloudflare\.com/.exec(text)?.[0];
      if (publicUrl && /Registered tunnel connection|Connection .* registered/i.test(text)) break;
      if (spawnError) throw spawnError;
      if (child.exitCode !== null) throw new Error(`cloudflared exited before readiness (${child.exitCode}): ${text}`);
      await new Promise((resolveWait) => setTimeout(resolveWait, 250));
    }
    if (!publicUrl) throw new Error(`cloudflared did not publish a Quick Tunnel URL: ${text}`);
    let proxied = false;
    for (let attempt = 0; attempt < 20; attempt += 1) {
      try {
        const response = await fetch(publicUrl, { signal: AbortSignal.timeout(5000) });
        if (response.ok && await response.text() === token) {
          proxied = true;
          break;
        }
      } catch {}
      await new Promise((resolveWait) => setTimeout(resolveWait, 500));
    }
    if (!proxied) throw new Error("Quick Tunnel did not proxy the synthetic origin.");
  } finally {
    if (child.exitCode === null && child.signalCode === null) child.kill();
    const result = await waitForExit(exit);
    if (result.error) throw result.error;
  }
}

export async function verifyCloudflared(binary) {
  const version = spawn(binary, ["--version"], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  const exit = observeExit(version);
  let output = "";
  version.stdout.on("data", (chunk) => { output += chunk; });
  version.stderr.on("data", (chunk) => { output += chunk; });
  const result = await waitForExit(exit);
  if (result.error) throw result.error;
  if (result.code !== 0 || !/cloudflared version \d{4}\.\d+\.\d+/.test(output))
    throw new Error(`cloudflared version probe failed: ${output}`);
  const token = `risunest-cloudflared-${process.pid}`;
  const server = createServer((_request, response) => response.end(token));
  await new Promise((resolveListen, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolveListen);
  });
  const { port } = server.address();
  try {
    await runTunnel(binary, `http://127.0.0.1:${port}`, token);
    await runTunnel(binary, `http://127.0.0.1:${port}`, token);
  } finally {
    await new Promise((resolveClose) => server.close(resolveClose));
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  await verifyCloudflared(requireArg(args, "binary"));
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  await main();
