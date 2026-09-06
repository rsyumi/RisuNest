# Roadmap 14 platform baseline

This directory defines the version 2 result contract and the four synthetic
blocking scenarios used by Windows and Android measurements. The fixtures are
local and synthetic, and the CDP runners block RisuRealm for the whole session
(see `scripts/realmBlocklist.mjs`).

## Result contract

`result.schema.json` is the shared JSON Schema. `result-schema.mjs` provides the
repository-local validator used by the runners. A result always includes:

- fixture name, version, SHA-256 identity, and descriptor
- build identity, source revision, profile, target, application version, and
  Realm-disabled state
- platform identity, OS, architecture, WebView identity, and optional device
  model
- heap, Windows RSS, or Android PSS samples
- DOM node, mounted message, and live resource URL counts
- named latency sample series and named byte artifacts
- a canonical output SHA-256 with exact provenance
- SHA-256 identities for every raw measurement artifact

Completed results cannot use pending memory or omit canonical output provenance,
UI counts, all memory samples, all latency samples, or raw artifact identities. Pending and failed results
keep the same fields and use null or empty measurements where no honest value
exists.

The live URL count is the number of distinct DOM-referenced `blob:` or
`risuasset` resource URLs at the sample boundary. It does not claim to count
browser resources that are no longer referenced by the DOM.

Run the focused contract tests with:

```powershell
node --test benchmarks/roadmap14/result-schema.node-test.mjs benchmarks/phase3/tauri-cdp.node-test.mjs
```

## Scenario identities

`scenarios.json` contains these frozen version 1 descriptors:

| Scenario | SHA-256 identity |
|---|---|
| `library-many` | `eb656882a5da2a70b8b48c62daf9bbe8980e808e86bc7933c11a73bcb16da329` |
| `save-large` | `8479aa2d62405a01fbe69114ebd91d3df75c1448b3017c970ca1b3075c79aca7` |
| `stream-postprocess` | `51cf472ea0c2c39cdfc3423b4d9900b3c7a06ddd237ad50bd169c472d62fc8fa` |
| `asset-library` | `7aaa1c5d14e5a065d8bdfbd7ae922d6b415847857c9026625de288b557d27ca3` |

Identity bytes are sorted-key JSON over the complete name, version, and
descriptor. Changing a descriptor requires a version increment and new checked
identity.

## Windows release entry point

The current reusable Windows path covers `save-large`. It invokes the existing
release Phase 3 Rust measurement and isolated release Tauri CDP measurement,
then converts both artifacts into the shared schema. The Rust benchmark writes
its exact deterministic serialized fixture for the Tauri measurement to stage
through the same native commands. The CDP measurement records memory, DOM,
mounted message, and live resource URL counts for that same fixture.

```powershell
node benchmarks/roadmap14/windows.mjs `
  --run-existing `
  --platform-identity windows-reference-host `
  --output src-tauri/target/roadmap14-baseline/windows-save-large.json
```

Existing raw results can be converted without rerunning the measurements:

```powershell
node benchmarks/roadmap14/windows.mjs `
  --phase3-result <phase3-save-large.json> `
  --tauri-result <phase3-tauri-cdp.json> `
  --platform-identity windows-reference-host `
  --output <windows-save-large.json>
```

The runner requires both raw results and the embedded Tauri identifier to match
the current Git HEAD. It also requires matching fixture bytes, SHA-256, and
500-character, 5,001-conversation, 510,000-message shape. The canonical output
hash is SHA-256 over each post-append export traversal JSON fragment prefixed by
its little-endian 64-bit byte length, in traversal order. It is not the input
fixture hash. Raw artifact hashes are calculated from the files and cannot be
overridden.

The `export-materialize-and-traversal-total` latency series maps to Rust
`exportTotalUs`. The framed traversal digest and fragment byte count describe
the traversal portion of that measured total.

The other three descriptors are ready for their workstream-specific release
measurement adapters. They must use the same result schema. No Windows result is
included in this directory unless the release measurement actually ran.

## Android instrumentation contract

Until a working physical device is available, generate explicit pending records:

```powershell
node benchmarks/roadmap14/android.mjs `
  --source-revision <revision> `
  --output src-tauri/target/roadmap14-baseline/android-pending.json
```

Physical-device instrumentation must emit a completed JSON array with exactly
one shared schema result for each scenario. Every result must use the same build,
source revision, and physical-device identity. Pending records and emulator
identities are rejected. It records Android PSS in `memory.pssBytes`,
WebView JavaScript heap when available, device and WebView identity, the three UI
counts, operation samples, bytes, and canonical output SHA-256. Validate and copy
the device output with:

```powershell
node benchmarks/roadmap14/android.mjs `
  --measurement <physical-device-results.json> `
  --output <validated-results.json>
```

An APK build, JVM test, or historically unstable emulator run is not physical
device performance evidence. The pending records remain pending until a real
device measurement passes this contract.
