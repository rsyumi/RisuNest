# Android streaming display smoke

This probe mounts the production `Chat` and `ChatBody` with synthetic display
snapshots. It exercises the real CBS, editdisplay regex, Markdown and sanitizer
paths. It does not start application bootstrap. The default display profiles
do not open a persistent database; the explicit `persistence` profile below does.
The probe has its own HTML and TypeScript entry, built through its own Vite
configuration in agent mode. It imports production components directly; the
normal app has no reference to this harness. Frontend output goes to
`benchmarks/streaming/dist`, selected by the dedicated Tauri config.
The entry no longer loads the unrelated application entry modules; regenerate
performance baselines before comparing timings with the former embedded probe.

This Android harness still uses the generated application's package on the
dedicated synthetic AVD. Never install it on a device/profile with real data.
The generated Android namespace and JNI package are unchanged.

Read `docs/android-testing.md` before starting the dedicated synthetic AVD.
Use `risunest_vm_retest`, `emulator-5554`, and the documented headless command.
Verify sustained ADB health before running this smoke. This runner does not
start or stop the emulator and does not change the startup benchmark's separate
AVD or private-server checks.

Build in PowerShell with the installed SDK/JDK and the shared Cargo target:

```powershell
$env:JAVA_HOME = 'D:/DevUtils/jdk-21.0.10+7'
$env:ANDROID_HOME = 'C:/Users/hyung/AppData/Local/Android/Sdk'
$env:ANDROID_SDK_ROOT = $env:ANDROID_HOME
$env:NDK_HOME = "$env:ANDROID_HOME/ndk/28.2.13676358"
$env:CARGO_TARGET_DIR = 'E:/Programming/Github/RisuNest/src-tauri/target'
pnpm benchmark:streaming:android:agent

node benchmarks/streaming/android-smoke.mjs --adb=C:/Users/hyung/AppData/Local/Android/Sdk/platform-tools/adb.exe
```

Use the documented process-local `ADB_SERVER_SOCKET` and
`ANDROID_ADB_SERVER_PORT=15037` when the default server port is reserved. The
runner verifies the AVD, cuts device networking before installing, and checks
both the Tauri package and the synthetic fixture marker before running tests.
It blocks Realm and the prohibited translator URL through CDP as well. The
synthetic AVD remains offline afterwards.

The result is `android-result.local.json` (override with `--output=...`). It
contains environment information, APK hash, fixed assertion IDs, boolean
results and timing metrics. No console events or network bodies are collected.
The runner force-stops its app and removes its CDP forwarding on exit.

The default `--profile=smoke` uses about 10,000 UTF-16 code units for the six
mode combinations, then more than 500,000 for bounded recent-preview cadence.
Use `--profile=stress --output=benchmarks/streaming/android-stress.local.json`
to run the mode combinations with the large mixed Korean/emoji source as well.
This retains the observed WebView 113 full-expansion stall as a failing
reproduction, separate from basic behavior acceptance. A CDP timeout is a
failure with its last progress stage, never a successful or missing metric.

Coverage: recent/collapsed/off with display deferral on/off; long, split and
nested Thoughts; replacement snapshots; expansion persistence; incomplete
finalization; full-input final display regex; source preservation; 30Hz and
requested 1000Hz display publication. Timer clamping and processing mean the
actual rate may be lower, so elapsed time and sample counts are reported.

Limits: accepted display snapshots are injected directly. Provider transport,
generation ownership, output processing, persistent saves, virtualized history,
touch/fling scrolling, real plugins, translation and real-device performance
require separate integration runs. Timing here includes display processing
and DOM observation, not a measured pixel-presentation timestamp.

## Android persistence profiles

Run `--profile=persistence-spike` to compare a tiny native return command with
JSON, Uint8Array (Android serializes this as a JSON number array), and bounded
string payloads. Inputs are synthetic ASCII or Korean/emoji with escaping at
0/256KiB/1MiB/6MiB. Size accounting is outside measured intervals. `wireBytes`
counts the serialized payload, excluding Tauri envelope metadata. This is an
IPC feasibility probe, not a durable-save benchmark.

Run `--profile=persistence --output=benchmarks/streaming/persistence-result.local.json`
to exercise the production Worker encoder and Android commit adapter against
ordinary JSON commits. **This replaces the native store with a tracked synthetic
fixture on the dedicated AVD.** It must never run on another device/profile.
The runner retains the AVD, package, agent entry and offline safety checks.

The suite performs two repetitions with reversed mode order for root mutations,
nested plugin storage, and a conversation message, with both alphabets and all
four sizes. Each commit is read back for exact source and revision equality.
During the measured save, accepted synthetic display snapshots are published at
a requested 30Hz. Worker startup/structured clone and native durable processing
are included in elapsed/frame metrics. Readback and byte accounting are outside
those intervals. Counts and timings are emitted, never the source values.

It also rejects overlapping producers, invalid/stale chunks, incomplete commits
and stale revisions, then verifies successful retry, page reload cleanup and
force-stop/restart durability. The assembly limit is 64MiB and each string carries
at most 32KiB of UTF-8; a larger save uses the existing JSON path before opening
a transfer. This is bounded message passing, not shared memory or zero-copy.

`jsHeapBytes` is a post-save browser heap sample when available, otherwise null.
It is not total native memory or peak allocation, and no GC duration is inferred.
Frame gaps are rAF intervals, not platform presentation times. These debug
emulator measurements do not establish real-device performance, provider
integration, background generation behavior, touch/fling correctness, or fixes
for the separate large expanded-Thought renderer stall.
