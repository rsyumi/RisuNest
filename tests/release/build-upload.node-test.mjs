import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { uploadBuild } from "../../scripts/release/upload-build.mjs";

function fixture(root) {
  mkdirSync(root, { recursive: true });
  writeFileSync(join(root, "app.zip"), "synthetic");
  writeFileSync(join(root, "app.zip.sig"), "synthetic-signature");
  return {
    schema: "risunest.release-build/v1",
    product: "app",
    tag: "app-v1.2.3",
    sourceCommit: "a".repeat(40),
    leg: "windows_x64",
    downloads: [{
      product: "app",
      variant: "desktop",
      os: "windows",
      arch: "x86_64",
      format: "zip",
      fileName: "app.zip",
      signatureFileName: "app.zip.sig",
    }],
    vendor: [],
  };
}

test("build upload binds the draft ID to tag and source before mutation", async () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-upload-identity-"));
  const inventory = fixture(root);
  const mutations = [];
  const github = {
    getById: async () => ({ id: 7, draft: true, prerelease: false, tag_name: "app-v9.9.9", target_commitish: inventory.sourceCommit, assets: [] }),
    request: async (...args) => mutations.push(args),
  };
  await assert.rejects(uploadBuild({ draftId: 7, directory: root, inventory, github }), /identity changed/);
  assert.deepEqual(mutations, []);
});

test("build upload rechecks draft state before deleting a retry asset", async () => {
  const root = mkdtempSync(join(tmpdir(), "risunest-upload-race-"));
  const inventory = fixture(root);
  const base = { id: 7, prerelease: false, tag_name: inventory.tag, target_commitish: inventory.sourceCommit };
  const responses = [
    { ...base, draft: true, assets: [{ id: 11, name: "app.zip", state: "uploaded" }] },
    { ...base, draft: false, assets: [{ id: 11, name: "app.zip", state: "uploaded" }] },
  ];
  const mutations = [];
  const github = {
    getById: async () => responses.shift(),
    request: async (...args) => mutations.push(args),
  };
  await assert.rejects(uploadBuild({ draftId: 7, directory: root, inventory, github }), /identity changed/);
  assert.deepEqual(mutations, []);
});
