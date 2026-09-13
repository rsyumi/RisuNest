// Run against explicitly built WASM and a native synthetic golden vector.
// This harness is never imported by the product.
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";
const require = createRequire(import.meta.url);
const wasm = require(resolve(process.argv[2]));
const vector = JSON.parse(readFileSync(process.argv[3], "utf8"));
const plain = Uint8Array.from(vector.plaintext);
assert.deepEqual(
  [
    ...wasm.decompress_chunk(
      Uint8Array.from(vector.compressed),
      plain.length,
      Uint8Array.from(vector.hash),
    ),
  ],
  vector.plaintext,
);
assert.throws(() =>
  wasm.decompress_chunk(
    Uint8Array.from(vector.compressed),
    plain.length - 1,
    Uint8Array.from(vector.hash),
  ),
);
assert.deepEqual(
  [
    ...wasm.recover_key(
      Uint8Array.from(vector.recovery),
      "synthetic-repository",
      vector.code,
    ),
  ],
  vector.key,
);
assert.throws(() =>
  wasm.recover_key(
    Uint8Array.from(vector.recovery),
    "wrong-repository",
    vector.code,
  ),
);
const compressed = wasm.compress_chunk(plain, false);
assert.equal(wasm.compress_chunk(plain, true)[0], 0);
const recovery = wasm.protect_recovery(
  "synthetic-repository",
  "https://synthetic.invalid/folder",
  Uint8Array.from(vector.key),
  vector.code,
);
writeFileSync(
  process.argv[4],
  JSON.stringify({
    plaintext: vector.plaintext,
    compressed: [...compressed],
    recovery: [...recovery],
    code: vector.code,
  }),
);
assert.deepEqual(
  [...wasm.content_hash(Uint8Array.from(vector.plaintext))],
  vector.hash,
);
assert.deepEqual(
  [
    ...wasm.verify_encrypted_object(
      Uint8Array.from(vector.ciphertext),
      Uint8Array.from(vector.key),
      vector.binding,
    ),
  ],
  vector.plaintext,
);
assert.throws(() =>
  wasm.verify_encrypted_object(
    Uint8Array.from(vector.ciphertext.slice(0, -1)),
    Uint8Array.from(vector.key),
    vector.binding,
  ),
);
assert.throws(() =>
  wasm.verify_encrypted_object(
    Uint8Array.from(vector.ciphertext),
    Uint8Array.from(vector.key),
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
  "Native/WASM hash, secretstream, record key, tamper and size vectors passed.",
);
