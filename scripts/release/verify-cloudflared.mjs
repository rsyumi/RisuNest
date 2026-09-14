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

export const tunnelArgs = (origin) => ["tunnel", "--no-autoupdate", "--output", "json", "--url", origin];

function diagnosticError(message, diagnostic) {
  return new Error(`${message}: ${JSON.stringify(diagnostic)}`);
}

function diagnosticCode(error) {
  const candidate = error?.cause?.code ?? error?.code ?? error?.name;
  return typeof candidate === "string" && /^[A-Za-z0-9_.-]{1,64}$/.test(candidate)
    ? candidate
    : "FETCH_ERROR";
}

async function runTunnel(binary, origin, token, options = {}) {
  const spawnProcess = options.spawnProcess ?? spawn;
  const fetchUrl = options.fetchUrl ?? fetch;
  const readinessTimeout = options.readinessTimeout ?? 60000;
  const pollInterval = options.pollInterval ?? 250;
  const proxyReadinessTimeout = options.proxyReadinessTimeout ?? 60000;
  const proxyPollInterval = options.proxyPollInterval ?? 500;
  const proxyTimeout = options.proxyTimeout ?? 5000;
  const child = spawnProcess(binary, tunnelArgs(origin), {
    windowsHide: true,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const exit = observeExit(child);
  let spawnError;
  child.once("error", (error) => { spawnError = error; });
  let text = "";
  let outputLength = 0;
  const append = (chunk) => {
    const value = String(chunk);
    outputLength += Buffer.byteLength(value);
    text = `${text}${value}`.slice(-8192);
  };
  child.stdout.on("data", append);
  child.stderr.on("data", append);
  let stopped;
  let stopRequested = false;
  let completed = false;
  try {
    const deadline = Date.now() + readinessTimeout;
    let publicUrl;
    let edgeRegistered = false;
    while (Date.now() < deadline) {
      publicUrl = /https:\/\/[a-z0-9-]+\.trycloudflare\.com/.exec(text)?.[0];
      edgeRegistered = /Registered tunnel connection|Connection .* registered/i.test(text);
      if (publicUrl && edgeRegistered) break;
      if (spawnError)
        throw diagnosticError("cloudflared failed before readiness", {
          code: diagnosticCode(spawnError), status: null, length: outputLength,
        });
      if (child.exitCode !== null || child.signalCode !== null)
        throw diagnosticError("cloudflared exited before readiness", {
          code: /Incorrect Usage/i.test(text) ? "CLOUDFLARED_USAGE_ERROR" : "CLOUDFLARED_EXITED",
          status: null,
          length: outputLength,
        });
      await new Promise((resolveWait) => setTimeout(resolveWait, pollInterval));
    }
    if (!publicUrl)
      throw diagnosticError("cloudflared did not publish a Quick Tunnel URL", {
        code: "QUICK_TUNNEL_URL_NOT_PUBLISHED", status: null, length: outputLength,
      });
    if (!edgeRegistered)
      throw diagnosticError("cloudflared did not register an edge connection", {
        code: "QUICK_TUNNEL_EDGE_NOT_REGISTERED", status: null, length: outputLength,
      });
    let proxied = false;
    let attempts = 0;
    let lastFailure = { code: "PROXY_NOT_ATTEMPTED", status: null, length: null };
    const proxyDeadline = Date.now() + proxyReadinessTimeout;
    while (Date.now() < proxyDeadline) {
      attempts += 1;
      try {
        const remaining = Math.max(1, proxyDeadline - Date.now());
        const response = await fetchUrl(publicUrl, {
          signal: AbortSignal.timeout(Math.min(proxyTimeout, remaining)),
        });
        const body = await response.text();
        const status = Number.isInteger(response.status) ? response.status : null;
        const length = Buffer.byteLength(body);
        if (response.ok && body === token) {
          proxied = true;
          break;
        }
        lastFailure = {
          code: response.ok ? "PROXY_BODY_MISMATCH" : "PROXY_HTTP_STATUS",
          status,
          length,
        };
      } catch (error) {
        lastFailure = { code: diagnosticCode(error), status: null, length: null };
      }
      if (spawnError)
        throw diagnosticError("cloudflared failed during proxy readiness", {
          code: diagnosticCode(spawnError), status: null, length: outputLength,
        });
      if (child.exitCode !== null || child.signalCode !== null)
        throw diagnosticError("cloudflared exited during proxy readiness", {
          code: "CLOUDFLARED_EXITED", status: null, length: outputLength,
        });
      const delay = Math.min(proxyPollInterval, proxyDeadline - Date.now());
      if (delay > 0) await new Promise((resolveWait) => setTimeout(resolveWait, delay));
    }
    if (!proxied)
      throw diagnosticError("Quick Tunnel did not proxy the synthetic origin", {
        ...lastFailure,
        attempts,
      });
    completed = true;
  } finally {
    const running = child.exitCode === null && child.signalCode === null;
    stopRequested = running && child.kill();
    const result = await waitForExit(exit);
    if (result.error) throw result.error;
    if (completed && !stopRequested) throw new Error("cloudflared exited before the requested stop.");
    stopped = result;
  }
  return {
    urlPublished: true,
    edgeRegistered: true,
    originProxied: true,
    stopRequested,
    stopped: true,
    exitCode: stopped.code ?? null,
    signal: stopped.signal ?? null,
  };
}

export async function verifyCloudflared(binary, options = {}) {
  const spawnProcess = options.spawnProcess ?? spawn;
  const version = spawnProcess(binary, ["--version"], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  const exit = observeExit(version);
  let output = "";
  version.stdout.on("data", (chunk) => { output += chunk; });
  version.stderr.on("data", (chunk) => { output += chunk; });
  const result = await waitForExit(exit);
  if (result.error) throw result.error;
  const versionMatch = /cloudflared version (\d{4}\.\d+\.\d+)/.exec(output);
  if (result.code !== 0 || !versionMatch)
    throw new Error(`cloudflared version probe failed: ${output}`);
  const token = `risunest-cloudflared-${process.pid}`;
  const server = createServer((_request, response) => response.end(token));
  await new Promise((resolveListen, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolveListen);
  });
  const { port } = server.address();
  try {
    const runs = [
      await runTunnel(binary, `http://127.0.0.1:${port}`, token, options),
      await runTunnel(binary, `http://127.0.0.1:${port}`, token, options),
    ];
    return {
      schema: "risunest-cloudflared-lifecycle/v1",
      version: versionMatch[1],
      output: "json",
      runs,
    };
  } finally {
    await new Promise((resolveClose) => server.close(resolveClose));
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const result = await verifyCloudflared(requireArg(args, "binary"));
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  await main();
