import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { verifyFileSignature } from "./signatures.mjs";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

export async function signAndVerify(path, publicKey) {
  if (!process.env.TAURI_SIGNING_PRIVATE_KEY)
    throw new Error("TAURI_SIGNING_PRIVATE_KEY is required.");
  const cli = join(repo, "node_modules/@tauri-apps/cli/tauri.js");
  if (!existsSync(cli)) throw new Error("Install the repository Tauri CLI before signing.");
  const result = spawnSync(process.execPath, [cli, "signer", "sign", path], {
    cwd: repo,
    env: process.env,
    stdio: "pipe",
    windowsHide: true,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Tauri signer failed for ${path}.`);
  const signaturePath = `${path}.sig`;
  if (!existsSync(signaturePath) || readFileSync(signaturePath).length === 0)
    throw new Error(`Tauri signer did not create ${signaturePath}.`);
  const signature = readFileSync(signaturePath, "utf8").trim();
  await verifyFileSignature(path, signature, publicKey);
  return { signaturePath, signature };
}
