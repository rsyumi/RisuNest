import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync } from "node:fs";
import { resolve, join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";

// Explicit binary paths; never attach to the user's installed daemon or data.
const [server, manager] = process.argv.slice(2);
assert(server && manager, "Pass the server and manager executable paths.");
const output = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../.test-output",
);
mkdirSync(output, { recursive: true });
const root = join(mkdtempSync(join(output, "daemon-")), "data");
function cli(args, input) {
  const result = spawnSync(
    manager,
    ["--data-dir", root, "--server", server, ...args],
    { encoding: "utf8", input, windowsHide: true, timeout: 40000 },
  );
  assert.equal(result.status, 0, `Manager failed: ${result.stderr}`);
  return result.stdout;
}
assert.equal(
  spawnSync(server, ["init", "--data-dir", root], {
    stdio: "ignore",
    windowsHide: true,
  }).status,
  0,
);
const daemon = spawn(
  server,
  ["serve", "--data-dir", root, "--listen", "127.0.0.1:0"],
  { stdio: "ignore", windowsHide: true },
);
const exited = new Promise((resolve) => daemon.once("exit", resolve));
try {
  let ready = false;
  for (let i = 0; i < 50; i++) {
    const probe = spawnSync(manager, ["--data-dir", root, "status"], {
      encoding: "utf8",
      windowsHide: true,
      timeout: 5000,
    });
    if (probe.status === 0) {
      const value = JSON.parse(probe.stdout);
      assert.equal(value.devices.length, 0);
      ready = true;
      break;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert(ready, "Synthetic daemon readiness");
  const tui = cli([], "1\n\n0\n");
  assert(tui.includes("서버 파일 용량"));
  assert(tui.includes("번호 선택"));
  assert.equal(
    JSON.parse(cli(["status"])).devices.length,
    0,
    "Closing TUI leaves daemon alive",
  );
  cli(["stop"]);
  assert.equal(
    await Promise.race([
      exited,
      new Promise((resolve) => setTimeout(() => resolve("timeout"), 5000)),
    ]),
    0,
  );
  console.log(
    "PASS: isolated daemon discovery, numeric TUI, client exit independence, graceful stop",
  );
} finally {
  if (daemon.exitCode === null) daemon.kill();
}
