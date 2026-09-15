import { existsSync, openAsBlob } from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs, readJson, requireArg } from "./common.mjs";
import { downloadKey, parseTag, requiredDownloads } from "./contracts.mjs";
import { GitHubReleases } from "./github.mjs";

function validateInventory(inventory, directory) {
  const keys = ["schema", "product", "tag", "sourceCommit", "leg", "downloads", "vendor"];
  if (JSON.stringify(Object.keys(inventory).sort()) !== JSON.stringify(keys.sort()))
    throw new Error("Invalid release build inventory fields.");
  const identity = parseTag(inventory.tag);
  if (
    inventory.schema !== "risunest.release-build/v1" ||
    inventory.product !== identity.product ||
    !/^[a-f0-9]{40}$/.test(inventory.sourceCommit) ||
    !Array.isArray(inventory.downloads) ||
    inventory.downloads.length === 0 ||
    !Array.isArray(inventory.vendor)
  ) throw new Error("Invalid release build inventory identity.");
  const expected = new Set(requiredDownloads(inventory.product).map(downloadKey));
  const names = new Set();
  const paths = [];
  for (const download of inventory.downloads) {
    if (!expected.has(downloadKey(download))) throw new Error("Unexpected release build download.");
    for (const name of [download.fileName, download.signatureFileName]) {
      if (typeof name !== "string" || basename(name) !== name || names.has(name))
        throw new Error(`Invalid or duplicate build output ${name}.`);
      names.add(name);
      const path = join(directory, name);
      if (!existsSync(path)) throw new Error(`Missing build output ${name}.`);
      paths.push({ name, path });
    }
  }
  return paths;
}

function assertDraftIdentity(release, draftId, inventory) {
  if (
    Number(release.id) !== Number(draftId) ||
    release.tag_name !== inventory.tag ||
    release.target_commitish !== inventory.sourceCommit ||
    release.prerelease ||
    !release.draft
  ) throw new Error("Release draft identity changed before asset upload.");
  return release;
}

export async function uploadBuild({ draftId, directory, inventory, token, github }) {
  const root = resolve(directory);
  const paths = validateInventory(inventory, root);
  const releases = github ?? new GitHubReleases(token);
  let release = assertDraftIdentity(await releases.getById(draftId), draftId, inventory);
  for (const { name, path } of paths) {
    let existing = release.assets?.find((asset) => asset.name === name);
    if (existing) {
      release = assertDraftIdentity(await releases.getById(draftId), draftId, inventory);
      existing = release.assets?.find((asset) => asset.name === name);
      if (existing) await releases.request(`/releases/assets/${existing.id}`, { method: "DELETE" });
    }
    release = assertDraftIdentity(await releases.getById(draftId), draftId, inventory);
    if (release.assets?.some((asset) => asset.name === name))
      throw new Error(`Build output ${name} appeared concurrently.`);
    const bytes = await openAsBlob(path, { type: "application/octet-stream" });
    await releases.request(`/releases/${Number(draftId)}/assets?name=${encodeURIComponent(name)}`, {
      method: "POST",
      body: bytes,
      upload: true,
    });
    release = assertDraftIdentity(await releases.getById(draftId), draftId, inventory);
    const uploaded = release.assets?.find((asset) => asset.name === name);
    if (!uploaded || uploaded.state !== "uploaded") throw new Error(`Build output ${name} was not uploaded.`);
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const inventoryPath = requireArg(args, "inventory");
  await uploadBuild({
    draftId: requireArg(args, "draft-id"),
    directory: requireArg(args, "directory"),
    inventory: readJson(inventoryPath),
    token: process.env.GITHUB_TOKEN ?? process.env.GH_TOKEN,
  });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  await main();
