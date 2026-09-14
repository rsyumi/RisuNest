import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import { publishRelease } from "../../scripts/release/publication.mjs";
import { catalogFixture, productFixture, entryFixture } from "./fixtures.mjs";
import { createSigner } from "./signing.mjs";

function setup({ latest = true, product = "app", version = "1.1.0" } = {}) {
  const signer = createSigner();
  const release = productFixture(product, version);
  const productBytes = Buffer.from(JSON.stringify(release));
  const old = catalogFixture();
  const oldRelease = { id: 1, tag_name: old.publicationTag, draft: false, prerelease: false, target_commitish: "a".repeat(40) };
  const nextRelease = { id: 2, tag_name: release.tag, draft: true, prerelease: false, target_commitish: release.sourceCommit };
  const assets = new Map();
  if (latest) {
    const bytes = Buffer.from(JSON.stringify(old));
    assets.set("1/manifest.json", bytes);
    assets.set("1/manifest.json.sig", Buffer.from(signer.sign(bytes)));
  }
  const events = [];
  const github = {
    latest: latest ? oldRelease : null,
    async getByTag(tag) { return tag === nextRelease.tag_name ? nextRelease : oldRelease; },
    async getLatest() { return this.latest; },
    async assertTagCommit() {},
    async hasPublishedStable() { return false; },
    async listPublishedStable() { return [oldRelease, nextRelease].filter(release => !release.draft); },
    async downloadAsset(r, name) { const bytes = assets.get(`${r.id}/${name}`); if (!bytes) throw new Error("missing-asset"); return bytes; },
    async uploadAsset(r, name, bytes) { events.push(name); assets.set(`${r.id}/${name}`, bytes); },
    async publish() { events.push("publish"); nextRelease.draft = false; this.latest = nextRelease; },
    async makeLatest() { events.push("make-latest"); this.latest = nextRelease; },
  };
  const options = { github, productBytes, productSignature: signer.sign(productBytes), publicKey: signer.key,
    expectedProduct: product, expectedTag: release.tag, expectedCommit: release.sourceCommit,
    publishedAt: "2026-09-16T00:00:00Z", signCatalog: async bytes => signer.sign(bytes),
    verifyAssets: async () => { events.push("verify-assets"); } };
  return { options, events, assets, nextRelease, signer, release };
}

test("only publishes after artifact validation and signed catalog readback", async () => {
  const t = setup();
  assert.equal((await publishRelease(t.options)).status, "published");
  assert.equal(t.events[0], "verify-assets");
  assert.equal(t.events.at(-1), "publish");
  const catalog = JSON.parse(t.assets.get("2/manifest.json"));
  assert.deepEqual(catalog.products.sync, catalogFixture().products.sync);
  assert.equal(catalog.products.app.manifestSha256, createHash("sha256").update(t.options.productBytes).digest("hex"));
});

test("missing or corrupted old catalog never becomes a bootstrap", async () => {
  for (const bytes of [undefined, Buffer.from("{}")]) {
    const t = setup();
    t.options.bootstrap = true;
    bytes ? t.assets.set("1/manifest.json", bytes) : t.assets.delete("1/manifest.json");
    await assert.rejects(publishRelease(t.options));
    assert.deepEqual(t.events, []);
  }
});

test("first publication requires explicit bootstrap and no existing stable releases", async () => {
  const t = setup({ latest: false });
  await assert.rejects(publishRelease(t.options), /bootstrap/);
  t.options.bootstrap = true;
  t.options.github.hasPublishedStable = async () => true;
  await assert.rejects(publishRelease(t.options), /bootstrap/);
  t.options.github.hasPublishedStable = async () => false;
  await publishRelease(t.options);
  assert.equal(JSON.parse(t.assets.get("2/manifest.json")).products.sync, null);
});

test("failed assets, changed source or invalid new signature cannot publish", async () => {
  for (const mutate of [
    t => t.options.verifyAssets = async () => { throw new Error("invalid-artifact"); },
    t => t.options.expectedCommit = "b".repeat(40),
    t => t.options.productSignature = t.options.productSignature.replace("timestamp:1", "timestamp:2"),
  ]) {
    const t = setup(); mutate(t);
    await assert.rejects(publishRelease(t.options));
    assert.equal(t.nextRelease.draft, true);
    assert.equal(t.events.includes("publish"), false);
  }
});

test("lost publish response is resolved by reading actual state", async () => {
  const t = setup();
  const original = t.options.github.publish.bind(t.options.github);
  t.options.github.publish = async () => { await original(); throw new Error("connection-lost"); };
  assert.equal((await publishRelease(t.options)).status, "published");
  assert.equal((await publishRelease(t.options)).status, "already-published");
  assert.equal(t.events.filter(event => event === "publish").length, 1);
});

test("a lost latest pointer is repaired without rewriting immutable assets", async () => {
  const t = setup();
  const old = t.options.github.latest;
  await publishRelease(t.options);
  const events = t.events.length;
  t.options.github.latest = old;
  assert.equal((await publishRelease(t.options)).status, "recovered");
  assert.deepEqual(t.events.slice(events), ["make-latest"]);
});

test("retrying an older publication preserves a later other-product catalog", async () => {
  const t = setup();
  await publishRelease(t.options);
  const later = JSON.parse(t.assets.get("2/manifest.json"));
  later.products.sync = entryFixture("sync", "1.2.0");
  later.publicationTag = "sync-v1.2.0";
  later.publishedAt = "2026-09-17T00:00:00Z";
  const bytes = Buffer.from(JSON.stringify(later));
  t.assets.set("3/manifest.json", bytes);
  t.assets.set("3/manifest.json.sig", Buffer.from(t.signer.sign(bytes)));
  t.options.github.latest = { id: 3, tag_name: later.publicationTag, draft: false, prerelease: false };
  const events = t.events.length;
  assert.equal((await publishRelease(t.options)).status, "already-published");
  assert.deepEqual(t.events.slice(events), []);
  assert.equal(t.options.github.latest.id, 3);
});

test("a missing latest pointer is recovered only after comparing every published catalog", async () => {
  const t = setup();
  await publishRelease(t.options);
  t.options.github.latest = null;
  t.options.github.hasPublishedStable = async () => true;
  const list = t.options.github.listPublishedStable;
  t.options.github.listPublishedStable = async () => [
    ...await list(), { id: 9, tag_name: "documentation-snapshot", draft: false, prerelease: false },
  ];
  const events = t.events.length;
  assert.equal((await publishRelease(t.options)).status, "recovered");
  assert.deepEqual(t.events.slice(events), ["make-latest"]);
  t.options.github.latest = null;
  t.assets.delete("1/manifest.json.sig");
  await assert.rejects(publishRelease(t.options), /missing-asset/);
  assert.equal(t.options.github.latest, null);
});

test("moving a tag after artifact upload prevents final publication", async () => {
  const t = setup();
  let checks = 0;
  t.options.github.assertTagCommit = async () => { if (++checks > 1) throw new Error("tag-commit-mismatch"); };
  await assert.rejects(publishRelease(t.options), /tag-commit-mismatch/);
  assert.equal(t.nextRelease.draft, true);
  assert.equal(t.events.includes("publish"), false);
});
