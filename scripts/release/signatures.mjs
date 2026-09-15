import { createHash, createPublicKey, timingSafeEqual, verify } from "node:crypto";
import { createReadStream } from "node:fs";
import { readFile } from "node:fs/promises";

function decode64(value) {
  if (typeof value !== "string" || !value.length || value.length % 4 !== 0
    || !/^[A-Za-z0-9+/]+={0,2}$/.test(value)) throw new Error("invalid-signature-encoding");
  const bytes = Buffer.from(value, "base64");
  if (bytes.toString("base64") !== value) throw new Error("invalid-signature-encoding");
  return bytes;
}

function lines(value, expected) {
  if (typeof value !== "string" || value.length > 16384) throw new Error("invalid-signature-encoding");
  let raw = value.trim();
  if (!raw.startsWith("untrusted comment: ")) raw = decode64(raw).toString("utf8").trim();
  const parts = raw.split(/\r?\n/);
  if (parts.length !== expected || !parts[0].startsWith("untrusted comment: ")) {
    throw new Error("invalid-signature-encoding");
  }
  return parts;
}

function prepare(signatureText, publicKeyText) {
  const keyBytes = decode64(lines(publicKeyText, 2)[1]);
  const signatureLines = lines(signatureText, 4);
  const packet = decode64(signatureLines[1]);
  const global = decode64(signatureLines[3]);
  if (keyBytes.length !== 42 || packet.length !== 74 || global.length !== 64
    || !["Ed", "ED"].includes(keyBytes.subarray(0, 2).toString())
    || !["Ed", "ED"].includes(packet.subarray(0, 2).toString())
    || !signatureLines[2].startsWith("trusted comment: ")) throw new Error("invalid-signature-packet");
  if (!timingSafeEqual(keyBytes.subarray(2, 10), packet.subarray(2, 10))) throw new Error("signature-key-mismatch");
  const key = createPublicKey({ format: "der", type: "spki",
    key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), keyBytes.subarray(10)]) });
  const signature = packet.subarray(10);
  const comment = Buffer.from(signatureLines[2].slice("trusted comment: ".length));
  if (!verify(null, Buffer.concat([signature, comment]), key, global)) throw new Error("invalid-global-signature");
  return { key, signature, prehashed: packet.subarray(0, 2).toString() === "ED" };
}

function verifyPrepared(message, prepared) {
  if (!verify(null, message, prepared.key, prepared.signature)) throw new Error("invalid-package-signature");
  return true;
}

export function verifySignature(bytes, signature, publicKey) {
  const prepared = prepare(signature, publicKey);
  const message = prepared.prehashed ? createHash("blake2b512").update(bytes).digest() : bytes;
  return verifyPrepared(message, prepared);
}

export async function verifyFileSignature(file, signature, publicKey) {
  const prepared = prepare(signature, publicKey);
  if (!prepared.prehashed) return verifyPrepared(await readFile(file), prepared);
  const hash = createHash("blake2b512");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return verifyPrepared(hash.digest(), prepared);
}
