import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import {
  mkdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { lstat } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import { parseTag, parseVersion as parseContractVersion } from "./contracts.mjs";

const SHA = /^[a-f0-9]{40}$/;

export function parseArgs(argv, booleanNames = new Set()) {
  const result = {};
  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];
    if (!token.startsWith("--")) throw new Error(`Unexpected argument: ${token}`);
    const name = token.slice(2);
    if (booleanNames.has(name)) {
      result[name] = true;
      continue;
    }
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--"))
      throw new Error(`Missing value for --${name}.`);
    result[name] = value;
    index += 1;
  }
  return result;
}

export function requireArg(args, name) {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0)
    throw new Error(`--${name} is required.`);
  return value;
}

export function parseVersion(value, label = "version") {
  let parsed;
  try {
    parsed = parseContractVersion(value);
  } catch {
    throw new Error(`${label} must be an unprefixed stable SemVer.`);
  }
  if (parsed.prerelease.length || parsed.build) throw new Error(`${label} must be an unprefixed stable SemVer.`);
  return parsed.numbers;
}

export function androidVersionCode(version) {
  const [major, minor, patch] = parseVersion(version, "App version");
  if (major > 2099n || minor > 999n || patch > 999n)
    throw new Error("App version cannot be represented as an Android versionCode.");
  const value = major * 1_000_000n + minor * 1_000n + patch;
  if (value < 1n || value > 2_100_000_000n)
    throw new Error("Android versionCode is outside the Play-supported range.");
  return Number(value);
}

export function iosBundleVersion(version) {
  const numbers = parseVersion(version, "App version");
  return numbers.map(String).join(".");
}

export function validateTag(product, tag, version) {
  const parsed = parseTag(tag);
  if (parsed.product !== product) throw new Error(`Tag ${tag} does not belong to ${product}.`);
  const expected = `${product}-v${version}`;
  if (tag !== expected) throw new Error(`Expected tag ${expected}, received ${tag}.`);
}

export function validateSourceCommit(value) {
  if (!SHA.test(value)) throw new Error("sourceCommit must be a full lowercase Git SHA.");
  return value;
}

export function normalizePublishedAt(value) {
  const parsed = new Date(value);
  if (!Number.isFinite(parsed.valueOf())) throw new Error("publishedAt must be an ISO date.");
  return parsed.toISOString();
}

export function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

export function writeJson(path, value) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

export function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

export async function describeFile(path) {
  const absolute = resolve(path);
  const stats = await lstat(absolute);
  if (!stats.isFile() || stats.size === 0) throw new Error(`${path} is not a non-empty file.`);
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(absolute)) hash.update(chunk);
  return {
    path: absolute,
    fileName: basename(absolute),
    size: stats.size,
    sha256: hash.digest("hex"),
  };
}

export function assetName({ product, variant, os, arch, format, version }) {
  const extension = {
    zip: ".zip",
    nsis: "-setup.exe",
    deb: ".deb",
    appimage: ".AppImage",
    dmg: ".dmg",
    apk: ".apk",
    ipa: "-unsigned.ipa",
    "tar.gz": ".tar.gz",
    "app.tar.gz": ".app.tar.gz",
  }[format];
  if (!extension) throw new Error(`Unsupported release format: ${format}`);
  const name = product === "app" ? "RisuNest" : "RisuNest-Sync";
  const variantPart = product === "sync" ? `-${variant}` : "";
  return `${name}-${version}${variantPart}-${os}-${arch}${extension}`;
}

export function assertHttpsBaseUrl(value, label) {
  const url = new URL(value);
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  )
    throw new Error(`${label} must be an HTTPS URL without credentials, query, or fragment.`);
  return url.href;
}
