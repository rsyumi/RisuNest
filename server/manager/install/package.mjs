import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  cpSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

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

function required(value, label) {
  if (!value) throw new Error(`${label} is required.`);
  return resolve(value);
}

function run(program, args, cwd = repo, capture = false) {
  const child = spawnSync(program, args, {
    cwd,
    encoding: capture ? "utf8" : undefined,
    stdio: capture ? "pipe" : "inherit",
    windowsHide: true,
    env: process.env,
  });
  if (child.error) throw child.error;
  if (child.status !== 0)
    throw new Error(`${program} failed (${child.status})${capture ? `: ${child.stderr}` : "."}`);
  return child.stdout ?? "";
}

function checksum(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function countBytes(bytes, needle) {
  let count = 0;
  for (let offset = 0; (offset = bytes.indexOf(needle, offset)) !== -1; offset += needle.length) count += 1;
  return count;
}

function assertBundleSelection(path, selection) {
  const bytes = readFileSync(path);
  const unknown = countBytes(bytes, Buffer.from("__TAURI_BUNDLE_TYPE_VAR_UNK"));
  const selected = countBytes(bytes, Buffer.from(`__TAURI_BUNDLE_TYPE_VAR_${selection}`));
  if (selection === "UNK" ? unknown === 0 : unknown !== 0 || selected === 0)
    throw new Error(`${path} does not have the ${selection} active Tauri bundle selection.`);
}

function normalizeOwnedPath(value) {
  if (typeof value !== "string" || !value || value.includes("\\") || value.startsWith("/") || value.split("/").some((part) => !part || part === "." || part === ".."))
    throw new Error(`Invalid bundle inventory path: ${value}.`);
  return value;
}

function validateOwnedInventory(root, markerPath, releaseInput, build, vendorSha256 = build.cloudflaredSha256) {
  const inventory = JSON.parse(readFileSync(markerPath, "utf8"));
  const keys = ["schema", "product", "variant", "version", "protocolId", "storeFormatId", "files", "vendor"];
  if (JSON.stringify(Object.keys(inventory).sort()) !== JSON.stringify(keys.sort()))
    throw new Error("Sync bundle inventory has unexpected fields.");
  if (
    inventory.schema !== "risunest-sync-bundle/v1" ||
    inventory.product !== "sync" ||
    inventory.variant !== "managed" ||
    inventory.version !== releaseInput.version ||
    inventory.protocolId !== releaseInput.compatibility.protocolId ||
    inventory.storeFormatId !== releaseInput.compatibility.storeFormatId
  ) throw new Error("Sync bundle inventory identity mismatch.");
  const marker = resolve(markerPath);
  const actual = filesUnder(root)
    .filter((path) => resolve(path) !== marker)
    .filter((path) => !relative(root, path).split(/[\\/]/)[0].startsWith("$"))
    .map((path) => {
      if (!lstatSync(path).isFile()) throw new Error(`Bundle entry is not a regular file: ${path}.`);
      return relative(root, path).replace(/\\/g, "/");
    })
    .sort();
  const listed = inventory.files.map(normalizeOwnedPath);
  if (new Set(listed).size !== listed.length || JSON.stringify([...listed].sort()) !== JSON.stringify(actual))
    throw new Error("Sync bundle inventory does not exactly list its regular files.");
  if (!Array.isArray(inventory.vendor) || inventory.vendor.length !== 1)
    throw new Error("Sync bundle inventory must contain one cloudflared record.");
  const vendor = inventory.vendor[0];
  const vendorKeys = ["name", "version", "os", "arch", "sha256", "path"];
  if (JSON.stringify(Object.keys(vendor).sort()) !== JSON.stringify(vendorKeys.sort()))
    throw new Error("Sync bundle vendor record has unexpected fields.");
  const vendorPath = normalizeOwnedPath(vendor.path);
  if (
    vendor.name !== "cloudflared" ||
    vendor.version !== releaseInput.vendorInput.version ||
    vendor.arch !== build.cloudflaredArch ||
    vendor.sha256 !== vendorSha256 ||
    !listed.includes(vendorPath) ||
    checksum(join(root, ...vendorPath.split("/"))) !== vendor.sha256
  ) throw new Error("Sync bundle cloudflared record does not match its packaged file.");
  return inventory;
}

function filesUnder(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? filesUnder(path) : [path];
  });
}

function directoriesUnder(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? [path, ...directoriesUnder(path)] : [];
  });
}

function unique(directory, predicate, label) {
  const matches = filesUnder(directory).filter(predicate);
  if (matches.length !== 1) throw new Error(`Expected one ${label}, found ${matches.length}.`);
  return matches[0];
}

function writeOwnedInventory(stage, releaseInput, files, vendorPath, build, vendorSha256 = build.cloudflaredSha256) {
  const name = "risunest-sync-bundle.json";
  writeFileSync(
    join(stage, name),
    `${JSON.stringify({
      schema: "risunest-sync-bundle/v1",
      product: "sync",
      variant: "managed",
      version: releaseInput.version,
      protocolId: releaseInput.compatibility.protocolId,
      storeFormatId: releaseInput.compatibility.storeFormatId,
      files: [...files].sort(),
      vendor: [{ name: "cloudflared", version: releaseInput.vendorInput.version,
        os: build.target.includes("windows") ? "windows" : build.target.includes("linux") ? "linux" : "darwin",
        arch: build.cloudflaredArch, sha256: vendorSha256, path: vendorPath }],
    }, null, 2)}\n`,
  );
}

function copyRuntime(stage, build, releaseInput, windows) {
  const names = windows
    ? [
        [build.gui, "risunest-sync-gui.exe"],
        [build.packagedDaemon, "risunest-sync-server.exe"],
        [build.manager, "risunest-sync-manager.exe"],
        [build.cloudflared, "cloudflared.exe"],
        [build.license, "CLOUDFLARED-LICENSE"],
      ]
    : [
        [build.daemon, "risunest-sync-server"],
        [build.manager, "risunest-sync-manager"],
        [build.cloudflared, "cloudflared"],
        [build.license, "CLOUDFLARED-LICENSE"],
        [join(manager, "install/install.sh"), "install.sh"],
      ];
  for (const [source, name] of names) {
    copyFileSync(source, join(stage, name));
    if (!windows && name !== "CLOUDFLARED-LICENSE") chmodSync(join(stage, name), 0o755);
  }
  writeOwnedInventory(stage, releaseInput, names.map(([, name]) => name), windows ? "cloudflared.exe" : "cloudflared", build);
}

function archiveEntries(stage) {
  const entries = readdirSync(stage).sort();
  if (!entries.length || entries.some((entry) => entry === "." || entry === ".."))
    throw new Error("Managed package staging directory is invalid.");
  return entries;
}

function macAppAtRoot(root) {
  const apps = readdirSync(root, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && entry.name.endsWith(".app"));
  if (apps.length !== 1 || apps[0].name !== "RisuNest Sync.app")
    throw new Error("Managed macOS package must contain exactly one RisuNest Sync.app.");
  return join(root, apps[0].name);
}

function inspectMacBundle(app, releaseInput, build, vendorSha256, destination, prefix) {
  run("codesign", ["--verify", "--deep", "--strict", app]);
  validateOwnedInventory(app, join(app, "Contents/Resources/risunest-sync-bundle.json"), releaseInput, build, vendorSha256);
  const definitions = [
    ["risunest-sync-gui", "gui"],
    ["risunest-sync-server", "daemon"],
    ["risunest-sync-manager", "manager"],
  ];
  const binaries = definitions.map(([name, role]) => {
    const path = join(destination, `${prefix}-${name}-proof`);
    copyFileSync(unique(app, (candidate) => basename(candidate) === name, `packaged ${name}`), path);
    return { path, role };
  });
  const cloudflared = join(destination, `${prefix}-cloudflared-proof`);
  copyFileSync(unique(app, (candidate) => basename(candidate) === "cloudflared", "packaged cloudflared"), cloudflared);
  if (checksum(cloudflared) !== vendorSha256) throw new Error("Packaged cloudflared changed after bundle signing.");
  return { binaries, cloudflared };
}

export function packageNativeSuite({ nativeBuild, rawArchive, output, releaseInput, cloudflaredSha256 }) {
  const build = typeof nativeBuild === "string" ? JSON.parse(readFileSync(nativeBuild, "utf8")) : nativeBuild;
  const target = build.target;
  const destination = resolve(output);
  mkdirSync(destination, { recursive: true });
  if (!process.env.CARGO_TARGET_DIR)
    throw new Error("Set CARGO_TARGET_DIR to the repository shared target directory.");
  const requiredInputs = target.includes("linux")
    ? ["daemon", "manager", "cloudflared", "license"]
    : ["daemon", "packagedDaemon", "manager", "gui", "cloudflared", "license"];
  for (const name of requiredInputs) {
    if (!existsSync(build[name])) throw new Error(`Native build input ${name} is missing.`);
  }
  if (!/^[a-f0-9]{64}$/.test(cloudflaredSha256) || checksum(build.cloudflared) !== cloudflaredSha256)
    throw new Error("Cloudflared executable checksum mismatch.");
  const architecture = target.startsWith("aarch64") ? "aarch64" : "x86_64";
  const os = target.includes("windows") ? "windows" : target.includes("linux") ? "linux" : "darwin";
  const rawFormat = os === "windows" ? "zip" : "tar.gz";
  const result = [{
    download: { product: "sync", variant: "raw", os, arch: architecture, format: rawFormat },
    path: required(rawArchive, "Raw daemon archive"),
    binaries: [{ path: build.daemon, arch: architecture, format: os === "windows" ? "pe" : os === "linux" ? "elf" : "macho", role: "daemon" }],
    checks: ["archive-layout"],
  }];
  const temporary = mkdtempSync(join(destination, ".sync-package-"));
  try {
    if (os === "linux") {
      copyRuntime(temporary, build, releaseInput, false);
      validateOwnedInventory(temporary, join(temporary, "risunest-sync-bundle.json"), releaseInput, build);
      const archive = join(destination, "managed.tar.gz");
      run("tar", ["-czf", archive, "-C", temporary, ...archiveEntries(temporary)]);
      result.push({
        download: { product: "sync", variant: "managed", os, arch: architecture, format: "tar.gz" },
        path: archive,
        binaries: [
          { path: build.daemon, arch: architecture, format: "elf", role: "daemon" },
          { path: build.manager, arch: architecture, format: "elf", role: "manager" },
        ],
        vendor: [{ path: build.cloudflared, arch: build.cloudflaredArch, format: "elf", sourceUrl: build.cloudflaredSourceUrl, sourceSha256: build.cloudflaredSourceSha256 }],
        checks: ["archive-layout", "bundle-inventory"],
      });
    } else {
      const cli = join(gui, "node_modules/@tauri-apps/cli/tauri.js");
      if (!existsSync(cli)) throw new Error("Install Sync GUI dependencies before packaging.");
      if (os === "windows") {
        const owned = ["risunest-sync-gui.exe", "risunest-sync-server.exe", "risunest-sync-manager.exe", "cloudflared.exe", "CLOUDFLARED-LICENSE"];
        writeOwnedInventory(join(gui, "src-tauri/binaries"), releaseInput, owned, "cloudflared.exe", build);
      }
      const config = JSON.stringify({
        version: releaseInput.version,
        bundle: {
          active: true,
          createUpdaterArtifacts: false,
          resources: os === "windows"
            ? { "binaries/CLOUDFLARED-LICENSE": "CLOUDFLARED-LICENSE", "binaries/risunest-sync-bundle.json": "risunest-sync-bundle.json" }
            : ["binaries/CLOUDFLARED-LICENSE"],
        },
      });
      const bundles = os === "windows" ? "nsis" : "app";
      run(process.execPath, [cli, "build", "--target", target, "--config", "src-tauri/tauri.bundle.conf.json", "--config", config, "--bundles", bundles, "--", "--locked"], gui);
      const bundleRoot = join(process.env.CARGO_TARGET_DIR, target, "release/bundle");
      if (os === "windows") {
        copyRuntime(temporary, build, releaseInput, true);
        assertBundleSelection(join(temporary, "risunest-sync-gui.exe"), "UNK");
        validateOwnedInventory(temporary, join(temporary, "risunest-sync-bundle.json"), releaseInput, build);
        const archive = join(destination, "managed.zip");
        run("tar", ["-a", "-cf", archive, "-C", temporary, ...archiveEntries(temporary)]);
        const installer = unique(bundleRoot, (path) => basename(path).startsWith(`RisuNest Sync_${releaseInput.version}_`) && path.endsWith("-setup.exe"), "NSIS installer");
        const installerStage = mkdtempSync(join(destination, ".sync-nsis-proof-"));
        run("7z", ["x", "-y", `-o${installerStage}`, installer]);
        const installedMarker = unique(installerStage, (path) => basename(path) === "risunest-sync-bundle.json", "installed Sync bundle marker");
        const installedRoot = dirname(installedMarker);
        validateOwnedInventory(installedRoot, installedMarker, releaseInput, build);
        const installedGui = join(installedRoot, "risunest-sync-gui.exe");
        const installedDaemon = join(installedRoot, "risunest-sync-server.exe");
        const installedManager = join(installedRoot, "risunest-sync-manager.exe");
        assertBundleSelection(installedGui, "NSS");
        const installerProofs = [
          [installedGui, join(destination, "nsis-gui-proof.exe"), "gui"],
          [installedDaemon, join(destination, "nsis-daemon-proof.exe"), "daemon-background"],
          [installedManager, join(destination, "nsis-manager-proof.exe"), "manager"],
        ];
        for (const [source, proof] of installerProofs) copyFileSync(source, proof);
        rmSync(installerStage, { recursive: true, force: true });
        result.push(
          {
            download: { product: "sync", variant: "managed", os, arch: architecture, format: "nsis" },
            path: installer,
            binaries: installerProofs.map(([, path, role]) => ({ path, arch: architecture, format: "pe", role })),
            vendor: [{ path: build.cloudflared, arch: build.cloudflaredArch, format: "pe", sourceUrl: build.cloudflaredSourceUrl, sourceSha256: build.cloudflaredSourceSha256 }],
            checks: ["installer-layout"],
          },
          {
            download: { product: "sync", variant: "managed", os, arch: architecture, format: "zip" },
            path: archive,
            binaries: [
              { path: build.gui, arch: architecture, format: "pe", role: "gui" },
              { path: build.packagedDaemon, arch: architecture, format: "pe", role: "daemon-background" },
              { path: build.manager, arch: architecture, format: "pe", role: "manager" },
            ],
            checks: ["archive-layout", "bundle-inventory"],
          },
        );
      } else {
        const app = directoriesUnder(bundleRoot).find((path) => basename(path) === "RisuNest Sync.app");
        if (!app) throw new Error("Expected one RisuNest Sync.app bundle.");
        const resources = join(app, "Contents/Resources");
        mkdirSync(resources, { recursive: true });
        const markerPath = join(resources, "risunest-sync-bundle.json");
        run("codesign", ["--force", "--deep", "--sign", "-", app]);
        const packagedCloudflared = unique(app, (path) => basename(path) === "cloudflared", "packaged cloudflared");
        const packagedVendorSha256 = checksum(packagedCloudflared);
        const owned = filesUnder(app)
          .filter((path) => path !== markerPath)
          .map((path) => relative(app, path).replace(/\\/g, "/"));
        writeOwnedInventory(resources, releaseInput, owned, relative(app, packagedCloudflared).replace(/\\/g, "/"), build, packagedVendorSha256);
        run("codesign", ["--force", "--sign", "-", app]);
        run("codesign", ["--verify", "--deep", "--strict", app]);
        if (checksum(packagedCloudflared) !== packagedVendorSha256)
          throw new Error("Outer app signing changed packaged cloudflared bytes.");
        validateOwnedInventory(app, markerPath, releaseInput, build, packagedVendorSha256);
        const updater = join(destination, "managed.app.tar.gz");
        run("tar", ["-czf", updater, "-C", dirname(app), basename(app)]);
        const dmg = join(destination, "managed.dmg");
        const dmgSource = mkdtempSync(join(destination, ".sync-dmg-source-"));
        cpSync(app, join(dmgSource, "RisuNest Sync.app"), { recursive: true, verbatimSymlinks: true });
        run("hdiutil", ["create", "-volname", "RisuNest Sync", "-srcfolder", dmgSource, "-ov", "-format", "UDZO", dmg]);
        rmSync(dmgSource, { recursive: true, force: true });
        const entries = run("tar", ["-tzf", updater], repo, true).trim().split(/\r?\n/).filter(Boolean);
        if (!entries.length || entries.some((entry) => !entry.startsWith("RisuNest Sync.app/")))
          throw new Error("macOS updater archive must contain only RisuNest Sync.app.");
        const updaterStage = mkdtempSync(join(destination, ".sync-updater-proof-"));
        run("tar", ["-xzf", updater, "-C", updaterStage]);
        const updaterProof = inspectMacBundle(macAppAtRoot(updaterStage), releaseInput, build, packagedVendorSha256, destination, "updater");
        rmSync(updaterStage, { recursive: true, force: true });
        const dmgStage = mkdtempSync(join(destination, ".sync-dmg-proof-"));
        let mounted = false;
        let dmgProof;
        try {
          run("hdiutil", ["attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint", dmgStage, dmg]);
          mounted = true;
          dmgProof = inspectMacBundle(macAppAtRoot(dmgStage), releaseInput, build, packagedVendorSha256, destination, "dmg");
        } finally {
          if (mounted) run("hdiutil", ["detach", dmgStage]);
          rmSync(dmgStage, { recursive: true, force: true });
        }
        result.push(
          { download: { product: "sync", variant: "managed", os, arch: architecture, format: "dmg" }, path: dmg, binaries: dmgProof.binaries.map((proof) => ({ ...proof, arch: architecture, format: "macho" })), vendor: [{ path: dmgProof.cloudflared, arch: build.cloudflaredArch, format: "macho", sourceUrl: build.cloudflaredSourceUrl, sourceSha256: build.cloudflaredSourceSha256 }], checks: ["app-bundle-layout", "codesign-verified", "bundle-inventory"] },
          { download: { product: "sync", variant: "managed", os, arch: architecture, format: "app.tar.gz" }, path: updater, binaries: updaterProof.binaries.map((proof) => ({ ...proof, arch: architecture, format: "macho" })), checks: ["whole-app", "codesign-verified", "bundle-inventory"] },
        );
      }
    }
    const manifest = join(destination, "assets.json");
    writeFileSync(manifest, `${JSON.stringify(result, null, 2)}\n`);
    return { assets: result, manifest };
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

function main() {
  const args = parse(process.argv.slice(2));
  const result = packageNativeSuite({
    nativeBuild: required(args["native-build"], "Native build manifest"),
    rawArchive: required(args["raw-archive"], "Raw daemon archive"),
    output: required(args.output, "Output directory"),
    releaseInput: JSON.parse(readFileSync(required(args["release-input"], "Release input"), "utf8")),
    cloudflaredSha256: args["cloudflared-sha256"],
  });
  process.stdout.write(`${JSON.stringify({ manifest: result.manifest })}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
