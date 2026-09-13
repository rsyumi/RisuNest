import { readFileSync, writeFileSync, mkdirSync, readdirSync } from "node:fs";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "../..");
const native = path.join(import.meta.dirname, "native");
const read = (file) => JSON.parse(readFileSync(path.join(root, file), "utf8"));
const config = read("src-tauri/tauri.conf.json");
config.identifier = "io.github.rsyumi.risunest.macos.bench";
config.productName = "RisuNest Mac Bench";
config.mainBinaryName = "risunest-macos-bench";
config.build = { frontendDist: "../dist" };
config.bundle = {
  active: true,
  targets: ["app"],
  icon: config.bundle.icon.map((file) => "../../../src-tauri/" + file),
  fileAssociations: config.bundle.fileAssociations,
  macOS: read("src-tauri/tauri.macos.conf.json").bundle.macOS,
};
mkdirSync(path.join(native, "capabilities"), { recursive: true });
for (const name of readdirSync(path.join(root, "src-tauri/capabilities"))) {
  if (!name.endsWith(".json")) continue;
  const capability = read("src-tauri/capabilities/" + name);
  writeFileSync(
    path.join(native, "capabilities", name),
    JSON.stringify(capability, null, 2),
  );
}
writeFileSync(
  path.join(native, "capabilities/benchmark.json"),
  JSON.stringify(
    {
      identifier: "macos-benchmark",
      windows: ["main"],
      permissions: ["core:window:allow-close", "core:window:allow-is-visible"],
    },
    null,
    2,
  ),
);
writeFileSync(
  path.join(native, "tauri.conf.json"),
  JSON.stringify(config, null, 2),
);
console.log("Prepared isolated macOS harness from product configuration");
