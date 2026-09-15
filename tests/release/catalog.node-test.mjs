import assert from "node:assert/strict";
import test from "node:test";
import { mergeCatalog } from "../../scripts/release/catalog.mjs";
import { catalogFixture, entryFixture } from "./fixtures.mjs";

test("app and server releases carry the other exact entry forward without reupload", () => {
  const original = catalogFixture();
  const server = entryFixture("sync", "1.1.0");
  const next = mergeCatalog(original, server, server.release.tag, "2026-09-16T00:00:00Z");
  assert.deepEqual(next.products.app, original.products.app);
  assert.deepEqual(next.products.sync, server);
  const app = entryFixture("app", "2.0.0");
  const last = mergeCatalog(next, app, app.release.tag, "2026-09-17T00:00:00Z");
  assert.deepEqual(last.products.sync, server);
  assert.deepEqual(original, catalogFixture());
});

test("old or same-version changed contents cannot repoint latest", () => {
  const original = catalogFixture();
  assert.throws(() => mergeCatalog(original, entryFixture("app", "0.9.0"), "app-v0.9.0", "2026-09-16T00:00:00Z"), /superseded/);
  const altered = entryFixture();
  altered.release.sourceCommit = "b".repeat(40);
  assert.throws(() => mergeCatalog(original, altered, altered.release.tag, "2026-09-16T00:00:00Z"), /immutable/);
});

test("retries of an already published product do not rewind another product", () => {
  const original = catalogFixture();
  const same = original.products.app;
  assert.deepEqual(mergeCatalog(original, same, same.release.tag, "2026-09-16T00:00:00Z"), original);
});

test("initial release requires explicit null and cannot smuggle a prerelease into stable", () => {
  const app = entryFixture();
  const initial = mergeCatalog(null, app, app.release.tag, "2026-09-16T00:00:00Z");
  assert.equal(initial.products.sync, null);
  assert.throws(() => mergeCatalog(undefined, app, app.release.tag, "2026-09-16T00:00:00Z"));
  const preview = entryFixture("app", "1.1.0-rc.1");
  assert.throws(() => mergeCatalog(initial, preview, preview.release.tag, "2026-09-16T00:00:00Z"), /prerelease/);
});
