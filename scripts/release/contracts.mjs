import { isDeepStrictEqual } from "node:util";

export const REPOSITORY = "rsyumi/RisuNest";
export const RELEASE_BASE = `https://github.com/${REPOSITORY}/releases`;
export const CATALOG_SCHEMA = "risunest.release-catalog/v1";
export const PRODUCT_SCHEMA = "risunest.product-release/v1";
export const CATALOG_LIMIT = 1024 * 1024;
export const PRODUCT_LIMIT = 512 * 1024;
const architectures = ["x86_64", "aarch64"];
const sha256 = /^[a-f0-9]{64}$/;

function requireValue(condition, code) {
  if (!condition) throw new Error(code);
}

function object(value, code) {
  requireValue(value !== null && typeof value === "object" && !Array.isArray(value), code);
}

function fields(value, expected, code) {
  object(value, code);
  requireValue(isDeepStrictEqual(Object.keys(value).sort(), expected.split(" ").sort()), code);
}

function text(value, code, maximum = 16384) {
  requireValue(typeof value === "string" && value.length > 0 && Buffer.byteLength(value) <= maximum, code);
}

export function parseVersion(value) {
  text(value, "invalid-version", 256);
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/.exec(value);
  requireValue(match, "invalid-version");
  const prerelease = match[4]?.split(".") ?? [];
  requireValue(prerelease.every(part => !/^0\d+$/.test(part)), "invalid-version");
  const numbers = match.slice(1, 4).map(BigInt);
  requireValue(numbers.every(number => number <= 18446744073709551615n), "invalid-version");
  return { numbers, prerelease, build: match[5] ?? "" };
}

export function compareVersions(left, right) {
  const a = parseVersion(left), b = parseVersion(right);
  for (let i = 0; i < 3; i++) {
    if (a.numbers[i] !== b.numbers[i]) return a.numbers[i] > b.numbers[i] ? 1 : -1;
  }
  if (!a.prerelease.length || !b.prerelease.length) {
    return a.prerelease.length === b.prerelease.length ? 0 : a.prerelease.length ? -1 : 1;
  }
  for (let i = 0; i < Math.max(a.prerelease.length, b.prerelease.length); i++) {
    const x = a.prerelease[i], y = b.prerelease[i];
    if (x === y) continue;
    if (x === undefined || y === undefined) return x === undefined ? -1 : 1;
    const xn = /^\d+$/.test(x), yn = /^\d+$/.test(y);
    if (xn && yn) return BigInt(x) > BigInt(y) ? 1 : -1;
    if (xn !== yn) return xn ? -1 : 1;
    return x > y ? 1 : -1;
  }
  return 0;
}

export function parseTag(tag) {
  text(tag, "invalid-tag", 270);
  const match = /^(app|sync)-v(.+)$/.exec(tag);
  requireValue(match, "invalid-tag");
  parseVersion(match[2]);
  return { product: match[1], version: match[2] };
}

export function releaseUrl(tag, asset) {
  parseTag(tag);
  requireValue(typeof asset === "string" && /^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(asset)
    && asset.length <= 240, "invalid-asset-name");
  return `${RELEASE_BASE}/download/${encodeURIComponent(tag)}/${asset}`;
}

export function validateAssetUrl(value, tag) {
  text(value, "invalid-download-url", 2048);
  let url;
  try { url = new URL(value); } catch { throw new Error("invalid-download-url"); }
  requireValue(url.protocol === "https:" && url.hostname === "github.com" && !url.port
    && !url.username && !url.password && !url.search && !url.hash, "invalid-download-url");
  const asset = url.pathname.split("/").at(-1);
  requireValue(value === releaseUrl(tag, asset), "invalid-download-url");
  return asset;
}

export function downloadKey(value) {
  return [value.product, value.variant, value.os, value.arch, value.format].join("/");
}

export function requiredDownloads(product) {
  requireValue(product === "app" || product === "sync", "invalid-product");
  const result = [];
  const add = (variant, os, arch, format) => result.push({ product, variant, os, arch, format });
  for (const arch of architectures) {
    if (product === "app") {
      for (const format of ["zip", "nsis"]) add("desktop", "windows", arch, format);
      for (const format of ["deb", "appimage"]) add("desktop", "linux", arch, format);
      for (const format of ["dmg", "app.tar.gz"]) add("desktop", "darwin", arch, format);
    } else {
      add("raw", "windows", arch, "zip");
      add("raw", "linux", arch, "tar.gz");
      add("raw", "darwin", arch, "tar.gz");
      for (const format of ["nsis", "zip"]) add("managed", "windows", arch, format);
      add("managed", "linux", arch, "tar.gz");
      for (const format of ["dmg", "app.tar.gz"]) add("managed", "darwin", arch, format);
    }
  }
  if (product === "app") {
    add("mobile", "android", "aarch64", "apk");
    add("mobile", "ios", "aarch64", "ipa");
  }
  return result;
}

export function updaterTarget(download) {
  if (download.variant === "raw") return null;
  const { os, arch, format } = download;
  if (os === "windows" && format === "nsis") return `windows-${arch}-nsis`;
  if (os === "darwin" && format === "app.tar.gz") return `darwin-${arch}-app`;
  if (os === "linux" && ["deb", "appimage"].includes(format)) return `linux-${arch}-${format}`;
  return null;
}

export function validateTimestamp(value) {
  requireValue(typeof value === "string"
    && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{3})?Z$/.test(value)
    && Number.isFinite(Date.parse(value))
    && new Date(value).toISOString().replace(".000Z", "Z") === value.replace(".000Z", "Z"), "invalid-timestamp");
}

export function validateProductRelease(release) {
  fields(release, "schema product version tag sourceCommit releasePage notes localizedNotes pub_date platforms downloads compatibility vendor", "invalid-product-release");
  requireValue(release.schema === PRODUCT_SCHEMA, "invalid-product-schema");
  const identity = parseTag(release.tag);
  requireValue(release.product === identity.product && release.version === identity.version, "product-version-mismatch");
  requireValue(parseVersion(release.version).build === "", "release-build-metadata-unsupported");
  requireValue(/^[a-f0-9]{40}$/.test(release.sourceCommit), "invalid-source-commit");
  requireValue(release.releasePage === `${RELEASE_BASE}/tag/${encodeURIComponent(release.tag)}`, "invalid-release-page");
  validateTimestamp(release.pub_date);
  requireValue(typeof release.notes === "string" && Buffer.byteLength(release.notes) <= 4096, "invalid-notes");
  object(release.localizedNotes, "invalid-localized-notes");
  for (const [language, notes] of Object.entries(release.localizedNotes)) {
    requireValue(/^[a-z]{2,3}(?:-[A-Za-z]{2,8})?$/.test(language)
      && typeof notes === "string" && Buffer.byteLength(notes) <= 4096, "invalid-localized-notes");
  }
  const required = requiredDownloads(release.product);
  requireValue(Array.isArray(release.downloads) && release.downloads.length === required.length, "incomplete-downloads");
  const keys = new Set(required.map(downloadKey)), urls = new Set();
  const expectedPlatforms = new Map();
  for (const download of release.downloads) {
    fields(download, "product variant os arch format version url size sha256 signatureUrl", "invalid-download");
    requireValue(keys.delete(downloadKey(download)), "unexpected-or-duplicate-download");
    requireValue(download.version === release.version, "download-version-mismatch");
    requireValue(Number.isSafeInteger(download.size) && download.size > 0, "invalid-download-size");
    requireValue(typeof download.sha256 === "string" && sha256.test(download.sha256), "invalid-download-hash");
    validateAssetUrl(download.url, release.tag);
    requireValue(!urls.has(download.url), "duplicate-download-url");
    urls.add(download.url);
    requireValue(download.signatureUrl === `${download.url}.sig`, "invalid-download-signature-url");
    const target = updaterTarget(download);
    if (target) expectedPlatforms.set(target, download.url);
  }
  requireValue(keys.size === 0, "incomplete-downloads");
  object(release.platforms, "invalid-platforms");
  requireValue(Object.keys(release.platforms).length === expectedPlatforms.size, "unexpected-platforms");
  for (const [target, url] of expectedPlatforms) {
    fields(release.platforms[target], "url signature", "missing-platform");
    requireValue(release.platforms[target].url === url, "platform-download-mismatch");
    text(release.platforms[target].signature, "invalid-platform-signature");
  }
  requireValue(Array.isArray(release.vendor), "invalid-vendor");
  if (release.product === "sync") {
    fields(release.compatibility, "protocolId storeFormatId automaticApply", "invalid-compatibility");
    text(release.compatibility.protocolId, "invalid-protocol-id", 128);
    text(release.compatibility.storeFormatId, "invalid-store-format", 128);
    requireValue(typeof release.compatibility.automaticApply === "boolean", "invalid-automatic-apply");
    requireValue(release.vendor.length > 0, "missing-cloudflared");
    for (const vendor of release.vendor) {
      fields(vendor, "name version os arch sha256", "invalid-vendor");
      requireValue(vendor.name === "cloudflared" && ["windows", "linux", "darwin"].includes(vendor.os)
        && architectures.includes(vendor.arch) && typeof vendor.sha256 === "string"
        && sha256.test(vendor.sha256), "invalid-vendor");
      text(vendor.version, "invalid-vendor-version", 128);
    }
  } else requireValue(release.compatibility === null && release.vendor.length === 0, "unexpected-app-compatibility");
  requireValue(Buffer.byteLength(JSON.stringify(release)) <= PRODUCT_LIMIT, "product-too-large");
  return release;
}

export function validateProductEntry(entry, product) {
  fields(entry, "manifestUrl manifestSha256 release", "invalid-product-entry");
  validateProductRelease(entry.release);
  requireValue(entry.release.product === product, "catalog-product-mismatch");
  requireValue(entry.manifestUrl === releaseUrl(entry.release.tag, "product-manifest.json"), "invalid-product-manifest-url");
  requireValue(typeof entry.manifestSha256 === "string" && sha256.test(entry.manifestSha256), "invalid-product-manifest-hash");
  return entry;
}

export function validateCatalog(catalog) {
  fields(catalog, "schema publishedAt publicationTag products", "invalid-catalog");
  requireValue(catalog.schema === CATALOG_SCHEMA, "invalid-catalog-schema");
  validateTimestamp(catalog.publishedAt);
  const publication = parseTag(catalog.publicationTag);
  object(catalog.products, "invalid-products");
  requireValue(isDeepStrictEqual(Object.keys(catalog.products).sort(), ["app", "sync"]), "invalid-products");
  for (const product of ["app", "sync"]) {
    const entry = catalog.products[product];
    if (entry !== null) {
      validateProductEntry(entry, product);
      requireValue(parseVersion(entry.release.version).prerelease.length === 0, "stable-catalog-prerelease");
    }
  }
  requireValue(catalog.products[publication.product]?.release.tag === catalog.publicationTag, "catalog-publication-mismatch");
  requireValue(Buffer.byteLength(JSON.stringify(catalog)) <= CATALOG_LIMIT, "catalog-too-large");
  return catalog;
}
