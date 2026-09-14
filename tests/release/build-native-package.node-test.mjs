import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { assertBinaryArchitecture } from "../../scripts/release/platform/native.mjs";
import { assertPackageFormat } from "../../scripts/release/platform/package.mjs";
import {
  assertIosBundleMetadata,
  inspectMacAppRoot,
  validateIosArchiveEntries,
} from "../../scripts/release/package-app.mjs";
import { validateOwnedInventory } from "../../server/manager/install/package.mjs";

function pe(machine) {
  const bytes = Buffer.alloc(128);
  bytes.write("MZ");
  bytes.writeUInt32LE(64, 0x3c);
  bytes.write("PE\0\0", 64);
  bytes.writeUInt16LE(machine, 68);
  return bytes;
}

function elf(machine) {
  const bytes = Buffer.alloc(64);
  Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1]).copy(bytes);
  bytes.writeUInt16LE(machine, 18);
  return bytes;
}

function macho(cpu) {
  const bytes = Buffer.alloc(64);
  bytes.writeUInt32LE(0xfeedfacf, 0);
  bytes.writeUInt32LE(cpu, 4);
  return bytes;
}

function universalMacho(cpus) {
  const bytes = Buffer.alloc(8 + cpus.length * 20);
  bytes.writeUInt32BE(0xcafebabe, 0);
  bytes.writeUInt32BE(cpus.length, 4);
  cpus.forEach((cpu, index) => bytes.writeUInt32BE(cpu, 8 + index * 20));
  return bytes;
}

test("native executable checks distinguish both release architectures", () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-native-"));
  const fixtures = [
    ["x64.exe", pe(0x8664), "x86_64", "pe"],
    ["arm.exe", pe(0xaa64), "aarch64", "pe"],
    ["x64.elf", elf(62), "x86_64", "elf"],
    ["arm.elf", elf(183), "aarch64", "elf"],
    ["x64.macho", macho(0x01000007), "x86_64", "macho"],
    ["arm.macho", macho(0x0100000c), "aarch64", "macho"],
  ];
  for (const [name, bytes, arch, format] of fixtures) {
    const path = join(root, name);
    writeFileSync(path, bytes);
    assert.deepEqual(assertBinaryArchitecture(path, arch, format), { format, arches: [arch] });
  }
  assert.throws(() => assertBinaryArchitecture(join(root, "arm.exe"), "x86_64", "pe"), /expected x86_64/);
  const universal = join(root, "universal.macho");
  writeFileSync(universal, universalMacho([0x01000007, 0x0100000c]));
  assert.throws(() => assertBinaryArchitecture(universal, "aarch64", "macho"), /targets x86_64,aarch64/);
});

test("release package checks reject extension-only fakes", () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-package-"));
  const deb = join(root, "app.deb");
  writeFileSync(deb, Buffer.from("!<arch>\ncontent"));
  assert.equal(assertPackageFormat(deb, "deb").format, "deb");
  const fake = join(root, "fake.deb");
  writeFileSync(fake, "not a package");
  assert.throws(() => assertPackageFormat(fake, "deb"), /Debian archive/);
});

test("raw packaging accepts native Windows ARM64 targets", () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-raw-package-"));
  const binary = join(root, "risunest-sync-server.exe");
  const output = join(root, "out");
  writeFileSync(binary, pe(0xaa64));
  const result = spawnSync("python", [
    "server/sync/distribution/package.py",
    "--binary", binary,
    "--target", "aarch64-pc-windows-msvc",
    "--version", "1.2.3",
    "--output", output,
  ], { cwd: new URL("../..", import.meta.url), encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  const report = JSON.parse(result.stdout.trim());
  assert.equal(report.target, "aarch64-pc-windows-msvc");
  assert.ok(readFileSync(join(output, "risunest-sync-server-1.2.3-aarch64-pc-windows-msvc.zip")).length > 0);
});

test("NSIS inspection excludes only generated installer metadata from the owned inventory", () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-nsis-inventory-"));
  const cloudflared = Buffer.from("synthetic cloudflared");
  const cloudflaredSha256 = createHash("sha256").update(cloudflared).digest("hex");
  writeFileSync(join(root, "cloudflared.exe"), cloudflared);
  writeFileSync(join(root, "risunest-sync-gui.exe"), pe(0xaa64));
  mkdirSync(join(root, "$PLUGINSDIR"));
  writeFileSync(join(root, "$PLUGINSDIR", "nsDialogs.dll"), "installer scaffold");
  writeFileSync(join(root, "uninstall.exe"), "generated uninstaller");
  const marker = join(root, "risunest-sync-bundle.json");
  writeFileSync(marker, JSON.stringify({
    schema: "risunest-sync-bundle/v1",
    product: "sync",
    variant: "managed",
    version: "1.2.3",
    protocolId: "risunest-sync/v1",
    storeFormatId: "risunest-sync-store/v8",
    files: ["cloudflared.exe", "risunest-sync-gui.exe"],
    vendor: [{
      name: "cloudflared",
      version: "2026.9.1",
      os: "windows",
      arch: "x86_64",
      sha256: cloudflaredSha256,
      path: "cloudflared.exe",
    }],
  }));
  const releaseInput = {
    version: "1.2.3",
    compatibility: {
      protocolId: "risunest-sync/v1",
      storeFormatId: "risunest-sync-store/v8",
    },
    vendorInput: { version: "2026.9.1" },
  };
  const build = { cloudflaredArch: "x86_64", cloudflaredSha256 };
  assert.throws(
    () => validateOwnedInventory(root, marker, releaseInput, build),
    /does not exactly list/,
  );
  assert.equal(
    validateOwnedInventory(root, marker, releaseInput, build, cloudflaredSha256, ["uninstall.exe"]).version,
    "1.2.3",
  );
  writeFileSync(join(root, "unexpected.exe"), "unexpected owned file");
  assert.throws(
    () => validateOwnedInventory(root, marker, releaseInput, build, cloudflaredSha256, ["uninstall.exe"]),
    /does not exactly list/,
  );
});

test("mounted DMG inspection rejects a mismatched app architecture", () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-dmg-proof-"));
  const executable = join(root, "RisuNest.app", "Contents", "MacOS", "RisuNest");
  mkdirSync(join(executable, ".."), { recursive: true });
  writeFileSync(executable, Buffer.concat([macho(0x01000007), Buffer.from("__TAURI_BUNDLE_TYPE_VAR_APP")]));
  assert.throws(() => inspectMacAppRoot(root, "aarch64", join(root, "proof")), /expected aarch64/);
});

const tauriConfig = {
  identifier: "io.github.rsyumi.risunest",
  bundle: { fileAssociations: [{ ext: ["risum", "risudat"] }] },
  plugins: {
    "deep-link": {
      mobile: [
        { scheme: ["risunestlocal"], appLink: false },
        { scheme: ["https"], appLink: true },
      ],
    },
  },
};
const iosConfig = { bundle: { iOS: { minimumSystemVersion: "16.4" } } };
const releaseInput = { version: "2026.8.250", iosBuildNumber: "2026.8.250" };

function iosInfo() {
  return {
    CFBundleIdentifier: "io.github.rsyumi.risunest",
    CFBundleSupportedPlatforms: ["iPhoneOS"],
    MinimumOSVersion: "16.4",
    CFBundleShortVersionString: "2026.8.250",
    CFBundleVersion: "2026.8.250",
    CFBundleDocumentTypes: [{
      LSHandlerRank: "Default",
      CFBundleTypeRole: "Editor",
      CFBundleTypeName: "risum",
      CFBundleTypeExtensions: ["risum", "risudat"],
    }],
    CFBundleURLTypes: [{
      CFBundleURLName: "risunestlocal",
      CFBundleURLSchemes: ["risunestlocal"],
    }],
    CFBundleExecutable: "RisuNest",
  };
}

test("iOS package inventory accepts only one complete RisuNest app", () => {
  assert.deepEqual(validateIosArchiveEntries([
    "Payload/",
    "Payload/RisuNest.app/",
    "Payload/RisuNest.app/RisuNest",
    "Payload/RisuNest.app/Info.plist",
    "Payload/RisuNest.app/assets/index.html",
  ]), [
    "Payload/",
    "Payload/RisuNest.app/",
    "Payload/RisuNest.app/Info.plist",
    "Payload/RisuNest.app/RisuNest",
    "Payload/RisuNest.app/assets/index.html",
  ]);
  assert.throws(() => validateIosArchiveEntries([
    "Payload/RisuNest.app/Info.plist",
    "SwiftSupport/libswiftCore.dylib",
  ]), /only the Payload\/RisuNest.app bundle/);
  assert.throws(() => validateIosArchiveEntries([
    "Payload/RisuNest.app/Info.plist",
    "Payload/RisuNest.app/../escaped",
  ]), /unsafe path/);
  assert.throws(() => validateIosArchiveEntries([
    "Payload/RisuNest.app/Info.plist",
    "Payload/RisuNest.app/Info.plist",
  ]), /duplicate path/);
});

test("iOS bundle proof requires the configured custom URL scheme", () => {
  assert.equal(
    assertIosBundleMetadata(iosInfo(), releaseInput, tauriConfig, iosConfig).executable,
    "RisuNest",
  );
  const missingUrls = iosInfo();
  delete missingUrls.CFBundleURLTypes;
  assert.throws(
    () => assertIosBundleMetadata(missingUrls, releaseInput, tauriConfig, iosConfig),
    /bundle metadata does not match/,
  );
});

test("committed iOS shell carries the configured custom URL scheme", () => {
  const repository = fileURLToPath(new URL("../..", import.meta.url));
  const plistPath = join(repository, "src-tauri/gen/apple/risunest_iOS/Info.plist");
  const parsed = spawnSync("python", [
    "-c",
    "import json, plistlib, sys; print(json.dumps(plistlib.load(open(sys.argv[1], 'rb'))['CFBundleURLTypes']))",
    plistPath,
  ], { encoding: "utf8" });
  assert.equal(parsed.status, 0, parsed.stderr);
  const config = JSON.parse(readFileSync(join(repository, "src-tauri/tauri.conf.json"), "utf8"));
  const expected = config.plugins["deep-link"].mobile
    .filter((deepLink) => !deepLink.appLink)
    .map((deepLink) => ({
      CFBundleURLSchemes: deepLink.scheme.filter((scheme) => scheme !== "http" && scheme !== "https"),
      CFBundleURLName: deepLink.scheme[0],
    }));
  assert.deepEqual(JSON.parse(parsed.stdout), expected);
  const project = readFileSync(join(repository, "src-tauri/gen/apple/project.yml"), "utf8");
  assert.match(project, /CFBundleURLSchemes: \[risunestlocal\]\s+CFBundleURLName: risunestlocal/);
});
