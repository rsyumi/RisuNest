import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { fetchCloudflared } from "../../../scripts/release/vendor.mjs";
import { sourceCheck } from "../../../scripts/release/source-check.mjs";
import { buildNativeSuite } from "./build.mjs";
import { packageNativeSuite } from "./package.mjs";

const installDirectory = dirname(fileURLToPath(import.meta.url));
const repository = resolve(installDirectory, "../../..");
const localOutputBase = join(repository, ".tmp", "sync-distribution");
const defaultRegistryUrl = "https://registry.rsyumi.workers.dev/";

const targets = Object.freeze({
  "x86_64-pc-windows-msvc": Object.freeze({
    target: "x86_64-pc-windows-msvc",
    os: "windows",
    arch: "x86_64",
    vendorTarget: "windows-x86_64",
    gui: true,
  }),
  "x86_64-unknown-linux-gnu": Object.freeze({
    target: "x86_64-unknown-linux-gnu",
    os: "linux",
    arch: "x86_64",
    vendorTarget: "linux-x86_64",
    gui: false,
  }),
  "aarch64-apple-darwin": Object.freeze({
    target: "aarch64-apple-darwin",
    os: "darwin",
    arch: "aarch64",
    vendorTarget: "darwin-aarch64",
    gui: true,
  }),
});

export function localSyncTarget(target) {
  const config = targets[target];
  if (!config) throw new Error(`Unsupported local Sync target: ${target}.`);
  return { ...config };
}

export function localCargoTargetDirectory({ commonGitDirectory, configuredTarget, root = repository }) {
  if (configuredTarget) return resolve(root, configuredTarget);
  const common = resolve(root, commonGitDirectory.trim());
  if (basename(common) !== ".git") throw new Error("Cannot locate the main checkout shared Cargo target.");
  return join(dirname(common), "src-tauri", "target");
}

function cargoTargetDirectory(env = process.env) {
  if (env.CARGO_TARGET_DIR) {
    return localCargoTargetDirectory({
      commonGitDirectory: "",
      configuredTarget: env.CARGO_TARGET_DIR,
    });
  }
  const result = spawnSync("git", ["rev-parse", "--path-format=absolute", "--git-common-dir"], {
    cwd: repository,
    encoding: "utf8",
    windowsHide: true,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error("Cannot locate the main checkout shared Cargo target.");
  return localCargoTargetDirectory({ commonGitDirectory: result.stdout });
}

function rustHost() {
  const result = spawnSync("rustc", ["-vV"], { encoding: "utf8", windowsHide: true });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error("rustc is unavailable.");
  const host = /^host: (.+)$/m.exec(result.stdout)?.[1];
  if (!host) throw new Error("Rust host target is unavailable.");
  return host;
}

export function validateLocalSyncEnvironment({ target, hostTarget = rustHost(), env = process.env, withoutUpdates = false }) {
  const config = localSyncTarget(target);
  if (!env.CARGO_TARGET_DIR)
    throw new Error("Set CARGO_TARGET_DIR to the repository shared target directory.");
  if (hostTarget !== target)
    throw new Error(`Local Sync distributions must be built on their native host target (${target}); current host is ${hostTarget}.`);
  const publicKey = withoutUpdates ? "" : env.RISUNEST_UPDATE_PUBLIC_KEY?.trim();
  if (!publicKey && !withoutUpdates) throw new Error("RISUNEST_UPDATE_PUBLIC_KEY is required for local Sync distributions. Use --without-updates for a development package that reports update-not-configured.");
  const registry = new URL(env.RISUNEST_DEFAULT_REGISTRY_URL || defaultRegistryUrl);
  if (registry.protocol !== "https:" || registry.username || registry.password || registry.search || registry.hash)
    throw new Error("RISUNEST_DEFAULT_REGISTRY_URL must be an HTTPS base URL without credentials, query, or fragment.");
  return { ...config, cargoTargetDir: resolve(repository, env.CARGO_TARGET_DIR), registryUrl: registry.href, publicKey, updatesEnabled: !withoutUpdates };
}

function command(name, args = ["--version"]) {
  const result = spawnSync(name, args, { encoding: "utf8", windowsHide: true, stdio: "ignore" });
  if (result.error?.code === "ENOENT") return false;
  if (result.error) throw result.error;
  return true;
}

function pythonCommand() {
  const candidates = process.platform === "win32" ? ["python", "python3"] : ["python3", "python"];
  const selected = candidates.find((candidate) => command(candidate));
  if (!selected) throw new Error("Python 3 is required to package the raw Sync server archive.");
  return selected;
}

function ensureSevenZip() {
  if (command("7z")) return;
  if (process.platform === "win32") {
    for (const path of ["C:/Program Files/7-Zip/7z.exe", "C:/Program Files (x86)/7-Zip/7z.exe"]) {
      if (!existsSync(path)) continue;
      const current = process.env.PATH ?? process.env.Path ?? "";
      process.env.PATH = `${dirname(path)};${current}`;
      if (command("7z")) return;
    }
  }
  throw new Error("7z is required to inspect the Windows Sync installer.");
}

function preflight(config) {
  pythonCommand();
  if (!command("tar")) throw new Error("tar is required to package the Sync distribution.");
  if (config.os === "windows") ensureSevenZip();
  if (config.os === "darwin") {
    if (!command("codesign", ["--version"])) throw new Error("codesign is required to verify the macOS Sync app.");
    if (!command("hdiutil", ["help"])) throw new Error("hdiutil is required to create the macOS Sync DMG.");
  }
  if (config.gui && !existsSync(join(repository, "server/manager/gui/node_modules/@tauri-apps/cli/tauri.js")))
    throw new Error("Install Sync GUI dependencies with `pnpm --dir server/manager/gui install --frozen-lockfile`.");
}

function sourceCommit() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], {
    cwd: repository,
    encoding: "utf8",
    windowsHide: true,
  });
  if (result.error) throw result.error;
  if (result.status !== 0 || !/^[a-f0-9]{40}$/.test(result.stdout.trim()))
    throw new Error("The local source commit could not be resolved.");
  return result.stdout.trim();
}

function resetDirectory(path) {
  const base = resolve(localOutputBase);
  const destination = resolve(path);
  const child = relative(base, destination);
  if (!child || child.startsWith(`..${sep}`) || child === ".." || !targets[child])
    throw new Error("Refusing to replace an unsafe local Sync output directory.");
  rmSync(destination, { recursive: true, force: true });
  mkdirSync(destination, { recursive: true });
}

function run(program, args) {
  const result = spawnSync(program, args, {
    cwd: repository,
    encoding: "utf8",
    windowsHide: true,
    stdio: "inherit",
    env: process.env,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${program} failed (${result.status}).`);
}

function packageRaw({ binary, target, version, output }) {
  run(pythonCommand(), [
    "server/sync/distribution/package.py",
    "--binary", binary,
    "--target", target,
    "--version", version,
    "--output", output,
  ]);
  const extension = target.includes("windows") ? ".zip" : ".tar.gz";
  const archive = join(output, `risunest-sync-server-${version}-${target}${extension}`);
  if (!existsSync(archive)) throw new Error("The raw Sync server archive was not created.");
  return archive;
}

function stageArtifacts(assets, output) {
  mkdirSync(output, { recursive: true });
  const names = new Set();
  return assets.map((asset) => {
    const name = basename(asset.path);
    if (names.has(name)) throw new Error(`Duplicate local Sync artifact name: ${name}.`);
    names.add(name);
    const destination = join(output, name);
    if (resolve(asset.path) !== resolve(destination)) copyFileSync(asset.path, destination);
    return { download: asset.download, path: resolve(destination) };
  });
}

const runtime = {
  cargoTargetDirectory,
  resetDirectory,
  preflight,
  sourceCommit,
  sourceCheck,
  fetchCloudflared,
  buildNativeSuite,
  packageRaw,
  packageNativeSuite,
};

export async function buildLocalSyncDistribution(options, dependencies = runtime) {
  const inputEnvironment = options.env ?? process.env;
  const effectiveEnvironment = {
    ...inputEnvironment,
    CARGO_TARGET_DIR: dependencies.cargoTargetDirectory(inputEnvironment),
  };
  const config = validateLocalSyncEnvironment({ ...options, env: effectiveEnvironment });
  const root = resolve(options.outputRoot ?? join(localOutputBase, config.target));
  const output = join(root, "output");
  dependencies.preflight(config);
  dependencies.resetDirectory(root);

  const version = JSON.parse(readFileSync(join(repository, "server/version.json"), "utf8")).version;
  const releaseInput = dependencies.sourceCheck({
    repository,
    product: "sync",
    tag: `sync-v${version}`,
    sourceCommit: dependencies.sourceCommit(),
    publishedAt: options.publishedAt ?? new Date().toISOString(),
    registryUrl: config.registryUrl,
  });
  const vendorDirectory = join(root, "vendor");
  const vendor = await dependencies.fetchCloudflared({
    target: config.vendorTarget,
    output: vendorDirectory,
  });

  const previousVendorInput = process.env.RISUNEST_CLOUDFLARED_INPUT;
  const previousRegistry = process.env.RISUNEST_DEFAULT_REGISTRY_URL;
  const previousCargoTarget = process.env.CARGO_TARGET_DIR;
  const previousPublicKey = process.env.RISUNEST_UPDATE_PUBLIC_KEY;
  process.env.RISUNEST_CLOUDFLARED_INPUT = join(vendorDirectory, "vendor.json");
  process.env.RISUNEST_DEFAULT_REGISTRY_URL = config.registryUrl;
  process.env.CARGO_TARGET_DIR = config.cargoTargetDir;
  process.env.RISUNEST_UPDATE_PUBLIC_KEY = config.publicKey;
  try {
    const nativeBuild = dependencies.buildNativeSuite({
      target: config.target,
      output: join(root, "native"),
      registryUrl: config.registryUrl,
      version: releaseInput.version,
    });
    const rawArchive = dependencies.packageRaw({
      binary: nativeBuild.daemon,
      target: config.target,
      version: releaseInput.version,
      output,
    });
    const packaged = dependencies.packageNativeSuite({
      nativeBuild,
      rawArchive,
      output,
      releaseInput,
      cloudflaredSha256: vendor.executableSha256,
    });
    const artifacts = stageArtifacts(packaged.assets, output);
    const result = {
      target: config.target,
      version: releaseInput.version,
      output: resolve(output),
      updatesEnabled: config.updatesEnabled,
      artifacts,
    };
    writeFileSync(join(output, "local-artifacts.json"), `${JSON.stringify(result, null, 2)}\n`);
    return result;
  } finally {
    if (previousVendorInput === undefined) delete process.env.RISUNEST_CLOUDFLARED_INPUT;
    else process.env.RISUNEST_CLOUDFLARED_INPUT = previousVendorInput;
    if (previousRegistry === undefined) delete process.env.RISUNEST_DEFAULT_REGISTRY_URL;
    else process.env.RISUNEST_DEFAULT_REGISTRY_URL = previousRegistry;
    if (previousCargoTarget === undefined) delete process.env.CARGO_TARGET_DIR;
    else process.env.CARGO_TARGET_DIR = previousCargoTarget;
    if (previousPublicKey === undefined) delete process.env.RISUNEST_UPDATE_PUBLIC_KEY;
    else process.env.RISUNEST_UPDATE_PUBLIC_KEY = previousPublicKey;
  }
}

export function parseLocalSyncArguments(args) {
  const { values } = parseArgs({ args, options: { target: { type: "string" }, "without-updates": { type: "boolean", default: false } } });
  if (!values.target) throw new Error("--target is required.");
  return { target: values.target, withoutUpdates: values["without-updates"] };
}

export function formatLocalSyncDistribution(result) {
  const lines = [`Local Sync distribution created in ${result.output}`];
  if (!result.updatesEnabled) lines.push("Updates are disabled for this development package. Update checks report update-not-configured.");
  return `${[...lines, ...result.artifacts.map(artifact => artifact.path)].join("\n")}\n`;
}

async function main() {
  const result = await buildLocalSyncDistribution(parseLocalSyncArguments(process.argv.slice(2)));
  process.stdout.write(formatLocalSyncDistribution(result));
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`);
    process.exitCode = 1;
  });
}
