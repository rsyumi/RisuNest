// Verification only. Product code must never import this fixed test key/nonce.
import { env, exports } from "cloudflare:workers";
import { applyD1Migrations } from "cloudflare:test";
import { beforeAll, beforeEach, expect, it } from "vitest";
import vector from "./envelope-vector.json";

const hex = (value: string) =>
  Uint8Array.from(value.match(/../g)!, (v) => parseInt(v, 16));
const decode = (value: string) =>
  Uint8Array.from(atob(value.replaceAll("-", "+").replaceAll("_", "/")), (v) =>
    v.charCodeAt(0),
  );
const encode = (value: Uint8Array) =>
  btoa(String.fromCharCode(...value))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replaceAll("=", "");
const key = () =>
  crypto.subtle.importKey("raw", hex(vector.keyHex), "AES-GCM", false, [
    "encrypt",
    "decrypt",
  ]);
async function decrypt(
  envelope: string,
  uuid = vector.uuid,
  secret?: CryptoKey,
) {
  const bytes = decode(envelope);
  const plaintext = await crypto.subtle.decrypt(
    {
      name: "AES-GCM",
      iv: bytes.slice(0, 12),
      additionalData: new TextEncoder().encode(uuid),
      tagLength: 128,
    },
    secret ?? (await key()),
    bytes.slice(12),
  );
  return new TextDecoder().decode(plaintext);
}

beforeAll(async () => {
  await applyD1Migrations(env.DB, env.TEST_MIGRATIONS);
});
beforeEach(async () => {
  await env.DB.exec("DELETE FROM endpoints");
});

it("matches the independent Node/OpenSSL AES-GCM golden vector in workerd Web Crypto", async () => {
  const ciphertext = await crypto.subtle.encrypt(
    {
      name: "AES-GCM",
      iv: hex(vector.nonceHex),
      additionalData: new TextEncoder().encode(vector.uuid),
      tagLength: 128,
    },
    await key(),
    new TextEncoder().encode(vector.plaintext),
  );
  const bytes = new Uint8Array(12 + ciphertext.byteLength);
  bytes.set(hex(vector.nonceHex));
  bytes.set(new Uint8Array(ciphertext), 12);
  expect(encode(bytes)).toBe(vector.envelope);
  expect(await decrypt(vector.envelope)).toBe(vector.plaintext);
});

it("roundtrips encrypted URLs while storing only UUID, opaque bytes and timestamp", async () => {
  const url = `https://registry.invalid/endpoints/${vector.uuid}`;
  const response = await exports.default.fetch(url, {
    method: "POST",
    headers: { "content-type": "text/plain" },
    body: vector.envelope,
  });
  expect(response.status).toBe(204);
  const fetched = await exports.default.fetch(url);
  expect(await decrypt(await fetched.text())).toBe(vector.plaintext);
  const stored = await env.DB.prepare("SELECT * FROM endpoints").all();
  expect(stored.results).toEqual([
    {
      uuid: vector.uuid,
      envelope: vector.envelope,
      updated_at: expect.any(Number),
    },
  ]);
});

it("leaves tag validation to the client, including UUID binding", async () => {
  const other = "12345678-1234-4234-8234-123456789abc";
  const url = `https://registry.invalid/endpoints/${other}`;
  expect(
    (
      await exports.default.fetch(url, {
        method: "POST",
        headers: { "content-type": "text/plain" },
        body: vector.envelope,
      })
    ).status,
  ).toBe(204);
  const response = await exports.default.fetch(url);
  await expect(decrypt(await response.text(), other)).rejects.toThrow();
});

it.each([0, 12, decode(vector.envelope).length - 1])(
  "client detects modification at nonce/ciphertext/tag byte %s",
  async (position) => {
    const bytes = decode(vector.envelope);
    bytes[position] = bytes[position]! ^ 1;
    const tampered = encode(bytes);
    const response = await exports.default.fetch(
      `https://registry.invalid/endpoints/${vector.uuid}`,
      {
        method: "POST",
        headers: { "content-type": "text/plain" },
        body: tampered,
      },
    );
    // The opaque store cannot check authentication and must not pretend to.
    expect(response.status).toBe(204);
    await expect(decrypt(tampered)).rejects.toThrow();
  },
);

it("client rejects an unrelated key", async () => {
  const other = hex(vector.keyHex);
  other[0] = other[0]! ^ 1;
  const wrongKey = await crypto.subtle.importKey(
    "raw",
    other,
    "AES-GCM",
    false,
    ["decrypt"],
  );
  await expect(
    decrypt(vector.envelope, vector.uuid, wrongKey),
  ).rejects.toThrow();
});
