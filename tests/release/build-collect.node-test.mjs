import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { recordBuild } from "../../scripts/release/build.mjs";
import { collectRelease } from "../../scripts/release/collect.mjs";
import { releaseDownloads } from "../../scripts/release/downloads.mjs";

function executable(os, arch) {
  const bytes = Buffer.alloc(128);
  if (os === "windows") {
    bytes.write("MZ");
    bytes.writeUInt32LE(64, 0x3c);
    bytes.write("PE\0\0", 64);
    bytes.writeUInt16LE(arch === "aarch64" ? 0xaa64 : 0x8664, 68);
    return { bytes, format: "pe" };
  }
  if (os === "linux" || os === "android") {
    Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1]).copy(bytes);
    bytes.writeUInt16LE(arch === "aarch64" ? 183 : 62, 18);
    return { bytes, format: "elf" };
  }
  bytes.writeUInt32LE(0xfeedfacf, 0);
  bytes.writeUInt32LE(arch === "aarch64" ? 0x0100000c : 0x01000007, 4);
  return { bytes, format: "macho" };
}

function packageBytes(format) {
  if (["zip", "apk", "ipa"].includes(format)) return Buffer.from("PK\x03\x04synthetic");
  if (format === "nsis") return Buffer.from("MZsynthetic");
  if (format === "deb") return Buffer.from("!<arch>\nsynthetic");
  if (format === "appimage") return Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]);
  if (["tar.gz", "app.tar.gz"].includes(format)) return Buffer.from([0x1f, 0x8b, 8, 0, 1]);
  if (format === "dmg") {
    const bytes = Buffer.alloc(512);
    bytes.write("koly");
    return bytes;
  }
  throw new Error(format);
}

function checks(download) {
  if (download.format === "apk") return ["abi-arm64", "apk-signed", "version-match", "zipaligned-16k"];
  if (download.format === "ipa") return ["iphoneos", "payload-single-app", "version-match", "archive-inventory", "bundle-metadata"];
  if (download.product === "sync" && download.variant === "managed") {
    if (download.os === "windows" && download.format === "zip") return ["archive-layout", "bundle-inventory"];
    if (download.os === "linux") return ["archive-layout", "bundle-inventory"];
    if (download.os === "darwin" && download.format === "app.tar.gz") return ["whole-app"];
  }
  return [];
}

test("record and collect require every signed app target and retain both Linux updater formats", async () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-collect-"));
  const key = join(root, "test-key");
  const generated = spawnSync(process.execPath, [
    "node_modules/@tauri-apps/cli/tauri.js", "signer", "generate", "--ci",
    "--password", "synthetic-password", "--write-keys", key,
  ], { cwd: new URL("../..", import.meta.url), encoding: "utf8" });
  assert.equal(generated.status, 0, generated.stderr);
  const publicKey = readFileSync(`${key}.pub`, "utf8");
  const previousKey = process.env.TAURI_SIGNING_PRIVATE_KEY;
  const previousPassword = process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD;
  process.env.TAURI_SIGNING_PRIVATE_KEY = readFileSync(key, "utf8");
  process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "synthetic-password";
  try {
    const version = "1.2.3";
    const releaseInput = {
      product: "app",
      version,
      tag: "app-v1.2.3",
      sourceCommit: "a".repeat(40),
      pub_date: "2026-09-15T00:00:00Z",
      releasePage: "https://github.com/rsyumi/RisuNest/releases/tag/app-v1.2.3",
      notes: "Synthetic",
      localizedNotes: {},
      compatibility: null,
      vendorInput: null,
      expectedDownloads: releaseDownloads("app", version),
    };
    const inputs = join(root, "inputs");
    const output = join(root, "output");
    mkdirSync(inputs);
    const assets = releaseInput.expectedDownloads.map((download, index) => {
      const packagePath = join(inputs, `package-${index}`);
      const binaryPath = join(inputs, `binary-${index}`);
      const proof = executable(download.os, download.arch);
      writeFileSync(packagePath, packageBytes(download.format));
      writeFileSync(binaryPath, proof.bytes);
      return {
        download: Object.fromEntries(Object.entries(download).filter(([name]) => name !== "fileName")),
        path: packagePath,
        binaries: [{ path: binaryPath, arch: download.arch, format: proof.format, role: "app" }],
        checks: checks(download),
      };
    });
    await recordBuild({ releaseInput, leg: "all_app", assets, output, publicKey });
    const publishedAt = "2026-09-15T04:00:00Z";
    const collected = await collectRelease({ releaseInput, inputDirectory: output, outputDirectory: join(root, "manifest"), publicKey, publishedAt });
    assert.equal(collected.release.pub_date, publishedAt);
    assert.notEqual(collected.release.pub_date, releaseInput.pub_date);
    assert.equal(collected.release.downloads.length, 14);
    assert.ok(collected.release.platforms["linux-x86_64-deb"]);
    assert.ok(collected.release.platforms["linux-x86_64-appimage"]);
    assert.deepEqual(collected.release.vendor, []);
  } finally {
    if (previousKey === undefined) delete process.env.TAURI_SIGNING_PRIVATE_KEY;
    else process.env.TAURI_SIGNING_PRIVATE_KEY = previousKey;
    if (previousPassword === undefined) delete process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD;
    else process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD = previousPassword;
  }
});

test("recording rejects executable proof for a different package architecture", async () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-proof-"));
  const packagePath = join(root, "app.zip");
  const binaryPath = join(root, "app.exe");
  writeFileSync(packagePath, packageBytes("zip"));
  writeFileSync(binaryPath, executable("windows", "x86_64").bytes);
  const releaseInput = { product: "app", version: "1.2.3", tag: "app-v1.2.3", sourceCommit: "a".repeat(40), expectedDownloads: releaseDownloads("app", "1.2.3") };
  await assert.rejects(recordBuild({
    releaseInput,
    leg: "wrong_arch",
    assets: [{ download: { product: "app", variant: "desktop", os: "windows", arch: "aarch64", format: "zip" }, path: packagePath, binaries: [{ path: binaryPath, arch: "x86_64", format: "pe", role: "app" }] }],
    output: join(root, "out"),
    publicKey: "unused",
  }), /uses x86_64, expected aarch64/);
});

test("record and collect require every signed sync target and exact vendor fields", async () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-sync-collect-"));
  const key = join(root, "test-key");
  const generated = spawnSync(process.execPath, [
    "node_modules/@tauri-apps/cli/tauri.js", "signer", "generate", "--ci",
    "--password", "synthetic-password", "--write-keys", key,
  ], { cwd: new URL("../..", import.meta.url), encoding: "utf8" });
  assert.equal(generated.status, 0, generated.stderr);
  const publicKey = readFileSync(`${key}.pub`, "utf8");
  const previousKey = process.env.TAURI_SIGNING_PRIVATE_KEY;
  const previousPassword = process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD;
  process.env.TAURI_SIGNING_PRIVATE_KEY = readFileSync(key, "utf8");
  process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "synthetic-password";
  try {
    const version = "2.3.4";
    const sourceSha256 = "e".repeat(64);
    const vendorInput = { version: "2026.9.1", assets: {} };
    for (const os of ["windows", "linux", "darwin"]) {
      for (const packageArch of ["x86_64", "aarch64"]) {
        const actualArch = os === "windows" && packageArch === "aarch64" ? "x86_64" : packageArch;
        vendorInput.assets[`${os}-${packageArch}`] = {
          actualArch,
          url: `https://example.invalid/cloudflared-${os}-${packageArch}`,
          sha256: sourceSha256,
        };
      }
    }
    const releaseInput = {
      product: "sync",
      version,
      tag: "sync-v2.3.4",
      sourceCommit: "b".repeat(40),
      pub_date: "2026-09-15T00:00:00Z",
      releasePage: "https://github.com/rsyumi/RisuNest/releases/tag/sync-v2.3.4",
      notes: "Synthetic",
      localizedNotes: {},
      compatibility: { protocolId: "risunest-sync/v1", storeFormatId: "risunest-sync-store/v8", automaticApply: true },
      vendorInput,
      expectedDownloads: releaseDownloads("sync", version),
    };
    const inputs = join(root, "inputs");
    const output = join(root, "output");
    mkdirSync(inputs);
    const attachedVendor = new Set();
    const assets = releaseInput.expectedDownloads.map((download, index) => {
      const packagePath = join(inputs, `package-${index}`);
      const binaryPath = join(inputs, `binary-${index}`);
      const proof = executable(download.os, download.arch);
      writeFileSync(packagePath, packageBytes(download.format));
      writeFileSync(binaryPath, proof.bytes);
      const target = `${download.os}-${download.arch}`;
      const vendor = [];
      if (download.variant === "managed" && !attachedVendor.has(target)) {
        attachedVendor.add(target);
        const pinned = vendorInput.assets[target];
        const vendorPath = join(inputs, `cloudflared-${target}`);
        const vendorProof = executable(download.os, pinned.actualArch);
        writeFileSync(vendorPath, vendorProof.bytes);
        vendor.push({
          path: vendorPath,
          arch: pinned.actualArch,
          format: vendorProof.format,
          sourceUrl: pinned.url,
          sourceSha256: pinned.sha256,
        });
      }
      return {
        download: Object.fromEntries(Object.entries(download).filter(([name]) => name !== "fileName")),
        path: packagePath,
        binaries: [{ path: binaryPath, arch: download.arch, format: proof.format, role: "sync" }],
        checks: checks(download),
        vendor,
      };
    });
    await recordBuild({ releaseInput, leg: "all_sync", assets, output, publicKey });
    const collected = await collectRelease({ releaseInput, inputDirectory: output, outputDirectory: join(root, "manifest"), publicKey });
    assert.equal(collected.release.downloads.length, 16);
    assert.equal(collected.release.vendor.length, 6);
    assert.deepEqual(Object.keys(collected.release.vendor[0]).sort(), ["arch", "name", "os", "sha256", "version"]);
    assert.equal(collected.release.vendor.filter((item) => item.os === "windows" && item.arch === "x86_64").length, 2);
    assert.ok(collected.release.platforms["windows-aarch64-nsis"]);
    assert.ok(collected.release.platforms["darwin-aarch64-app"]);

    const inventoryPath = join(output, "inventory-all_sync.json");
    const originalInventory = JSON.parse(readFileSync(inventoryPath, "utf8"));
    const changedVersion = structuredClone(originalInventory);
    changedVersion.vendor[0].version = "2026.9.2";
    writeFileSync(inventoryPath, JSON.stringify(changedVersion));
    await assert.rejects(collectRelease({ releaseInput, inputDirectory: output, outputDirectory: join(root, "bad-version"), publicKey }), /pinned target/);
    const changedTarget = structuredClone(originalInventory);
    changedTarget.vendor[0].packageArch = changedTarget.vendor[0].packageArch === "x86_64" ? "aarch64" : "x86_64";
    writeFileSync(inventoryPath, JSON.stringify(changedTarget));
    await assert.rejects(collectRelease({ releaseInput, inputDirectory: output, outputDirectory: join(root, "bad-target"), publicKey }), /pinned target|Duplicate cloudflared/);
    const missingVendor = structuredClone(originalInventory);
    missingVendor.vendor.pop();
    writeFileSync(inventoryPath, JSON.stringify(missingVendor));
    await assert.rejects(collectRelease({ releaseInput, inputDirectory: output, outputDirectory: join(root, "missing-vendor"), publicKey }), /Expected six/);
  } finally {
    if (previousKey === undefined) delete process.env.TAURI_SIGNING_PRIVATE_KEY;
    else process.env.TAURI_SIGNING_PRIVATE_KEY = previousKey;
    if (previousPassword === undefined) delete process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD;
    else process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD = previousPassword;
  }
});
