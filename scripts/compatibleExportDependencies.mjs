/** Isolated libraries for executing the pinned reference readers and writers. */
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import {
  references,
  contractDirectory,
  verifySourceFilePins,
  verifyReferenceRevision,
} from "./compatibleExportContract.mjs";

const libraries = {
  risuai: ["msgpackr", "fflate", "rfdc"],
  pocket: ["msgpackr", "fflate", "rfdc", "better-sqlite3"],
};
const cacheRoot = fileURLToPath(
  new URL("../.tmp/test-reference-dependencies/", import.meta.url),
);

export function referenceDependencyDirectory(target) {
  const variable = target === "risuai"
    ? "RISUAI_REFERENCE_DEPENDENCIES"
    : "POCKETRISU_REFERENCE_DEPENDENCIES";
  return path.resolve(process.env[variable] ?? path.join(cacheRoot, target));
}

export function referenceDependencyVersions(directory, names) {
  const manifest = JSON.parse(
    fs.readFileSync(path.join(directory, "package.json"), "utf8"),
  );
  const lock = fs.readFileSync(path.join(directory, "pnpm-lock.yaml"), "utf8")
    .replaceAll("\r\n", "\n");
  // Only the root dependency block of the approved pnpm v9 lockfiles is supported.
  const root = lock.match(/^  \.:\n((?:\n| {4}[^\n]*\n)*)/m)?.[1];
  const dependencies = root?.match(/^    dependencies:\n((?: {6}[^\n]*\n)*)/m)?.[1];
  return Object.fromEntries(names.map(name => {
    if (!/^[a-z0-9-]+$/.test(name))
      throw new Error(`Unsupported reference dependency: ${name}`);
    const entry = dependencies?.match(new RegExp(`^      ${name}:\\n        specifier: ([^\\n]+)\\n        version: (\\d+\\.\\d+\\.\\d+)\\n`, "m"));
    if (!/^lockfileVersion: '9\.0'$/m.test(lock) || !entry || manifest.dependencies?.[name] !== entry[1])
      throw new Error(`Reference manifest/lockfile dependency mismatch: ${name}`);
    return [name, entry[2]];
  }));
}

export function createReferenceRequire(directory, dependencyDirectory, names) {
  const versions = referenceDependencyVersions(directory, names);
  const require = createRequire(path.join(path.resolve(dependencyDirectory), "package.json"));
  const roots = new Map();
  for (const [name, expected] of Object.entries(versions)) {
    const root = path.join(dependencyDirectory, "node_modules", name);
    const manifestPath = path.join(root, "package.json");
    if (!fs.existsSync(manifestPath))
      throw new Error(`Missing reference dependency ${name}@${expected} in ${dependencyDirectory}; prepare the isolated reference libraries first`);
    const actual = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
    if (actual.name !== name || actual.version !== expected)
      throw new Error(`Reference dependency mismatch: ${name} expected ${expected}, found ${actual.version}`);
    roots.set(name, fs.realpathSync(root));
  }
  const resolve = specifier => {
    const name = specifier.split("/")[0];
    const root = roots.get(name);
    if (!root) throw new Error(`Unapproved reference dependency: ${specifier}`);
    const entry = require.resolve(specifier);
    const relative = path.relative(root, fs.realpathSync(entry));
    if (relative === ".." || relative.startsWith(".." + path.sep) || path.isAbsolute(relative))
      throw new Error(`Reference dependency resolved outside its installation: ${specifier}`);
    return entry;
  };
  const checkedRequire = specifier => require(resolve(specifier));
  checkedRequire.resolve = resolve;
  return checkedRequire;
}

export function createTargetReferenceRequire(target) {
  return createReferenceRequire(
    references[target].directory,
    referenceDependencyDirectory(target),
    libraries[target],
  );
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const usage = "Usage: node scripts/compatibleExportDependencies.mjs --prepare [--target risuai|pocket] [--reference-update]";
  const args = process.argv.slice(2);
  if (args.shift() !== "--prepare") throw new Error(usage);
  let targets = Object.keys(libraries);
  let referenceUpdate = false;
  while (args.length) {
    const arg = args.shift();
    if (arg === "--target" && Object.hasOwn(libraries, args[0] ?? "")) targets = [args.shift()];
    else if (arg === "--reference-update") referenceUpdate = true;
    else throw new Error(usage);
  }
  const pins = JSON.parse(
    fs.readFileSync(path.join(contractDirectory, "runtime.json"), "utf8"),
  );
  const { packageManager } = JSON.parse(
    fs.readFileSync(new URL("../package.json", import.meta.url), "utf8"),
  );
  for (const target of targets) {
    const directory = references[target].directory;
    const files = ["package.json", "pnpm-lock.yaml"];
    // A reference update regenerates runtime.json after this step, so it checks
    // the moved revision pin instead of the previous manifest and lockfile hashes.
    if (referenceUpdate) verifyReferenceRevision(target);
    else verifySourceFilePins(target, files, Object.fromEntries(files.map(file => [file, pins[target][file]])));
    const destination = referenceDependencyDirectory(target);
    fs.mkdirSync(destination, { recursive: true });
    fs.writeFileSync(path.join(destination, "package.json"), JSON.stringify({
      private: true,
      packageManager,
      dependencies: referenceDependencyVersions(directory, libraries[target]),
    }, null, 2) + "\n");
    fs.writeFileSync(path.join(destination, "pnpm-workspace.yaml"), "packages: []\nminimumReleaseAge: 0\nallowBuilds:\n  better-sqlite3: true\n  msgpackr-extract: false\n");
    fs.copyFileSync(path.join(directory, "pnpm-lock.yaml"), path.join(destination, "pnpm-lock.yaml"));
    console.log(`${target}: prepared isolated library manifest in ${destination}`);
  }
}
