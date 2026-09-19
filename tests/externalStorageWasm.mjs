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
const documents = [
  {
    bytes: vector.state,
    envelope: vector.stateEnvelope,
    objectId: "state-synthetic",
    role: "state",
    canonical: wasm.canonical_sync_state_document,
  },
  {
    bytes: vector.head,
    envelope: vector.headEnvelope,
    objectId: "head",
    role: "head",
    canonical: wasm.canonical_head_document,
  },
  {
    bytes: vector.bundle,
    envelope: vector.bundleEnvelope,
    objectId: "bundle-synthetic",
    role: "bundle",
    canonical: wasm.canonical_backup_bundle_document,
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
    asBytes(vector.stateEnvelope),
    key,
    "synthetic-repository",
    "another-state",
    "state",
    64 * 1024,
  ),
);
assert.throws(() =>
  wasm.seal_object_envelope(
    new Uint8Array([1]),
    key,
    "synthetic-repository",
    "invalid-state",
    "state",
  ),
);

const stateHash = wasm.content_hash(asBytes(vector.state));
const keyedPackId = wasm.keyed_object_id(key, vector.objectNamespace, "pack", stateHash);
const keyedCatalogId = wasm.keyed_object_id(key, vector.objectNamespace, "catalog", stateHash);
assert.equal(keyedPackId, vector.keyedPackId);
assert.equal(keyedCatalogId, vector.keyedCatalogId);
assert.match(keyedPackId, /^pack-[0-9a-f]{64}$/u);
assert.match(keyedCatalogId, /^catalog-[0-9a-f]{64}$/u);
assert.notEqual(
  wasm.keyed_object_id(key, "another-capture-job", "pack", stateHash),
  keyedPackId,
);
assert.throws(() => wasm.keyed_object_id(key, "", "pack", stateHash));
assert.throws(() =>
  wasm.keyed_object_id(key, vector.objectNamespace, "state", stateHash),
);

const compressed = wasm.compress_chunk(plain, false);
assert.equal(wasm.compress_chunk(plain, true)[0], 0);
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

// Independently encode the section fixture in JavaScript, rather than echoing
// Rust's byte arrays back. These entries are not a WASM SectionEntry API test.
const section = (kind, key, value, version = null) => ({
  codec: "risunest.section-codec/v1", kind, key, value, version,
});
const sectionEntries = [
  section("hypa", "3f2a1b0c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8", {
    hypa: {
      producer: "hypa-v2", model: "text-embedding-3-small", endpoint: null,
      preprocessVersion: 1, dimensions: 8,
      vector: { inline: Buffer.from(Array.from({ length: 32 }, (_, index) => index)).toString("base64url") },
      metadata: null,
    },
  }, { writeClock: "1", writerId: "writer-a" }),
  section("hypa", "0".repeat(64), {
    tombstone: { firstPublishedGeneration: "9", firstPublishedAtMs: 1760000000000 },
  }, { writeClock: "18446744073709551615", writerId: "writer-b" }),
  section("local-plugins", JSON.stringify(["provider-manager", "json", "settings"]), {
    localPlugin: { space: "json", value: { zeta: [1, 2], alpha: null } },
  }, { writeClock: "4", writerId: "writer-a" }),
  section("local-plugins", JSON.stringify(["yumi-translator", "string", "cache:index"]), {
    localPlugin: { space: "string", value: "kept verbatim" },
  }),
  section("local-settings", "risuNestDeviceSettings", {
    localSetting: { value: { startup: "restore" } },
  }),
].map(value => asArray(new TextEncoder().encode(JSON.stringify(value))));
assert.deepEqual(sectionEntries, vector.sectionEntries);

writeFileSync(
  process.argv[4],
  JSON.stringify({
    plaintext: vector.plaintext,
    compressed: asArray(compressed),
    stateEnvelope: asArray(wasmEnvelopes[0]),
    headEnvelope: asArray(wasmEnvelopes[1]),
    bundleEnvelope: asArray(wasmEnvelopes[2]),
    pointEnvelope: asArray(wasmEnvelopes[3]),
    sectionEntries,
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
