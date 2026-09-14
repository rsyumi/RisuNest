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

const options = (spawnProcess) => ({
  spawnProcess,
  fetchUrl: async () => ({ ok: true, text: async () => `risunest-cloudflared-${process.pid}` }),
  readinessTimeout: 100,
  pollInterval: 1,
  proxyAttempts: 1,
  proxyTimeout: 100,
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
    /exited before readiness \(0\): Incorrect Usage.*logformat/s,
  );
});
