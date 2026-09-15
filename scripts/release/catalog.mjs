import { isDeepStrictEqual } from "node:util";
import { CATALOG_SCHEMA, compareVersions, parseVersion, validateCatalog, validateProductEntry } from "./contracts.mjs";

export function mergeCatalog(previous, entry, publicationTag, publishedAt) {
  const product = entry?.release?.product;
  validateProductEntry(entry, product);
  if (entry.release.tag !== publicationTag) throw new Error("publication-tag-mismatch");
  if (parseVersion(entry.release.version).prerelease.length) throw new Error("stable-publication-prerelease");
  if (previous !== null) validateCatalog(previous);
  const existing = previous?.products[product];
  if (existing) {
    const comparison = compareVersions(entry.release.version, existing.release.version);
    if (comparison < 0) throw new Error("publication-superseded");
    if (comparison === 0) {
      if (!isDeepStrictEqual(existing, entry)) throw new Error("immutable-product-conflict");
      return structuredClone(previous);
    }
  }
  const catalog = { schema: CATALOG_SCHEMA, publishedAt, publicationTag,
    products: previous ? structuredClone(previous.products) : { app: null, sync: null } };
  catalog.products[product] = structuredClone(entry);
  return validateCatalog(catalog);
}
