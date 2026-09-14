import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const manager = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const gui = join(manager, "gui");
const repo = resolve(manager, "../..");

function parse(argv) {
  const result = {};
  for (let index = 0; index < argv.length; index += 2) {
    if (!argv[index]?.startsWith("--") || argv[index + 1] === undefined)
      throw new Error(`Invalid argument near ${argv[index] ?? "end"}.`);
    result[argv[index].slice(2)] = argv[index + 1];
  }
  return result;
}

function required(args, name) {
  if (!args[name]) throw new Error(`--${name} is required.`);
  return args[name];
}

function run(program, args, cwd = repo) {
  const child = spawnSync(program, args, { cwd, stdio: "inherit", windowsHide: true, env: process.env });
  if (child.error) throw child.error;
  if (child.status !== 0) throw new Error(`${program} failed (${child.status}).`);
}

function rustHost() {
  const result = spawnSync("rustc", ["-vV"], { encoding: "utf8", windowsHide: true });
  if (result.status !== 0) throw new Error("rustc is unavailable.");
  const host = /^host: (.+)$/m.exec(result.stdout)?.[1];
  if (!host) throw new Error("Rust host target is unavailable.");
  return host;
}

export function buildNativeSuite({ target, output, registryUrl, version }) {
  if (!process.env.CARGO_TARGET_DIR)
    throw new Error("Set CARGO_TARGET_DIR to the repository shared target directory.");
  if (rustHost() !== target) throw new Error("Sync release binaries must be built on their native host target.");
  const registry = new URL(registryUrl);
  if (registry.protocol !== "https:" || registry.username || registry.password || registry.search || registry.hash)
    throw new Error("Registry must be an HTTPS base URL without credentials, query, or fragment.");
  const extension = target.includes("windows") ? ".exe" : "";
  const targetRelease = join(process.env.CARGO_TARGET_DIR, target, "release");
  const destination = resolve(output);
  mkdirSync(destination, { recursive: true });
  const build = (manifest, extra = []) =>
    run("cargo", ["build", "--release", "--locked", "--target", target, "--manifest-path", manifest, ...extra]);

  build("server/sync/Cargo.toml");
  const daemon = join(destination, `risunest-sync-server-console${extension}`);
  copyFileSync(join(targetRelease, `risunest-sync-server${extension}`), daemon);
  build("server/manager/Cargo.toml");
  const managerBinary = join(destination, `risunest-sync-manager${extension}`);
  copyFileSync(join(targetRelease, `risunest-sync-manager${extension}`), managerBinary);
  let packagedDaemon = daemon;
  if (target.includes("windows")) {
    build("server/sync/Cargo.toml", ["--features", "windows-background"]);
    packagedDaemon = join(destination, "risunest-sync-server-background.exe");
    copyFileSync(join(targetRelease, "risunest-sync-server.exe"), packagedDaemon);
  }

  const vendorInputPath = process.env.RISUNEST_CLOUDFLARED_INPUT;
  if (!vendorInputPath) throw new Error("Set RISUNEST_CLOUDFLARED_INPUT to vendor.json.");
  const vendor = JSON.parse(readFileSync(vendorInputPath, "utf8"));
  const cloudflared = vendor.executable;
  const license = vendor.license;
  if (!existsSync(cloudflared) || !existsSync(license)) throw new Error("Verified cloudflared inputs are missing.");
  let guiBinary = null;
  if (!target.includes("linux")) {
    const binaries = join(gui, "src-tauri/binaries");
    mkdirSync(binaries, { recursive: true });
    copyFileSync(packagedDaemon, join(binaries, `risunest-sync-server-${target}${extension}`));
    copyFileSync(managerBinary, join(binaries, `risunest-sync-manager-${target}${extension}`));
    copyFileSync(cloudflared, join(binaries, `cloudflared-${target}${extension}`));
    copyFileSync(license, join(binaries, "CLOUDFLARED-LICENSE"));
    const cli = join(gui, "node_modules/@tauri-apps/cli/tauri.js");
    if (!existsSync(cli)) throw new Error("Install the Sync GUI dependencies before building.");
    const config = JSON.stringify({ version, bundle: { active: false } });
    run(process.execPath, [cli, "build", "--target", target, "--no-bundle", "--config", config, "--", "--locked"], gui);
    guiBinary = join(destination, `risunest-sync-gui${extension}`);
    copyFileSync(join(targetRelease, `risunest-sync-gui${extension}`), guiBinary);
  }
  return {
    target,
    daemon,
    packagedDaemon,
    manager: managerBinary,
    gui: guiBinary,
    cloudflared,
    license,
    cloudflaredArch: vendor.actualArch,
    cloudflaredSourceUrl: vendor.sourceUrl,
    cloudflaredSourceSha256: vendor.sourceSha256,
    cloudflaredSha256: vendor.executableSha256,
    registryUrl: registry.href,
  };
}

function main() {
  const args = parse(process.argv.slice(2));
  const result = buildNativeSuite({
    target: required(args, "target"),
    output: required(args, "output"),
    registryUrl: required(args, "registry-url"),
    version: required(args, "version"),
  });
  const manifest = join(resolve(args.output), "native-build.json");
  writeFileSync(manifest, `${JSON.stringify(result, null, 2)}\n`);
  process.stdout.write(`${JSON.stringify({ manifest })}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
