import { execFileSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const adb = process.env.ANDROID_HOME + "/platform-tools/adb.exe";
const run = (...args) =>
  execFileSync(adb, ["-P", "15037", "-s", "emulator-5554", ...args], {
    timeout: 5000,
    encoding: "utf8",
    windowsHide: true,
  }).trim();
if (run("emu", "avd", "name").split(/\s+/)[0] !== "risunest_vm_retest") {
  throw new Error("Refusing an unverified Android profile");
}
const root = fileURLToPath(new URL(".local/", import.meta.url));
mkdirSync(root, { recursive: true });
const bytes = randomBytes(1024 * 1024);
writeFileSync(root + "probe-source.bin", bytes);
run(
  "push",
  root + "probe-source.bin",
  "/data/local/tmp/risunest-sync-synthetic-probe.bin",
);
run(
  "pull",
  "/data/local/tmp/risunest-sync-synthetic-probe.bin",
  root + "probe-returned.bin",
);
const hash = (value) => createHash("sha256").update(value).digest("hex");
if (hash(readFileSync(root + "probe-returned.bin")) !== hash(bytes))
  throw new Error("ADB round-trip mismatch");
const started = Date.now();
for (let index = 0; index < 10; index++) {
  await new Promise((resolve) => setTimeout(resolve, 15000));
  if (run("shell", "echo", "sync-synthetic-ok") !== "sync-synthetic-ok")
    throw new Error("ADB probe failed");
  console.log(
    JSON.stringify({
      probe: index + 1,
      elapsedMs: Date.now() - started,
      ok: true,
    }),
  );
}
writeFileSync(
  root + "adb-probe.json",
  JSON.stringify(
    {
      elapsedMs: Date.now() - started,
      probes: 10,
      roundTripBytes: bytes.length,
      verified: true,
    },
    null,
    2,
  ),
);
