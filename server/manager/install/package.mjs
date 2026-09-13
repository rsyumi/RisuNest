import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  readFileSync,
  copyFileSync,
  mkdirSync,
  chmodSync,
  writeFileSync,
  existsSync,
} from "node:fs";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

// Native host builds only. Feed verified vendor binaries, never download at install time.
const manager = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const gui = join(manager, "gui");
const repo = resolve(manager, "../..");
const target = process.env.CARGO_TARGET_DIR;
if (!target)
  throw new Error(
    "Set CARGO_TARGET_DIR to the shared repository Cargo target.",
  );
const cloudflared = process.env.RISUNEST_CLOUDFLARED;
const digest = process.env.RISUNEST_CLOUDFLARED_SHA256?.toLowerCase();
const license = process.env.RISUNEST_CLOUDFLARED_LICENSE;
if (!cloudflared || !license || !/^[a-f0-9]{64}$/.test(digest ?? ""))
  throw new Error(
    "Set RISUNEST_CLOUDFLARED, RISUNEST_CLOUDFLARED_SHA256, and RISUNEST_CLOUDFLARED_LICENSE from the verified vendor release.",
  );
if (
  createHash("sha256").update(readFileSync(cloudflared)).digest("hex") !==
  digest
)
  throw new Error("Cloudflared checksum mismatch.");
if (!process.env.RISUNEST_DEFAULT_REGISTRY_URL)
  throw new Error("Set the production RISUNEST_DEFAULT_REGISTRY_URL.");
const registry = new URL(process.env.RISUNEST_DEFAULT_REGISTRY_URL);
if (
  registry.protocol !== "https:" ||
  registry.username ||
  registry.password ||
  registry.search ||
  registry.hash
)
  throw new Error(
    "Registry must be an HTTPS base URL without credentials, query or fragment.",
  );
function run(program, args, cwd = repo) {
  const child = spawnSync(program, args, {
    cwd,
    stdio: "inherit",
    windowsHide: true,
    env: process.env,
  });
  if (child.error) throw child.error;
  if (child.status !== 0)
    throw new Error(`${program} failed (${child.status}).`);
}
const version = spawnSync("rustc", ["-vV"], {
  encoding: "utf8",
  windowsHide: true,
});
if (version.status !== 0) throw new Error("rustc unavailable");
const triple = /^host: (.+)$/m.exec(version.stdout)?.[1];
if (!triple) throw new Error("Rust host target unavailable");
if (process.env.CARGO_BUILD_TARGET && process.env.CARGO_BUILD_TARGET !== triple)
  throw new Error("Packaging requires the native host target.");
const ext = process.platform === "win32" ? ".exe" : "";
for (const manifest of [
  "server/sync/Cargo.toml",
  "server/manager/Cargo.toml",
]) {
  const args = ["build", "--release", "--locked", "--manifest-path", manifest];
  if (process.platform === "win32" && manifest === "server/sync/Cargo.toml")
    args.push("--features", "windows-background");
  run("cargo", args);
}
const binaries = join(gui, "src-tauri/binaries");
mkdirSync(binaries, { recursive: true });
for (const name of [
  "risunest-sync-server",
  "risunest-sync-manager",
  "cloudflared",
]) {
  const source =
    name === "cloudflared" ? cloudflared : join(target, "release", name + ext);
  const destination = join(binaries, `${name}-${triple}${ext}`);
  copyFileSync(source, destination);
  if (!ext) chmodSync(destination, 0o755);
}
copyFileSync(license, join(binaries, "CLOUDFLARED-LICENSE"));
if (process.platform === "linux") {
  const output = join(manager, "release", `risunest-sync-${triple}`);
  mkdirSync(output, { recursive: true });
  for (const name of [
    "risunest-sync-server",
    "risunest-sync-manager",
    "cloudflared",
  ]) {
    copyFileSync(join(binaries, `${name}-${triple}`), join(output, name));
    chmodSync(join(output, name), 0o755);
  }
  copyFileSync(license, join(output, "CLOUDFLARED-LICENSE"));
  copyFileSync(join(manager, "install/install.sh"), join(output, "install.sh"));
  chmodSync(join(output, "install.sh"), 0o755);
  run("tar", [
    "-czf",
    `${output}.tar.gz`,
    "-C",
    dirname(output),
    output.split("/").at(-1),
  ]);
} else {
  const localModules = existsSync(
    join(gui, "node_modules/@tauri-apps/cli/tauri.js"),
  )
    ? join(gui, "node_modules")
    : join(repo, "node_modules");
  run(
    process.execPath,
    [
      join(localModules, "@tauri-apps/cli/tauri.js"),
      "build",
      "--config",
      "src-tauri/tauri.bundle.conf.json",
      "--bundles",
      process.platform === "win32" ? "nsis" : "app,dmg",
    ],
    gui,
  );
}
writeFileSync(
  join(binaries, "build-inputs.json"),
  JSON.stringify(
    { triple, cloudflaredSha256: digest, registry: registry.href },
    null,
    2,
  ) + "\n",
);
