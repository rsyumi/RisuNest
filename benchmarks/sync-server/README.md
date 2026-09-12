# Synthetic server checks

These scripts do not read installed user profiles. Android checks require the
documented `risunest_vm_retest` AVD on `emulator-5554`, private ADB port 15037,
and the separate application ID
`io.github.rsyumi.risunest.syncservervalidation20260911`. Do not substitute the
normal application package. Build the app with `pnpm android:build:emulator:agent`
before creating the isolated APK.

The Gradle init script keeps the native JNI namespace, changes the installed
application ID, and redirects APK output into ignored `.local/android-build`.
From `src-tauri/gen/android`, after the agent build:

```powershell
./gradlew.bat :app:assembleX86_64Debug -x :app:rustBuildX86_64Debug `
  '-Pkotlin.incremental=false' '-Pkotlin.compiler.execution.strategy=in-process' `
  -I '../../../benchmarks/sync-server/android-profile.gradle' --offline
```

Verify the APK package with Android's `aapt dump badging` before installing it.
The runtime check confirms both the AVD and the exact Android process name before
connecting to its WebView. Tauri's compiled identifier retains its JNI namespace;
Android's application ID supplies the isolated OS data directory. Realm and the
excluded translator path are additionally blocked in CDP.

With `ANDROID_HOME` and the project's shared `CARGO_TARGET_DIR` set, run from the
checkout root:

```powershell
node benchmarks/sync-server/android-probe.mjs
node benchmarks/sync-server/android-runtime.mjs
node benchmarks/sync-server/android-runtime.mjs --ui
node benchmarks/sync-server/android-runtime.mjs --unresponsive-startup
```

The probe verifies a synthetic 1 MiB push/pull hash and ten ADB responses across
150 seconds. Run it before runtime validation. The runtime script uses the current
debug daemon, creates a fresh synthetic server, obtains an ephemeral credential
without logging it, binds the isolated app, force-stops and restarts it, and runs
another native cycle. A prior validation registration is removed from this
isolated app on rerun. Only identifiers, phases, timings and boolean results are
written to `.local`; console and page content are not retained. The runtime test
covers native IPC, Android Keystore retrieval, progress IPC and process restart.
`--ui` seeds a completed-onboarding synthetic profile, opens the actual settings,
disconnects and registers a new synthetic credential through the form, waits for
successful synchronization and checks mobile overflow. Screenshots are captured
only after the credential inputs disappear. `--unresponsive-startup` additionally
holds server responses for 35 seconds during a fresh app start and verifies local
settings interaction before the 30-second request deadline. These checks do not
cover an Android foreground service.

On 2026-09-11, the agent APK at server commit `1299af193` passed registration and
restart (`pending` to `idle`), and the ADB probe passed in 150604 ms. The local
Windows cross-drive Kotlin incremental cache was disabled for these builds.
The later progress/width implementation passed actual form registration and
completion at a 393 CSS-pixel viewport; the connection card's visible and scroll
widths both measured 355 pixels. This check exposed and verified fixes for the
long device-ID overflow and for cycle results assigned to a stale controller
snapshot while progress events were publishing.

## Isolated Windows application

The validation configuration compiles a different Tauri identifier and executable;
its WebView profile lives under ignored `.local`. Build it before running:

```powershell
$env:CARGO_TARGET_DIR = 'E:/Programming/Github/RisuNest/src-tauri/target'
pnpm exec tauri build --debug --no-bundle --config src-tauri/tauri.agent.conf.json --config benchmarks/sync-server/windows-validation.conf.json
node benchmarks/sync-server/windows-runtime.mjs
```

The launcher checks the compiled identifier, PID, executable path and process start
time. Before opening CDP on 19421, it verifies that the listener descends from that
owned process. The script registers a fresh synthetic daemon on 19422 through the
settings form, waits for success, restarts only its app, verifies another cycle and
stops both owned processes. It never reads the installed user's profile. The current
build passed these checks on 2026-09-12. English and Korean success labels are both
accepted because synchronized root settings can change the app language.

## Transfer and TLS comparisons

Build the standalone release daemon first, using the shared target. Node runs only
the measurement harness, not the server. These scripts create synthetic data and
count TCP bytes without logging request bodies or credentials:

```powershell
cargo build --manifest-path server/sync/Cargo.toml --release --locked
node benchmarks/sync-server/transfer-comparison.mjs
$env:OPENSSL_EXE = 'C:/Program Files/Git/usr/bin/openssl.exe'
node benchmarks/sync-server/tls-head.mjs
```

`transfer-comparison.mjs` compares 1,000 individual object requests against four
250-object full batches, in both directions, using loopback ports 19424/19425.
The batch cases run with two or four workers and separate device credentials.
Four workers do not override the daemon's two-request limit for a single device.
Every returned object is checked exactly. A subsequent missing query must return
no missing objects and send no object bodies. It still sends 89,467 HTTP bytes of
candidate metadata in this deliberately complete 1,000-object baseline; normal
native warm cycles send only candidates from the changed closure.

Representative 2026-09-12 text-fixture results (887,000 raw payload bytes):

| Mode                          | Upload HTTP bytes / requests / ms | Download HTTP bytes / requests / ms |
| ----------------------------- | --------------------------------- | ----------------------------------- |
| Individual objects, 2 workers | 1,466,000 / 1,000 / 10,259        | 1,459,000 / 1,000 / 9,519           |
| Full batches, 2 workers       | 996,904 / 4 / 3,590               | 996,884 / 4 / 437                   |
| Full batches, 4 workers       | 996,904 / 4 / 3,069               | 996,884 / 4 / 291                   |

The same runs include deterministic random bytes and already-gzipped random
objects. Offline gzip is measured separately from HTTP framing: text frames shrink
from 928,008 to 42,281 bytes (about 4 ms compression), random frames from 553,008
to 548,875 bytes (about 12 ms), and precompressed frames from 576,008 to 554,604
bytes (about 11 ms). This shows potential text savings, not shipped gzip support.
Product transport currently rejects request compression and uses identity Range
bytes. Full-vs-delta selection and stable reference pages are measured in native
integration tests, separately from this batch comparison.

`tls-head.mjs` uses ports 19427–19429 and an ephemeral self-signed fixture
certificate. Its client trusts only that certificate; no OS trust setting changes
and no disabled certificate validation. It counts 20 conditional head responses,
asserting 304 with zero body. TLS session resumption is disabled for the reconnect
comparison. Measured totals include actual TLS 1.3 records:

| Transport | Connection            | TCP bytes (20 requests) |   Mean / P95 latency |
| --------- | --------------------- | ----------------------: | -------------------: |
| HTTP      | Reused                |                   9,740 |   12.416 / 12.658 ms |
| HTTP      | Reconnected each time |                   9,740 |  86.351 / 110.151 ms |
| TLS 1.3   | Reused                |                  10,620 |   13.237 / 13.514 ms |
| TLS 1.3   | Reconnected each time |                 104,120 | 147.230 / 156.678 ms |

These local debug-client comparisons are not Internet or Cloudflare Tunnel
measurements. Native clients reuse the HTTP pool within a cycle; a new cycle
currently creates a new client, so this table does not claim cross-cycle TLS reuse.
Registry, WebSocket hints and cloudflared were not running and contribute no bytes
to these experiments. JSON results stay under ignored `.local`.

## Native scale and failure gates

Run selected ignored measurements explicitly; the ordinary test suite does not
run them automatically:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib server_sync_large_value_and_sequence_http_matrix -- --ignored --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib slow_network_full_transfer_gate -- --ignored --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib server_sync_local_commit_and_large_json_costs -- --ignored --nocapture
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib server_sync_hundred_thousand_resolved_assets_http_gate -- --ignored --nocapture
```

The 100,000-asset gate creates and transfers unique synthetic CAS bodies, edits one
owner name, checks both warm HTTP budgets and compares all received raw bytes.
It can take an hour because it creates and durably publishes hundreds of thousands
of small files. The reference-pages-only gate is a faster component regression,
not a substitute for this end-to-end test.

For the combined 500-character follow-up, set `RISUNEST_SYNTHETIC_OWNER_REPORT` to a
new absolute `.local` JSON path before the initial owner gate. That explicit mode
retains only its three marked synthetic temporary directories and writes their
paths. After the first process exits and both replicas have settled, run
`server_sync_retained_owner_library_500_character_gate` with `--ignored --nocapture`
and the same report path. The follow-up validates temporary-directory boundaries,
markers, clean replica state, the 100,000-entry owner and three exact body samples.
It then makes a new six-byte owner-name change, measures both complete HTTP cycles,
compares every one of the 100,000 received bodies and enforces a D + 32 KiB
regression ceiling for this extreme owner fixture. On 2026-09-12 the user directed
us to stop byte-level micro-optimization: D + 16 KiB remains a reference target,
not a pass/fail requirement for the 100,000-entry owner. The preceding measured
result was 19,451 upload / 16,555 download HTTP bytes for D = 6. Ordinary small
record gates retain their existing budget. Each rerun uses a new revision-derived name to avoid
passing through an already cached historical target. Only after that gate passes
does it add characters up to 500 and check a warm root change in the combined
library. A failed budget does not count as a completed scale gate.
The daemon owner lock prevents overlapping use. No retained fixture is an installed
app profile, and no credential or synthetic body is printed in the report.

The final retained run on 2026-09-12 passed after exact comparison of all 100,000
bodies: owner D = 6 used 16,402 upload / 14,652 download HTTP bytes, taking
397,053 / 742,195 ms in the debug native test. The combined 500-character library
then used 11,908 / 8,941 bytes for a six-byte root change with exact apply.
The complete retained run took 1,585.62 seconds. These are local debug IO costs,
not release latency or network-only transfer times. This gate uses `e0204e3dd`
product behavior; the subsequent revision-zero placeholder fix affects only
uninitialized stores and has its own full PDS regression coverage.

`windows-runtime.mjs --linux-server` and `android-runtime.mjs --ui --linux-server`
run against the already-built Linux x86_64 release binary in WSL Ubuntu-24.04.
The launcher invokes WSL with `--exec`, waits for daemon readiness and closes the
owned child's stdin supervisor for graceful shutdown. It creates a fresh synthetic
server directory per run. Windows and Android registration, completion and restart
passed against that Linux daemon on 2026-09-12. The default Windows daemon backend
also passed both app clients. The current Android APK's 35-second stall run allowed
local settings interaction in 5,687 ms and preserved the remote head; its preceding
ADB probe completed all ten responses in 150,621 ms.
