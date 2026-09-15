// Run against explicitly built WASM and a native synthetic golden vector.
// This harness is never imported by the product.
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";

const require = createRequire(import.meta.url);
const wasm = require(resolve(process.argv[2]));
const vector = JSON.parse(readFileSync(process.argv[3], "utf8"));
const asBytes = (value) => Uint8Array.from(value);
const asArray = (value) => [...value];
const key = asBytes(vector.key);
const plain = asBytes(vector.plaintext);

assert.deepEqual(
  asArray(
    wasm.decompress_chunk(
      asBytes(vector.compressed),
      plain.length,
      asBytes(vector.hash),
    ),
  ),
  vector.plaintext,
);
assert.throws(() =>
  wasm.decompress_chunk(
    asBytes(vector.compressed),
    plain.length - 1,
    asBytes(vector.hash),
  ),
);
assert.deepEqual(
  asArray(
    wasm.recover_key(
      asBytes(vector.recovery),
      "synthetic-repository",
      vector.code,
    ),
  ),
  vector.key,
);
assert.equal(
  wasm.recover_connection_metadata(
    asBytes(vector.recovery),
    "synthetic-repository",
    vector.code,
  ),
  "https://synthetic.invalid/folder",
);
assert.throws(() =>
  wasm.recover_key(
    asBytes(vector.recovery),
    "wrong-repository",
    vector.code,
  ),
);
assert.deepEqual(
  asArray(wasm.canonical_recovery_envelope(asBytes(vector.recovery))),
  vector.recovery,
);

const recoveryDocument = JSON.parse(
  new TextDecoder().decode(asBytes(vector.recovery)),
);
assert.equal(typeof recoveryDocument.wrappedKey, "string");
assert.match(recoveryDocument.wrappedKey, /^[A-Za-z0-9_-]+$/u);
assert.equal(recoveryDocument.wrappedKey.includes("="), false);
assert.deepEqual(
  asArray(new TextEncoder().encode(JSON.stringify(recoveryDocument))),
  vector.recovery,
);

const documents = [
  {
    bytes: vector.snapshot,
    envelope: vector.snapshotEnvelope,
    objectId: "snapshot-synthetic",
    role: "snapshot",
    canonical: wasm.canonical_snapshot_document,
  },
  {
    bytes: vector.head,
    envelope: vector.headEnvelope,
    objectId: "head",
    role: "head",
    canonical: wasm.canonical_head_document,
  },
  {
    bytes: vector.point,
    envelope: vector.pointEnvelope,
    objectId: "point-synthetic",
    role: "backupPoint",
    canonical: wasm.canonical_backup_point_document,
  },
];

for (const document of documents) {
  assert.equal(String.fromCharCode(...document.envelope.slice(0, 4)), "RNX1");
  const opened = wasm.open_object_envelope(
    asBytes(document.envelope),
    key,
    "synthetic-repository",
    document.objectId,
    document.role,
    64 * 1024,
  );
  assert.deepEqual(asArray(opened), document.bytes);
  assert.deepEqual(asArray(document.canonical(opened)), document.bytes);

  const tampered = asBytes(document.envelope);
  tampered[tampered.length - 1] ^= 1;
  assert.throws(() =>
    wasm.open_object_envelope(
      tampered,
      key,
      "synthetic-repository",
      document.objectId,
      document.role,
      64 * 1024,
    ),
  );
}
assert.throws(() =>
  wasm.open_object_envelope(
    asBytes(vector.snapshotEnvelope),
    key,
    "synthetic-repository",
    "another-snapshot",
    "snapshot",
    64 * 1024,
  ),
);
assert.throws(() =>
  wasm.seal_object_envelope(
    new Uint8Array([1]),
    key,
    "synthetic-repository",
    "invalid-snapshot",
    "snapshot",
  ),
);

const snapshotHash = wasm.content_hash(asBytes(vector.snapshot));
const keyedPackId = wasm.keyed_object_id(
  key,
  vector.objectNamespace,
  "pack",
  snapshotHash,
);
const keyedCatalogId = wasm.keyed_object_id(
  key,
  vector.objectNamespace,
  "catalog",
  snapshotHash,
);
assert.equal(keyedPackId, vector.keyedPackId);
assert.equal(keyedCatalogId, vector.keyedCatalogId);
assert.match(keyedPackId, /^pack-[0-9a-f]{64}$/u);
assert.match(keyedCatalogId, /^catalog-[0-9a-f]{64}$/u);
assert.notEqual(
  wasm.keyed_object_id(key, "another-capture-job", "pack", snapshotHash),
  keyedPackId,
);
assert.throws(() => wasm.keyed_object_id(key, "", "pack", snapshotHash));
assert.throws(() =>
  wasm.keyed_object_id(key, vector.objectNamespace, "snapshot", snapshotHash),
);

const compressed = wasm.compress_chunk(plain, false);
assert.equal(wasm.compress_chunk(plain, true)[0], 0);
const recovery = wasm.protect_recovery(
  "synthetic-repository",
  "https://synthetic.invalid/folder",
  key,
  vector.code,
);
const wasmRecoveryDocument = JSON.parse(new TextDecoder().decode(recovery));
assert.equal(typeof wasmRecoveryDocument.wrappedKey, "string");
assert.match(wasmRecoveryDocument.wrappedKey, /^[A-Za-z0-9_-]+$/u);
assert.equal(wasmRecoveryDocument.wrappedKey.includes("="), false);

const wasmEnvelopes = documents.map((document) =>
  wasm.seal_object_envelope(
    asBytes(document.bytes),
    key,
    "synthetic-repository",
    document.objectId,
    document.role,
  ),
);
for (const envelope of wasmEnvelopes) {
  assert.equal(String.fromCharCode(...envelope.slice(0, 4)), "RNX1");
}

writeFileSync(
  process.argv[4],
  JSON.stringify({
    plaintext: vector.plaintext,
    compressed: asArray(compressed),
    recovery: asArray(recovery),
    code: vector.code,
    snapshotEnvelope: asArray(wasmEnvelopes[0]),
    headEnvelope: asArray(wasmEnvelopes[1]),
    pointEnvelope: asArray(wasmEnvelopes[2]),
    keyedPackId,
    keyedCatalogId,
  }),
);

assert.deepEqual(asArray(wasm.content_hash(plain)), vector.hash);
assert.deepEqual(
  asArray(wasm.verify_encrypted_object(asBytes(vector.ciphertext), key, vector.binding)),
  vector.plaintext,
);
assert.throws(() =>
  wasm.verify_encrypted_object(
    asBytes(vector.ciphertext.slice(0, -1)),
    key,
    vector.binding,
  ),
);
assert.throws(() =>
  wasm.verify_encrypted_object(
    asBytes(vector.ciphertext),
    key,
    "other-repository",
  ),
);
assert.throws(() => wasm.content_hash(new Uint8Array(1024 * 1024 + 1)));

const keys = JSON.parse(
  readFileSync(
    new URL(
      "../src/ts/storage/tests/fixtures/logicalRecordKeyV1Golden.json",
      import.meta.url,
    ),
    "utf8",
  ),
);
for (const { encoded } of keys.roundTrip) {
  assert.equal(wasm.canonical_record_key(encoded), encoded);
}

console.log(
  "Native-to-WASM RNX1 vectors passed; WASM reverse vectors generated.",
);
