import assert from "node:assert/strict";
import test from "node:test";
import { assertProductionBundle } from "./productionBundle.mjs";

const chunk = (modules = {}, code = "export {};") => ({
  type: "chunk",
  fileName: "assets/app.js",
  modules,
  code,
});
const map = (sources, sourcesContent = []) => ({
  type: "asset",
  fileName: "assets/app.js.map",
  source: JSON.stringify({ sources, sourcesContent }),
});

test("accepts product code and RegExp.test polyfill", () => {
  assert.deepEqual(
    assertProductionBundle([
      chunk({
        "/src/main.ts": {},
        "/node_modules/core-js/modules/es.regexp.test.js": {},
      }),
    ]),
    { javascriptFiles: 1, sourceMaps: 0 },
  );
});
test("rejects a test module in a dynamic chunk and a test framework", () => {
  for (const id of [
    "/src/foo.test.ts",
    "/src/lib/ComponentHarness.test.svelte",
    "/benchmarks/streaming/fixture.ts",
    "/benchmarks/linux/main.ts",
    "/tests/support/helper.ts",
    "/node_modules/vitest/dist/index.js",
  ]) {
    assert.throws(
      () =>
        assertProductionBundle([
          chunk(),
          {
            ...chunk({ [id]: {} }),
            fileName: "assets/lazy.js",
            isDynamicEntry: true,
          },
        ]),
      /Verification module/,
    );
  }
});
test("rejects a worker asset containing a benchmark interface", () => {
  for (const source of [
    "window.__streamingSmoke = {}",
    "window.__RISUNEST_LINUX_BENCHMARK__ = {}",
  ]) {
    assert.throws(
      () =>
        assertProductionBundle([
          chunk(),
          {
            type: "asset",
            fileName: "assets/worker.js",
            source,
          },
        ]),
      /Verification marker/,
    );
  }
});
test("rejects test modules and removed test helpers in source maps", () => {
  assert.throws(
    () =>
      assertProductionBundle([
        chunk(),
        map(["../../benchmarks/tokenizer/main.ts"]),
      ]),
    /Verification source/,
  );
  assert.throws(
    () =>
      assertProductionBundle([
        chunk(),
        map(["../../src/main.ts"], ["export const __testResponsesAPI = {}"]),
      ]),
    /Verification source text/,
  );
});
test("rejects an empty bundle instead of claiming validation", () => {
  assert.throws(() => assertProductionBundle([]), /No JavaScript/);
});

test("rejects removed peer transport modules but preserves upstream PeerJS", () => {
  assertProductionBundle([
    chunk({
      "/src/ts/sync/multiuser.ts": {},
      "/node_modules/peerjs/dist/bundler.mjs": {},
    }),
  ]);
  for (const id of [
    "/src/ts/storage/sync/peerClone.ts",
    "/src/ts/storage/sync/deviceSyncController.ts",
    "/src/lib/Setting/Pages/DeviceSyncSettings.svelte",
  ]) {
    assert.throws(
      () => assertProductionBundle([chunk({ [id]: {} })]),
      /module/,
    );
    assert.throws(() => assertProductionBundle([chunk(), map([id])]), /source/);
  }
  assert.throws(() =>
    assertProductionBundle([chunk({}, "window.RisuPeerCloneBridge = {}")]),
  );
});

test("rejects retired backup modules, commands, and magic in product chunks and maps", () => {
  for (const id of [
    "/src/ts/storage/losslessBackupFileRoute.ts",
    "/src/ts/storage/losslessBackupFileRouteProduction.svelte.ts",
  ]) {
    assert.throws(
      () => assertProductionBundle([chunk({ [id]: {} })]),
      /module/,
    );
    assert.throws(() => assertProductionBundle([chunk(), map([id])]), /source/);
  }
  for (const marker of [
    "RISUNESTLOSSLESS",
    ".risulossless",
    "restore-lossless-backup",
    "export-lossless-backup",
    "native_lossless_handoff_cleanup",
  ]) {
    assert.throws(() => assertProductionBundle([chunk({}, marker)]), /marker/);
    assert.throws(
      () => assertProductionBundle([chunk(), map(["/src/main.ts"], [marker])]),
      /source text/,
    );
  }
});

test("rejects retired GGUF code while preserving Pyodide scripting", () => {
  assertProductionBundle([
    chunk({ "/src/ts/process/pyworker.ts": {} }, "loadPyodide()"),
  ]);
  const local = "/src/ts/process/models/local.ts";
  assert.throws(
    () => assertProductionBundle([chunk({ [local]: {} })]),
    /module/,
  );
  assert.throws(
    () => assertProductionBundle([chunk(), map([local])]),
    /source/,
  );
  for (const marker of [
    "install_python",
    "install_pip",
    "post_py_install",
    "install_py_dependencies",
    "run_py_server",
    "check_requirements_local",
    "localhost:10026",
    "tokenizeGGUFModel",
  ]) {
    assert.throws(() => assertProductionBundle([chunk({}, marker)]), /marker/);
    assert.throws(
      () => assertProductionBundle([chunk(), map(["/src/main.ts"], [marker])]),
      /source text/,
    );
  }
});
