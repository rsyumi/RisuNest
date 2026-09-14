import { createHash } from "node:crypto";
import { isDeepStrictEqual } from "node:util";
import { CATALOG_LIMIT, PRODUCT_LIMIT, compareVersions, releaseUrl,
  validateCatalog, validateProductRelease } from "./contracts.mjs";
import { mergeCatalog } from "./catalog.mjs";
import { verifySignature } from "./signatures.mjs";

export function verifiedJson(bytes, signature, publicKey, limit) {
  if (bytes.length > limit) throw new Error("manifest-too-large");
  verifySignature(bytes, signature, publicKey);
  return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
}

async function releaseCatalog(github, release, publicKey) {
  const [bytes, signature] = await Promise.all([
    github.downloadAsset(release, "manifest.json", CATALOG_LIMIT),
    github.downloadAsset(release, "manifest.json.sig", 16384),
  ]);
  const catalog = validateCatalog(verifiedJson(bytes, signature.toString("utf8"), publicKey, CATALOG_LIMIT));
  if (catalog.publicationTag !== release.tag_name) throw new Error("release-catalog-tag-mismatch");
  return catalog;
}

function dominates(left, right) {
  return ["app", "sync"].every(product => {
    const a = left.products[product], b = right.products[product];
    if (!b) return true;
    if (!a) return false;
    const comparison = compareVersions(a.release.version, b.release.version);
    return comparison > 0 || comparison === 0 && isDeepStrictEqual(a, b);
  });
}

async function recoverMissingLatest(github, release, entry, publicKey) {
  const published = await releaseCatalog(github, release, publicKey);
  if (!isDeepStrictEqual(published.products[entry.release.product], entry)) throw new Error("immutable-product-conflict");
  const stable = await github.listPublishedStable();
  if (!stable.some(candidate => candidate.id === release.id)) throw new Error("publication-recovery-unconfirmed");
  const candidates = [];
  for (const candidate of stable) {
    if (candidate.draft || candidate.prerelease) throw new Error("invalid-stable-release");
    if (!/^(app|sync)-v/.test(candidate.tag_name)) continue;
    candidates.push({ release: candidate, catalog: await releaseCatalog(github, candidate, publicKey) });
  }
  const winners = candidates.filter(candidate => candidates.every(other =>
    dominates(candidate.catalog, other.catalog)
    && Date.parse(candidate.catalog.publishedAt) >= Date.parse(other.catalog.publishedAt)));
  if (winners.length !== 1) throw new Error("publication-recovery-conflict");
  const winner = winners[0].release;
  await github.makeLatest(winner.id);
  if ((await github.getLatest())?.id !== winner.id) throw new Error("latest-recovery-unconfirmed");
  return { status: "recovered", releaseId: winner.id };
}

export async function publishRelease({ github, productBytes, productSignature, publicKey,
  expectedProduct, expectedTag, expectedCommit, bootstrap = false, publishedAt,
  signCatalog, verifyAssets }) {
  const product = validateProductRelease(verifiedJson(productBytes, productSignature, publicKey, PRODUCT_LIMIT));
  if (product.product !== expectedProduct || product.tag !== expectedTag || product.sourceCommit !== expectedCommit) {
    throw new Error("publication-identity-mismatch");
  }
  const entry = { manifestUrl: releaseUrl(product.tag, "product-manifest.json"),
    manifestSha256: createHash("sha256").update(productBytes).digest("hex"), release: product };
  const release = await github.getByTag(product.tag);
  if (!release || release.prerelease || release.target_commitish !== product.sourceCommit) {
    throw new Error("publication-release-mismatch");
  }
  await github.assertTagCommit(product.tag, product.sourceCommit);
  const latest = await github.getLatest();
  if (!latest && !release.draft) return recoverMissingLatest(github, release, entry, publicKey);
  let previous = null;
  if (latest) {
    if (latest.draft || latest.prerelease) throw new Error("invalid-stable-release");
    previous = await releaseCatalog(github, latest, publicKey);
  } else if (!bootstrap || await github.hasPublishedStable()) throw new Error("bootstrap-not-authorized");

  if (!release.draft) {
    const published = await releaseCatalog(github, release, publicKey);
    if (!isDeepStrictEqual(published.products[product.product], entry)) throw new Error("immutable-product-conflict");
    if (previous && dominates(previous, published)) return { status: "already-published", releaseId: release.id };
    if (previous && (!dominates(published, previous) || Date.parse(published.publishedAt) < Date.parse(previous.publishedAt))) {
      throw new Error("publication-recovery-conflict");
    }
    await github.makeLatest(release.id);
    const recovered = await github.getLatest();
    if (recovered?.id !== release.id) throw new Error("latest-recovery-unconfirmed");
    return { status: "recovered", releaseId: release.id };
  }

  const catalog = mergeCatalog(previous, entry, product.tag, publishedAt);
  if (catalog.publicationTag !== product.tag) throw new Error("draft-already-superseded");
  await verifyAssets(release, product);
  const catalogBytes = Buffer.from(`${JSON.stringify(catalog, null, 2)}\n`);
  if (catalogBytes.length > CATALOG_LIMIT) throw new Error("manifest-too-large");
  const catalogSignature = await signCatalog(catalogBytes);
  verifySignature(catalogBytes, catalogSignature, publicKey);
  for (const [name, bytes] of [
    ["product-manifest.json", productBytes],
    ["product-manifest.json.sig", Buffer.from(productSignature)],
    ["manifest.json", catalogBytes],
    ["manifest.json.sig", Buffer.from(catalogSignature)],
  ]) await github.uploadAsset(release, name, bytes);

  const prepared = await github.getByTag(product.tag);
  if (!prepared?.draft || prepared.target_commitish !== expectedCommit) throw new Error("draft-changed-before-publish");
  const remoteCatalog = await releaseCatalog(github, prepared, publicKey);
  if (!isDeepStrictEqual(remoteCatalog, catalog)) throw new Error("uploaded-catalog-mismatch");
  const remoteProduct = await github.downloadAsset(prepared, "product-manifest.json", PRODUCT_LIMIT);
  const remoteSignature = await github.downloadAsset(prepared, "product-manifest.json.sig", 16384);
  verifiedJson(remoteProduct, remoteSignature.toString("utf8"), publicKey, PRODUCT_LIMIT);
  if (!remoteProduct.equals(productBytes)) throw new Error("uploaded-product-mismatch");

  await github.assertTagCommit(product.tag, product.sourceCommit);
  try { await github.publish(prepared.id); }
  catch (error) {
    const observed = await github.getByTag(product.tag);
    if (!observed || observed.draft) throw error;
  }
  const observed = await github.getByTag(product.tag);
  const observedLatest = await github.getLatest();
  if (observed?.draft !== false || observedLatest?.id !== release.id) throw new Error("publication-unconfirmed");
  return { status: "published", releaseId: release.id };
}
