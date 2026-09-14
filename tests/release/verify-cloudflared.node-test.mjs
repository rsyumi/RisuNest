import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import test from "node:test";
import { tunnelArgs, verifyCloudflared } from "../../scripts/release/verify-cloudflared.mjs";

function fakeChild() {
  const child = new EventEmitter();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  child.exitCode = null;
  child.signalCode = null;
  child.kill = () => {
    child.signalCode = "SIGTERM";
    queueMicrotask(() => child.emit("exit", null, "SIGTERM"));
    return true;
  };
  return child;
}

function fakeSpawn({ usageError = false } = {}) {
  const invocations = [];
  const spawnProcess = (_binary, args) => {
    invocations.push(args);
    const child = fakeChild();
    queueMicrotask(() => {
      if (args[0] === "--version") {
        child.stdout.write("cloudflared version 2026.9.1 (synthetic)\n");
        child.exitCode = 0;
        child.emit("exit", 0, null);
      } else if (usageError) {
        child.stderr.write("Incorrect Usage. flag provided but not defined: -logformat\n");
        child.exitCode = 0;
        child.emit("exit", 0, null);
      } else {
        child.stderr.write('{"message":"| https://synthetic-tunnel.trycloudflare.com |"}\n');
        child.stderr.write('{"message":"Registered tunnel connection"}\n');
      }
    });
    return child;
  };
  return { invocations, spawnProcess };
}

const response = (body, status = 200) => ({
  ok: status >= 200 && status < 300,
  status,
  text: async () => body,
});

const options = (spawnProcess, overrides = {}) => ({
  spawnProcess,
  fetchUrl: async () => response(`risunest-cloudflared-${process.pid}`),
  readinessTimeout: 100,
  pollInterval: 1,
  proxyReadinessTimeout: 100,
  proxyPollInterval: 1,
  proxyTimeout: 100,
  ...overrides,
});

test("cloudflared probe uses the pinned JSON output flag and reports two complete lifecycles", async () => {
  assert.deepEqual(tunnelArgs("http://127.0.0.1:1234"), [
    "tunnel", "--no-autoupdate", "--output", "json", "--url", "http://127.0.0.1:1234",
  ]);
  const fake = fakeSpawn();
  const evidence = await verifyCloudflared("synthetic-cloudflared", options(fake.spawnProcess));
  assert.deepEqual(evidence, {
    schema: "risunest-cloudflared-lifecycle/v1",
    version: "2026.9.1",
    output: "json",
    runs: [0, 1].map(() => ({
      urlPublished: true,
      edgeRegistered: true,
      originProxied: true,
      stopRequested: true,
      stopped: true,
      exitCode: null,
      signal: "SIGTERM",
    })),
  });
  assert.equal(fake.invocations.length, 3);
  for (const args of fake.invocations.slice(1)) {
    assert.ok(args.includes("--output"));
    assert.ok(!args.includes("--logformat"));
  }
});

test("cloudflared usage text with exit code zero cannot pass readiness", async () => {
  const fake = fakeSpawn({ usageError: true });
  await assert.rejects(
    verifyCloudflared("synthetic-cloudflared", options(fake.spawnProcess)),
    (error) => {
      assert.match(error.message, /cloudflared exited before readiness: {"code":"CLOUDFLARED_USAGE_ERROR","status":null,"length":\d+}/);
      assert.doesNotMatch(error.message, /logformat/);
      return true;
    },
  );
});

test("cloudflared proxy readiness tolerates transient network and HTTP failures", async () => {
  const fake = fakeSpawn();
  let calls = 0;
  const fetchUrl = async () => {
    calls += 1;
    if (calls === 1) {
      const error = new TypeError("synthetic lookup failure");
      error.cause = { code: "ENOTFOUND" };
      throw error;
    }
    if (calls === 2) return response("synthetic unavailable", 503);
    if (calls === 3) return response("synthetic mismatch");
    return response(`risunest-cloudflared-${process.pid}`);
  };
  const evidence = await verifyCloudflared(
    "synthetic-cloudflared",
    options(fake.spawnProcess, { fetchUrl }),
  );
  assert.equal(calls, 5);
  assert.equal(fake.invocations.length, 3);
  assert.equal(evidence.runs.length, 2);
  assert.ok(evidence.runs.every((run) => run.originProxied && run.stopped));
});

test("permanent HTTP proxy failure reports only structured status and length", async () => {
  const fake = fakeSpawn();
  await assert.rejects(
    verifyCloudflared("synthetic-cloudflared", options(fake.spawnProcess, {
      fetchUrl: async () => response("synthetic unavailable", 503),
      proxyReadinessTimeout: 10,
    })),
    (error) => {
      assert.match(error.message, /{"code":"PROXY_HTTP_STATUS","status":503,"length":21,"attempts":\d+}/);
      assert.doesNotMatch(error.message, /synthetic unavailable/);
      return true;
    },
  );
  assert.equal(fake.invocations.length, 2);
});

test("permanent network proxy failure reports a bounded error code", async () => {
  const fake = fakeSpawn();
  await assert.rejects(
    verifyCloudflared("synthetic-cloudflared", options(fake.spawnProcess, {
      fetchUrl: async () => {
        const error = new TypeError("synthetic private diagnostic");
        error.cause = { code: "ENOTFOUND" };
        throw error;
      },
      proxyReadinessTimeout: 10,
    })),
    (error) => {
      assert.match(error.message, /{"code":"ENOTFOUND","status":null,"length":null,"attempts":\d+}/);
      assert.doesNotMatch(error.message, /synthetic private diagnostic/);
      return true;
    },
  );
  assert.equal(fake.invocations.length, 2);
});
