# Synthetic startup measurements

Requires Node.js 24 or newer for the built-in SQLite fixture writer.

This harness builds an isolated Tauri release with agent-mode Realm blocking. It
uses a separate application identifier and WebView directory, and redirects
APPDATA and LOCALAPPDATA for child tools. Windows native Known Folder lookup can
ignore these environment variables. The runner obtains the actual app-data path
through native IPC and verifies its isolated identifier before fixture IO.
Before seeding or reloading, CDP verifies the native identifier.
Never point it at an installed application or a profile containing real data.

Windows runs on a separate, unselected desktop by default. Native windows and
all restarted children stay there. The runner never switches the user's input
desktop and never falls back to foreground execution. Only use
`--display=foreground` for an explicitly requested visible run. The result records
the display mode and document visibility; keep different display modes separate
when interpreting timing. Intermediate runner logs use the output filename plus
`.runner.log`.

Run from the repository root on Windows:

```powershell
node --test benchmarks/startup/startup.node-test.mjs
node benchmarks/startup/profile.mjs --suite=true --runs=20 --output=docs/research/startup-results.local.json
node benchmarks/startup/summarize.mjs docs/research/startup-results.local.json
```

The suite holds the 500-character, roughly 100 MiB database and selected chat
constant while comparing 11,345 and 100,000 unique CAS objects. It tests API 2.1
compatibility and scalable mode, with 20 reloads and 20 process restarts per
condition. Each condition has a separately recorded warm-up. Sixteen generated
256×256 PNG images exercise visible media loading. Every object has a catalog
entry and alias; body references are counted separately. File creation happens
outside the timed interval, with at most 16 concurrent writes.

Single-condition options use `--name=value`:

| Option                                  | Meaning                                                                                        |
| --------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `characters`, `assets`, `bytes`, `runs` | Fixture dimensions and samples per restart/reload                                              |
| `mode=compatibility` or `mode=scalable` | Enabled no-op API 2.1 plugin or scalable working set                                           |
| `interact=true`, `images=true`          | Open the visible sidebar, select a chat, edit unsent input, scroll                             |
| `mutation=true`                         | Synthetic API 2.1 plugin toggles a supported root setting                                      |
| `snapshot=missing`                      | Remove only the isolated app's snapshots before each measurement                               |
| `autoListen=true`                       | Enable LAN listening in the isolated device settings                                           |
| `legacyAssets=100`                      | Re-create separate synthetic legacy files before each sample                                   |
| `referencedAssets=100000`               | Additional body references, separate from the file-count comparison                            |
| `coldStorage=true`                      | Exercise the cold-storage cleanup guard                                                        |
| `locale=ko`                             | Korean startup presentation                                                                    |
| `shape=investigation`                   | Eleven synthetic characters with the earlier investigation's numeric size distribution         |
| `variant=gc-only`                       | Build the original frontend at the documented base commit with the actual modified native open |
| `config=<isolated config>`              | Reuse a binary only if its live native identity matches this config                            |
| `rebuild=true`                          | Rebuild while retaining that isolated identity                                                 |
| `fresh=true`                            | Move the previous synthetic store into a retained sibling before creating a fresh fixture      |

Use separate result files for every run. Output contains allowlisted numbers,
fixed stage names and success flags, plus build provenance. It never stores
console events, exception details, page text, request bodies, record identifiers
or profile paths. The SHA-256 field identifies the executable, not user data.
Do not publish temporary profiles or raw build logs as benchmark evidence.

Measurements continue at least ten seconds after interactive and wait for the
first flush and active observed jobs, up to a thirty-second stabilization bound.
Selection intentionally updates the character's last-interaction timestamp, so
use `interact=false` when asserting zero commits and an unchanged revision.
Mutation and selection runs should commit their changes normally.

The first-scroll readiness time includes waiting for overflowing content and
three consecutive frames of stable geometry. The scroll itself must still move
the viewport on the following frame. Selection readiness requires a rendered
chat row as well as the input. These waits remain in the reported timing.

`interactiveMs` starts at WebView navigation. The summary also subtracts the
observed bootstrap entry. Process launch duration includes CDP readiness
polling. OS caches are not cleared. IPC duration includes transport and queueing;
overlap with a snapshot is not a direct measurement of native mutex ownership.
Heap values are samples at operation boundaries, not an OS process peak.
Resource timing counts visible image requests; blob/native IPC media has a
separate counter. Neither count should be interpreted as all disk reads.

The summary derives `maxImageRequests` from overlapping Resource Timing
intervals. Custom-protocol images can bypass the fetch observer, so a zero
native-fetch counter does not mean there were no media requests.

`launchToFirstPaintMs` aligns Node's launch timestamp and WebView's first
contentful paint using their same-host performance time origins. It is recorded
for process restarts; reloads and older results without this observation remain
unmeasured. The summary reports the number of measured values for each metric.

`observe.mjs` instruments existing function bodies only in the benchmark build.
It preserves real return values, promises, native opens and revisions. Ordinary
agent and production builds contain none of these observation hooks.

The optional `android-smoke.mjs` uses only `emulator-5580` named
`risunest_startup_synthetic`, with a private ADB server on port 5038. Create that
AVD with isolated, disposable synthetic data and launch it with `-no-window`.
Build `pnpm android:build:emulator:agent` first, set
`ANDROID_ADB_SERVER_PORT=5038` and `ADB_SERVER_SOCKET=tcp:127.0.0.1:5038`, then run
the script with `--adb=<SDK>/platform-tools/adb.exe` and `--output=<local JSON>`.
It verifies the device identity, cuts and verifies networking before installing
or launching the app, and checks Korean startup phases plus no-op/mutating
plugin commits. A failed ADB connection is a failed runtime check, not a pass.
