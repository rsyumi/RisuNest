// Synthetic protocol comparison. No installed app, profile, or network service.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { Agent, request } from "node:http";
import { connect, createServer } from "node:net";
import { fileURLToPath } from "node:url";
import { gzipSync, gunzipSync } from "node:zlib";

const output = fileURLToPath(new URL(".local/", import.meta.url));
mkdirSync(output, { recursive: true });
const binary =
  process.env.CARGO_TARGET_DIR + "/release/risunest-sync-server.exe";
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const json = (value) => Buffer.from(JSON.stringify(value));
function encode(objects) {
  const count = Buffer.alloc(4);
  count.writeUInt32BE(objects.length);
  return Buffer.concat([
    Buffer.from("RNSF"),
    count,
    ...objects.flatMap((bytes) => {
      const size = Buffer.alloc(8);
      size.writeBigUInt64BE(BigInt(bytes.length));
      return [Buffer.from([0]), Buffer.from(hash(bytes), "hex"), size, bytes];
    }),
  ]);
}
function verify(bytes, objects) {
  assert.equal(bytes.subarray(0, 4).toString(), "RNSF");
  assert.equal(bytes.readUInt32BE(4), objects.length);
  let offset = 8;
  for (const expected of objects) {
    assert.equal(bytes[offset++], 0);
    assert.equal(
      bytes.subarray(offset, offset + 32).toString("hex"),
      hash(expected),
    );
    offset += 32;
    assert.equal(bytes.readBigUInt64BE(offset), BigInt(expected.length));
    offset += 8;
    assert.deepEqual(
      bytes.subarray(offset, offset + expected.length),
      expected,
    );
    offset += expected.length;
  }
  assert.equal(offset, bytes.length);
}
function fixture(kind) {
  return Array.from({ length: 1000 }, (_, i) => {
    let bytes;
    if (kind === "text")
      bytes = Buffer.from(
        `synthetic-${i.toString().padStart(6, "0")}:` +
          "repeatable synthetic content ".repeat(30),
      );
    else
      bytes = Buffer.concat(
        Array.from({ length: 16 }, (_, j) =>
          createHash("sha256").update(`synthetic-${i}-${j}`).digest(),
        ),
      );
    return kind === "precompressed" ? gzipSync(bytes) : bytes;
  });
}
async function scenario(kind, mode, concurrency) {
  const data =
    output + `comparison-${kind}-${mode}-${concurrency}-${Date.now()}`;
  const cli = (...args) =>
    execFileSync(binary, [...args, "--data-dir", data], {
      encoding: "utf8",
      windowsHide: true,
    });
  cli("init");
  const credentials = Array.from({ length: concurrency }, () =>
    JSON.parse(cli("device", "add")),
  );
  const daemon = spawn(
    binary,
    ["serve", "--data-dir", data, "--listen", "127.0.0.1:19424"],
    { windowsHide: true, stdio: "ignore" },
  );
  let sent = 0,
    received = 0,
    requests = 0;
  const sockets = new Set();
  const proxy = createServer((socket) => {
    const upstream = connect(19424, "127.0.0.1");
    sockets.add(socket);
    sockets.add(upstream);
    socket.on("data", (b) => {
      sent += b.length;
    });
    upstream.on("data", (b) => {
      received += b.length;
    });
    socket.on("error", () => upstream.destroy());
    upstream.on("error", () => socket.destroy());
    socket.on("close", () => sockets.delete(socket));
    upstream.on("close", () => sockets.delete(upstream));
    socket.pipe(upstream).pipe(socket);
  });
  const agents = credentials.map(
    () => new Agent({ keepAlive: true, maxSockets: 1 }),
  );
  const call = (worker, method, path, body) =>
    new Promise((resolve, reject) => {
      requests++;
      const credential = credentials[worker];
      const req = request(
        {
          hostname: "127.0.0.1",
          port: 19425,
          path,
          method,
          agent: agents[worker],
          headers: {
            authorization: `Bearer ${credential.token}`,
            "x-risu-library": credential.libraryId,
            "accept-encoding": "identity",
            ...(body ? { "content-length": body.length } : {}),
          },
        },
        (res) => {
          const chunks = [];
          res.on("data", (b) => chunks.push(b));
          res.on("error", reject);
          res.on("end", () => {
            const bytes = Buffer.concat(chunks);
            if (res.statusCode < 200 || res.statusCode >= 300)
              reject(new Error(`Synthetic HTTP status ${res.statusCode}`));
            else resolve(bytes);
          });
        },
      );
      req.setTimeout(120000, () =>
        req.destroy(new Error("Synthetic request timeout")),
      );
      req.on("error", reject);
      req.end(body);
    });
  try {
    await new Promise((resolve, reject) => {
      proxy.once("error", reject);
      proxy.listen(19425, "127.0.0.1", resolve);
    });
    await delay(500);
    await call(0, "GET", "/head");
    const objects = fixture(kind);
    const groupSize = mode === "single" ? 1 : 250;
    const groups = [];
    for (let i = 0; i < objects.length; i += groupSize)
      groups.push(objects.slice(i, i + groupSize));
    const run = async (operation) => {
      sent = 0;
      received = 0;
      requests = 0;
      const start = performance.now();
      let next = 0;
      await Promise.all(
        agents.map(async (_, worker) => {
          while (next < groups.length) {
            const group = groups[next++];
            await operation(worker, group);
          }
        }),
      );
      return {
        httpBytes: sent + received,
        requestBytes: sent,
        responseBytes: received,
        requests,
        ms: Math.round(performance.now() - start),
      };
    };
    const upload = await run(async (worker, group) => {
      const result = JSON.parse(
        await call(worker, "POST", "/uploads/batch", encode(group)),
      );
      assert.deepEqual(result.verified, group.map(hash));
    });
    const download = await run(async (worker, group) => {
      if (mode === "single")
        assert.deepEqual(
          await call(worker, "GET", "/objects/" + hash(group[0])),
          group[0],
        );
      else
        verify(
          await call(worker, "POST", "/objects/batch", json(group.map(hash))),
          group,
        );
    });
    sent = 0;
    received = 0;
    requests = 0;
    const missing = JSON.parse(
      await call(
        0,
        "POST",
        "/objects/missing",
        json(objects.map((b) => ({ hash: hash(b), size: String(b.length) }))),
      ),
    );
    assert.deepEqual(missing.missing, []);
    const repeated = {
      httpBytes: sent + received,
      requests,
      objectBodyBytes: 0,
    };
    const framed = encode(objects);
    const start = performance.now();
    const compressed = gzipSync(framed);
    const gzipMs = performance.now() - start;
    const decompressStart = performance.now();
    assert.deepEqual(gunzipSync(compressed), framed);
    const gunzipMs = performance.now() - decompressStart;
    return {
      kind,
      mode,
      concurrency,
      objects: objects.length,
      payloadBytes: objects.reduce((n, b) => n + b.length, 0),
      upload,
      download,
      repeated,
      offlineGzip: {
        framedBytes: framed.length,
        gzipBytes: compressed.length,
        gzipMs,
        gunzipMs,
      },
    };
  } finally {
    for (const agent of agents) agent.destroy();
    for (const socket of sockets) socket.destroy();
    await new Promise((resolve) => proxy.close(resolve));
    if (daemon.exitCode === null && daemon.signalCode === null) {
      daemon.kill();
      await new Promise((resolve) => daemon.once("exit", resolve));
    }
  }
}
const results = [];
for (const kind of ["text", "random", "precompressed"]) {
  for (const [mode, concurrency] of [
    ["single", 2],
    ["batch", 2],
    ["batch", 4],
  ]) {
    const result = await scenario(kind, mode, concurrency);
    results.push(result);
    console.log(JSON.stringify(result));
  }
}
writeFileSync(
  output + "transfer-comparison.json",
  JSON.stringify(results, null, 2),
);
