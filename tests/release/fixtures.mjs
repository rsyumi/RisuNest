import { createHash } from "node:crypto";

const base = "https://github.com/rsyumi/RisuNest/releases";
const hash = createHash("sha256").update("synthetic release bytes").digest("hex");
const signature = Buffer.from("synthetic signature checked separately").toString("base64");

export function productFixture(product = "app", version = "1.0.0") {
  const tag = `${product}-v${version}`;
  const downloads = [];
  const platforms = {};
  const add = (variant, os, arch, format, target) => {
    const url = `${base}/download/${tag}/${product}-${variant}-${os}-${arch}.${format}`;
    downloads.push({ product, variant, os, arch, format, version, url,
      size: 23, sha256: hash, signatureUrl: `${url}.sig` });
    if (target) platforms[target] = { url, signature };
  };
  for (const arch of ["x86_64", "aarch64"]) {
    if (product === "app") {
      add("desktop", "windows", arch, "zip");
      add("desktop", "windows", arch, "nsis", `windows-${arch}-nsis`);
      add("desktop", "linux", arch, "deb", `linux-${arch}-deb`);
      add("desktop", "linux", arch, "appimage", `linux-${arch}-appimage`);
      add("desktop", "darwin", arch, "dmg");
      add("desktop", "darwin", arch, "app.tar.gz", `darwin-${arch}-app`);
    } else {
      add("raw", "windows", arch, "zip");
      add("raw", "linux", arch, "tar.gz");
      add("raw", "darwin", arch, "tar.gz");
      add("managed", "windows", arch, "nsis", `windows-${arch}-nsis`);
      add("managed", "windows", arch, "zip");
      add("managed", "linux", arch, "tar.gz");
      add("managed", "darwin", arch, "dmg");
      add("managed", "darwin", arch, "app.tar.gz", `darwin-${arch}-app`);
    }
  }
  if (product === "app") {
    add("mobile", "android", "aarch64", "apk");
    add("mobile", "ios", "aarch64", "ipa");
  }
  return { schema: "risunest.product-release/v1", product, version, tag,
    sourceCommit: "a".repeat(40), releasePage: `${base}/tag/${tag}`,
    notes: "Synthetic release", localizedNotes: { en: "Synthetic release" },
    pub_date: "2026-09-15T00:00:00Z", platforms, downloads,
    compatibility: product === "sync"
      ? { protocolId: "test-wire/v1", storeFormatId: "test-store/v1", automaticApply: true }
      : null,
    vendor: product === "sync"
      ? ["windows", "linux", "darwin"].flatMap(os => ["x86_64", "aarch64"].map(arch => ({
        name: "cloudflared", version: "2026.1.0", os, arch, sha256: hash,
      }))) : [] };
}

export function entryFixture(product = "app", version = "1.0.0") {
  const release = productFixture(product, version);
  return { manifestUrl: `${base}/download/${release.tag}/product-manifest.json`,
    manifestSha256: createHash("sha256").update(JSON.stringify(release)).digest("hex"), release };
}

export function catalogFixture() {
  return { schema: "risunest.release-catalog/v1", publishedAt: "2026-09-15T00:00:00Z",
    publicationTag: "sync-v1.0.0", products: { app: entryFixture(), sync: entryFixture("sync") } };
}
