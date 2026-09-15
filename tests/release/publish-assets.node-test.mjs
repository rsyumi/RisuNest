import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, writeFile, unlink, rmdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { releaseUrl } from "../../scripts/release/contracts.mjs";
import { verifyUploadedFiles } from "../../scripts/release/publish.mjs";
import { createSigner } from "./signing.mjs";

test("publication binds local payload, remote digest, detached and updater signatures", async () => {
  const directory = await mkdtemp(join(tmpdir(), "risunest-publish-test-"));
  const name = "synthetic.zip", file = join(directory, name);
  const bytes = Buffer.from("synthetic archive"), signer = createSigner();
  const signature = signer.sign(bytes), sha256 = createHash("sha256").update(bytes).digest("hex");
  const tag = "sync-v1.0.0", url = releaseUrl(tag, name);
  const product = { tag, downloads: [{ url, size: bytes.length, sha256 }], platforms: { windows: { url, signature } } };
  const release = { assets: [{ name, state: "uploaded", size: bytes.length, digest: `sha256:${sha256}` }] };
  let remoteSignature = signature;
  const github = { downloadAsset: async () => Buffer.from(remoteSignature) };
  try {
    await writeFile(file, bytes); await writeFile(`${file}.sig`, signature);
    await verifyUploadedFiles(github, release, product, directory, signer.key);
    release.assets[0].digest = `sha256:${"0".repeat(64)}`;
    await assert.rejects(verifyUploadedFiles(github, release, product, directory, signer.key), /uploaded-release-hash/);
    release.assets[0].digest = `sha256:${sha256}`;
    remoteSignature = createSigner().sign(bytes);
    await assert.rejects(verifyUploadedFiles(github, release, product, directory, signer.key), /uploaded-signature/);
    remoteSignature = signature;
    product.platforms.windows.signature = remoteSignature + "changed";
    await assert.rejects(verifyUploadedFiles(github, release, product, directory, signer.key), /platform-signature/);
    product.platforms.windows.signature = signature;
    await writeFile(file, Buffer.alloc(bytes.length));
    await assert.rejects(verifyUploadedFiles(github, release, product, directory, signer.key));
  } finally {
    await Promise.all([unlink(file), unlink(`${file}.sig`)]); await rmdir(directory);
  }
});
