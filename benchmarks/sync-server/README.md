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
```

The probe verifies a synthetic 1 MiB push/pull hash and ten ADB responses across
150 seconds. Run it before runtime validation. The runtime script uses the current
debug daemon, creates a fresh synthetic server, obtains an ephemeral credential
without logging it, binds the isolated app, force-stops and restarts it, and runs
another native cycle. A prior validation registration is removed from this
isolated app on rerun. Only identifiers, phases, timings and boolean results are
written to `.local`; console and page content are not retained. The runtime test
covers native IPC, Android Keystore retrieval and process restart, not visual UI
interaction or background service behavior.

On 2026-09-11, the agent APK at server commit `1299af193` passed registration and
restart (`pending` to `idle`), and the ADB probe passed in 150604 ms. The local
Windows cross-drive Kotlin incremental cache was disabled for these builds.
