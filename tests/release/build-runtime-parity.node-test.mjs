import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import test from "node:test";
import { assertOneTestResult, parityInputs } from "../../scripts/release/runtime-parity.mjs";
import { productFixture } from "./fixtures.mjs";

function updaterDownloads(release) {
  const formats = new Map([["windows", "zip"], ["linux", "tar.gz"], ["darwin", "app.tar.gz"]]);
  return release.downloads.filter((download) =>
    download.variant === "managed" && formats.get(download.os) === download.format);
}

test("runtime parity selects the six real managed updater archives", async () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-runtime-parity-"));
  const nested = join(root, "assets");
  mkdirSync(nested);
  const release = productFixture("sync");
  for (const download of updaterDownloads(release))
    writeFileSync(join(nested, basename(new URL(download.url).pathname)), "synthetic archive");
  const inputs = await parityInputs(release, root);
  assert.deepEqual(inputs.map(({ os, arch, format }) => [os, arch, format]), [
    ["windows", "x86_64", "zip"],
    ["linux", "x86_64", "tar.gz"],
    ["darwin", "x86_64", "app.tar.gz"],
    ["windows", "aarch64", "zip"],
    ["linux", "aarch64", "tar.gz"],
    ["darwin", "aarch64", "app.tar.gz"],
  ]);
});

test("runtime parity rejects missing and ambiguous package bytes", async () => {
  const release = productFixture("sync");
  const root = mkdtempSync(join(tmpdir(), "risunest-runtime-parity-"));
  await assert.rejects(parityInputs(release, root), /asset is missing/);
  const name = basename(new URL(updaterDownloads(release)[0].url).pathname);
  writeFileSync(join(root, name), "one");
  const nested = join(root, "duplicate");
  mkdirSync(nested);
  writeFileSync(join(nested, name), "two");
  await assert.rejects(parityInputs(release, root), /Duplicate runtime parity asset/);
});

test("runtime parity rejects a successful Cargo command that ran zero tests", () => {
  assert.doesNotThrow(() => assertOneTestResult(
    "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 80 filtered out",
    "linux/x86_64/tar.gz",
  ));
  assert.throws(() => assertOneTestResult(
    "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 81 filtered out",
    "linux/x86_64/tar.gz",
  ), /did not execute/);
});
