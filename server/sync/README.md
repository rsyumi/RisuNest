# RisuNest sync daemon

Standalone Rust daemon with no Tauri, WebView, Node, provider, or plugin runtime.
The server and application integration are under development. This checkout is
not yet an end-user sync release; use synthetic data until the remaining gates
are verified.

## Run

```powershell
$env:CARGO_TARGET_DIR = 'E:/Programming/Github/RisuNest/src-tauri/target'
cargo build --manifest-path server/sync/Cargo.toml --locked
$daemon = 'E:/Programming/Github/RisuNest/src-tauri/target/debug/risunest-sync-server.exe'
& $daemon init --data-dir E:/sync-server-synthetic
& $daemon device add --data-dir E:/sync-server-synthetic
& $daemon serve --data-dir E:/sync-server-synthetic
```

`device add` prints a device ID, library ID, and 256-bit token once. Transfer this
credential privately to its device. Only the token's SHA-256 verifier is stored
on the server. Do not put credentials into URLs, logs, or library content.

`status`, `device add`, `device revoke ID`, `maintain`, `backup`, `restore`, and `restore-epoch` require
the daemon to be stopped. All commands take `--data-dir ABSOLUTE_PATH` and use
an exclusive owner lock. A second daemon or `init` fails without replacing data.
Ctrl+C and Unix SIGTERM request graceful shutdown.

The default is `127.0.0.1:4319`. `--listen` accepts loopback addresses only.
Configure a trusted HTTPS reverse proxy or Tunnel for remote clients and pass
`--https-proxy` to acknowledge that origin configuration. This flag does not
install a proxy, provide TLS, or enable public cleartext binding. Direct TLS and
live proxy verification remain pending. Keep the data directory on a local disk
owned exclusively by the daemon user. Existing symlinks/junctions are rejected.

## HTTP contract

Every route requires `Authorization: Bearer TOKEN` and
`X-Risu-Library: LIBRARY_ID`, before reading request bodies. There are no HTTP
administration routes. Limits are two active requests per device and eight total;
streaming responses retain their slots until consumed or disconnected. Saturation
returns 429 and `Retry-After: 1`. Body/handler timeout is 60 seconds. Request
compression is rejected with 415; object Range offsets address identity bytes.

| Endpoint                                     | Contract                                                                                                     |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `GET /session`                               | Authenticated identity, operation watermark and pending state; binding requires an unused operation identity |
| `GET /devices/{id}/status`                   | Authenticated read-only active/revoked status used before replacing a lost device identity                   |
| `GET /scopes?scope=NAME`                     | Current scope version and last clear identity; distinguishes independent key edits from a clear              |
| `GET /head`                                  | Small stored head, ETag, bodyless conditional 304                                                            |
| `POST /objects/missing`                      | Candidate `{hash,size}` array, decimal string sizes                                                          |
| `POST /uploads/batch`, `POST /objects/batch` | Bounded full object batches                                                                                  |
| `POST /uploads/frames`                       | Full/delta transfer batch, exact target verification                                                         |
| `POST /objects/transfer`                     | Array of `{target,bases}`; delta, full, or full-required frames                                              |
| `GET /objects/{hash}`                        | Bounded-memory stream, single Range/If-Range, ETag                                                           |
| `POST /objects/pins`                         | Renew device-owned 24-hour leases for up to 1024 hashes                                                      |
| `POST /uploads`                              | Begin `{hash,size}` manifest; returns `uploadId`                                                             |
| `PUT /uploads/{id}/chunks/{index}`           | Exact 8 MiB chunk except final remainder; `X-Content-SHA256` required                                        |
| `GET /uploads/{id}?after=INDEX`              | Paged verified chunk bitmap and completion status; optional `wait=true` waits up to 20 seconds               |
| `POST /uploads/{id}/complete`                | Below 64 MiB: verified CAS publication; otherwise durable 202 finalization job, resumed after restart        |
| `DELETE /uploads/{id}`                       | Owner-scoped cancellation                                                                                    |
| `PUT /uploads/{id}/delta`                    | Attach an exact `RNSL` file recipe and queue durable reconstruction (202)                                    |
| `POST /objects/delta`                        | Queue a file delta for `{target,bases}`, returning a stable device-owned `jobId`                             |
| `GET /object-deltas/{id}?wait=true`          | Delta bytes (200), explicit full-required (204), or pending (202); waits at most 20 seconds                  |
| `DELETE /object-deltas/{id}`                 | Release or cancel this device's file-delta job                                                               |
| `POST /staged-changes`                       | Single-page convenience staging                                                                              |
| `POST /staged-changes/start`                 | Begin a multi-page staging set                                                                               |
| `PUT /staged-changes/{id}/pages/{index}`     | Consecutive pages, identical-page retry, conflicting retry rejected                                          |
| `GET /staged-changes/{id}`                   | Next page and optional sealed digest                                                                         |
| `POST /staged-changes/{id}/seal`             | Partition-independent streaming digest                                                                       |
| `DELETE /staged-changes/{id}`                | Cancellation before operation reservation                                                                    |
| `POST /commits`                              | Durable operation reservation and atomic commit; required `If-Match`                                         |
| `GET /operations/{id}`                       | Owner-scoped pending status or terminal receipt                                                              |
| `GET /changes`                               | Fixed-through journal traversal                                                                              |
| `POST /read-pins`                            | Pin `{epoch,afterSeq}` through current head                                                                  |
| `GET /read-pins/{id}`                        | Pinned journal pages with `afterSeq`, `afterOrdinal`, `limit`                                                |
| `DELETE /read-pins/{id}`                     | Release this device's traversal                                                                              |
| `POST /checkpoints`                          | Capture fixed record metadata                                                                                |
| `GET /checkpoints/{id}`                      | Pages with optional `afterKey` and `limit`                                                                   |
| `DELETE /checkpoints/{id}`                   | Release this device's checkpoint                                                                             |
| `POST /acks`                                 | Monotonic per-device `{epoch,seq}`                                                                           |

For unpinned `/changes`, supply `epoch`, `afterSeq`, `afterOrdinal`, `throughSeq`,
and optional `limit` (1–1024). Start after an applied commit using
`afterOrdinal=9223372036854775807`; use the returned `next` while `hasMore`.
Use read pins when maintenance may run. History below `minRetainedSeq` returns
410 `checkpoint-required`; clients must retain unsent local changes.

Multi-page commits return 202 and an operation ID. The worker resumes reserved
jobs after restart and checks the expected head at actual commit time. A device
has one reserved operation at a time; other devices keep their own work. A
single-page request also reserves durably, then normally returns its terminal
receipt directly. A lost response never authorizes allocating a replacement
operation without checking the original result.

## Storage, identity, and validation

SQLite uses WAL and synchronous FULL with separate reader/writer connections.
The commit transaction updates records, relations/scopes, journal, head, receipt,
watermark, and job removal together. Pages are normalized into staging tables;
large commits are iterated without constructing a full change-set in memory.

CAS publication orders write, file flush, same-volume rename, then registration.
Windows uses write-through MoveFileExW; Unix syncs destination directories.
Incomplete objects never become visible via registered object APIs. Publication
and GC coordinate; request-body IO and byte hashing do not hold the writer lock.

Content identity is SHA-256 over exact bytes. The server validates control
metadata, before versions, dependencies, generic parent relations, record read
fences, and scope clear intent. It does not parse application payload schemas or
execute plugin contents. Clients must validate the entire staged apply before
advancing their local base or acknowledgement.

Operation IDs derive from library/device/decimal sequence. Intent digests include
the expected content head and sorted changes/fences, independent of page layout,
staging locator, and full/delta choice. Committed/stale/failed outcomes consume
the sequence. Identical retries return the receipt; different intent returns 409.
Stale heads return 412. A pruned receipt below the durable watermark returns
410 `operation-history-expired`, never a new execution. Retention metadata alone
does not change the content revision or make an otherwise valid proposal stale.

## Wire bounds

Control JSON uses restricted RFC 8785 canonicalization: strings, booleans, null,
arrays, objects, no JSON numbers. Sizes and counters are canonical decimal
strings up to 64 digits. Object properties sort by UTF-16; record keys sort by
UTF-8 lexical order. Duplicate keys, unknown fields, invalid Unicode, malformed
hashes, and trailing bytes fail. Application content is not normalized by these
control rules. Rust/ECMAScript tests share exact UTF-8 golden vectors.

- Metadata pages: 1 MiB, up to 1024 changes/fences per category, 64 KiB keys.
- Staging: 16 sets per device, 256 MiB total metadata per set, 500,000 changes.
- Record descriptors: immutable bounded reference trees, up to 1,000,000
  references, depth 8, generic dependency/relation roots and up to 16 scopes.
- Full `RNSF` batches: at most 8 MiB including framing, 1024 frames.
- Mixed `RNSB` batches: full, `RNSD` delta, or download-only full-required frames.
- `RNSD` is a custom exact COPY/INSERT profile, not VCDIFF: ordered base IDs,
  checked offsets, exact base/target hashes and lengths, no output-copy chains.
  Current encoder/materializer limits: 4 bases, 32 MiB aggregate base bytes,
  16 MiB target, 8 MiB patch, 262,144 operations.
- `RNSL` uses file IO for larger objects: 1 TiB target/aggregate bases, 4 bases,
  8 MiB recipe, 262,144 operations, 64 KiB IO buffers. A sorted sparse anchor
  index uses about 6 MiB for a 1 GiB base, bounded to about 262,144 entries.
  Generation checks cancellation and a 120-second CPU/elapsed budget. Poor
  matches or exhausted generation budget explicitly request full chunks/Range.
  Every base and reconstructed target is verified in full. Private temporary
  output is published only after verification. Upload reconstruction and download
  recipe generation use separate durable workers and return 202 immediately.
  Waiting requests have their own bounded slots so head/transfer requests remain
  available. Recipe and base identities survive daemon restart.
- Upload manifests: up to 1 TiB per object and per device's active uploads, 16
  concurrent manifests, 24-hour expiry, bounded 1024-entry bitmap pages.
- Read pins: 16 per device; checkpoints: 2 per device; each expires after 24 hours.
- Small-recipe cache, queued upload recipes, and download recipes each have a
  64 MiB cap. Download jobs expire after one hour (16 per device); upload recipes
  follow their 24-hour manifests. Maintenance removes expired recipes and keeps
  every active job's bases as GC roots. Missing bases require only the target's
  full transfer.

## Retention and restore

Run `maintain` while stopped to collect acknowledged history and unreachable
registered objects. Every active device ack and unexpired read pin limits the
history floor. Offline devices are never automatically forgotten. Revoke a
retired device explicitly and register it anew if it returns. Tombstone identities
remain as fences. Receipt pruning also requires an age of at least 24 hours;
terminal operation watermarks survive pruning. Checkpoints, staging, object
leases, and upload manifests protect their referenced content.

The daemon runs maintenance every minute. Expired staging cannot resume or hold
quota; an already reserved commit retains its staging until a terminal receipt.
Completed/expired upload chunks enter a durable deletion queue, drained in pages
of 1024 files. Object publication and each device's leases protect complete bytes.

Stop the daemon and run `backup --data-dir SOURCE --backup-dir NEW_BACKUP`.
The command copies a consistent SQLite image and verifies every registered CAS
object, then writes `backup.json` as a completion marker. Restore with
`restore --backup-dir BACKUP --data-dir NEW_DATA_DIR`. Both destinations must be
new absolute paths. Restore verifies metadata and object hashes, rotates epoch,
and clears old server cursors/jobs before the new directory can serve clients.
It retains content and device watermarks. Clients must reconcile the changed
epoch. An interrupted copy without its completion marker is not a valid backup.
Power-loss and cross-OS backup validation remain pending.

## Native application integration

The native RisuNest settings page accepts the server URL and a separate device
credential for each installation. PDS transactions record outgoing edits; HTTP
preparation and publication run outside the UI replacement fence. Only atomic
local activation and the working-set/plugin refresh hold that fence. Lost IPC
replies are retried without enabling editing against an unconfirmed revision.

Conflicts retain the local revision and remote head used for the preview. Both
complete `.risulossless` packages are verified before applying either choice.
The settings page can restore either archive through the normal validated
lossless-import workflow, which backs up the current library and refreshes plugin
state. Restored content remains paused for inspection. Parent/child and order
conflicts are resolved as groups. Plugin clear retains its original scope and
membership, including empty clear and clear followed by an identical set.

After losing device operation history, revoke that device while the daemon is
stopped, issue a new credential, restart the daemon, and select **Register a new
device** in the app. The old device must be revoked before replacement. Local
edits and bases survive this reset and differences require comparison. A changed
server epoch has a separate **Compare restored server** action. Independent,
nonempty libraries are not automatically combined on first registration.

## Verification and remaining work

```powershell
cargo test --manifest-path crates/sync-wire/Cargo.toml --locked
cargo test --manifest-path server/sync/Cargo.toml --locked
node --test crates/sync-wire/tests/golden.test.mjs
cargo clippy --manifest-path server/sync/Cargo.toml --locked --all-targets -- -D warnings
```

Tests use synthetic stores and real loopback TCP: two-device races, auth before
body reads, chunk restart/retry, SQL rollback, abrupt process exit, fixed-through
pages, checkpoint/pin GC, clear fences, operation recovery, and bidirectional
10 MiB value delta transfer. The ignored crash-test child is executed by its
parent. The large HTTP integration uses a multithread Tokio runtime matching
the daemon. Windows single-thread client/server co-location stalled before
request dispatch and is not used as a daemon performance measurement.

On 2026-09-11, the standalone server suite passed 45 tests and the wire suite
passed 18. Both crates also passed Clippy with warnings denied. The native PDS modules passed 335 tests, including two real TCP
replicas, both conflict choices, clear in both directions, parent deletion versus
child edits, operation recovery, and backup corruption rejection. Settings,
backup routing, and server facade/controller tests passed 88 cases; svelte-check
reported no errors or warnings. The ignored crash child is run by its parent.

The real HTTP append gate passed at 1/127/128/129/1024/10000 existing messages.
For 72 new bytes, total request-plus-response bytes (including headers) were
14595–15263 for upload and 10240–10578 for download, each below D + 16 KiB.
These debug timings and message-only results do not establish the remaining
large-library CPU, RSS, latency, asset, or owner-list gates.
File-based delta is connected for opaque objects above 16 MiB. A 17 MiB file's
22-byte insertion used 6,974 upload and 6,253 download HTTP bytes in the direct
TCP integration test (2,343/2,408 ms, debug). The 1 GiB fixture passed too:
22 inserted bytes used 8,530 upload and 7,630 download HTTP bytes, including
long-poll requests, at 119,820/143,202 ms in the debug build. Both endpoints
verify the whole result hash. This combined native-client/server test process
had an observed peak working set of 68,096,000 bytes; that is not a separate
daemon or four-transfer RSS measurement. The test counts at the accepted server
socket, without a forwarding proxy. Bounded client projection/apply, cache/orphan collection, Android foreground integration,
proxy fault scenarios and OS distribution validation remain in progress.
Windows x86_64 is exercised locally; Linux/macOS and app-device combinations
remain unverified. This is still an intermediate implementation.

Registry, GUI/tray, service installation, peer transfer optimization, content
E2EE, and same-PC deduplication are separate from this server implementation.
