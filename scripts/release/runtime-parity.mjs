import { readdir } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { validateProductRelease } from "./contracts.mjs";
import { parseArgs, readJson, requireArg } from "./common.mjs";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const parityFormats = new Map([
  ["windows", "zip"],
  ["linux", "tar.gz"],
  ["darwin", "app.tar.gz"],
]);

async function indexFiles(directory, result = new Map()) {
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`Runtime parity input cannot contain a link: ${path}`);
    if (entry.isDirectory()) await indexFiles(path, result);
    else if (entry.isFile()) {
      if (result.has(entry.name)) throw new Error(`Duplicate runtime parity asset: ${entry.name}`);
      result.set(entry.name, path);
    }
  }
  return result;
}

export async function parityInputs(release, assetsDirectory) {
  validateProductRelease(release, "sync");
  const indexed = await indexFiles(resolve(assetsDirectory));
  const downloads = release.downloads.filter((download) =>
    download.variant === "managed" && parityFormats.get(download.os) === download.format);
  if (downloads.length !== 6) throw new Error("Runtime parity requires six managed updater packages.");
  return downloads.map((download) => {
    const name = basename(decodeURIComponent(new URL(download.url).pathname));
    const archive = indexed.get(name);
    if (!archive) throw new Error(`Runtime parity asset is missing: ${name}`);
    return { archive, format: download.format, os: download.os, arch: download.arch };
  });
}

export function assertOneTestResult(output, target) {
  if (!/test result: ok\. 1 passed; 0 failed; 0 ignored;/.test(output))
    throw new Error(`Runtime extractor test did not execute for ${target}.`);
}

export async function runRuntimeParity({ productManifest, assetsDirectory }) {
  const manifest = resolve(productManifest);
  const release = readJson(manifest);
  const inputs = await parityInputs(release, assetsDirectory);
  for (const input of inputs) {
    const child = spawnSync(
      "cargo",
      [
        "test",
        "--manifest-path", "server/manager/Cargo.toml",
        "--release",
        "--locked",
        "--lib",
        "update::archive::tests::package_script_windows_zip_matches_the_runtime_extractor",
        "--",
        "--ignored",
        "--exact",
        "--test-threads=1",
      ],
      {
        cwd: repo,
        env: {
          ...process.env,
          RISUNEST_PACKAGE_FIXTURE: input.archive,
          RISUNEST_PRODUCT_MANIFEST: manifest,
          RISUNEST_PACKAGE_FORMAT: input.format,
          RISUNEST_PACKAGE_OS: input.os,
          RISUNEST_PACKAGE_ARCH: input.arch,
        },
        encoding: "utf8",
        windowsHide: true,
      },
    );
    if (child.error) throw child.error;
    process.stdout.write(child.stdout ?? "");
    process.stderr.write(child.stderr ?? "");
    if (child.status !== 0)
      throw new Error(`Runtime extractor rejected ${input.os}/${input.arch}/${input.format}.`);
    assertOneTestResult(
      `${child.stdout ?? ""}\n${child.stderr ?? ""}`,
      `${input.os}/${input.arch}/${input.format}`,
    );
  }
  return inputs;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const inputs = await runRuntimeParity({
    productManifest: requireArg(args, "product-manifest"),
    assetsDirectory: requireArg(args, "assets-dir"),
  });
  process.stdout.write(`${JSON.stringify({ verified: inputs.length })}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
