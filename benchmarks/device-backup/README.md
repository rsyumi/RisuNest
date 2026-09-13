# Synthetic device backup WebView smoke

This separate agent build consumes the production device backup modules and native file jobs. Product entries never import this directory. It creates only synthetic values, including an 8 MiB binary graph, shared references, cycles, unusual UTF-16, two IndexedDB schemas, deleted automatic keys, an open raw connection, a dedicated worker, and an iframe writer.

The run exports the selected device sections through a native `.risunest` job, crosses a real maintenance navigation, replaces the synthetic current values and schemas, restores the archive through another maintenance navigation, and checks logical fingerprints and keys outside the selected prefixes. A final export cancels after the first binary transfer and checks that native storage opens again. Result fields include the largest IPC binary chunk, sampled renderer heap, operation duration and cancellation latency. Native process memory is collected by platform runners where available. Operation duration includes archive work and is not an exact native fence duration.

Windows runs require a newly generated synthetic native identifier and a separate WebView data directory. The runner verifies title, identifier and the expected native directory before starting. Android runs require the dedicated `risunest_vm_retest` AVD, the documented private ADB server, sustained health and transfer checks, and the agent harness APK. Read the main checkout's `docs/android-testing.md` before running Android. Never point these runners at an existing installed app profile, a user's AVD, or a user-provided backup.

Build the web entry with `node node_modules/vite/bin/vite.js build --mode agent --config benchmarks/device-backup/vite.config.ts`. Native Android builds use `benchmarks/device-backup/tauri.android.conf.json`. Keep the shared Cargo target directory required by the project. Platform runners and their `.tmp` result files provide the actual execution evidence; a successful build or emulator health check alone does not establish a passing WebView smoke test.

The native runners use separate frontend output directories so a concurrent Vite build cannot remove assets while Rust embeds them. Windows creates a unique output directory for each run. Android uses `.tmp/device-webview-android/frontend`.

Run a fresh Windows fixture with `node benchmarks/device-backup/windows-smoke.mjs`. Its checked `result.json` identifies the unique profile and executable hash. The runner retains the synthetic archive and a separate `exchange.json` descriptor with its SHA-256 and section fingerprints. After a successful baseline, reuse only that generated profile:

```text
node benchmarks/device-backup/windows-smoke.mjs --reuse=<baseline-result.json> --peer=<other-platform-exchange.json>
node benchmarks/device-backup/windows-smoke.mjs --reuse=<baseline-result.json> --fault=before-commit
node benchmarks/device-backup/windows-smoke.mjs --reuse=<baseline-result.json> --fault=after-device-marker
```

The fault runs stop at a durable command boundary in this test entry, terminate only the owned synthetic process, and reopen the same checked profile. The first point follows the first section's application and precedes the device-only marker. The second follows the device-only marker. The restarted maintenance entry must restore and verify the complete old or committed state before normal storage opens. Fault controls live under a separate synthetic localStorage key outside the selected prefixes; product modules contain no fault hooks.

A hard kill can discard the last test-control localStorage write. The Windows runner handles only that exact fixture assertion when its external record proves the kill boundary, the first cold start adds a recovery acknowledgement, and native bootstrap reports normal. It then restores only the checked test controls and navigates again for the independent fingerprint comparison. Reports distinguish the first cold recovery from the later verification navigation. Normal completed-run shutdown requests a graceful browser close; injected fault shutdown remains a hard kill.

The Android runner accepts `--adb=<adb-executable> --apk=<synthetic-apk> --health=<health-summary.json> --output=<result.json>` and requires `ANDROID_ADB_SERVER_PORT=15037` and `ADB_SERVER_SOCKET=tcp:127.0.0.1:15037`. Its `--exchange=<descriptor.json>` output can be used by Windows. A later Android invocation with `--peer=<windows-exchange.json>` imports that exact checked synthetic archive into the still-running completed fixture without reinstalling it. Never reuse this mode against an arbitrary installed package or device.

Only an actual passing result establishes coverage for the chosen scenario. This smoke does not establish process-kill behavior around a PDS library commit, every section boundary, additional native windows, service-worker shutdown, all supported clone types, large-library limits, or exact peak memory. Those remain separate acceptance checks.
