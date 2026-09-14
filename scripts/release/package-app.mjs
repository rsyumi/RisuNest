import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { isDeepStrictEqual } from "node:util";
import { parseArgs, readJson, requireArg, writeJson } from "./common.mjs";
import { assertBinaryArchitecture } from "./platform/native.mjs";

function run(program, args, capture = false, cwd) {
  const child = spawnSync(program, args, {
    cwd,
    encoding: capture ? "utf8" : undefined,
    stdio: capture ? "pipe" : "inherit",
    windowsHide: true,
  });
  if (child.error) throw child.error;
  if (child.status !== 0)
    throw new Error(`${program} failed (${child.status})${capture ? `: ${child.stderr}` : "."}`);
  return child.stdout ?? "";
}

function walk(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? walk(path) : [path];
  });
}

function unique(directory, predicate, label) {
  const matches = walk(directory).filter(predicate);
  if (matches.length !== 1) throw new Error(`Expected one ${label}, found ${matches.length}.`);
  return matches[0];
}

export function validateIosArchiveEntries(entries) {
  if (!Array.isArray(entries) || entries.length === 0) throw new Error("IPA is empty.");
  const seen = new Set();
  const normalized = [];
  for (const entry of entries) {
    if (typeof entry !== "string" || !entry || entry.includes("\\") || entry.startsWith("/") || /^[A-Za-z]:/.test(entry))
      throw new Error(`IPA contains an unsafe path: ${entry}`);
    const parts = entry.split("/").filter(Boolean);
    if (parts.some((part) => part === "." || part === "..")) throw new Error(`IPA contains an unsafe path: ${entry}`);
    const key = `${parts.join("/")}${entry.endsWith("/") ? "/" : ""}`;
    if (seen.has(key)) throw new Error(`IPA contains a duplicate path: ${entry}`);
    seen.add(key);
    normalized.push(key);
  }
  if (normalized.some((entry) => entry !== "Payload/" && entry !== "Payload/RisuNest.app/" && !entry.startsWith("Payload/RisuNest.app/")))
    throw new Error("IPA must contain only the Payload/RisuNest.app bundle.");
  if (!seen.has("Payload/RisuNest.app/Info.plist")) throw new Error("IPA is missing Payload/RisuNest.app/Info.plist.");
  return normalized.sort();
}

function expectedIosDocuments(tauriConfig) {
  return tauriConfig.bundle.fileAssociations.map((association) => ({
    CFBundleTypeExtensions: association.ext,
    CFBundleTypeName: association.ext[0],
    CFBundleTypeRole: "Editor",
    LSHandlerRank: "Default",
  }));
}

function expectedIosUrls(tauriConfig) {
  return tauriConfig.plugins["deep-link"].mobile
    .filter((deepLink) => !deepLink.appLink)
    .map((deepLink) => ({
      CFBundleURLSchemes: deepLink.scheme.filter((scheme) => scheme !== "http" && scheme !== "https"),
      CFBundleURLName: deepLink.scheme[0],
    }));
}

export function assertIosBundleMetadata(info, releaseInput, tauriConfig, iosConfig) {
  const expected = {
    identifier: tauriConfig.identifier,
    platforms: ["iPhoneOS"],
    minimumOS: iosConfig.bundle.iOS.minimumSystemVersion,
    version: releaseInput.version,
    build: releaseInput.iosBuildNumber,
    documents: expectedIosDocuments(tauriConfig),
    urls: expectedIosUrls(tauriConfig),
  };
  const actual = {
    identifier: info.CFBundleIdentifier,
    platforms: info.CFBundleSupportedPlatforms,
    minimumOS: info.MinimumOSVersion,
    version: info.CFBundleShortVersionString,
    build: info.CFBundleVersion,
    documents: info.CFBundleDocumentTypes,
    urls: info.CFBundleURLTypes,
  };
  if (!isDeepStrictEqual(actual, expected))
    throw new Error(`IPA bundle metadata does not match the release configuration: ${JSON.stringify(actual)}`);
  if (typeof info.CFBundleExecutable !== "string" || !info.CFBundleExecutable)
    throw new Error("IPA bundle metadata does not name an executable.");
  return { ...actual, executable: info.CFBundleExecutable };
}

function binaryFormat(os) {
  return os === "windows" ? "pe" : os === "linux" ? "elf" : "macho";
}

function countBytes(bytes, needle) {
  let count = 0;
  for (let offset = 0; (offset = bytes.indexOf(needle, offset)) !== -1; offset += needle.length) count += 1;
  return count;
}

function assertBundleSelection(path, expected) {
  const bytes = readFileSync(path);
  const unknown = countBytes(bytes, Buffer.from("__TAURI_BUNDLE_TYPE_VAR_UNK"));
  const selected = countBytes(bytes, Buffer.from(`__TAURI_BUNDLE_TYPE_VAR_${expected}`));
  if (expected === "UNK" ? unknown === 0 : unknown !== 0 || selected === 0)
    throw new Error(`${path} does not have the ${expected} active Tauri bundle selection.`);
}

export function inspectMacAppRoot(root, arch, proofPath) {
  const apps = readdirSync(root, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && entry.name.endsWith(".app"));
  if (apps.length !== 1 || apps[0].name !== "RisuNest.app")
    throw new Error("macOS package must contain exactly one RisuNest.app bundle.");
  const executable = join(root, apps[0].name, "Contents/MacOS/RisuNest");
  assertBinaryArchitecture(executable, arch, "macho");
  copyFileSync(executable, proofPath);
  return proofPath;
}

function desktopAssets({ os, arch, bundleDirectory, binary, output }) {
  const proof = [{ path: resolve(binary), arch, format: binaryFormat(os), role: "app" }];
  if (os === "windows") {
    assertBundleSelection(binary, "UNK");
    const stage = mkdtempSync(join(output, ".app-zip-"));
    try {
      copyFileSync(binary, join(stage, "RisuNest.exe"));
      copyFileSync(resolve("LICENSE"), join(stage, "LICENSE"));
      const zip = join(output, "app.zip");
      run("tar", ["-a", "-cf", zip, "-C", stage, "."]);
      const installer = unique(bundleDirectory, (path) => path.endsWith("-setup.exe"), "NSIS installer");
      const installerStage = mkdtempSync(join(output, ".nsis-proof-"));
      run("7z", ["x", "-y", `-o${installerStage}`, installer]);
      const installedBinary = unique(installerStage, (path) => basename(path).toLowerCase() === "risunest.exe", "installed NSIS app executable");
      assertBinaryArchitecture(installedBinary, arch, "pe");
      assertBundleSelection(installedBinary, "NSS");
      const installedProof = join(output, "nsis-app-proof.exe");
      copyFileSync(installedBinary, installedProof);
      rmSync(installerStage, { recursive: true, force: true });
      return [
        { download: { product: "app", variant: "desktop", os, arch, format: "zip" }, path: zip, binaries: proof, checks: ["archive-layout"] },
        { download: { product: "app", variant: "desktop", os, arch, format: "nsis" }, path: installer, binaries: [{ path: installedProof, arch, format: "pe", role: "installed-app" }], checks: ["installer-layout", "bundle-marker-nsis"] },
      ];
    } finally {
      rmSync(stage, { recursive: true, force: true });
    }
  }
  if (os === "linux") {
    const deb = unique(bundleDirectory, (path) => path.endsWith(".deb"), "Debian package");
    const appimage = unique(bundleDirectory, (path) => path.endsWith(".AppImage"), "AppImage");
    const stage = mkdtempSync(join(output, ".deb-proof-"));
    run("dpkg-deb", ["--extract", deb, stage]);
    const installed = unique(stage, (path) => basename(path) === "RisuNest", "installed Debian app executable");
    assertBinaryArchitecture(installed, arch, "elf");
    assertBundleSelection(installed, "DEB");
    const debProof = join(output, "deb-app-proof");
    copyFileSync(installed, debProof);
    rmSync(stage, { recursive: true, force: true });
    assertBinaryArchitecture(appimage, arch, "elf");
    const appImageStage = mkdtempSync(join(output, ".appimage-proof-"));
    run(appimage, ["--appimage-extract"], false, appImageStage);
    const appImageBinary = unique(join(appImageStage, "squashfs-root"), (path) => basename(path) === "RisuNest", "packaged AppImage executable");
    assertBinaryArchitecture(appImageBinary, arch, "elf");
    assertBundleSelection(appImageBinary, "APP");
    const appImageProof = join(output, "appimage-app-proof");
    copyFileSync(appImageBinary, appImageProof);
    rmSync(appImageStage, { recursive: true, force: true });
    return [
      { download: { product: "app", variant: "desktop", os, arch, format: "deb" }, path: deb, binaries: [{ path: debProof, arch, format: "elf", role: "installed-app" }], checks: ["package-inspected", "bundle-marker-deb"] },
      { download: { product: "app", variant: "desktop", os, arch, format: "appimage" }, path: appimage, binaries: [{ path: appImageProof, arch, format: "elf", role: "packaged-app" }], checks: ["package-inspected", "bundle-marker-app"] },
    ];
  }
  const updater = unique(bundleDirectory, (path) => path.endsWith(".app.tar.gz"), "macOS updater archive");
  const entries = run("tar", ["-tzf", updater], true).trim().split(/\r?\n/).filter(Boolean);
  if (!entries.length || entries.some((entry) => !entry.startsWith("RisuNest.app/")))
    throw new Error("macOS updater archive must contain only RisuNest.app.");
  const stage = mkdtempSync(join(output, ".mac-proof-"));
  run("tar", ["-xzf", updater, "-C", stage]);
  const packagedProof = join(output, "macos-app-proof");
  inspectMacAppRoot(stage, arch, packagedProof);
  rmSync(stage, { recursive: true, force: true });
  const dmg = unique(bundleDirectory, (path) => path.endsWith(".dmg"), "DMG");
  const dmgStage = mkdtempSync(join(output, ".dmg-proof-"));
  let mounted = false;
  const dmgProof = join(output, "macos-dmg-app-proof");
  try {
    run("hdiutil", ["attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint", dmgStage, dmg]);
    mounted = true;
    inspectMacAppRoot(dmgStage, arch, dmgProof);
  } finally {
    if (mounted) run("hdiutil", ["detach", dmgStage]);
    rmSync(dmgStage, { recursive: true, force: true });
  }
  return [
    { download: { product: "app", variant: "desktop", os, arch, format: "dmg" }, path: dmg, binaries: [{ path: dmgProof, arch, format: "macho", role: "packaged-app" }], checks: ["app-bundle-layout", "dmg-mounted"] },
    { download: { product: "app", variant: "desktop", os, arch, format: "app.tar.gz" }, path: updater, binaries: [{ path: packagedProof, arch, format: "macho", role: "packaged-app" }], checks: ["whole-app"] },
  ];
}

function androidAssets({ arch, packagePath, releaseInput, output }) {
  if (arch !== "aarch64") throw new Error("The Android release is ARM64 only.");
  const verify = run("apksigner", ["verify", "--verbose", packagePath], true);
  if (!/Verified using v[234] scheme.*true/i.test(verify))
    throw new Error("APK does not have a modern Android signature.");
  run("zipalign", ["-c", "-P", "16", "4", packagePath]);
  const badge = run("aapt", ["dump", "badging", packagePath], true);
  if (!badge.includes(`versionName='${releaseInput.version}'`) || !badge.includes(`versionCode='${releaseInput.mobileBuildNumber}'`))
    throw new Error("APK version metadata does not match release input.");
  if (!/native-code:.*'arm64-v8a'/.test(badge) || /native-code:.*'(x86|x86_64|armeabi-v7a)'/.test(badge))
    throw new Error("APK ABI set is not ARM64-only.");
  const stage = mkdtempSync(join(output, ".apk-proof-"));
  run("unzip", ["-q", packagePath, "lib/arm64-v8a/*", "-d", stage]);
  const libraries = walk(join(stage, "lib/arm64-v8a"));
  if (!libraries.length) throw new Error("APK does not contain an ARM64 native library.");
  for (const library of libraries) assertBinaryArchitecture(library, arch, "elf");
  const proofBinary = join(output, "android-native-proof.so");
  copyFileSync(libraries[0], proofBinary);
  rmSync(stage, { recursive: true, force: true });
  return [{
    download: { product: "app", variant: "mobile", os: "android", arch, format: "apk" },
    path: resolve(packagePath),
    binaries: [{ path: proofBinary, arch, format: "elf", role: "packaged-native-library" }],
    checks: ["abi-arm64", "apk-signed", "version-match", "zipaligned-16k"],
  }];
}

function iosAssets({ arch, packagePath, releaseInput, output, tauriConfig, iosConfig }) {
  if (arch !== "aarch64") throw new Error("The iOS release is ARM64 only.");
  const stage = mkdtempSync(join(output, ".ipa-proof-"));
  try {
    const entries = validateIosArchiveEntries(run("unzip", ["-Z1", packagePath], true).trim().split(/\r?\n/).filter(Boolean));
    run("unzip", ["-q", packagePath, "-d", stage]);
    const apps = readdirSync(join(stage, "Payload"), { withFileTypes: true }).filter((entry) => entry.isDirectory() && entry.name.endsWith(".app"));
    if (apps.length !== 1 || apps[0].name !== "RisuNest.app") throw new Error("IPA must contain exactly one RisuNest.app bundle.");
    const app = join(stage, "Payload", apps[0].name);
    const plist = join(app, "Info.plist");
    const info = JSON.parse(run("plutil", ["-convert", "json", "-o", "-", plist], true));
    const metadata = assertIosBundleMetadata(info, releaseInput, tauriConfig, iosConfig);
    const binary = join(app, metadata.executable);
    if (!entries.includes(`Payload/RisuNest.app/${metadata.executable}`)) throw new Error("IPA is missing its declared executable.");
    const platform = run("xcrun", ["vtool", "-show-build", binary], true);
    if (!/platform IOS\b/.test(platform) || /IOSSIMULATOR/.test(platform))
      throw new Error("IPA binary is not an iPhoneOS device build.");
    if (existsSync(join(app, "_CodeSignature")) || existsSync(join(app, "embedded.mobileprovision")))
      throw new Error("Unsigned IPA contains signing material.");
    const signing = spawnSync("codesign", ["-d", binary], { stdio: "ignore", windowsHide: true });
    if (signing.status === 0) throw new Error("IPA executable is code signed.");
    const proofBinary = join(output, "ios-app-proof");
    copyFileSync(binary, proofBinary);
    writeJson(join(output, "ios-package-inventory.json"), {
      schema: "risunest-ios-package-inventory/v1",
      entries,
      metadata,
    });
    return [{
      download: { product: "app", variant: "mobile", os: "ios", arch, format: "ipa" },
      path: resolve(packagePath),
      binaries: [{ path: proofBinary, arch, format: "macho", role: "app" }],
      checks: ["iphoneos", "payload-single-app", "version-match", "archive-inventory", "bundle-metadata"],
    }];
  } finally {
    rmSync(stage, { recursive: true, force: true });
  }
}

export function packageApp(options) {
  const output = resolve(options.output);
  mkdirSync(output, { recursive: true });
  let assets;
  if (options.kind === "desktop") assets = desktopAssets({ ...options, output });
  else if (options.kind === "android") assets = androidAssets(options);
  else if (options.kind === "ios") assets = iosAssets({
    ...options,
    output,
    tauriConfig: options.tauriConfig ?? readJson(resolve("src-tauri/tauri.conf.json")),
    iosConfig: options.iosConfig ?? readJson(resolve("src-tauri/tauri.ios.conf.json")),
  });
  else throw new Error(`Unsupported app package kind: ${options.kind}.`);
  const manifest = join(output, "assets.json");
  writeJson(manifest, assets);
  return { assets, manifest };
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const releaseInput = readJson(requireArg(args, "release-input"));
  const result = packageApp({
    kind: requireArg(args, "kind"),
    os: args.os,
    arch: requireArg(args, "arch"),
    bundleDirectory: args["bundle-dir"] ? resolve(args["bundle-dir"]) : undefined,
    binary: args.binary ? resolve(args.binary) : undefined,
    packagePath: args.package ? resolve(args.package) : undefined,
    tauriConfig: args["tauri-config"] ? readJson(resolve(args["tauri-config"])) : undefined,
    iosConfig: args["ios-config"] ? readJson(resolve(args["ios-config"])) : undefined,
    output: requireArg(args, "output"),
    releaseInput,
  });
  process.stdout.write(`${JSON.stringify({ manifest: result.manifest })}\n`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
