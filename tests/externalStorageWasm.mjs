// Run against explicitly built WASM and a native synthetic golden vector.
// This harness is never imported by the product.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";
const require = createRequire(import.meta.url);
const wasm = require(resolve(process.argv[2]));
const vector = JSON.parse(readFileSync(process.argv[3], "utf8"));
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
