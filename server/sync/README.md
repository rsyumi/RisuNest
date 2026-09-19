# RisuNest sync daemon

Standalone Rust daemon with no Tauri, WebView, Node, provider, or plugin runtime.
The server and application integration are under development. This checkout is
not yet an end-user sync release; use synthetic data until the remaining gates
are verified.

## Run

After extracting a binary archive, open a terminal in that directory. Choose a
new absolute data directory, then initialize, register each device and start:

```powershell
# Windows, PowerShell
$data = Join-Path $env:LOCALAPPDATA 'RisuNestSync'
./risunest-sync-server.exe init --data-dir $data
./risunest-sync-server.exe device add --data-dir $data
./risunest-sync-server.exe serve --data-dir $data
```

```sh
# Linux or macOS
./risunest-sync-server init --data-dir "$HOME/RisuNestSync"
./risunest-sync-server device add --data-dir "$HOME/RisuNestSync"
./risunest-sync-server serve --data-dir "$HOME/RisuNestSync"
```

In RisuNest's server synchronization settings, register the endpoint, library ID,
device ID and token printed for that installation. Use `http://127.0.0.1:4319`
only on the server machine; other devices need the HTTPS proxy URL described
below. Pause the daemon before adding another device, then restart it.

To build from this repository on the local development machine:

```powershell
$env:CARGO_TARGET_DIR = 'E:/Programming/Github/RisuNest/src-tauri/target'
cargo build --manifest-path server/sync/Cargo.toml --locked
$daemon = 'E:/Programming/Github/RisuNest/src-tauri/target/debug/risunest-sync-server.exe'
& $daemon init --data-dir E:/sync-server-synthetic
& $daemon device add --data-dir E:/sync-server-synthetic
& $daemon serve --data-dir E:/sync-server-synthetic
```

Release builds also provide a read-only update check:

```powershell
& $daemon update check
```

The command verifies the signed RisuNest release catalog and reports the current
version, latest Sync version, and matching raw package URL for this OS and
architecture. It does not download, install, or reconfigure the daemon or
cloudflared.

`device add` prints a device ID, library ID, and 256-bit token once. Transfer this
credential privately to its device. Only the token's SHA-256 verifier is stored
on the server. Keep credentials out of HTTP URLs, logs, and library content.
The private registration URI below is an explicit credential transport.

The app keeps only a credential reference in PDS. Windows protects a separate
local file with user-scoped DPAPI; Android encrypts it with an Android Keystore
AES-GCM key. Apple clients use Keychain, and Linux clients require an unlocked
Secret Service (the native Linux build also needs the libdbus development
package). Key-store errors do not fall back to plaintext. A data backup cannot
transfer a device credential. Unlock the OS store or revoke the previous device
and register a new credential when one is lost. Windows protection and raw-source
backup exclusion have automated coverage; Android registration and retrieval after
process restart passed in an isolated agent APK. Apple/Linux key-store runtime
checks remain pending. See [DPAPI](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata)
and [Keyring's platform stores](https://docs.rs/keyring/3.6.3/keyring/).

`status`, `device add`, `device revoke ID`, `maintain`, `backup`, `restore`, and `restore-epoch` require
the daemon to be stopped. All commands take `--data-dir ABSOLUTE_PATH` and use
an exclusive owner lock. A second daemon or `init` fails without replacing data.
Ctrl+C and Unix SIGTERM request graceful shutdown.

The default is `127.0.0.1:4319`. `--listen` accepts loopback addresses only.
Configure a trusted HTTPS reverse proxy or Tunnel for remote clients and pass
`--https-proxy` to acknowledge that origin configuration. This flag does not
install a proxy, provide TLS, or enable public cleartext binding. Live HTTPS proxy
verification remains pending. Keep the data directory on a local disk
owned exclusively by the daemon user. Existing symlinks/junctions are rejected.

## Registration and address discovery

Configure an address while the daemon is stopped. An optional registry stores only
an encrypted address. It does not receive the device token or decryption key.

```powershell
./risunest-sync-server.exe connection configure --data-dir $data --endpoint https://sync.example --registry https://registry.example
./risunest-sync-server.exe device add --data-dir $data --qr
./risunest-sync-server.exe connection status --data-dir $data
```

Omit `--registry` for a fixed address without discovery. Configured `device add`
prints one reusable, device-specific `risunestlocal://sync-server/register#...`
URI. `--qr` additionally renders that exact value as a terminal QR. Paste it or
scan it in the app, review the prefilled fields, and explicitly connect. Treat the
whole code as the device credential; it is not a one-time token. Oversized codes
are rejected before allocating a device. Unconfigured `device add` continues to
provide the manual four-field credential.

For a daemon-owned Quick Tunnel, replace `--endpoint` with
`--cloudflared ABSOLUTE_EXECUTABLE`. The operator supplies cloudflared; the daemon
never downloads or updates it. Start `serve`, wait for Tunnel readiness, stop it,
then issue a device code using the saved address. Restarting the Tunnel may change
its address; the stable registry identity lets the app discover that change.
Issuance before an address has been observed fails with
`public-endpoint-not-ready`. Offline managed-mode issuance requires a registry
(`managed-registration-needs-directory` otherwise), so a code remains usable after
the next Tunnel start. A future live management caller can issue against its
current running endpoint through the domain API.
Fixed/external mode never starts or terminates an external Tunnel.

The daemon publishes after both a Quick Tunnel URL and an edge-registration log
have been observed. It supervises its own child and bounds restart delays and log
sizes. Normal shutdown reaps the child; on Windows a kill-on-close Job also covers
abrupt daemon termination after child attachment. GUI subscribers do not own the
runtime. Registry failure does not stop the sync listener. Same successful address
is reposted seven days after the last successful publication, checked every
minute. The successful timestamp survives restarts; an uncertain POST reuses its
persisted envelope. The pre-release connection-state format changed in place;
older state is rejected and is not migrated automatically.
`connection repost` explicitly requests another publication without changing the
registry identity. All CLI administration still requires the daemon to be stopped.

The private `connection-state` file persists directory identity/key and publication
state. Windows uses user-scoped DPAPI; Unix requires owner-only file permissions.
It is operational configuration, outside library exports and the database/object
backup. Preserve it separately with the same daemon OS account when restoring the
same directory identity. Public status omits the UUID, key, device token, and code.
Its endpoint is the last observed address, not a live reachability assertion.

The management GUI/TUI can consume these Rust interfaces after acquiring Store's
existing exclusive ownership:

- Store: `configure_connection`, `connection_status`, `issue_registration`,
  `request_republication`. Each new registration allocates a separate device.
- ConnectionRuntime: start after binding the loopback listener; subscribe to
  `tunnel` and `publication` watch channels; `publication_changed` wakes the
  publisher after an explicit repost; `shutdown` stops owned tasks and child.
- Configuration changes require restart in this iteration. Live administration,
  service installation, GUI transport/authentication, and hot add/revoke belong to
  the separate management application work. No public management listener is added.

## HTTP contract

For persistent remote access, configure a named Tunnel or another trusted HTTPS
terminator to forward to the loopback listener, then register that HTTPS URL in
RisuNest settings. No router port forwarding or public HTTP bind is needed with
an outbound Tunnel. Creating or installing the Tunnel is a separate operator step.
[Cloudflare's setup guide](https://developers.cloudflare.com/tunnel/setup/)
describes the named Tunnel flow. Quick Tunnels are for development, have a
200 in-flight request limit and do not support SSE; this protocol uses HTTP
requests and foreground polling. [Quick Tunnel limits](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/do-more-with-tunnels/trycloudflare/).

Cloudflare's proxied request-body upload limit is 100 MB on Free/Pro plans;
an operator can configure a lower limit. It applies per request, not to the whole
library or a resumable object. This protocol's 8 MiB ceiling and 1 MiB default
chunks stay below it. [Cloudflare upload limits](https://developers.cloudflare.com/support/troubleshooting/http-status-codes/4xx-client-error/error-413/).

The default Cloudflare Proxy Read Timeout is 125 seconds per request, not a limit
on the entire resumable job; Proxy Write Timeout is 30 seconds. Large work returns
202 and uses bounded status waits, and full uploads default to 1 MiB chunks.
413/429/524 responses retain verified transfer state for retry. The fault and
bandwidth tests use a synthetic local transport, not a live Cloudflare connection.
[Cloudflare timeout reference](https://developers.cloudflare.com/fundamentals/reference/connection-limits/).

Every route requires `Authorization: Bearer TOKEN` and
`X-Risu-Library: LIBRARY_ID`, before reading request bodies. There are no HTTP
administration routes. Limits are two active requests per device and eight total;
streaming responses retain their slots until consumed or disconnected. Saturation
returns 429 and `Retry-After: 1`. Bulk bodies have four additional memory slots,
reserved before reading the body and retained through blocking work and response
consumption. CPU materialization uses one queued worker slot, keeping head reads
available while bounding aggregate base/index memory. Body/handler timeout is 60 seconds, or 110 seconds
for bounded full/frame/recipe upload bodies. Request
compression is rejected with 415; object Range offsets address identity bytes.

| Endpoint                                     | Contract                                                                                                                |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `GET /session`                               | Authenticated head/identity, operation watermark and pending state in one snapshot; binding requires an unused identity |
| `GET /devices/{id}/status`                   | Authenticated read-only active/revoked status used before replacing a lost device identity                              |
| `GET /scopes?scope=NAME`                     | Scope version, last clear identity and head in one snapshot; distinguishes independent edits from a clear               |
| `GET /head`                                  | Small stored head, ETag, bodyless conditional 304                                                                       |
| `POST /objects/missing`                      | Candidate `{hash,size}` array, decimal string sizes; response lists missing hashes                                      |
| `POST /uploads/frames`                       | Full/delta transfer batch, exact target verification; successful whole batch returns bodyless 204                       |
| `POST /objects/transfer`                     | Array of `{target,bases}`; delta, full, or full-required frames                                                         |
| `GET /objects/{hash}`                        | Bounded-memory stream, single Range/If-Range, ETag                                                                      |
| `POST /objects/pins`                         | Renew device-owned 24-hour leases for up to 1024 hashes                                                                 |
| `POST /uploads`                              | Begin `{hash,size}` manifest; returns `uploadId`                                                                        |
| `PUT /uploads/{id}/chunks/{index}`           | Exact 1 MiB chunk except final remainder; `X-Content-SHA256` required                                                   |
| `GET /uploads/{id}?after=INDEX`              | Paged verified chunk bitmap and completion status; optional `wait=true` waits up to 20 seconds                          |
| `POST /uploads/{id}/complete`                | Below 64 MiB: verified CAS publication; otherwise durable 202 finalization job, resumed after restart                   |
| `DELETE /uploads/{id}`                       | Owner-scoped cancellation                                                                                               |
| `PUT /uploads/{id}/delta`                    | Attach an exact `RNSL` file recipe and queue durable reconstruction (202)                                               |
| `POST /objects/delta`                        | Queue a file delta for `{target,bases}`, returning a stable device-owned `jobId`                                        |
| `GET /object-deltas/{id}?wait=true`          | Delta bytes (200), explicit full-required (204), or pending (202); waits at most 20 seconds                             |
| `DELETE /object-deltas/{id}`                 | Release or cancel this device's file-delta job                                                                          |
| `POST /staged-changes`                       | Single-page convenience staging                                                                                         |
| `POST /staged-changes/start`                 | Begin a multi-page staging set                                                                                          |
| `PUT /staged-changes/{id}/pages/{index}`     | Consecutive pages, identical-page retry, conflicting retry rejected                                                     |
| `GET /staged-changes/{id}`                   | Next page and optional sealed digest                                                                                    |
| `POST /staged-changes/{id}/seal`             | Partition-independent streaming digest                                                                                  |
| `DELETE /staged-changes/{id}`                | Cancellation before operation reservation                                                                               |
| `POST /commits`                              | Durable operation reservation and atomic commit; required `If-Match`                                                    |
| `GET /operations/{id}`                       | Owner-scoped pending status or terminal receipt                                                                         |
| `GET /changes`                               | Fixed-through journal traversal                                                                                         |
| `POST /read-pins`                            | Pin `{epoch,afterSeq}` through current head                                                                             |
| `GET /read-pins/{id}`                        | Pinned journal pages with `afterSeq`, `afterOrdinal`, `limit`                                                           |
| `DELETE /read-pins/{id}`                     | Release this device's traversal                                                                                         |
| `POST /checkpoints`                          | Capture fixed record metadata                                                                                           |
| `GET /checkpoints/{id}`                      | Pages with optional `afterKey` and `limit`                                                                              |
| `DELETE /checkpoints/{id}`                   | Release this device's checkpoint                                                                                        |
| `POST /acks`                                 | Monotonic per-device `{epoch,seq}`                                                                                      |

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
- Native full batches prefer 1 MiB. Upload and resumed Range chunks are 1 MiB,
  with at most two concurrent chunk requests per direction;
  larger useful delta frames retain the 8 MiB wire ceiling. Native recipe/frame
  requests have a 120-second deadline, ordinary requests 30 seconds.
- `RNSB` batches: full, `RNSD` delta, or download-only full-required frames;
  at most 8 MiB including framing, 1024 frames, and 32 MiB aggregate
  materialized full/delta bytes. Full-required carries the exact target hash
  and size; fetch the object through the authenticated `GET /objects/{hash}`
  path and verify its hash and length. Empty-base requests can return full or
  full-required, not a delta.
- `RNSD` is a custom exact COPY/INSERT profile, not VCDIFF: ordered base IDs,
  checked offsets, exact base/target hashes and lengths, no output-copy chains.
  Current encoder/materializer limits: 4 bases, 32 MiB aggregate base bytes,
  16 MiB target, 8 MiB patch, 262,144 operations. The compact sorted anchor index
  is bounded to 524,288 entries (8 MiB), with two matches per fingerprint/base.
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
The settings view reports saving, preparation, local activation, refresh and
publication, plus verified object bytes. This byte counter describes reconstructed
content, not wire traffic; it excludes reused local objects. Progress polling
cannot delay completion or update a subsequent cycle after cancellation.

Conflicts retain the local revision and remote head used for the preview. Both
library sides are preserved as verified references before applying either choice.
These entries are not independent offline backup files. Each side reports whether
its content is available locally, requires the server connection, or is unavailable.
The settings page restores an available side through the validated restore workflow,
which backs up the current library and refreshes plugin state. Export a portable
backup to keep a self-contained file. Restored content remains paused for inspection.
Optional device sections are not included in this library conflict preservation.
Parent/child and order
conflicts are resolved as groups. Plugin clear retains its original scope and
membership, including empty clear and clear followed by an identical set.

After losing device operation history, revoke that device while the daemon is
stopped, issue a new credential, restart the daemon, and select **Register a new
device** in the app. The old device must be revoked before replacement. Local
edits and bases survive this reset and differences require comparison. A changed
server epoch has a separate **Compare restored server** action. Independent,
nonempty libraries are not automatically combined on first registration.

## Verification and distribution

The workflow in `.github/workflows/sync-server-check.yml` defines native runners
for Windows x86_64, Linux x86_64/arm64 and macOS x86_64/arm64. It tests the standalone
crate without building RisuNest, then packages the tested binary. This session did
not dispatch or publish that workflow. See the [runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).

Locally exercised targets on 2026-09-12:

| Target                          | Tested host               |  Release binary | Runtime dependencies                        |
| ------------------------------- | ------------------------- | --------------: | ------------------------------------------- |
| Windows x86_64                  | Windows 11 build 26200    | 5,209,600 bytes | Windows system DLLs, UCRT, VCRUNTIME140.dll |
| Linux x86_64                    | Ubuntu 24.04.4 under WSL2 | 6,945,024 bytes | glibc loader, libc, libm, libgcc_s          |
| Linux arm64, macOS x86_64/arm64 | Not run locally           |    Not measured | Not verified                                |

These hosts do not establish minimum supported OS versions. Linux used an isolated
Rust 1.97.1 toolchain and the same shared Cargo target as Windows. The Windows
server suite passed 50 tests and the Linux release suite passed 51, including the
Unix SIGTERM case. Wire tests passed 19 cases plus an ECMAScript golden-vector
check. Standalone subprocess tests clear PATH and verify serving, directory/port
ownership, missing storage, process loss and verified-chunk resume. Synthetic
SQLite `max_page_count` exhaustion verifies rollback and restart integrity without
filling the host disk. Physical power loss and cross-OS backup restore remain
unverified.

To package an already-tested binary, use Python 3.11+ (packaging only):

```powershell
python server/sync/distribution/package.py --binary E:/Programming/Github/RisuNest/src-tauri/target/release/risunest-sync-server.exe --target x86_64-pc-windows-msvc --output server/sync/distribution/artifacts
```

Archives contain the executable, this operating guide and the repository license,
with a separate SHA-256 checksum. Existing archives are never overwritten. Unix
archives preserve the executable permission even when packaged on Windows.
Packaging does not publish a release.

```powershell
cargo test --manifest-path crates/sync-wire/Cargo.toml --locked
cargo test --manifest-path server/sync/Cargo.toml --locked
node --test crates/sync-wire/tests/golden.test.mjs
cargo clippy --manifest-path server/sync/Cargo.toml --locked --all-targets -- -D warnings
```

Native PDS regression tests passed 343 cases (11 explicit measurement/child tests
ignored in that invocation). Separate native transport tests passed 11, including
exact large-object reconstruction, two concurrent chunk requests in each direction,
413/429/524 retry, accepted-response loss and preserving a successful Range when
its parallel sibling fails. PDS activation tests cover transactional cursor/base
updates, outbox tails, clear intent, conflict backups and corrupt-payload refusal.
The required eight TypeScript sync suites passed 79 cases; svelte-check reported
no errors or warnings. The latest isolated Windows agent app passed actual form
registration, synchronization and credential recovery after process restart.
The latest Android agent APK passed Keystore recovery, registration, progress and
mobile layout checks. During a 35-second server stall, local settings interaction
completed in 5,687 ms and the head stayed unchanged. Both Windows and Android apps
also passed registration and restart against the Linux x86_64 release daemon.
Apple/Linux application key-store runtime checks remain unverified, separately
from the Linux daemon tests.

## Measured transfer behavior

Measurements use synthetic content. HTTP totals include request and response
headers, but exclude TLS unless stated. Debug native tests are not release CPU
benchmarks, and a combined test-process working set is not daemon RSS.

- A 72-byte message append at 1/127/128/129/1024/10000 existing messages used
  14,595–15,263 upload and 10,240–10,578 download HTTP bytes. Every case met
  D + 16 KiB in both directions.
- The 13-case native matrix covers prefix/middle insertion and deletion, message
  edit/reorder, root, preset, lorebook, Unicode plugin key, new conversation and
  asset metadata. A six-byte insertion in a 10 MiB plugin string used 13,543
  upload / 9,841 download bytes. The remaining twelve cases used at most 12,659
  upload / 9,330 download bytes, with exact values and order checked.
- A 22-byte insertion into a 17 MiB opaque file used 6,974 upload / 6,253 download
  bytes (2,343/2,408 ms). The 1 GiB case used 8,530/7,630 bytes
  (119,820/143,202 ms). Both verify the entire reconstructed hash. The observed
  combined native test-process peak was 68,096,000 bytes, not daemon RSS.
- A 100,000-reference control-tree regression used 3,311 upload / 3,558 download
  bytes. It excludes raw asset bodies. The separate end-to-end 100,000-unique-CAS
  gate includes those bodies; its result is recorded separately in the plan.
- A 2 MiB + 17-byte full object round trip used 10 requests and 4,200,473 HTTP
  bytes with two concurrent chunks per direction. At a per-socket 10 Mbps cap
  and 80 ms request delay, upload/download took 1,817/1,296 ms; at 1 Mbps and
  200 ms, 9,746/9,005 ms. Two sockets can together exceed one socket's cap.
  These are injected request delays, not measured Internet RTT or Tunnel tests.

Separate release-daemon measurements keep the client, GUI and cloudflared outside
the reported memory. Four distinct uncached 8 MiB delta targets each use four
bases totalling 32 MiB. Exact reconstruction is asserted for all four devices.

| Host       | Idle resident/working-set bytes | Four-transfer peak bytes |  Head P95 |
| ---------- | ------------------------------: | -----------------------: | --------: |
| Windows    |                      11,603,968 |               64,847,872 | 12.735 ms |
| Linux WSL2 |                       7,532,544 |              103,792,640 |  0.429 ms |

Both passed the candidate 64 MiB idle / 128 MiB peak / 50 ms head gates on this
local SSD host. The earlier allocation-per-anchor implementation failed at
516,591,616 bytes; bounded sorted indexing and one materializer fixed that case.

A separate 500-character, 10 MiB plugin fixture measures local costs independently
of wire bytes. Nine small root saves per configuration had median commit times
of 271/457 microseconds without a pinned revision (outbox off/on), and 939/882
microseconds with a pinned revision. Reading/parsing the large record took about
1.00–1.41 s, encoding/caching 1.47–1.82 s, and restoration 1.17–1.67 s. These debug
samples ran alongside other synthetic IO; they do not establish a statistical
outbox overhead or eliminate per-record JSON materialization.

See [the benchmark guide](../../benchmarks/sync-server/README.md) for the
single-object/batch/concurrency/gzip comparison and separately counted TLS 1.3
handshakes. Product Range and batch transport currently use identity bytes.
Local transfer caches remain retained after disconnect; server-side retention,
leases and GC are implemented. Registry, daemon GUI/tray, service installation,
peer transfer optimization, content E2EE and same-PC deduplication are separate.
