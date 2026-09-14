import { createWriteStream, existsSync, mkdirSync, readFileSync, renameSync } from "node:fs";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { describeFile, parseArgs, readJson, requireArg, writeJson } from "./common.mjs";

const configPath = new URL("./cloudflared.json", import.meta.url);

async function download(url, path) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok || !response.body) throw new Error(`Download failed (${response.status}) for ${url}.`);
  await pipeline(Readable.fromWeb(response.body), createWriteStream(path, { flags: "wx" }));
}

function run(program, args) {
  const child = spawnSync(program, args, { stdio: "inherit", windowsHide: true });
  if (child.error) throw child.error;
  if (child.status !== 0) throw new Error(`${program} failed (${child.status}).`);
}

export async function fetchCloudflared({ target, output }) {
  const config = readJson(configPath);
  const asset = config.assets[target];
  if (!asset) throw new Error(`Unsupported cloudflared target: ${target}.`);
  const destination = resolve(output);
  mkdirSync(destination, { recursive: true });
  const source = join(destination, basename(new URL(asset.url).pathname));
  await download(asset.url, source);
  if ((await describeFile(source)).sha256 !== asset.sha256) throw new Error("Cloudflared download checksum mismatch.");
  let executable = source;
  if (asset.archive) {
    run("tar", ["-xzf", source, "-C", destination]);
    const extracted = join(destination, "cloudflared");
    if (!existsSync(extracted)) throw new Error("Cloudflared archive did not contain the executable.");
    executable = extracted;
  } else {
    const finalName = target.startsWith("windows") ? "cloudflared.exe" : "cloudflared";
    const finalPath = join(destination, finalName);
    if (source !== finalPath) renameSync(source, finalPath);
    executable = finalPath;
  }
  const license = join(destination, "CLOUDFLARED-LICENSE");
  await download(config.license.url, license);
  if ((await describeFile(license)).sha256 !== config.license.sha256)
    throw new Error("Cloudflared license checksum mismatch.");
  const result = {
    target,
    version: config.version,
    actualArch: asset.actualArch,
    sourceUrl: asset.url,
    sourceSha256: asset.sha256,
    executable: resolve(executable),
    executableSha256: (await describeFile(executable)).sha256,
    license: resolve(license),
  };
  writeJson(join(destination, "vendor.json"), result);
  return result;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const result = await fetchCloudflared({
    target: requireArg(args, "target"),
    output: requireArg(args, "output"),
  });
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  await main();
