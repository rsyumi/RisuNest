import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  androidVersionCode,
  iosBundleVersion,
  assertHttpsBaseUrl,
  normalizePublishedAt,
  parseArgs,
  parseVersion,
  readJson,
  requireArg,
  validateSourceCommit,
  validateTag,
  writeJson,
} from "./common.mjs";
import { releaseDownloads } from "./downloads.mjs";

const scriptDirectory = dirname(fileURLToPath(import.meta.url));
const defaultRepository = resolve(scriptDirectory, "../..");

function requireFile(path, label) {
  if (!existsSync(path)) throw new Error(`${label} is missing: ${path}`);
  return path;
}

function requireVersion(value, expected, label) {
  if (value !== expected)
    throw new Error(`${label} version ${JSON.stringify(value)} does not match ${expected}.`);
}

export function validateVendorInput(value) {
  if (!/^\d{4}\.\d{1,2}\.\d+$/.test(value.version))
    throw new Error("Pinned cloudflared version is invalid.");
  if (!/^[a-f0-9]{64}$/.test(value.license?.sha256 ?? ""))
    throw new Error("Pinned cloudflared license checksum is invalid.");
  const licenseUrl = new URL(value.license.url);
  if (
    licenseUrl.protocol !== "https:" ||
    licenseUrl.hostname !== "raw.githubusercontent.com" ||
    licenseUrl.pathname !== `/cloudflare/cloudflared/${value.version}/LICENSE`
  ) throw new Error("Pinned cloudflared license URL is invalid.");
  const expected = [
    "windows-x86_64",
    "windows-aarch64",
    "linux-x86_64",
    "linux-aarch64",
    "darwin-x86_64",
    "darwin-aarch64",
  ];
  if (Object.keys(value.assets ?? {}).sort().join("\n") !== expected.sort().join("\n"))
    throw new Error("Pinned cloudflared assets are incomplete.");
  for (const [target, asset] of Object.entries(value.assets)) {
    const url = new URL(asset.url);
    if (
      url.protocol !== "https:" ||
      url.hostname !== "github.com" ||
      !url.pathname.startsWith(`/cloudflare/cloudflared/releases/download/${value.version}/`) ||
      !/^[a-f0-9]{64}$/.test(asset.sha256 ?? "")
    )
      throw new Error(`Pinned cloudflared input is invalid for ${target}.`);
    const targetArch = target.endsWith("-aarch64") ? "aarch64" : "x86_64";
    const expectedArch = target === "windows-aarch64" ? "x86_64" : targetArch;
    if (asset.actualArch !== expectedArch)
      throw new Error(`Pinned cloudflared architecture is invalid for ${target}.`);
    if (asset.archive !== target.startsWith("darwin-"))
      throw new Error(`Pinned cloudflared archive declaration is invalid for ${target}.`);
  }
  return value;
}

export function sourceCheck({
  repository = defaultRepository,
  product,
  tag,
  sourceCommit,
  publishedAt,
  registryUrl,
}) {
  const repo = resolve(repository);
  const versionFile = product === "app" ? "version.json" : "server/version.json";
  const versionInput = readJson(requireFile(join(repo, versionFile), "Version input"));
  const version = versionInput.version;
  parseVersion(version, `${product} version`);
  validateTag(product, tag, version);
  validateSourceCommit(sourceCommit);
  const pubDate = normalizePublishedAt(publishedAt);
  const notesPath = requireFile(
    join(repo, "release-notes", product, `${version}.md`),
    "Release notes",
  );
  const notes = readFileSync(notesPath, "utf8").trimEnd();
  if (!/^#\s+\S+/m.test(notes)) throw new Error("Release notes must contain a title.");

  const lockfiles = product === "app"
    ? ["pnpm-lock.yaml", "src-tauri/Cargo.lock"]
    : [
        "crates/sync-wire/Cargo.lock",
        "server/sync/Cargo.lock",
        "server/manager/Cargo.lock",
        "server/manager/gui/pnpm-lock.yaml",
        "server/manager/gui/src-tauri/Cargo.lock",
      ];
  for (const lockfile of ["crates/release-update/Cargo.lock", ...lockfiles])
    requireFile(join(repo, lockfile), "Lockfile");

  let compatibility = null;
  let vendorInput = null;
  let mobileBuildNumber = null;
  let iosBuildNumber = null;
  if (product === "app") {
    requireVersion(readJson(join(repo, "src-tauri/tauri.conf.json")).version, version, "Tauri");
    mobileBuildNumber = androidVersionCode(version);
    iosBuildNumber = iosBundleVersion(version);
  } else {
    const versions = [
      ["server/manager/Cargo.toml", /\[package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/],
      ["server/sync/Cargo.toml", /\[package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/],
      ["server/manager/gui/package.json", null],
      ["server/manager/gui/src-tauri/Cargo.toml", /\[package\][\s\S]*?\nversion\s*=\s*"([^"]+)"/],
      ["server/manager/gui/src-tauri/tauri.conf.json", null],
    ];
    for (const [file, pattern] of versions) {
      const path = join(repo, file);
      const actual = pattern
        ? pattern.exec(readFileSync(path, "utf8"))?.[1]
        : readJson(path).version;
      requireVersion(actual, version, file);
    }
    const configured = versionInput.compatibility;
    if (
      !configured ||
      typeof configured.protocolId !== "string" ||
      !configured.protocolId ||
      typeof configured.storeFormatId !== "string" ||
      !configured.storeFormatId ||
      typeof configured.automaticApply !== "boolean"
    )
      throw new Error("server/version.json must define complete compatibility metadata.");
    const daemonSource = readFileSync(join(repo, "server/sync/src/lib.rs"), "utf8");
    const protocol = /pub const PROTOCOL_ID:\s*&str\s*=\s*"([^"]+)"/.exec(daemonSource)?.[1];
    const store = /pub const STORE_FORMAT_ID:\s*&str\s*=\s*"([^"]+)"/.exec(daemonSource)?.[1];
    if (configured.protocolId !== protocol || configured.storeFormatId !== store)
      throw new Error("server/version.json compatibility does not match daemon constants.");
    compatibility = configured;
    vendorInput = validateVendorInput(readJson(join(scriptDirectory, "cloudflared.json")));
    assertHttpsBaseUrl(registryUrl, "Registry");
  }

  return {
    product,
    tag,
    version,
    sourceCommit,
    pub_date: pubDate,
    releasePage: `https://github.com/rsyumi/RisuNest/releases/tag/${encodeURIComponent(tag)}`,
    notes,
    localizedNotes: {},
    compatibility,
    vendorInput,
    mobileBuildNumber,
    iosBuildNumber,
    expectedDownloads: releaseDownloads(product, version),
  };
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const result = sourceCheck({
    repository: args.repository,
    product: requireArg(args, "product"),
    tag: requireArg(args, "tag"),
    sourceCommit: requireArg(args, "source-commit"),
    publishedAt: requireArg(args, "published-at"),
    registryUrl: args["registry-url"],
  });
  writeJson(requireArg(args, "output"), result);
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
