# RisuNest sync daemon

This is the first implementation slice of the standalone server. It runs without
Tauri, a WebView, Node, providers, or plugins. It is **not yet an end-user sync
release**: the RisuNest client adapter and the remaining protocol gates below are
not implemented. Use synthetic data while developing it.

## Run locally

Use the repository's shared Cargo target on the development machine:

```powershell
$env:CARGO_TARGET_DIR = 'E:/Programming/Github/RisuNest/src-tauri/target'
cargo build --manifest-path server/sync/Cargo.toml --locked
$daemon = 'E:/Programming/Github/RisuNest/src-tauri/target/debug/risunest-sync-server.exe'
& $daemon init --data-dir E:/sync-server-synthetic
& $daemon device add --data-dir E:/sync-server-synthetic
& $daemon serve --data-dir E:/sync-server-synthetic
```

`init` prints the library head. `device add` prints a fresh device ID, library ID,
and 256-bit token **once**, for secure transfer to that device. Only the token's
SHA-256 verifier is stored. Do not put the credential output into logs, scripts,
URLs, library data, or screenshots. Normal errors contain bounded codes only.

`status`, `device add`, and `device revoke ID` require the daemon to be stopped in
this slice. They use the same exclusive data-directory lock as `serve`. A second
daemon or an accidental second `init` fails without replacing the library.
Ctrl+C requests graceful shutdown; Unix SIGTERM is also handled in code.

The listener defaults to `127.0.0.1:4319`. `--listen` accepts only a loopback
address. For remote development, configure a trusted HTTPS reverse proxy or
Tunnel pointing at the loopback listener and acknowledge that configuration with
`--https-proxy`. This flag does not install a proxy, enable TLS, or permit a public
cleartext bind. Direct TLS serving and live proxy verification remain pending.
The private data directory belongs on a local disk under the daemon user's
exclusive control. Existing symlinks/junctions in storage paths are rejected.

## Implemented contract

Every sync request requires `Authorization: Bearer TOKEN` and
`X-Risu-Library: LIBRARY_ID`. Authentication precedes body consumption and declared
body-length checks. There are no public administration routes. Authenticated
requests have a 60-second timeout, at most two active requests per device and
eight overall; saturation returns 429 with `Retry-After: 1`.

| Endpoint                        | Behavior                                                                   |
| ------------------------------- | -------------------------------------------------------------------------- |
| `GET /head`                     | Stored small head, strong ETag, exact `If-None-Match` returns bodyless 304 |
| `POST /uploads/batch`           | Verified full binary frames, durable immutable CAS publication             |
| `POST /objects/missing`         | Array of `{hash,size}` candidates; size is a decimal string                |
| `POST /objects/batch`           | Array of target hashes; bounded full binary response                       |
| `GET /objects/{hash}`           | Exact bytes, hash verification, single Range/If-Range, strong ETag         |
| `POST /staged-changes`          | Validated change set; returns `stagedChangesId` and `changesDigest`        |
| `DELETE /staged-changes/{id}`   | Owner-scoped cancellation; releases a staging slot                         |
| `POST /commits`                 | `CommitIntent` plus required matching `If-Match`; atomic acceptance        |
| `GET /operations/{operationId}` | Owner-scoped terminal receipt lookup                                       |
| `GET /changes`                  | Fixed-through cursor pages over immutable journal rows                     |
| `POST /acks`                    | Monotonic per-device `{epoch,seq}` acknowledgement                         |

For `/changes`, supply `epoch`, `afterSeq`, `afterOrdinal`, `throughSeq`, and
optional `limit` (1–1024). Start after an applied commit with `afterOrdinal=1024`.
Continue using the returned `next` cursor while `hasMore` is true and preserve
the initial `throughSeq`. A subsequent head is a separate traversal. All journal
rows, tombstones, receipts, and objects are retained in this slice; acknowledgement
does not trigger deletion. No background GC or checkpoint expiration runs.

SQLite uses WAL and `synchronous=FULL`. Object publication orders write, file
flush, same-volume rename, then metadata registration. Windows uses
`MoveFileExW` with write-through; Unix syncs the destination directory. A commit
atomically writes records, journal changes, head, receipt, and device watermark.
Network transfers and object IO do not hold the library writer mutex. No commit
creates a full manifest or copies all existing records.

The server checks exact expected head, before versions, declared object
dependencies, and exact-record read fences. It does not parse or re-encode content
objects, execute plugins, or enforce application payload schemas. Application
staged validation remains a future client responsibility.

Committed, stale, and failed receipts are terminal and consume the device's
operation sequence. Retry lookup precedes current-head/staging checks. The same
intent returns the same receipt even after restart or with a different staging
locator; a different intent on that identity returns 409. Stale heads return 412.
A missing receipt at or below the durable watermark returns 410
`operation-history-expired`. Reconciliation must allocate a new operation sequence.
HTTP timeout is not evidence that a commit failed: query/retry the same operation.

## Wire profile and current bounds

`crates/sync-wire` owns the pure codec. Control metadata uses a restricted
[RFC 8785 profile](https://www.rfc-editor.org/rfc/rfc8785.html): strings, booleans,
null, arrays, and objects. All JSON numbers are forbidden; counters/sizes use
canonical unsigned decimal strings (up to 64 digits). Object property ordering
uses UTF-16 code units. Logical change/fence keys use strict UTF-8 lexical order;
hash lists use ascending lowercase hexadecimal order. Duplicate JSON keys, lone
surrogates, unknown fields, duplicate logical keys, and trailing input fail.

Intent digest includes the expected head and the digest of sorted changes/read
fences. It excludes staging locators and transport representation. Operation ID
is domain-separated SHA-256 over the canonical library/device/sequence tuple.
Object identity is SHA-256 over exact content bytes, independent of these rules.
Rust and an independent ECMAScript reference share golden UTF-8 vectors.

A full batch is `RNSF`, a big-endian u32 frame count, then frames containing a
one-byte codec (`0` only), 32 raw hash bytes, big-endian u64 target length, and
exact target bytes. The complete batch including framing is at most 8 MiB and
1024 objects. Empty objects work. Unknown codecs are rejected; delta support is
not advertised. Compressed request bodies are currently rejected with 415.

Staged changes are currently one page, at most 1 MiB and 1024 changes/fences with
64 KiB logical keys. A device may hold 16 staged sets and explicitly cancel them.
These are **intermediate bounds**, not a claim that all existing library shapes
fit. Large descriptors, large dependency lists, initial full-library transactions,
and objects exceeding a batch need the pending paging/chunk work before client
integration. The implementation does not truncate or discard such source data.

## Validation and remaining gates

```powershell
$env:CARGO_TARGET_DIR = 'E:/Programming/Github/RisuNest/src-tauri/target'
cargo test --manifest-path crates/sync-wire/Cargo.toml --locked
cargo test --manifest-path server/sync/Cargo.toml --locked
node --test crates/sync-wire/tests/golden.test.mjs
cargo clippy --manifest-path server/sync/Cargo.toml --locked --all-targets -- -D warnings
```

Tests use temporary synthetic stores and real loopback TCP. They cover two-device
commit races, interrupted uploads, auth before body reads, Range, failed SQL
transactions, process exit without destructors, replay, and fixed-through pages.
The ignored child test is invoked by its parent recovery test, not skipped
recovery coverage. Power-loss durability and every fsync/rename failure boundary
are not yet verified. Windows x86_64 is the currently exercised target; Linux,
macOS, arm64 servers, and Windows/Android application clients remain unverified.

Remaining implementation gates from the approved plan:

- Task 0: full synthetic-library compatibility matrix, PDS outbox/projection and
  plugin boundary audit; the existing application and startup paths are untouched.
- Tasks 1–2: immutable descriptor/multi-page staged changes, generic relation and
  keyspace-clear fences, durable asynchronous jobs with sequence reservation,
  large-object chunk upload/resume, fuller fault injection, administration/config.
- Task 3: checkpoints, persistent pins/leases, bounded retention and GC, restored
  snapshot epoch rotation, and verified resume metadata.
- Tasks 4–6: native/TS client, durable PDS outbox, conflict-preserving staged apply,
  multi-base byte delta in both directions, small-change traffic/CPU/RSS gates,
  and three-OS distribution verification.

Until epoch rotation/reconciliation exists, an old backup must not be restarted
as if it were the current live server. A development backup is a stopped,
consistent copy of the complete metadata and objects directory, including SQLite
sidecars if present. Do not copy just a live SQLite file. Production restore
instructions belong to the pending restore gate.

Endpoint directory/registry, GUI/tray, service installation, peer-mode changes,
content E2EE, and same-PC storage deduplication are not part of this slice.
