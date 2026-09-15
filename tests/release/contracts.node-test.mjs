import assert from "node:assert/strict";
import test from "node:test";
import { compareVersions, requiredDownloads, validateCatalog, validateProductRelease,
  releaseUrl } from "../../scripts/release/contracts.mjs";
import { catalogFixture, productFixture } from "./fixtures.mjs";

test("both products require complete native and mobile distributions", () => {
  for (const product of ["app", "sync"]) {
    const release = productFixture(product);
    assert.equal(validateProductRelease(release), release);
    assert.equal(requiredDownloads(product).length, product === "app" ? 14 : 16);
    for (let i = 0; i < release.downloads.length; i++) {
      const missing = structuredClone(release);
      missing.downloads.splice(i, 1);
      assert.throws(() => validateProductRelease(missing), /download/);
    }
  }
});

test("rejects duplicate, wrong product, architecture and installer mappings", () => {
  for (const mutate of [
    r => r.downloads.push(r.downloads[0]),
    r => r.downloads[0].product = "sync",
    r => r.downloads[0].arch = "i686",
    r => r.platforms["linux-x86_64-deb"].url = r.platforms["linux-x86_64-appimage"].url,
    r => r.platforms["windows-x86_64-nsis"].signature = "",
    r => r.platforms["linux-x86_64"] = r.platforms["linux-x86_64-appimage"],
    r => r.tag = "sync-v1.0.0",
    r => r.downloads[0].size = -1,
    r => r.downloads[0].sha256 = "0",
    r => r.localizedNotes.en = "a".repeat(4097),
    r => r.platforms["windows-x86_64-nsis"] = r.platforms["windows-aarch64-nsis"],
    r => r.downloads[0].unrecognized = true,
    r => delete r.compatibility,
  ]) {
    const release = productFixture();
    mutate(release);
    assert.throws(() => validateProductRelease(release));
  }
});

test("release URLs cannot escape the exact repository, tag or asset path", () => {
  assert.equal(releaseUrl("app-v1.0.0", "product-manifest.json"),
    "https://github.com/rsyumi/RisuNest/releases/download/app-v1.0.0/product-manifest.json");
  for (const url of [
    "https://github.com/rsyumi/RisuNest/releases/latest/download/app.zip",
    "https://github.com/rsyumi/RisuNest.evil/releases/download/app-v1.0.0/app.zip",
    "https://github.com@evil.invalid/rsyumi/RisuNest/releases/download/app-v1.0.0/app.zip",
    "https://github.com/rsyumi/RisuNest/releases/download/app-v1.0.1/app.zip",
    "https://github.com/rsyumi/RisuNest/releases/download/app-v1.0.0/a%2fb.zip",
    "http://github.com/rsyumi/RisuNest/releases/download/app-v1.0.0/app.zip",
  ]) {
    const release = productFixture();
    release.downloads[0].url = url;
    assert.throws(() => validateProductRelease(release));
  }
});

test("SemVer compares numeric components and prerelease identifiers precisely", () => {
  assert.equal(compareVersions("2026.9.1", "2026.8.250"), 1);
  assert.equal(compareVersions("1.0.0-rc.10", "1.0.0-rc.2"), 1);
  assert.equal(compareVersions("1.0.0-rc.2", "1.0.0"), -1);
  assert.equal(compareVersions("1.0.0+build.2", "1.0.0+build.1"), 0);
  assert.equal(compareVersions("99999999999999999.0.0", "99999999999999998.0.0"), 1);
  for (const invalid of ["1", "1.2", "01.0.0", "1.0.0-01", "1.0.0-", "1.0.0+", "v1.0.0", "18446744073709551616.0.0"]) {
    assert.throws(() => compareVersions(invalid, "1.0.0"), /version/);
  }
  assert.throws(() => validateProductRelease(productFixture("app", "1.0.0+build.1")), /build-metadata/);
});

test("catalog preserves separate product versions and validates fixed product URLs", () => {
  const catalog = catalogFixture();
  assert.equal(validateCatalog(catalog), catalog);
  catalog.products.app.manifestUrl = catalog.products.sync.manifestUrl;
  assert.throws(() => validateCatalog(catalog), /manifest/);
});

test("bootstrap null is explicit and malformed or empty catalogs are rejected", () => {
  const catalog = catalogFixture();
  catalog.products.app = null;
  validateCatalog(catalog);
  catalog.products.sync = null;
  assert.throws(() => validateCatalog(catalog));
  assert.throws(() => validateCatalog({ ...catalogFixture(), products: { app: null } }));
});
