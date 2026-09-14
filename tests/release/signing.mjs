import { createHash, generateKeyPairSync, randomBytes, sign } from "node:crypto";

export function createSigner() {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const keyId = randomBytes(8);
  const packet = Buffer.concat([Buffer.from("Ed"), keyId,
    publicKey.export({ format: "der", type: "spki" }).subarray(-32)]);
  const key = `untrusted comment: synthetic test key\n${packet.toString("base64")}\n`;
  return { key, sign(bytes, prehashed = true) {
    const message = prehashed ? createHash("blake2b512").update(bytes).digest() : bytes;
    const signature = sign(null, message, privateKey);
    const comment = "timestamp:1\tfile:synthetic.bin";
    const signedPacket = Buffer.concat([Buffer.from(prehashed ? "ED" : "Ed"), keyId, signature]);
    const global = sign(null, Buffer.concat([signature, Buffer.from(comment)]), privateKey);
    return `untrusted comment: synthetic test signature\n${signedPacket.toString("base64")}\ntrusted comment: ${comment}\n${global.toString("base64")}\n`;
  } };
}
