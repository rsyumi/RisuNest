import { copyFileSync, existsSync, mkdirSync, readFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  describeFile,
  parseArgs,
  readJson,
  requireArg,
  writeJson,
} from "./common.mjs";
import { downloadKey } from "./contracts.mjs";
import { assertBinaryArchitecture } from "./platform/native.mjs";
import { assertPackageFormat } from "./platform/package.mjs";
import { signAndVerify } from "./sign.mjs";

function loadPublicKey(value) {
  if (!value) throw new Error("A release public key is required.");
  return existsSync(value) ? readFileSync(value, "utf8") : value;
}

function copyFinalAsset(source, output, fileName) {
  const destination = resolve(output, fileName);
  const absoluteSource = resolve(source);
  if (absoluteSource !== destination) copyFileSync(absoluteSource, destination);
  return destination;
}

export async function recordBuild({ releaseInput, leg, assets, output, publicKey }) {
  if (!leg || !/^[a-z0-9][a-z0-9_-]*$/.test(leg)) throw new Error("Invalid build leg.");
  if (!Array.isArray(assets) || assets.length === 0) throw new Error("Build leg has no assets.");
  mkdirSync(output, { recursive: true });
  const expected = new Map(
    releaseInput.expectedDownloads.map((download) => [downloadKey(download), download]),
  );
  const seen = new Set();
  const downloads = [];
  const vendor = [];
  const keyText = loadPublicKey(publicKey);

  for (const asset of assets) {
    const identity = asset.download;
    const key = downloadKey(identity);
    const definition = expected.get(key);
    if (!definition || seen.has(key)) throw new Error(`Unexpected or duplicate build asset ${key}.`);
    seen.add(key);
    if (asset.fileName && asset.fileName !== definition.fileName)
      throw new Error(`Build asset ${key} must be named ${definition.fileName}.`);
    assertPackageFormat(asset.path, identity.format);
    if (!Array.isArray(asset.binaries) || asset.binaries.length === 0)
      throw new Error(`Build asset ${key} has no executable proof.`);
    const binaryProof = asset.binaries.map((binary) => {
      if (binary.arch !== identity.arch)
        throw new Error(`Executable proof for ${key} uses ${binary.arch}, expected ${identity.arch}.`);
      const inspected = assertBinaryArchitecture(binary.path, binary.arch, binary.format);
      return { role: binary.role, format: inspected.format, arches: inspected.arches };
    });
    const finalPath = copyFinalAsset(asset.path, output, definition.fileName);
    const signing = await signAndVerify(finalPath, keyText);
    const file = await describeFile(finalPath);
    downloads.push({
      ...identity,
      version: releaseInput.version,
      fileName: definition.fileName,
      size: file.size,
      sha256: file.sha256,
      signatureFileName: basename(signing.signaturePath),
      signature: signing.signature,
      binaryProof,
      checks: [...new Set(asset.checks ?? [])].sort(),
    });
    for (const item of asset.vendor ?? []) {
      const vendorFile = await describeFile(item.path);
      const inspected = assertBinaryArchitecture(item.path, item.arch, item.format);
      const pinned = releaseInput.vendorInput?.assets?.[`${identity.os}-${identity.arch}`];
      if (
        !pinned ||
        pinned.actualArch !== item.arch ||
        pinned.url !== item.sourceUrl ||
        pinned.sha256 !== item.sourceSha256
      )
        throw new Error(`Cloudflared proof does not match the pinned ${identity.os}-${identity.arch} input.`);
      vendor.push({
        name: "cloudflared",
        version: releaseInput.vendorInput.version,
        os: identity.os,
        arch: item.arch,
        packageArch: identity.arch,
        sha256: vendorFile.sha256,
        format: inspected.format,
        sourceUrl: item.sourceUrl,
        sourceSha256: item.sourceSha256,
      });
    }
  }

  const inventory = {
    schema: "risunest.release-build/v1",
    product: releaseInput.product,
    tag: releaseInput.tag,
    sourceCommit: releaseInput.sourceCommit,
    leg,
    downloads,
    vendor,
  };
  writeJson(join(output, `inventory-${leg}.json`), inventory);
  return inventory;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const releaseInput = readJson(requireArg(args, "release-input"));
  const assets = readJson(requireArg(args, "assets-json"));
  const inventory = await recordBuild({
    releaseInput,
    leg: requireArg(args, "leg"),
    assets,
    output: requireArg(args, "output"),
    publicKey: args["public-key"] ?? process.env.RISUNEST_UPDATE_PUBLIC_KEY,
  });
  process.stdout.write(`${JSON.stringify(inventory)}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  await main();
