import { existsSync, readFileSync, readdirSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  describeFile,
  parseArgs,
  readJson,
  requireArg,
  writeJson,
} from "./common.mjs";
import {
  downloadKey,
  PRODUCT_SCHEMA,
  releaseUrl,
  requiredDownloads,
  updaterTarget,
  validateProductRelease,
} from "./contracts.mjs";
import { assertPackageFormat } from "./platform/package.mjs";
import { signAndVerify } from "./sign.mjs";
import { verifyFileSignature } from "./signatures.mjs";

function walk(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? walk(path) : [path];
  });
}

function loadPublicKey(value) {
  if (!value) throw new Error("A release public key is required.");
  return existsSync(value) ? readFileSync(value, "utf8") : value;
}

function requiredChecks(download) {
  if (download.format === "apk")
    return ["abi-arm64", "apk-signed", "version-match", "zipaligned-16k"];
  if (download.format === "ipa")
    return ["iphoneos", "payload-single-app", "version-match"];
  if (download.product === "sync" && download.variant === "managed") {
    if (download.os === "windows" && download.format === "zip")
      return ["archive-layout", "bundle-inventory"];
    if (download.os === "linux") return ["archive-layout", "bundle-inventory"];
    if (download.os === "darwin" && download.format === "app.tar.gz")
      return ["whole-app"];
  }
  return [];
}

function assertBuildProof(download) {
  if (!Array.isArray(download.binaryProof) || download.binaryProof.length === 0)
    throw new Error(`Missing executable proof for ${downloadKey(download)}.`);
  if (!download.binaryProof.some((proof) => proof.arches?.includes(download.arch)))
    throw new Error(`No ${download.arch} executable proof for ${downloadKey(download)}.`);
  const checks = new Set(download.checks ?? []);
  for (const check of requiredChecks(download)) {
    if (!checks.has(check)) throw new Error(`Missing ${check} proof for ${downloadKey(download)}.`);
  }
}

export async function collectRelease({ releaseInput, inputDirectory, outputDirectory, publicKey, publishedAt = new Date().toISOString() }) {
  const files = walk(resolve(inputDirectory));
  const pathsByName = new Map();
  for (const path of files) {
    const name = basename(path);
    if (pathsByName.has(name)) throw new Error(`Duplicate collected filename: ${name}.`);
    pathsByName.set(name, path);
  }
  const inventories = files
    .filter((path) => /^inventory-[a-z0-9_-]+\.json$/.test(basename(path)))
    .map(readJson);
  if (inventories.length === 0) throw new Error("No build inventories were collected.");

  const expected = new Set(requiredDownloads(releaseInput.product).map(downloadKey));
  const downloads = [];
  const vendorByTarget = new Map();
  const keyText = loadPublicKey(publicKey);
  for (const inventory of inventories) {
    if (
      inventory.schema !== "risunest.release-build/v1" ||
      inventory.product !== releaseInput.product ||
      inventory.tag !== releaseInput.tag ||
      inventory.sourceCommit !== releaseInput.sourceCommit
    )
      throw new Error(`Inventory identity mismatch in ${inventory.leg ?? "unknown"}.`);
    for (const item of inventory.downloads ?? []) {
      const key = downloadKey(item);
      if (!expected.delete(key)) throw new Error(`Unexpected or duplicate collected download ${key}.`);
      const path = pathsByName.get(item.fileName);
      const signaturePath = pathsByName.get(item.signatureFileName);
      if (!path || !signaturePath || item.signatureFileName !== `${item.fileName}.sig`)
        throw new Error(`Missing asset or signature for ${key}.`);
      const file = await describeFile(path);
      if (file.size !== item.size || file.sha256 !== item.sha256)
        throw new Error(`Collected asset does not match inventory for ${key}.`);
      const signature = readFileSync(signaturePath, "utf8").trim();
      if (signature !== item.signature) throw new Error(`Collected signature differs for ${key}.`);
      await verifyFileSignature(path, signature, keyText);
      assertPackageFormat(path, item.format);
      assertBuildProof(item);
      const url = releaseUrl(releaseInput.tag, item.fileName);
      downloads.push({
        product: item.product,
        variant: item.variant,
        os: item.os,
        arch: item.arch,
        format: item.format,
        version: releaseInput.version,
        url,
        size: file.size,
        sha256: file.sha256,
        signatureUrl: `${url}.sig`,
      });
    }
    for (const item of inventory.vendor ?? []) {
      const pinned = releaseInput.vendorInput?.assets?.[`${item.os}-${item.packageArch}`];
      if (
        item.name !== "cloudflared" ||
        item.version !== releaseInput.vendorInput?.version ||
        !["windows", "linux", "darwin"].includes(item.os) ||
        !["x86_64", "aarch64"].includes(item.packageArch) ||
        !["x86_64", "aarch64"].includes(item.arch) ||
        !/^[a-f0-9]{64}$/.test(item.sha256 ?? "") ||
        !pinned ||
        item.arch !== pinned.actualArch ||
        item.sourceUrl !== pinned.url ||
        item.sourceSha256 !== pinned.sha256
      )
        throw new Error("Cloudflared build proof does not match the pinned target input.");
      const target = `${item.os}/${item.packageArch}`;
      if (vendorByTarget.has(target)) throw new Error(`Duplicate cloudflared proof for ${target}.`);
      vendorByTarget.set(target, item);
    }
  }
  if (expected.size) throw new Error(`Missing downloads: ${[...expected].sort().join(", ")}.`);

  downloads.sort((left, right) => downloadKey(left).localeCompare(downloadKey(right)));
  const platforms = {};
  for (const download of downloads) {
    const target = updaterTarget(download);
    if (!target) continue;
    const inventoryItem = inventories
      .flatMap((inventory) => inventory.downloads)
      .find((item) => downloadKey(item) === downloadKey(download));
    platforms[target] = { url: download.url, signature: inventoryItem.signature };
  }
  const vendorProof = releaseInput.product === "sync"
    ? [...vendorByTarget.values()].sort((left, right) =>
        `${left.os}/${left.packageArch ?? left.arch}`.localeCompare(`${right.os}/${right.packageArch ?? right.arch}`),
      )
    : [];
  if (releaseInput.product === "sync" && vendorProof.length !== 6)
    throw new Error(`Expected six cloudflared target proofs, found ${vendorProof.length}.`);
  const vendor = vendorProof.map(({ name, version, os, arch, sha256 }) => ({
    name,
    version,
    os,
    arch,
    sha256,
  }));

  const release = {
    schema: PRODUCT_SCHEMA,
    product: releaseInput.product,
    version: releaseInput.version,
    tag: releaseInput.tag,
    sourceCommit: releaseInput.sourceCommit,
    releasePage: releaseInput.releasePage,
    notes: releaseInput.notes,
    localizedNotes: releaseInput.localizedNotes,
    pub_date: publishedAt,
    platforms,
    downloads,
    compatibility: releaseInput.compatibility,
    vendor,
  };
  validateProductRelease(release);
  const manifestPath = join(resolve(outputDirectory), "product-manifest.json");
  writeJson(manifestPath, release);
  await signAndVerify(manifestPath, keyText);
  return { release, manifestPath };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const releaseInput = readJson(requireArg(args, "release-input"));
  if (releaseInput.product !== requireArg(args, "product"))
    throw new Error("Product argument does not match release input.");
  if (releaseInput.tag !== requireArg(args, "tag"))
    throw new Error("Tag argument does not match release input.");
  if (releaseInput.sourceCommit !== requireArg(args, "source-commit"))
    throw new Error("Source commit argument does not match release input.");
  const result = await collectRelease({
    releaseInput,
    inputDirectory: requireArg(args, "input-dir"),
    outputDirectory: requireArg(args, "output-dir"),
    publicKey: args["public-key"] ?? process.env.RISUNEST_UPDATE_PUBLIC_KEY,
  });
  process.stdout.write(`${JSON.stringify({ manifestPath: result.manifestPath })}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  await main();
