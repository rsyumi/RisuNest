import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import test from "node:test";
import {
  MAX_BATCH_BYTES,
  MAX_BATCH_OBJECTS,
  decodeTransferFrames,
  downloadFullFrames,
  encodeFullFrames,
  uploadFullFrames,
} from "./transfer-comparison.mjs";

const hash = (bytes) => createHash("sha256").update(bytes).digest("hex");
const goldens = JSON.parse(readFileSync(
  new URL("../../crates/sync-wire/tests/transfer-golden.json", import.meta.url),
  "utf8",
));

function fullRequired(expected, size = BigInt(expected.length)) {
  const frame = encodeFullFrames([Buffer.alloc(0)]);
  frame[12] = 2;
  Buffer.from(hash(expected), "hex").copy(frame, 13);
  frame.writeBigUInt64BE(size, 45);
  return frame;
}

function combine(...batches) {
  const header = encodeFullFrames([]);
  header.writeUInt32BE(batches.reduce((count, bytes) => count + bytes.readUInt32BE(4), 0), 4);
  return Buffer.concat([header, ...batches.map((bytes) => bytes.subarray(8))]);
}

test("full frames match the shared Rust golden bytes", () => {
  assert.equal(goldens.length, 3);
  for (const { name, objects, encoded } of goldens) {
    const expected = objects.map((hex) => Buffer.from(hex, "hex"));
    const bytes = encodeFullFrames(expected);
    assert.equal(bytes.toString("hex"), encoded, name);
    const frames = decodeTransferFrames(Buffer.from(encoded, "hex"));
    assert.deepEqual(frames.map((frame) => frame.bytes), expected, name);
    for (const [index, frame] of frames.entries()) {
      assert.equal(frame.codec, 0);
      assert.equal(frame.hash, hash(expected[index]));
      assert.equal(frame.size, BigInt(expected[index].length));
    }
  }
});

test("full frame byte limits include all framing", () => {
  const expected = Buffer.alloc(MAX_BATCH_BYTES - 53);
  const bytes = encodeFullFrames([expected]);
  assert.equal(bytes.length, MAX_BATCH_BYTES);
  assert.deepEqual(decodeTransferFrames(bytes)[0].bytes, expected);
  assert.throws(() => encodeFullFrames([Buffer.alloc(MAX_BATCH_BYTES - 52)]), /batch-too-large/);
  assert.throws(() => decodeTransferFrames(Buffer.concat([bytes, Buffer.alloc(1)])), /batch-too-large/);
});

test("encoder and decoder enforce the exact frame count ceiling", () => {
  const objects = Array.from({ length: MAX_BATCH_OBJECTS }, () => Buffer.alloc(0));
  assert.equal(decodeTransferFrames(encodeFullFrames(objects)).length, MAX_BATCH_OBJECTS);
  assert.throws(() => encodeFullFrames([...objects, Buffer.alloc(0)]), /too-many-frames/);
  assert.throws(() => encodeFullFrames(null), /too-many-frames/);
  const invalid = encodeFullFrames([]);
  invalid.writeUInt32BE(MAX_BATCH_OBJECTS + 1, 4);
  assert.throws(() => decodeTransferFrames(invalid), /too-many-frames/);
});

test("all truncated prefixes and outer trailing bytes are rejected", () => {
  for (const encoded of [encodeFullFrames([Buffer.from("data")]), fullRequired(Buffer.from("large"))]) {
    for (let end = 0; end < encoded.length; end++) {
      assert.throws(() => decodeTransferFrames(encoded.subarray(0, end)), undefined, `prefix ${end}`);
    }
    assert.throws(() => decodeTransferFrames(Buffer.concat([encoded, Buffer.alloc(1)])), /trailing-bytes/);
  }
});

test("magic is exact bytes and empty-base downloads reject delta and unknown codecs", () => {
  const encoded = encodeFullFrames([Buffer.from("data")]);
  const obsolete = Buffer.from(encoded);
  obsolete.write("RNSF");
  assert.throws(() => decodeTransferFrames(obsolete), /invalid-magic/);
  for (let i = 0; i < 4; i++) {
    const invalid = Buffer.from(encoded);
    invalid[i] |= 0x80;
    assert.throws(() => decodeTransferFrames(invalid), /invalid-magic/);
  }
  for (const codec of [1, 3, 255]) {
    const invalid = Buffer.from(encoded);
    invalid[12] = codec;
    assert.throws(() => decodeTransferFrames(invalid), codec === 1 ? /unexpected-delta/ : /unsupported-codec/);
  }
});

test("payload sizes, full object hashes and inner trailing bytes are validated", () => {
  const encoded = encodeFullFrames([Buffer.from("data")]);
  for (const length of [0, 40, 44, 46, 0xffffffff]) {
    const invalid = Buffer.from(encoded);
    invalid.writeUInt32BE(length, 8);
    assert.throws(() => decodeTransferFrames(invalid), undefined, `length ${length}`);
  }
  for (const index of [13, encoded.length - 1]) {
    const invalid = Buffer.from(encoded);
    invalid[index] ^= 1;
    assert.throws(() => decodeTransferFrames(invalid), /hash-mismatch/);
  }
  for (const size of [0n, 3n, 5n, 0xffffffffffffffffn]) {
    const invalid = Buffer.from(encoded);
    invalid.writeBigUInt64BE(size, 45);
    assert.throws(() => decodeTransferFrames(invalid), /frame-(size-mismatch|too-large)/);
  }
  const trailing = Buffer.concat([encoded, Buffer.alloc(1)]);
  trailing.writeUInt32BE(46, 8);
  assert.throws(() => decodeTransferFrames(trailing), /frame-size-mismatch/);
});

test("FullRequired keeps the exact u64 size and has no payload bytes", () => {
  const expected = Buffer.from("synthetic large object");
  const encoded = fullRequired(expected, 0xffffffffffffffffn);
  assert.deepEqual(decodeTransferFrames(encoded), [{ codec: 2, hash: hash(expected), size: 0xffffffffffffffffn }]);
  const trailing = Buffer.concat([encoded, Buffer.alloc(1)]);
  trailing.writeUInt32BE(42, 8);
  assert.throws(() => decodeTransferFrames(trailing), /trailing-bytes/);
});

test("upload uses full frames and requires the bodyless success response", async () => {
  const objects = [Buffer.from("synthetic upload")];
  await uploadFullFrames(async (worker, method, path, body, expectedStatus) => {
    assert.equal(worker, 2);
    assert.equal(method, "POST");
    assert.equal(path, "/uploads/frames");
    assert.equal(expectedStatus, 204);
    assert.deepEqual(body, encodeFullFrames(objects));
    return Buffer.alloc(0);
  }, 2, objects);
  await assert.rejects(uploadFullFrames(async () => Buffer.from("unexpected response"), 0, objects));
  await assert.rejects(uploadFullFrames(async () => { throw new Error("unauthorized"); }, 0, objects), /unauthorized/);
});

test("FullRequired GET shares the counted authenticated request path", async () => {
  const objects = [Buffer.from("small full"), Buffer.from("synthetic fallback")];
  const encoded = combine(encodeFullFrames([objects[0]]), fullRequired(objects[1]));
  const calls = [];
  let requestBytes = 0;
  let responseBytes = 0;
  await downloadFullFrames(async (worker, method, path, body) => {
    assert.equal(worker, 3);
    calls.push({ method, path });
    requestBytes += body?.length ?? 0;
    let response;
    if (method === "POST") {
      assert.equal(path, "/objects/transfer");
      assert.deepEqual(JSON.parse(body), objects.map((bytes) => ({ target: hash(bytes), bases: [] })));
      response = encoded;
    } else {
      assert.equal(method, "GET");
      assert.equal(path, "/objects/" + hash(objects[1]));
      assert.equal(body, undefined);
      response = objects[1];
    }
    responseBytes += response.length;
    return response;
  }, 3, objects);
  assert.equal(calls.length, 2);
  assert.equal(requestBytes, Buffer.byteLength(JSON.stringify(objects.map((bytes) => ({ target: hash(bytes), bases: [] })))));
  assert.equal(responseBytes, encoded.length + objects[1].length);
});

test("inline full responses do not issue a fallback GET", async () => {
  const objects = [Buffer.alloc(0), Buffer.from([255, 0, 7])];
  let requests = 0;
  await downloadFullFrames(async (_, method, path) => {
    requests++;
    assert.equal(method, "POST");
    assert.equal(path, "/objects/transfer");
    return encodeFullFrames(objects);
  }, 0, objects);
  assert.equal(requests, 1);
});

test("wrong counts, targets or sizes fail before any fallback request", async () => {
  const expected = Buffer.from("synthetic expected");
  for (const encoded of [
    encodeFullFrames([]),
    fullRequired(Buffer.from("another target")),
    fullRequired(expected, BigInt(expected.length + 1)),
    fullRequired(expected, 0xffffffffffffffffn),
  ]) {
    let requests = 0;
    await assert.rejects(downloadFullFrames(async () => {
      requests++;
      return encoded;
    }, 0, [expected]));
    assert.equal(requests, 1);
  }
  let requests = 0;
  const encoded = combine(fullRequired(expected), fullRequired(Buffer.from("another target")));
  await assert.rejects(downloadFullFrames(async () => {
    requests++;
    return encoded;
  }, 0, [expected, expected]));
  assert.equal(requests, 1);
});

test("fallback GET must succeed with exact size and hash", async () => {
  const expected = Buffer.from("synthetic fallback");
  for (const response of [Buffer.alloc(expected.length), expected.subarray(1)]) {
    let requests = 0;
    await assert.rejects(downloadFullFrames(async (_, method) => {
      requests++;
      return method === "POST" ? fullRequired(expected) : response;
    }, 0, [expected]));
    assert.equal(requests, 2);
  }
  await assert.rejects(downloadFullFrames(async (_, method) => {
    if (method === "POST") return fullRequired(expected);
    throw new Error("unauthorized");
  }, 0, [expected]), /unauthorized/);
});
