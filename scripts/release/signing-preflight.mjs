import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { writeAndroidSigning } from "./android-signing.mjs";
import { assertHttpsBaseUrl, parseArgs, requireArg } from "./common.mjs";
import { signAndVerify } from "./sign.mjs";

const UPDATE_FAILURE = "update-signing-preflight-failed";
const ANDROID_FAILURE = "android-signing-preflight-failed";

function requiredEnvironment(name, environment) {
  const value = environment[name];
  if (typeof value !== "string" || value.length === 0) throw new Error(`${name} is required.`);
  return value;
}

function createTemporaryDirectory(root) {
  const parent = resolve(root ?? tmpdir());
  mkdirSync(parent, { recursive: true });
  return mkdtempSync(join(parent, "risunest-signing-preflight-"));
}

function applyEnvironment(names, environment) {
  const previous = new Map(names.map(name => [name, process.env[name]]));
  for (const name of names) {
    const value = environment[name];
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
  }
  return () => {
    for (const [name, value] of previous) {
      if (value === undefined) delete process.env[name];
      else process.env[name] = value;
    }
  };
}

export async function preflightUpdateSigning({
  directory,
  environment = process.env,
  signer = signAndVerify,
}) {
  let restoreEnvironment = () => {};
  try {
    const publicKey = requiredEnvironment("RISUNEST_UPDATE_PUBLIC_KEY", environment);
    requiredEnvironment("TAURI_SIGNING_PRIVATE_KEY", environment);
    requiredEnvironment("TAURI_SIGNING_PRIVATE_KEY_PASSWORD", environment);
    const payload = join(directory, "synthetic-update.bin");
    writeFileSync(payload, "RisuNest signing preflight\n", { mode: 0o600 });
    restoreEnvironment = applyEnvironment([
      "TAURI_SIGNING_PRIVATE_KEY",
      "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
    ], environment);
    await signer(payload, publicKey);
  } catch {
    throw new Error(UPDATE_FAILURE);
  } finally {
    restoreEnvironment();
  }
}

export function preflightAndroidSigning({
  directory,
  environment = process.env,
  keytool = "keytool",
  androidWriter = writeAndroidSigning,
}) {
  const androidNames = [
    "ANDROID_KEYSTORE_BASE64",
    "ANDROID_KEYSTORE_PASSWORD",
    "ANDROID_KEY_ALIAS",
    "ANDROID_KEY_PASSWORD",
  ];
  let restoreEnvironment = () => {};
  try {
    for (const name of androidNames) requiredEnvironment(name, environment);
    const keystore = join(directory, "release-keystore.jks");
    const properties = join(directory, "keystore.properties");
    const request = join(directory, "synthetic.csr");
    restoreEnvironment = applyEnvironment(androidNames, environment);
    androidWriter({ keystore, properties });
    const result = spawnSync(keytool, [
      "-certreq",
      "-keystore", keystore,
      "-alias", environment.ANDROID_KEY_ALIAS,
      "-file", request,
      "-storepass:env", "ANDROID_KEYSTORE_PASSWORD",
      "-keypass:env", "ANDROID_KEY_PASSWORD",
    ], {
      env: environment,
      stdio: "pipe",
      windowsHide: true,
    });
    if (result.error || result.status !== 0 || !existsSync(request) || readFileSync(request).length === 0)
      throw new Error(ANDROID_FAILURE);
  } catch {
    throw new Error(ANDROID_FAILURE);
  } finally {
    restoreEnvironment();
  }
}

export async function signingPreflight({
  product,
  registryUrl,
  temporaryRoot,
  environment = process.env,
  signer,
  keytool,
  androidWriter,
}) {
  if (!new Set(["app", "sync"]).has(product)) throw new Error("signing-preflight-product-invalid");
  if (product === "sync") {
    try {
      assertHttpsBaseUrl(registryUrl, "Registry");
    } catch {
      throw new Error("registry-signing-preflight-failed");
    }
  }

  const directory = createTemporaryDirectory(temporaryRoot);
  try {
    await preflightUpdateSigning({ directory, environment, signer });
    if (product === "app")
      preflightAndroidSigning({ directory, environment, keytool, androidWriter });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  await signingPreflight({
    product: requireArg(args, "product"),
    registryUrl: process.env.RISUNEST_DEFAULT_REGISTRY_URL,
    temporaryRoot: process.env.SIGNING_PREFLIGHT_ROOT,
  });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : "signing-preflight-failed");
    process.exitCode = 1;
  });
}
