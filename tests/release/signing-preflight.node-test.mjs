import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";
import {
  preflightAndroidSigning,
  preflightUpdateSigning,
  signingPreflight,
} from "../../scripts/release/signing-preflight.mjs";

function temporaryRoot() {
  return mkdtempSync(join(tmpdir(), "risunest-signing-preflight-test-"));
}

const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const tauriCli = join(repository, "node_modules/@tauri-apps/cli/tauri.js");

function syntheticEnvironment(overrides = {}) {
  return {
    ...process.env,
    TAURI_SIGNING_PRIVATE_KEY: "synthetic-private-key",
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD: "synthetic-update-password",
    RISUNEST_UPDATE_PUBLIC_KEY: "synthetic-public-key",
    ANDROID_KEYSTORE_BASE64: Buffer.from("synthetic-keystore").toString("base64"),
    ANDROID_KEYSTORE_PASSWORD: "synthetic-store-password",
    ANDROID_KEY_ALIAS: "synthetic-alias",
    ANDROID_KEY_PASSWORD: "synthetic-entry-password",
    ...overrides,
  };
}

test("validates common update signing for both products and Android only for app", async () => {
  const root = temporaryRoot();
  const calls = [];
  const signer = async (path, publicKey) => calls.push(["update", readFileSync(path, "utf8"), publicKey]);
  const androidWriter = () => calls.push(["android"]);
  const keytool = process.execPath;
  try {
    await signingPreflight({
      product: "sync",
      registryUrl: "https://registry.rsyumi.workers.dev/",
      temporaryRoot: root,
      environment: syntheticEnvironment(),
      signer,
      androidWriter,
      keytool,
    });
    assert.deepEqual(calls.map(call => call[0]), ["update"]);
    calls.length = 0;
    await assert.rejects(signingPreflight({
      product: "app",
      temporaryRoot: root,
      environment: syntheticEnvironment(),
      signer,
      androidWriter,
      keytool,
    }), /android-signing-preflight-failed/);
    assert.deepEqual(calls.map(call => call[0]), ["update", "android"]);
    assert.deepEqual(readdirSync(root), []);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects missing update inputs and malformed registry URLs with bounded errors", async () => {
  const root = temporaryRoot();
  try {
    await assert.rejects(signingPreflight({
      product: "sync",
      registryUrl: "http://registry.example.test/",
      temporaryRoot: root,
      environment: syntheticEnvironment(),
      signer: async () => {},
    }), { message: "registry-signing-preflight-failed" });
    await assert.rejects(preflightUpdateSigning({
      directory: root,
      environment: syntheticEnvironment({ TAURI_SIGNING_PRIVATE_KEY: "" }),
      signer: async () => {},
    }), { message: "update-signing-preflight-failed" });
    await assert.rejects(preflightUpdateSigning({
      directory: root,
      environment: syntheticEnvironment({ RISUNEST_UPDATE_PUBLIC_KEY: "" }),
      signer: async () => {},
    }), { message: "update-signing-preflight-failed" });
    await assert.rejects(preflightUpdateSigning({
      directory: root,
      environment: syntheticEnvironment({ TAURI_SIGNING_PRIVATE_KEY_PASSWORD: "" }),
      signer: async () => {},
    }), { message: "update-signing-preflight-failed" });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("bounds update signer errors and removes temporary files after failure", async () => {
  const root = temporaryRoot();
  try {
    await assert.rejects(signingPreflight({
      product: "sync",
      registryUrl: "https://registry.example.test/",
      temporaryRoot: root,
      environment: syntheticEnvironment(),
      signer: async () => { throw new Error("secret-bearing signer output"); },
    }), { message: "update-signing-preflight-failed" });
    assert.deepEqual(readdirSync(root), []);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("rejects absent and malformed Android inputs without exposing keytool output", () => {
  const root = temporaryRoot();
  try {
    for (const name of [
      "ANDROID_KEYSTORE_BASE64",
      "ANDROID_KEYSTORE_PASSWORD",
      "ANDROID_KEY_ALIAS",
      "ANDROID_KEY_PASSWORD",
    ]) {
      assert.throws(() => preflightAndroidSigning({
        directory: root,
        environment: syntheticEnvironment({ [name]: "" }),
      }), { message: "android-signing-preflight-failed" });
    }
    assert.throws(() => preflightAndroidSigning({
      directory: root,
      environment: syntheticEnvironment({ ANDROID_KEYSTORE_BASE64: "not base64" }),
    }), { message: "android-signing-preflight-failed" });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("uses the real Tauri signer to validate a synthetic update key pair", async () => {
  const root = temporaryRoot();
  const first = join(root, "first.key");
  const second = join(root, "second.key");
  const password = "synthetic-update-password";
  try {
    for (const key of [first, second]) {
      const generated = spawnSync(process.execPath, [
        tauriCli,
        "signer", "generate",
        "--write-keys", key,
        "--password", password,
        "--ci",
      ], { cwd: repository, stdio: "pipe", windowsHide: true });
      assert.equal(generated.status, 0, "Synthetic Tauri key generation failed");
    }
    const environment = syntheticEnvironment({
      TAURI_SIGNING_PRIVATE_KEY: readFileSync(first, "utf8"),
      TAURI_SIGNING_PRIVATE_KEY_PASSWORD: password,
      RISUNEST_UPDATE_PUBLIC_KEY: readFileSync(`${first}.pub`, "utf8"),
    });
    await assert.doesNotReject(preflightUpdateSigning({ directory: root, environment }));
    await assert.rejects(preflightUpdateSigning({
      directory: root,
      environment: {
        ...environment,
        RISUNEST_UPDATE_PUBLIC_KEY: readFileSync(`${second}.pub`, "utf8"),
      },
    }), { message: "update-signing-preflight-failed" });
    await assert.rejects(preflightUpdateSigning({
      directory: root,
      environment: { ...environment, TAURI_SIGNING_PRIVATE_KEY_PASSWORD: "wrong-password" },
    }), { message: "update-signing-preflight-failed" });
    await assert.rejects(preflightUpdateSigning({
      directory: root,
      environment: { ...environment, RISUNEST_UPDATE_PUBLIC_KEY: "malformed-public-key" },
    }), { message: "update-signing-preflight-failed" });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("uses environment-backed keytool passwords and accepts a synthetic Android keystore", () => {
  const root = temporaryRoot();
  const keystore = join(root, "source.jks");
  const environment = syntheticEnvironment({
    SYNTHETIC_STORE_PASSWORD: "synthetic-store-password",
    SYNTHETIC_ENTRY_PASSWORD: "synthetic-entry-password",
  });
  const generated = spawnSync("keytool", [
    "-genkeypair",
    "-keystore", keystore,
    "-storetype", "JKS",
    "-alias", environment.ANDROID_KEY_ALIAS,
    "-keyalg", "RSA",
    "-keysize", "2048",
    "-validity", "1",
    "-dname", "CN=Synthetic Signing Preflight",
    "-storepass:env", "SYNTHETIC_STORE_PASSWORD",
    "-keypass:env", "SYNTHETIC_ENTRY_PASSWORD",
    "-noprompt",
  ], { env: environment, stdio: "pipe", windowsHide: true });
  try {
    assert.equal(generated.status, 0, "Synthetic keytool fixture generation failed");
    environment.ANDROID_KEYSTORE_BASE64 = readFileSync(keystore).toString("base64");
    assert.doesNotThrow(() => preflightAndroidSigning({ directory: root, environment }));
    for (const override of [
      { ANDROID_KEY_ALIAS: "missing-alias" },
      { ANDROID_KEYSTORE_PASSWORD: "wrong-store-password" },
      { ANDROID_KEY_PASSWORD: "wrong-entry-password" },
    ]) assert.throws(() => preflightAndroidSigning({
      directory: root,
      environment: { ...environment, ...override },
    }), { message: "android-signing-preflight-failed" });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
