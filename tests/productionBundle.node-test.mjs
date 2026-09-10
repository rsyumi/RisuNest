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
    "/benchmarks/streaming/fixture.ts",
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
  assert.throws(
    () =>
      assertProductionBundle([
        chunk(),
        {
          type: "asset",
          fileName: "assets/worker.js",
          source: "window.__streamingSmoke = {}",
        },
      ]),
    /Verification marker/,
  );
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
