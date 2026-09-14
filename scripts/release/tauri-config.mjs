import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs, readJson, requireArg, writeJson } from "./common.mjs";

export function releaseTauriConfig(releaseInput, publicKey) {
  if (!publicKey) throw new Error("RISUNEST_UPDATE_PUBLIC_KEY is required.");
  const config = {
    version: releaseInput.version,
    bundle: { createUpdaterArtifacts: true },
  };
  if (releaseInput.product === "app" && releaseInput.mobileBuildNumber) {
    config.bundle.android = { versionCode: releaseInput.mobileBuildNumber };
    config.bundle.iOS = { bundleVersion: releaseInput.iosBuildNumber };
  }
  if (releaseInput.product === "app") {
    config.plugins = {
      updater: {
        pubkey: publicKey,
        endpoints: [],
      },
    };
  }
  return config;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const input = readJson(requireArg(args, "release-input"));
  writeJson(
    requireArg(args, "output"),
    releaseTauriConfig(input, process.env.RISUNEST_UPDATE_PUBLIC_KEY),
  );
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
