import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs, requireArg } from "./common.mjs";

function property(value) {
  if (!value) throw new Error("Android signing secret is missing.");
  return value.replace(/\\/g, "\\\\").replace(/([:=#! ])/g, "\\$1");
}

export function writeAndroidSigning({ keystore, properties }) {
  const encoded = process.env.ANDROID_KEYSTORE_BASE64;
  if (!encoded || !/^[A-Za-z0-9+/\r\n]+={0,2}$/.test(encoded))
    throw new Error("ANDROID_KEYSTORE_BASE64 is missing or invalid.");
  const bytes = Buffer.from(encoded.replace(/\s/g, ""), "base64");
  if (!bytes.length) throw new Error("Android keystore is empty.");
  mkdirSync(dirname(keystore), { recursive: true });
  writeFileSync(keystore, bytes, { mode: 0o600 });
  writeFileSync(properties, [
    `storeFile=${property(resolve(keystore).replace(/\\/g, "/"))}`,
    `storePassword=${property(process.env.ANDROID_KEYSTORE_PASSWORD)}`,
    `keyAlias=${property(process.env.ANDROID_KEY_ALIAS)}`,
    `keyPassword=${property(process.env.ANDROID_KEY_PASSWORD)}`,
    "",
  ].join("\n"), { mode: 0o600 });
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  writeAndroidSigning({
    keystore: requireArg(args, "keystore"),
    properties: requireArg(args, "properties"),
  });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
