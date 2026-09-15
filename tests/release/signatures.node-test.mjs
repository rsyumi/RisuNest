import assert from "node:assert/strict";
import { createHash, generateKeyPairSync, randomBytes, sign } from "node:crypto";
import test from "node:test";
import { verifySignature } from "../../scripts/release/signatures.mjs";

export function signingFixture(bytes, prehashed = true) {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const keyId = randomBytes(8);
  const key = Buffer.concat([Buffer.from("Ed"), keyId,
    publicKey.export({ format: "der", type: "spki" }).subarray(-32)]);
  const message = prehashed ? createHash("blake2b512").update(bytes).digest() : bytes;
  const signature = sign(null, message, privateKey);
  const comment = "timestamp:1\tfile:synthetic.bin";
  const packet = Buffer.concat([Buffer.from(prehashed ? "ED" : "Ed"), keyId, signature]);
  const global = sign(null, Buffer.concat([signature, Buffer.from(comment)]), privateKey);
  return { key: `untrusted comment: synthetic test key\n${key.toString("base64")}\n`,
    signature: `untrusted comment: synthetic test signature\n${packet.toString("base64")}\ntrusted comment: ${comment}\n${global.toString("base64")}\n` };
}

test("accepts Tauri wrapped and native minisign encodings, including both supported algorithms", () => {
  const bytes = Buffer.from("synthetic update");
  for (const prehashed of [true, false]) {
    const fixture = signingFixture(bytes, prehashed);
    assert.equal(verifySignature(bytes, fixture.signature, fixture.key), true);
    assert.equal(verifySignature(bytes, Buffer.from(fixture.signature).toString("base64"),
      Buffer.from(fixture.key).toString("base64")), true);
  }
});

test("rejects changed bytes, another key and a changed trusted comment", () => {
  const bytes = Buffer.from("synthetic update");
  const fixture = signingFixture(bytes);
  assert.throws(() => verifySignature(Buffer.from("changed update"), fixture.signature, fixture.key), /signature/);
  assert.throws(() => verifySignature(bytes, fixture.signature, signingFixture(bytes).key), /key/);
  assert.throws(() => verifySignature(bytes, fixture.signature.replace("timestamp:1", "timestamp:2"), fixture.key), /signature/);
  assert.throws(() => verifySignature(bytes, fixture.signature, ""));
});

test("verifies an independent minisign-verify upstream vector", () => {
  const key = "untrusted comment: upstream vector\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
  const signature = "untrusted comment: signature from minisign secret key\n"
    + "RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\n"
    + "trusted comment: timestamp:1555779966\tfile:test\n"
    + "QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==";
  assert.equal(verifySignature(Buffer.from("test"), signature, key), true);
});

test("does not accept malformed base64, truncated packets or appended signature data", () => {
  const bytes = Buffer.from("synthetic update");
  const fixture = signingFixture(bytes);
  for (const signature of ["?", fixture.signature + "unexpected\n",
    fixture.signature.replace(/^(.+\n)[^\n]+/, "$1AAAA")]) {
    assert.throws(() => verifySignature(bytes, signature, fixture.key));
  }
});
