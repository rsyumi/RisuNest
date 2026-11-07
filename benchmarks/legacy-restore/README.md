# Synthetic legacy restore measurements

Run the Windows entry-count matrix from a checkout with its dependencies installed:

```text
node benchmarks/device-backup/windows-smoke.mjs --legacy-entries=all
```

The runner builds an isolated agent executable and restores 20,000, 50,000,
100,000 and 200,001 assets with 384-byte names and 32-byte bodies. Each case
starts a fresh native process and profile. It measures native working set and
renderer heap, verifies every restored name, hash and body, and checks that a
truncated restore preserves the active root and revision. Results include the
source and executable hashes. Earlier synthetic profiles are retained at the
paths recorded in the result.

Use `--legacy-entries=20000` (or another matrix count) for one case. These
measurements are separate from normal unit tests. The existing decoded-size
axis remains available through the Android and iOS harnesses.

For the Android entry-count axis, build the synthetic agent APK using
`benchmarks/device-backup/tauri.android.conf.json`, then run
`benchmarks/device-backup/android-smoke.mjs --legacy-entries=200001` with its
required explicit emulator, APK, ADB and recent health-summary arguments.
This uses an app-owned synthetic file and does not exercise a document provider.

The Android decoded-size axis takes `--legacy-mb=100|300|600` and
`--legacy-encoding=raw|gzip` instead. Add `--legacy-binary-bytes=<n>`, at most
262,144, to give each synthetic character a MessagePack `bin` field of that many
bytes within the same decoded size; the run then also verifies one restored field.

Measure database, attachment and record sizes separately:

```text
node benchmarks/legacy-restore/resource-fixtures.mjs encrypted-database 100 .tmp/encrypted-100
node benchmarks/legacy-restore/resource-fixtures.mjs attachment 100 .tmp/attachment-100
node benchmarks/legacy-restore/resource-fixtures.mjs record 1 .tmp/record-1
node benchmarks/legacy-restore/resource-fixtures.mjs assets 64 .tmp/assets-64
node benchmarks/legacy-restore/resource-fixtures.mjs messages 100000 .tmp/messages-100000
cargo test --manifest-path src-tauri/Cargo.toml --lib --locked --no-run
node benchmarks/legacy-restore/resource-measure.mjs <compiled-test-executable> .tmp/encrypted-100/manifest.json .tmp/attachment-100/manifest.json .tmp/record-1/manifest.json
```

Use new fixture directories. Database and attachment cases accept 100, 300 and
600 decimal MB; record cases accept 1, 8 and 32 MiB; asset-count cases accept
64 and 256 assets of 96 KiB each; the message-count case accepts 100,000
messages (512-byte text, 1,000 per conversation) in a block RisuSave database.
Pass their manifest to the same measurement runner. Each native case runs in a
fresh process, authenticates synthetic ciphertext when applicable, replaces a
real persistent store, and verifies every conversation and attachment payload.
Only the account-key request is replaced by a synthetic key. The attachment
measurement verifies prepared CAS payloads; alias activation is covered by the
entry-count runner.

The result records process working-set peaks, sampled owned-disk peaks, staging
and database disk components, source hashes, and the copied test binary hash.
Fixture source files are outside the measured temporary staging tree. Disk
samples can miss short-lived peaks, and working set does not isolate the OS file
cache. These measurements are ignored native tests, outside ordinary test gates.
