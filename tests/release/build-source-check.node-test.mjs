import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { androidVersionCode, iosBundleVersion } from "../../scripts/release/common.mjs";
import { releaseDownloads } from "../../scripts/release/downloads.mjs";
import { sourceCheck, validateVendorInput } from "../../scripts/release/source-check.mjs";
import cloudflared from "../../scripts/release/cloudflared.json" with { type: "json" };
import { releaseTauriConfig } from "../../scripts/release/tauri-config.mjs";

function write(path, value = "locked\n") {
  mkdirSync(join(path, ".."), { recursive: true });
  writeFileSync(path, value);
}

function json(path, value) {
  write(path, `${JSON.stringify(value)}\n`);
}

function repository(product, version) {
  const root = mkdtempSync(join(tmpdir(), "risunest-source-check-"));
  write(join(root, "crates/release-update/Cargo.lock"));
  write(join(root, "release-notes", product, `${version}.md`), `# ${product} ${version}\n`);
  if (product === "app") {
    json(join(root, "version.json"), { version });
    json(join(root, "src-tauri/tauri.conf.json"), { version });
    write(join(root, "pnpm-lock.yaml"));
    write(join(root, "src-tauri/Cargo.lock"));
  } else {
    json(join(root, "server/version.json"), {
      version,
      compatibility: {
        protocolId: "risunest-sync/v1",
        storeFormatId: "risunest-sync-store/v8",
        automaticApply: true,
      },
    });
    for (const file of [
      "crates/sync-wire/Cargo.lock",
      "server/sync/Cargo.lock",
      "server/manager/Cargo.lock",
      "server/manager/gui/pnpm-lock.yaml",
      "server/manager/gui/src-tauri/Cargo.lock",
    ]) write(join(root, file));
    write(join(root, "server/sync/src/lib.rs"), [
      'pub const PROTOCOL_ID: &str = "risunest-sync/v1";',
      'pub const STORE_FORMAT_ID: &str = "risunest-sync-store/v8";',
    ].join("\n"));
    for (const file of [
      "server/sync/Cargo.toml",
      "server/manager/Cargo.toml",
      "server/manager/gui/src-tauri/Cargo.toml",
    ]) write(join(root, file), `[package]\nname = "synthetic"\nversion = "${version}"\n`);
    json(join(root, "server/manager/gui/package.json"), { version });
    json(join(root, "server/manager/gui/src-tauri/tauri.conf.json"), { version });
  }
  return root;
}

test("source check binds an app tag, commit, notes and mobile build number", () => {
  const root = repository("app", "2026.8.250");
  const result = sourceCheck({
    repository: root,
    product: "app",
    tag: "app-v2026.8.250",
    sourceCommit: "a".repeat(40),
    publishedAt: "2026-09-15T00:00:00Z",
  });
  assert.equal(result.expectedDownloads.length, 14);
  assert.equal(result.mobileBuildNumber, 2_026_008_250);
  assert.equal(result.iosBuildNumber, "2026.8.250");
  assert.equal(result.compatibility, null);
});

test("source check carries authoritative Sync compatibility and pinned vendor input", () => {
  const root = repository("sync", "0.1.0");
  const result = sourceCheck({
    repository: root,
    product: "sync",
    tag: "sync-v0.1.0",
    sourceCommit: "b".repeat(40),
    publishedAt: "2026-09-15T00:00:00Z",
    registryUrl: "https://sync.example.invalid/registry/",
  });
  assert.equal(result.expectedDownloads.length, 16);
  assert.equal(result.compatibility.storeFormatId, "risunest-sync-store/v8");
  assert.equal(result.vendorInput.version, "2026.9.1");
  assert.equal(result.vendorInput.assets["windows-aarch64"].actualArch, "x86_64");
});

test("source check rejects tag drift, missing notes and incomplete compatibility", () => {
  const app = repository("app", "1.2.3");
  assert.throws(() => sourceCheck({
    repository: app,
    product: "app",
    tag: "app-v1.2.4",
    sourceCommit: "c".repeat(40),
    publishedAt: "2026-09-15T00:00:00Z",
  }), /Expected tag/);
  const sync = repository("sync", "1.2.3");
  json(join(sync, "server/version.json"), { version: "1.2.3" });
  assert.throws(() => sourceCheck({
    repository: sync,
    product: "sync",
    tag: "sync-v1.2.3",
    sourceCommit: "d".repeat(40),
    publishedAt: "2026-09-15T00:00:00Z",
    registryUrl: "https://sync.example.invalid/",
  }), /compatibility/);
});

test("source check requires the directly tested sync-wire lockfile", () => {
  const sync = repository("sync", "1.2.3");
  rmSync(join(sync, "crates/sync-wire/Cargo.lock"));
  assert.throws(() => sourceCheck({
    repository: sync,
    product: "sync",
    tag: "sync-v1.2.3",
    sourceCommit: "e".repeat(40),
    publishedAt: "2026-09-15T00:00:00Z",
    registryUrl: "https://sync.example.invalid/",
  }), /Lockfile is missing/);
});

test("both products require the shared native update test lockfile", () => {
  for (const product of ["app", "sync"]) {
    const root = repository(product, "1.2.3");
    rmSync(join(root, "crates/release-update/Cargo.lock"));
    assert.throws(() => sourceCheck({
      repository: root,
      product,
      tag: `${product}-v1.2.3`,
      sourceCommit: "e".repeat(40),
      publishedAt: "2026-09-15T00:00:00Z",
      registryUrl: "https://sync.example.invalid/",
    }), /Lockfile is missing/);
  }
});

test("release names are unique and Android build numbers are bounded", () => {
  for (const product of ["app", "sync"]) {
    const names = releaseDownloads(product, "1.2.3").map((download) => download.fileName);
    assert.equal(new Set(names).size, names.length);
  }
  assert.throws(() => androidVersionCode("2100.0.0"), /versionCode/);
  assert.equal(iosBundleVersion("2026.8.250"), "2026.8.250");
});

test("pinned cloudflared inputs reject a wrong native architecture", () => {
  const input = structuredClone(cloudflared);
  input.assets["linux-aarch64"].actualArch = "x86_64";
  assert.throws(() => validateVendorInput(input), /architecture.*linux-aarch64/);
});

test("release Tauri config leaves updater discovery to the verified native boundary", () => {
  const config = releaseTauriConfig({ product: "app", version: "1.2.3", mobileBuildNumber: 1_002_003, iosBuildNumber: "1.2.3" }, "synthetic-public-key");
  assert.deepEqual(config.plugins.updater.endpoints, []);
  assert.equal(JSON.stringify(config).includes("releases/latest/download/manifest.json"), false);
});
