# Endpoint Registry

A small Cloudflare Worker + D1 service that stores an encrypted server URL under a
UUID. This package runs, tests and builds independently of the RisuNest app.

The daemon and native client integration are separate work. This package implements
the opaque store and fixes the wire contract they will consume. It has no accounts,
publisher tokens, leases, listing, endpoint fetching, relay or synchronization data.

## Local development

Requires Node.js 22.12+ and pnpm 10.34.1. Run from this directory:

```powershell
pnpm install --frozen-lockfile
pnpm run db:migrate
pnpm run dev
```

The default local address is `http://localhost:8787`. Local D1 state persists under
`.wrangler/state/`. The zero database ID is an explicit local placeholder.
No Cloudflare login or remote database is needed for these commands.

`dev`, `db:migrate`, `types`, `build`, and the test configuration explicitly use
the committed `wrangler.local.jsonc`. They do not load deployment configuration.

Verification:

```powershell
pnpm run check
pnpm test
pnpm run format:check
pnpm run build
```

`check` generates ignored runtime/binding declarations and checks product, test
and tooling TypeScript separately. `build` is **only a Wrangler dry run**, writing
`dist/`; it does not deploy. Tests run real local workerd and D1 migrations.
Their `*.worker.ts` suffix keeps them out of the app's separate Vitest suite.
The Worker entry imports only `src/`, never tests or synthetic crypto fixtures.

The lockfile pins a compatible Wrangler/Vitest runtime. The compatibility date
`2026-09-03` matches that runtime, avoiding untested date fallback. Upgrade the
date together with the local runtime and rerun verification.

## HTTP contract

Only `/endpoints/{uuid}` exists. UUIDs must be hyphenated UUIDv4 with a valid
RFC variant; hex is case insensitive and normalized to lowercase without a
redirect. Query parameters and extra path segments are rejected.

| Request                  | Success                                                      |
| ------------------------ | ------------------------------------------------------------ |
| `POST /endpoints/{uuid}` | `204 No Content`, after the whole envelope is stored         |
| `GET /endpoints/{uuid}`  | `200 OK`, exact stored envelope, `text/plain; charset=utf-8` |

POST uses `Content-Type: text/plain`, optionally `; charset=utf-8`. Its body is the
**raw unpadded base64url string**, not JSON and not a quoted string. Whitespace,
BOMs, padding, noncanonical unused bits and non-base64url characters are rejected.
Content encoding must be absent or `identity`. The body is read with a byte limit,
including when Content-Length is absent or understates its size.

All application responses include `Cache-Control: no-store`. GET reads the D1
primary through the default binding, without the Sessions API or an edge cache.
GET never creates or updates a record, including on a miss. No CORS/preflight
interface is provided; native clients make these requests.

Errors are JSON with one stable code, for example:

```json
{ "error": "invalid-envelope" }
```

| HTTP status | Error codes                                                                                       |
| ----------- | ------------------------------------------------------------------------------------------------- |
| 400         | `invalid-uuid`, `query-not-allowed`, `invalid-envelope`, `invalid-content-length`, `invalid-body` |
| 404         | `not-found` (missing UUID or unsupported path)                                                    |
| 405         | `method-not-allowed`, with `Allow: GET, POST`                                                     |
| 413         | `body-too-large`                                                                                  |
| 415         | `unsupported-media-type`, `unsupported-content-encoding`                                          |
| 500         | `invalid-configuration` (operator supplied an invalid MAX_RECORDS)                                |
| 503         | `registry-full`, `storage-unavailable`, with `Retry-After: 60`                                    |

Error bodies never contain request values or underlying D1 exceptions. Cloudflare
platform failures before Worker execution, including exhausted Worker quota, can
return their own non-JSON errors. Clients must handle these as service failures.

A repeated POST leaves the same envelope and refreshes its update timestamp.
Anyone knowing a UUID can overwrite it, with no owner credential. An old valid
envelope can be posted again. There
is no freshness/replay guarantee, device-specific lookup revocation or automatic
deletion/expiry on shutdown. The record describes the last completed write, not
whether a server is online. No older RisuNest registry formats are supported.

## Encryption contract for the daemon and native client

- Generate UUIDv4 and an independent CSPRNG 32-byte key once; keep them across restarts.
- Plaintext: the ready HTTPS endpoint URL, UTF-8, including any base path.
- AES-256-GCM, fresh random 12-byte nonce for each encryption, 16-byte tag.
- AAD: lowercase hyphenated UUID string, encoded as UTF-8.
- Envelope: `base64url_without_padding(nonce || ciphertext || tag)`.
- Maximum URL plaintext: 4096 bytes; decoded envelope: 29 through 4124 bytes;
  encoded request body: at most 5499 ASCII bytes.
- A retry of the same publication can resend the same envelope. Never encrypt
  different content using the same nonce under the same key.
- Only use the decrypted URL after successful GCM verification and HTTPS URL
  validation. Reject userinfo, fragments and credential/query parameters. Registry
  base URLs, keys and sync tokens are separate values; keys and sync credentials
  must never be sent to this service.

The Worker only checks encoding and length. It cannot validate GCM tags or URLs;
even a correctly shaped envelope that is cryptographically invalid is stored.
The client must not connect or transmit a sync token after failed authentication.
This package does not yet implement that native connection behavior.

`tests/envelope-vector.json` is a **synthetic, public test vector** generated with
Node's OpenSSL-backed `node:crypto`. Tests independently reproduce it with workerd
Web Crypto and check wrong key, UUID, nonce, ciphertext and tag failures. The
fixed key/nonce are exclusively for interoperability tests.

For a local-only smoke request using that fixture:

```powershell
$vector = Get-Content tests/envelope-vector.json -Raw | ConvertFrom-Json
$uri = "http://127.0.0.1:8787/endpoints/" + $vector.uuid
Invoke-WebRequest -Uri $uri -Method Post -ContentType 'text/plain' -Body $vector.envelope
(Invoke-WebRequest -Uri $uri).Content -eq $vector.envelope
```

Rust interoperability, native key storage, publication scheduling, pairing and
app reconnect/cache behavior remain for the daemon/client work.

## Storage and operation

D1 has one product table:
`endpoints(uuid TEXT PRIMARY KEY, envelope TEXT, updated_at INTEGER)`.
Wrangler additionally manages its migration bookkeeping table. A single atomic
conditional upsert checks capacity and writes, so concurrent inserts cannot
exceed the cap or partially replace a value.

Each successful POST records the server's Unix timestamp in milliseconds in
`updated_at`, including retries with the same envelope. GET and failed POSTs do
not refresh it. A daily Cron Trigger at 00:00 UTC deletes rows whose last update
was at least 30 days before the scheduled time, using an `updated_at` index.
With successful daily runs, deletion occurs between 30 and 31 days after the
last update. Until cleanup, old rows remain readable and count toward capacity.
Failed cleanup invocations report a generic error; the next daily run retries
all eligible rows. Publishers must POST again within 30 days to retain a record,
even if their endpoint URL has not changed. Client GETs do not keep it alive.

The pre-release initial schema is updated in place. Previously initialized local
development databases need a fresh synthetic database; rerunning an already
applied migration does not add the column. No old-schema migration is provided.

`MAX_RECORDS` is a JSON integer from 1 through 10000, default 10000. Existing UUIDs
can still be updated at capacity. Lowering the setting does not evict records;
new UUIDs are refused until the count is below the setting. GET is independent
of this admission setting.

New UUID admission counts existing rows; updating an existing UUID skips that
count. This deliberately keeps the schema to one table. New-registration bursts
can consume D1 read quota well before the storage cap. Count queries, primary-key
index writes (including the timestamp index), daily cleanup, failed capacity
checks and repeated POSTs must be included in the budget; do not equate one HTTP
request with one billed row.

As checked on 2026-09-13, the Free plan includes 100,000 Worker requests/day and
10 ms HTTP CPU time; D1 includes 5 million rows read/day, 100,000 rows written/day
and 5 GB total storage. Quotas are shared with other account usage. Keep the
service on the Free plan; no code or configuration automatically upgrades it.
Measure actual row costs and burst behavior in the Cloudflare dashboard after
separately authorized deployment. Local tests do not establish production cost,
latency or quota behavior.

References: [Workers limits](https://developers.cloudflare.com/workers/platform/limits/),
[D1 pricing](https://developers.cloudflare.com/d1/platform/pricing/),
[D1 primary reads](https://developers.cloudflare.com/d1/best-practices/read-replication/).

Application logging is absent and Workers observability is disabled to avoid
persisting UUID-bearing invocation URLs. Do not add request/body dumps or
`console.error(error)` around D1 calls. Platform networking still exposes
operational metadata such as IP, time and ciphertext size to the operator.

Future clients should use their last successful URL first and query only when
missing, unreachable, or explicitly retried. Apply bounded backoff with jitter,
honor Retry-After and preserve the cache and local library during failures.
Successful connections must not cause periodic registry requests.

If a record is lost, an explicit daemon repost to the same UUID restores it.
If the UUID/key pair is lost or compromised, generate a new pair and share it
again. The registry has no key recovery, ownership recovery or listing API.

## Deployment, separately authorized

No cloud resources are created by this implementation or its local checks.
There is deliberately no deploy npm script. When deployment is requested:

1. Copy the committed template with
   `Copy-Item wrangler.example.jsonc wrangler.jsonc` (first setup only).
   `wrangler.jsonc` is ignored by Git and holds actual deployment values.
2. Create the D1 database with
   `pnpm exec wrangler d1 create endpoint-registry --config wrangler.jsonc`
   and set `database_id` in `wrangler.jsonc` to the returned ID.
3. Select the public address explicitly: enable `workers_dev`, or configure a
   custom domain route in `wrangler.jsonc`. Set the Worker/database names and
   account ID if needed. Preview URLs remain disabled.
4. Apply the migration with
   `pnpm exec wrangler d1 migrations apply DB --remote --config wrangler.jsonc`.
5. Rerun the local verification commands above, then run `pnpm run build:deploy`
   to dry-run the actual deployment configuration. Deploy with
   `pnpm exec wrangler deploy --config wrangler.jsonc`.
6. Verify synthetic POST/GET and the real D1 row costs and quota settings.

The D1 `remote: false` setting keeps development local; it does not substitute
a local DB when the Worker is deployed. Do not enable remote bindings for tests.

Keep shared settings (entry point, compatibility date/flags, bindings, migration
path, Cron schedule and operational defaults) aligned across `wrangler.local.jsonc`,
`wrangler.example.jsonc` and your ignored `wrangler.jsonc` when changing them.
These are complete configurations, not automatically merged overrides.
Commit source, migrations, the local configuration and deployment template;
keep actual resource IDs and domains in `wrangler.jsonc`. Keep API tokens out
of all configuration files; authenticate Wrangler separately. Local Worker
secrets belong in ignored `.dev.vars` files and deployed secrets are configured
with Wrangler's secret commands.
