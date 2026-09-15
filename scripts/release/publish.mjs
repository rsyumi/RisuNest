import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { appendFile, lstat, mkdtemp, readFile, rmdir, unlink, writeFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { GitHubReleases } from "./github.mjs";
import { REPOSITORY, validateAssetUrl } from "./contracts.mjs";
import { publishRelease } from "./publication.mjs";
import { verifyFileSignature } from "./signatures.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

export async function verifyUploadedFiles(github, release, product, directory, publicKey) {
  const assetMap = new Map(release.assets.map(asset => [asset.name, asset]));
  if (assetMap.size !== release.assets.length) throw new Error("duplicate-release-asset");
  for (const download of product.downloads) {
    const name = validateAssetUrl(download.url, product.tag);
    const file = join(directory, name), signaturePath = `${file}.sig`;
    const [info, signatureInfo] = await Promise.all([lstat(file), lstat(signaturePath)]);
    if (!info.isFile() || !signatureInfo.isFile() || info.size !== download.size || signatureInfo.size > 16384) {
      throw new Error("invalid-local-release-asset");
    }
    const signature = await readFile(signaturePath, "utf8");
    await verifyFileSignature(file, signature, publicKey);
    const hash = createHash("sha256");
    for await (const chunk of createReadStream(file)) hash.update(chunk);
    if (hash.digest("hex") !== download.sha256) throw new Error("local-release-hash-mismatch");
    const remote = assetMap.get(name);
    if (!remote || remote.state !== "uploaded" || remote.size !== download.size
      || remote.digest !== `sha256:${download.sha256}`) throw new Error("uploaded-release-hash-mismatch");
    const remoteSignature = await github.downloadAsset(release, `${name}.sig`, 16384);
    if (remoteSignature.toString("utf8").trim() !== signature.trim()) throw new Error("uploaded-signature-mismatch");
    for (const platform of Object.values(product.platforms)) {
      if (platform.url === download.url && platform.signature.trim() !== signature.trim()) {
        throw new Error("platform-signature-mismatch");
      }
    }
  }
}

async function signCatalog(bytes) {
  if (!process.env.TAURI_SIGNING_PRIVATE_KEY && !process.env.TAURI_SIGNING_PRIVATE_KEY_PATH) {
    throw new Error("signing-key-required");
  }
  const directory = await mkdtemp(join(tmpdir(), "risunest-release-sign-"));
  const file = join(directory, "manifest.json");
  try {
    await writeFile(file, bytes, { mode: 0o600 });
    try {
      execFileSync(process.execPath, [join(root, "node_modules/@tauri-apps/cli/tauri.js"), "signer", "sign", file],
        { cwd: root, env: process.env, stdio: "pipe", timeout: 60000, windowsHide: true });
    } catch { throw new Error("catalog-signing-failed"); }
    return await readFile(`${file}.sig`, "utf8");
  } finally {
    await Promise.allSettled([unlink(file), unlink(`${file}.sig`)]);
    await rmdir(directory).catch(() => {});
  }
}

async function main() {
  const { values } = parseArgs({ options: {
    "prepare-draft": { type: "boolean" }, publish: { type: "boolean" }, bootstrap: { type: "boolean" },
    product: { type: "string" }, tag: { type: "string" }, "source-commit": { type: "string" },
    "release-input": { type: "string" }, "product-manifest": { type: "string" },
    assets: { type: "string" }, "draft-id": { type: "string" }, "published-at": { type: "string" },
  } });
  if (process.env.GITHUB_REPOSITORY !== REPOSITORY) throw new Error("production-publication-repository-mismatch");
  if (!!values["prepare-draft"] === !!values.publish) throw new Error("select-prepare-draft-or-publish");
  if (!values.tag || !values["source-commit"]) throw new Error("tag-and-source-commit-required");
  const github = new GitHubReleases(process.env.GITHUB_TOKEN ?? process.env.GH_TOKEN);
  const input = values["release-input"] ? JSON.parse(await readFile(values["release-input"], "utf8")) : null;
  if (input && (input.tag !== values.tag || input.sourceCommit !== values["source-commit"]
    || values.product && input.product !== values.product)) throw new Error("release-input-mismatch");
  if (values["prepare-draft"]) {
    const release = await github.prepareDraft(values.tag, values["source-commit"], input?.notes ?? "");
    if (process.env.GITHUB_OUTPUT) await appendFile(process.env.GITHUB_OUTPUT,
      `release_id=${release.id}\nalready_published=${!release.draft}\n`);
    console.log(JSON.stringify({ releaseId: release.id, draft: release.draft }));
    return;
  }
  if (!values.product || !values["product-manifest"] || !values.assets || !process.env.RISUNEST_UPDATE_PUBLIC_KEY) {
    throw new Error("publication-inputs-required");
  }
  if (values["draft-id"]) {
    const release = await github.getByTag(values.tag);
    if (String(release?.id) !== values["draft-id"]) throw new Error("draft-id-mismatch");
  }
  const bytes = await readFile(values["product-manifest"]);
  const signature = await readFile(`${values["product-manifest"]}.sig`, "utf8");
  const result = await publishRelease({ github, productBytes: bytes, productSignature: signature,
    publicKey: process.env.RISUNEST_UPDATE_PUBLIC_KEY, expectedProduct: values.product,
    expectedTag: values.tag, expectedCommit: values["source-commit"], bootstrap: values.bootstrap,
    publishedAt: values["published-at"] ?? new Date().toISOString(), signCatalog,
    verifyAssets: (release, product) => verifyUploadedFiles(github, release, product, resolve(values.assets), process.env.RISUNEST_UPDATE_PUBLIC_KEY) });
  console.log(JSON.stringify(result));
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
