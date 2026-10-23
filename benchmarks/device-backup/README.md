# Synthetic native device backup smoke

Exercises native device backup and restore with a separate agent build and synthetic data. Product builds never import this harness.

The baseline covers native Hypa, plugin-local values, device settings, WebView reload, restore, and cancellation. Fault mode terminates the owned synthetic process during activation and verifies startup recovery. Platform exchange mode restores an archive created by the other platform and compares checked fingerprints.

## Windows

```text
node benchmarks/device-backup/windows-smoke.mjs
node benchmarks/device-backup/windows-smoke.mjs --reuse=<baseline-result.json> --peer=<other-platform-exchange.json>
node benchmarks/device-backup/windows-smoke.mjs --reuse=<baseline-result.json> --fault=activating-database
```

The runner creates a named agent profile and verifies its native identifier, WebView profile, and resolved app-data directory before initializing data. Reuse only a baseline created by this harness.

## Android

Use a disposable synthetic AVD, an owned local ADB server, a sustained health-check result, and a separate agent APK. Set `ANDROID_HOME` or pass `--adb=<adb-executable>`. The runner requires the exact emulator serial, AVD name, and ADB port; it rejects a different AVD. SDK Build Tools 36.0.0 supplies `aapt`, or select it with `--aapt=<executable>`.

```text
node benchmarks/device-backup/android-smoke.mjs --serial=<emulator-serial> --avd=<synthetic-avd-name> --adb-port=<owned-port> --apk=<synthetic-apk> --health=<health-summary.json> --output=<result.json>
```

Use `--exchange=<descriptor.json>` to publish a checked archive descriptor and `--peer=<windows-exchange.json>` to import one into the still-owned fixture.

The health JSON must identify the same `Serial`, cover at least 120 `Seconds` and 8 `Samples`, report zero `Failures`, and set `Passed`, `HashMatch`, `Push`, `Pull`, and `ConsolePing` to true. Generate it from sustained ADB command, push/pull hash, and emulator-console checks; a result older than 30 minutes is rejected.

A successful build is not a passing smoke test. The result must report successful assertions for the selected scenario. Never point either runner at an installed application, arbitrary device, user AVD, or user-provided backup.

## Large plugin values

The plugin measurement writes and reads synthetic UTF-8 string and JSON values, backs them up and restores them, and synchronizes them between fresh profiles through a private loopback Sync server. JSON cases use flat strings. Results include exact payload hashes, elapsed times, renderer heap samples, and native memory peaks. The Windows runner also records app and WebView process working sets. The payload sizes count the UTF-8 bytes of the JSON-encoded stored string.

```text
node benchmarks/device-backup/windows-smoke.mjs --plugin-values=all
node benchmarks/device-backup/android-smoke.mjs --serial=<emulator-serial> --avd=<synthetic-avd-name> --adb-port=<owned-port> --apk=<synthetic-apk> --health=<health-summary.json> --output=<result.json> --plugin-values=64-string --sync-server=<sync-server-executable>
```

Windows accepts `all`, `json` (all three JSON sizes), or one of `64-string`, `64-json`, `256-string`, `256-json`, `512-string`, and `512-json`. Set `RISUNEST_PLUGIN_SYNC_SERVER` to select the server executable; the default is the shared Cargo target's release binary. Android takes one case per run and requires `--sync-server`; it clears only the validated disposable AVD's synthetic application's data between peers. Its loopback server uses ADB reverse while device networking remains disabled. Registration credentials stay in private fixture files and are excluded from reports. Keep those files and synthetic profile directories outside published artifacts.

## Local plugin storage performance

The local-JSON RPC optimization targets root arrays of at least 1,024 entries; smaller inputs and flat strings retain the ordinary wire path. The RPC matrix also includes the 1,024-entry boundary. Binary/rich values fall back to the original graph path, and host JSON syntax validation remains inside timing.

```text
node benchmarks/device-backup/windows-smoke.mjs --plugin-performance
node benchmarks/device-backup/windows-smoke.mjs --plugin-performance-json
node benchmarks/device-backup/windows-smoke.mjs --plugin-performance-built=<run-uuid>
node benchmarks/device-backup/windows-smoke.mjs --plugin-performance-json-built=<run-uuid>
node benchmarks/device-backup/plugin-frontend-cost.mjs <baseline-ref> <output.json>
node benchmarks/device-backup/plugin-frontend-cost.mjs <baseline-ref> <output.json> <before-source-directory>
node benchmarks/device-backup/plugin-frontend-breakdown.mjs <baseline-ref> <output.json>
node benchmarks/device-backup/plugin-rpc-cost.mjs <output.json>
node benchmarks/device-backup/compare-plugin-performance.mjs <before-result.json> <after-result.json> <report.md>
```

This Windows measurement builds a disposable debug agent and exercises the production `SafeLocalPluginStorage` wrapper, Tauri IPC and native SQLite device store. It measures small JSON, arrays of 1,000/50,000 objects, an 8 MiB flat JSON string, and 1/8/32 MiB ArrayBuffer/Blob values. The mixed-owner case reads a small setting after invalidating a keyspace that also holds a 1 MiB Blob. Inputs are synthetic and each owner is cleared after use.

Each operation has two warm-up batches and seven measured batches. `samplesMs` contains elapsed batch time divided by its sequential operation count; median/min/max are batch averages, **not individual-operation latency percentiles**. A warm read reuses the wrapper; values above the cache budget may still require IPC. A cold read invalidates the frontend keyspace before every read, but does not flush OS/SQLite disk caches. Fixture construction and correctness comparisons are outside timing; encoding, queueing, IPC, commits and decoding are inside. There is no plugin iframe/RPC, backup or sync in these timings.

To compare revisions, use separate checkouts with the **identical harness files**, lockfile, dependencies and build mode, run sequentially, and retain both result JSON files and executables. The report records source hashes, HEAD, tracked product diff hash, executable hash and environment. Also retain hashes/copies of any untracked product sources. A binary correctness failure skips its timing: losing the payload cannot count as a speed advantage. Debug timings describe this benchmark environment; they do not establish release or mobile performance.

`--plugin-performance-built` repeats a completed build from this checkout's owned `.tmp/device-windows-smoke-<uuid>` directory, validates the synthetic identifier and native data root, and overwrites `result.json`; copy previous reports first. The executable embeds its original source and port: keep its original build log/source hashes and run only one instance at a time. Repeating a build does not rebuild changed product sources. The CPU-only diagnostic runs the actual wrappers and codec against an injected in-memory asynchronous KV boundary under Node, alternating before/after order. Use it to isolate serialization/queue overhead; its timings do not describe a native app.

`--plugin-performance-json` builds and measures the twelve JSON operations plus the mixed-owner settings read, omitting the separate binary throughput workload. `-json-built` repeats this scope in newly built agents. For older agents that do not accept the scope argument, it stops the disposable process after the twelve JSON measurements and final correctness checks, at the first binary correctness result; those reports mark `scope: json-only` and omit the remaining binary workload and mixed-owner result.

`plugin-frontend-breakdown.mjs` separates the JSON frontend overhead using the actual current wrapper/codec and baseline source. In diagnostic copies only, it independently bypasses rich-value detection, queueing, or both, and verifies identical output for known plain JSON fixtures. Those bypasses are invalid for general plugin input or concurrent operations and are never imported by product code. Fifteen measured batches follow four warmups, with rotating/reversed variant order. The script separately times synchronous detection/stringification and writes a CPU profile of the unmodified current wrapper after timing. Standalone stage medians are diagnostic and need not add exactly to full-call latency; CPU profiles are sampled attribution, not exact per-operation accounting.

The optional `before-source-directory` for `plugin-frontend-cost.mjs` compares an uncommitted snapshot against the working tree. The directory must contain `pluginSafeClass.ts` and `localPluginStorageValue.ts`; the baseline Git reference is ignored in this mode. Each side loads its own codec, and the report records both codec hashes, wrapper hashes and the snapshot directory. This isolates a codec change without committing or conflating it with earlier wrapper changes.

`plugin-rpc-cost.mjs` builds the separate browser fixture and measures real opaque plugin iframes under Chromium and WebKit. It compares the ordinary remote-reference path against the local-JSON capability using the same production storage receiver, queue, cache and asynchronous memory persistence boundary. The forwarding reference represents the implementation immediately before RPC JSON preparation, not the original Git main revision. Browser message capture is disabled during timing so diagnostic retention cannot pin large argument graphs. The measurements include guest preparation, real postMessage/structured cloning, host JSON validation and responses; they exclude native IPC and SQLite. Small settings, 256/1,000/50,000-object arrays, an 8 MiB flat string, and nested 1 MiB ArrayBuffer/Blob fallback are checked. JSON has three warm-up and twelve alternating measured batches; binary has two warm-up and seven measured operations, with fixture creation and full type/byte comparisons outside timing. Source and built-asset hashes are retained; source changes during measurement reject the run. Install this repository's Playwright Chromium and WebKit runtimes before running it. The corresponding iframe behavior checks are in `tests/browser/pluginIframe.browser.spec.ts`.
