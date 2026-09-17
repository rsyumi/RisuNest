// Synthetic protocol comparison. No installed app, profile, or network service.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { Agent, request } from "node:http";
import { connect, createServer } from "node:net";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync, gunzipSync } from "node:zlib";

const output = fileURLToPath(new URL(".local/", import.meta.url));
const binary = process.env.RISUNEST_SYNC_SERVER_BINARY ??
  (process.env.CARGO_TARGET_DIR && resolve(process.env.CARGO_TARGET_DIR,
    "release", process.platform === "win32" ? "risunest-sync-server.exe" : "risunest-sync-server"));
const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const json = (value) => Buffer.from(JSON.stringify(value));
export const MAX_BATCH_BYTES = 8 * 1024 * 1024;
export const MAX_BATCH_OBJECTS = 1024;

export function encodeFullFrames(objects) {
  if (!Array.isArray(objects) || objects.length > MAX_BATCH_OBJECTS)
    throw new Error("too-many-frames");
  const header = Buffer.alloc(8);
  header.write("RNSB", 0, "ascii");
  header.writeUInt32BE(objects.length, 4);
  const parts = [header];
  let total = header.length;
  for (const object of objects) {
    const bytes = Buffer.isBuffer(object) ? object : Buffer.from(object);
    const payloadLength = 41 + bytes.length;
    total += 4 + payloadLength;
    if (total > MAX_BATCH_BYTES) throw new Error("batch-too-large");
    const prefix = Buffer.alloc(45);
    prefix.writeUInt32BE(payloadLength, 0);
    prefix[4] = 0;
    createHash("sha256").update(bytes).digest().copy(prefix, 5);
    prefix.writeBigUInt64BE(BigInt(bytes.length), 37);
    parts.push(prefix, bytes);
  }
  return Buffer.concat(parts, total);
}

export function decodeTransferFrames(input) {
  const bytes = Buffer.isBuffer(input) ? input : Buffer.from(input);
  if (bytes.length > MAX_BATCH_BYTES) throw new Error("batch-too-large");
  let offset = 0;
  const take = (length) => {
    if (length > bytes.length - offset) throw new Error("truncated-frame");
    const part = bytes.subarray(offset, offset + length);
    offset += length;
    return part;
  };
  if (!take(4).equals(Buffer.from("RNSB"))) throw new Error("invalid-magic");
  const count = take(4).readUInt32BE();
  if (count > MAX_BATCH_OBJECTS) throw new Error("too-many-frames");
  const frames = [];
  for (let i = 0; i < count; i++) {
    const payload = take(take(4).readUInt32BE());
    if (payload.length === 0) throw new Error("truncated-frame");
    const codec = payload[0];
    if (codec === 1) throw new Error("unexpected-delta-without-bases");
    if (codec !== 0 && codec !== 2) throw new Error("unsupported-codec");
    if (payload.length < 41) throw new Error("truncated-frame");
    const digest = payload.subarray(1, 33).toString("hex");
    const size = payload.readBigUInt64BE(33);
    if (codec === 2) {
      if (payload.length !== 41) throw new Error("trailing-bytes");
      frames.push({ codec, hash: digest, size });
    } else {
      if (size > BigInt(MAX_BATCH_BYTES)) throw new Error("frame-too-large");
      const content = payload.subarray(41);
      if (BigInt(content.length) !== size) throw new Error("frame-size-mismatch");
      if (hash(content) !== digest) throw new Error("hash-mismatch");
      frames.push({ codec, hash: digest, size, bytes: content });
    }
  }
  if (offset !== bytes.length) throw new Error("trailing-bytes");
  return frames;
}

function verifyObject(bytes, expected) {
  assert.equal(bytes.length, expected.length);
  assert.equal(hash(bytes), hash(expected));
  assert.deepEqual(bytes, expected);
}

export async function uploadFullFrames(call, worker, objects) {
  const response = await call(worker, "POST", "/uploads/frames", encodeFullFrames(objects), 204);
  assert.equal(response.length, 0);
}

export async function downloadFullFrames(call, worker, objects) {
  const response = await call(worker, "POST", "/objects/transfer", json(
    objects.map((bytes) => ({ target: hash(bytes), bases: [] })),
  ));
  const frames = decodeTransferFrames(response);
  assert.equal(frames.length, objects.length);
  for (let i = 0; i < frames.length; i++) {
    const frame = frames[i];
    assert.equal(frame.hash, hash(objects[i]));
    assert.equal(frame.size, BigInt(objects[i].length));
  }
  for (let i = 0; i < frames.length; i++) {
    const frame = frames[i];
    const bytes = frame.codec === 0 ? frame.bytes :
      await call(worker, "GET", "/objects/" + frame.hash);
    verifyObject(bytes, objects[i]);
  }
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
  const call = (worker, method, path, body, expectedStatus = 200) =>
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
            if (res.statusCode !== expectedStatus)
              reject(new Error(`Synthetic HTTP status ${res.statusCode}; expected ${expectedStatus}`));
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
    const upload = await run((worker, group) => uploadFullFrames(call, worker, group));
    const download = await run(async (worker, group) => {
      if (mode === "single")
        verifyObject(
          await call(worker, "GET", "/objects/" + hash(group[0])),
          group[0],
        );
      else
        await downloadFullFrames(call, worker, group);
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
    const framed = encodeFullFrames(objects);
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
export async function runComparison() {
  if (!binary) throw new Error("Set CARGO_TARGET_DIR or RISUNEST_SYNC_SERVER_BINARY to a locally built server");
  mkdirSync(output, { recursive: true });
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
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await runComparison();
}
