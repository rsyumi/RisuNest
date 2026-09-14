import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { assertBinaryArchitecture } from "../../scripts/release/platform/native.mjs";
import { assertPackageFormat } from "../../scripts/release/platform/package.mjs";
import { inspectMacAppRoot } from "../../scripts/release/package-app.mjs";

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

test("mounted DMG inspection rejects a mismatched app architecture", () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-dmg-proof-"));
  const executable = join(root, "RisuNest.app", "Contents", "MacOS", "RisuNest");
  mkdirSync(join(executable, ".."), { recursive: true });
  writeFileSync(executable, Buffer.concat([macho(0x01000007), Buffer.from("__TAURI_BUNDLE_TYPE_VAR_APP")]));
  assert.throws(() => inspectMacAppRoot(root, "aarch64", join(root, "proof")), /expected aarch64/);
});
