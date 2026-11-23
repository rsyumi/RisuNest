import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const bash = process.env.RISUNEST_TEST_BASH ?? "bash";
const skip = process.platform === "win32" && !process.env.RISUNEST_TEST_BASH;
const script = fileURLToPath(new URL("../../scripts/install-apt-packages.sh", import.meta.url)).replaceAll("\\", "/");
const runnerMirrors = "# runner mirrors\nhttp://azure.archive.ubuntu.com/ubuntu/\tpriority:1\nhttps://archive.ubuntu.com/ubuntu/\tpriority:2\nhttps://security.ubuntu.com/ubuntu/\tpriority:3\n";

// `outcome` decides each apt-get call from its action ("update", "download" or "install") and the
// number of earlier calls with that action, counted in files because bounded calls run in a child
// shell. An unprivileged `timeout` cannot signal root's apt-get, so the stub refuses to bound a
// command outside sudo.
function run(outcome, mirrors = runnerMirrors) {
  const root = mkdtempSync(join(tmpdir(), "risunest-apt-"));
  try {
    const list = join(root, "apt-mirrors.txt").replaceAll("\\", "/");
    if (mirrors !== null) writeFileSync(list, mirrors);
    const result = spawnSync(bash, ["-c", `
      sudo() { as_root=1 "$@"; }
      timeout() {
        [ -n "$as_root" ] || { echo "timeout runs outside sudo" >&2; return 99; }
        printf 'bounded:%s ' "$2"; shift 2; "$@"
      }
      apt-get() {
        case "$*" in
          *--download-only*) action=download ;;
          *" update") action=update ;;
          *) action=install ;;
        esac
        seen=$(( $(cat "$APT_STUB_CALLS/$action" 2>/dev/null || echo 0) + 1 ))
        echo "$seen" > "$APT_STUB_CALLS/$action"
        printf '%s:%s %s\\n' "$action" "$seen" "$*"
        ${outcome}
      }
      export -f sudo timeout apt-get
      exec bash "$@"
    `, "test", script, "libwebkit2gtk-4.1-dev", "gnome-keyring"], {
      encoding: "utf8", timeout: 10000, windowsHide: true,
      env: { ...process.env, APT_MIRROR_LIST: list, APT_ATTEMPT_SECONDS: "7", APT_STUB_CALLS: root.replaceAll("\\", "/") },
    });
    return { ...result, mirrors: mirrors === null ? null : readFileSync(list, "utf8") };
  } finally { rmSync(root, { recursive: true, force: true }); }
}

test("a stalled download drops its mirror, retries, and installs from the downloaded archives", { skip }, () => {
  const result = run('[ "$action" != download ] || [ "$seen" -ge 2 ] || return 124');
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  // One bound covers each attempt's index refresh and download.
  assert.deepEqual(result.stdout.trim().split(/\r?\n/), [
    "bounded:7 update:1 -o Acquire::Retries=2 --error-on=any update",
    "download:1 -o Acquire::Retries=2 install --download-only -y libwebkit2gtk-4.1-dev gnome-keyring",
    "bounded:7 update:2 -o Acquire::Retries=2 --error-on=any update",
    "download:2 -o Acquire::Retries=2 install --download-only -y libwebkit2gtk-4.1-dev gnome-keyring",
    "install:1 -o Acquire::Retries=2 install -y libwebkit2gtk-4.1-dev gnome-keyring",
  ]);
  assert.match(result.stderr, /retrying without http:\/\/azure\.archive\.ubuntu\.com\/ubuntu\//);
  assert.equal(result.mirrors, runnerMirrors.replace(/^http:\/\/azure.*\n/m, ""));
  // An indented mirror is the one dropped and named.
  const indented = run('[ "$action" != download ] || [ "$seen" -ge 2 ] || return 124', runnerMirrors.replace("http://azure", "  http://azure"));
  assert.equal(indented.status, 0, indented.stderr);
  assert.match(indented.stderr, /retrying without http:\/\/azure\.archive\.ubuntu\.com\/ubuntu\/\./);
  assert.equal(indented.mirrors, runnerMirrors.replace(/^http:\/\/azure.*\n/m, ""));

  const unlisted = run('[ "$action" != update ] || [ "$seen" -ge 2 ] || return 100', null);
  assert.equal(unlisted.status, 0, unlisted.stderr);
  assert.match(unlisted.stdout, /^install:1 /m);
});

test("index refresh failures stay failures and interrupted attempts are not retried", { skip }, () => {
  const failing = run('[ "$action" != update ] || return 100');
  assert.equal(failing.status, 100);
  assert.deepEqual(failing.stdout.match(/^bounded:7 \w+/gm), ["bounded:7 update", "bounded:7 update", "bounded:7 update"]);
  assert.equal(failing.mirrors, "# runner mirrors\nhttps://security.ubuntu.com/ubuntu/\tpriority:3\n");

  const interrupted = run('[ "$action" != download ] || return 130');
  assert.equal(interrupted.status, 130);
  assert.doesNotMatch(interrupted.stdout, /update:2|install:/);
  assert.equal(interrupted.mirrors, runnerMirrors);
});
