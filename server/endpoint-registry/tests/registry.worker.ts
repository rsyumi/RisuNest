import { env, exports } from "cloudflare:workers";
import { applyD1Migrations, createScheduledController } from "cloudflare:test";
import { beforeAll, beforeEach, afterEach, expect, it, vi } from "vitest";
import worker from "../src/index";
import { MAX_BODY_BYTES, MAX_ENVELOPE_BYTES } from "../src/protocol";

const UUID = "12345678-1234-4234-9234-123456789abc";
const endpoint = (id = UUID) => `https://registry.invalid/endpoints/${id}`;
const opaque = (byte = 120, length = 40) =>
  btoa(String.fromCharCode(byte).repeat(length))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replaceAll("=", "");
const post = (
  body = opaque(),
  id = UUID,
  headers: Record<string, string> = {},
) =>
  exports.default.fetch(endpoint(id), {
    method: "POST",
    headers: { "content-type": "text/plain", ...headers },
    body,
  });
const count = async () =>
  (await env.DB.prepare("SELECT COUNT(*) AS count FROM endpoints").first<{
    count: number;
  }>())!.count;

beforeAll(async () => {
  await applyD1Migrations(env.DB, env.TEST_MIGRATIONS);
});
beforeEach(async () => {
  await env.DB.exec("DROP TRIGGER IF EXISTS reject_write");
  await env.DB.exec("DELETE FROM endpoints");
});
afterEach(() => {
  vi.restoreAllMocks();
});

it("stores and retrieves the exact opaque envelope without caching", async () => {
  const body = opaque(255);
  const posted = await post(body);
  expect(posted.status).toBe(204);
  expect(await posted.text()).toBe("");
  expect(posted.headers.get("cache-control")).toBe("no-store");
  const fetched = await exports.default.fetch(endpoint());
  expect(fetched.status).toBe(200);
  expect(fetched.headers.get("content-type")).toBe("text/plain; charset=utf-8");
  expect(fetched.headers.get("cache-control")).toBe("no-store");
  expect(await fetched.text()).toBe(body);
  const stored = await env.DB.prepare("SELECT * FROM endpoints").all();
  expect(stored.results).toEqual([
    { uuid: UUID, envelope: body, updated_at: expect.any(Number) },
  ]);
  const timestamp = stored.results[0]!.updated_at as number;
  expect(Number.isInteger(timestamp)).toBe(true);
  expect(Math.abs(Date.now() - timestamp)).toBeLessThan(60_000);
});

it("refreshes the timestamp on identical POSTs but not GET or rejected POSTs", async () => {
  await post();
  const oldTime = Date.now() - 29 * 24 * 60 * 60 * 1000;
  await env.DB.prepare("UPDATE endpoints SET updated_at = ?1 WHERE uuid = ?2")
    .bind(oldTime, UUID)
    .run();
  const readTime = async () =>
    env.DB.prepare("SELECT updated_at FROM endpoints WHERE uuid = ?1")
      .bind(UUID)
      .first<number>("updated_at");
  expect((await exports.default.fetch(endpoint())).status).toBe(200);
  expect((await post("invalid")).status).toBe(400);
  expect(await readTime()).toBe(oldTime);
  const before = Date.now();
  expect((await post()).status).toBe(204);
  expect(await readTime()).toBeGreaterThanOrEqual(before);
  expect(await readTime()).toBeLessThanOrEqual(Date.now());
});

it("deletes at the 30-day boundary, preserves newer rows and frees capacity", async () => {
  const scheduledTime = Date.now();
  const cutoff = scheduledTime - 30 * 24 * 60 * 60 * 1000;
  const ids = Array.from({ length: 3 }, () => crypto.randomUUID());
  for (const [i, offset] of [-1, 0, 1].entries()) {
    await env.DB.prepare(
      "INSERT INTO endpoints (uuid, envelope, updated_at) VALUES (?1, ?2, ?3)",
    )
      .bind(ids[i]!, opaque(), cutoff + offset)
      .run();
  }
  expect((await post()).status).toBe(503);
  // Expired rows remain readable until the scheduled cleanup runs.
  expect((await exports.default.fetch(endpoint(ids[0]!))).status).toBe(200);
  await worker.scheduled(createScheduledController({ scheduledTime }), env);
  expect(await count()).toBe(1);
  for (const id of ids.slice(0, 2)) {
    expect((await exports.default.fetch(endpoint(id))).status).toBe(404);
  }
  expect((await exports.default.fetch(endpoint(ids[2]!))).status).toBe(200);
  expect((await post()).status).toBe(204);
  await worker.scheduled(createScheduledController({ scheduledTime }), env);
  expect(await count()).toBe(2);
});

it("preserves a refreshed row and allows reposting after cleanup", async () => {
  await post();
  const scheduledTime = Date.now();
  await env.DB.prepare("UPDATE endpoints SET updated_at = ?1")
    .bind(scheduledTime - 31 * 24 * 60 * 60 * 1000)
    .run();
  expect((await post()).status).toBe(204);
  await worker.scheduled(createScheduledController({ scheduledTime }), env);
  expect(await count()).toBe(1);
  await worker.scheduled(
    createScheduledController({
      scheduledTime: Date.now() + 31 * 24 * 60 * 60 * 1000,
    }),
    env,
  );
  expect(await count()).toBe(0);
  expect((await post()).status).toBe(204);
  expect(await (await exports.default.fetch(endpoint())).text()).toBe(opaque());
});

it("reports cleanup failures without leaking D1 details and recovers next run", async () => {
  await post();
  await env.DB.exec("UPDATE endpoints SET updated_at = 0");
  const prepare = vi.spyOn(env.DB, "prepare").mockImplementation(() => {
    throw new Error("synthetic-private-db-detail");
  });
  const errorLog = vi.spyOn(console, "error");
  await expect(
    worker.scheduled(
      createScheduledController({ scheduledTime: Date.now() }),
      env,
    ),
  ).rejects.toThrow(/^endpoint-cleanup-failed$/);
  expect(errorLog).not.toHaveBeenCalled();
  prepare.mockRestore();
  expect(await count()).toBe(1);
  await worker.scheduled(
    createScheduledController({ scheduledTime: Date.now() }),
    env,
  );
  expect(await count()).toBe(0);
});

it("normalizes UUID case without redirecting or creating another record", async () => {
  expect((await post(opaque(), UUID.toUpperCase())).status).toBe(204);
  expect((await post(opaque(121))).status).toBe(204);
  expect(await count()).toBe(1);
  const response = await exports.default.fetch(endpoint(UUID.toUpperCase()));
  expect(response.status).toBe(200);
  expect(response.headers.has("location")).toBe(false);
  expect(await response.text()).toBe(opaque(121));
});

it("missing GET is read-only, including after repeated misses", async () => {
  for (let i = 0; i < 3; i++) {
    const response = await exports.default.fetch(endpoint());
    expect(response.status).toBe(404);
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(await response.json()).toEqual({ error: "not-found" });
  }
  expect(await count()).toBe(0);
});

it("existing GET makes no writes", async () => {
  await post();
  await env.DB.exec(
    "CREATE TRIGGER reject_write BEFORE UPDATE ON endpoints BEGIN SELECT RAISE(ABORT, 'synthetic-write-rejected'); END;",
  );
  const response = await exports.default.fetch(endpoint());
  expect(response.status).toBe(200);
  expect(await response.text()).toBe(opaque());
});

it.each([
  ["not-a-uuid", "invalid-uuid"],
  ["12345678-1234-1234-9234-123456789abc", "invalid-uuid"],
  ["12345678-1234-4234-7234-123456789abc", "invalid-uuid"],
  [UUID.replaceAll("-", ""), "invalid-uuid"],
  ["%31" + UUID.slice(1), "invalid-uuid"],
  [UUID + "?key=synthetic", "query-not-allowed"],
])("rejects malformed endpoint %s before writing", async (id, code) => {
  const response = await post(opaque(), id);
  expect(response.status).toBe(400);
  expect(await response.json()).toEqual({ error: code });
  expect(await count()).toBe(0);
});

it.each([
  "/",
  "/endpoints",
  "/endpoints/",
  `/endpoints/${UUID}/`,
  "/health",
  "/relay",
  "/endpoints/a/b",
])("does not expose another route %s", async (path) => {
  const response = await exports.default.fetch(
    "https://registry.invalid" + path,
  );
  expect(response.status).toBe(404);
  expect(response.headers.get("cache-control")).toBe("no-store");
  expect(await count()).toBe(0);
});

it.each(["PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"])(
  "rejects %s and advertises only GET/POST",
  async (method) => {
    const response = await exports.default.fetch(endpoint(), { method });
    expect(response.status).toBe(405);
    expect(response.headers.get("allow")).toBe("GET, POST");
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(await count()).toBe(0);
  },
);

it.each([
  "",
  "a",
  "a===",
  opaque() + "=",
  opaque() + "\n",
  " " + opaque(),
  "{}",
  "https://host.invalid",
  opaque(255).replaceAll("_", "/"),
  opaque(120, 28),
])(
  "rejects invalid envelope %# without replacing a stored value",
  async (body) => {
    await post();
    const response = await post(body);
    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: "invalid-envelope" });
    expect(await (await exports.default.fetch(endpoint())).text()).toBe(
      opaque(),
    );
  },
);

it("rejects nonzero unused base64 bits instead of accepting alternate encodings", async () => {
  const canonical = opaque(0, 29);
  const bad = canonical.slice(0, -1) + "B";
  const response = await post(bad);
  expect(response.status).toBe(400);
  expect(await response.json()).toEqual({ error: "invalid-envelope" });
});

it("accepts both exact envelope size boundaries and rejects a byte beyond the maximum", async () => {
  expect((await post(opaque(1, 29))).status).toBe(204);
  const largest = opaque(255, MAX_ENVELOPE_BYTES);
  expect(largest.length).toBe(MAX_BODY_BYTES);
  expect((await post(largest)).status).toBe(204);
  expect(await (await exports.default.fetch(endpoint())).text()).toBe(largest);
  const rejected = await post(opaque(1, MAX_ENVELOPE_BYTES + 1));
  expect(rejected.status).toBe(413);
  expect(await rejected.json()).toEqual({ error: "body-too-large" });
  expect(await (await exports.default.fetch(endpoint())).text()).toBe(largest);
});

it.each([
  [{ "content-type": "application/json" }, "unsupported-media-type"],
  [{ "content-type": "text/plain; charset=utf-16" }, "unsupported-media-type"],
  [{ "content-encoding": "gzip" }, "unsupported-content-encoding"],
])("rejects unsupported media/encoding %#", async (headers, code) => {
  const response = await post(opaque(), UUID, headers);
  expect(response.status).toBe(415);
  expect(await response.json()).toEqual({ error: code });
  expect(await count()).toBe(0);
});

it("accepts the documented UTF-8 media type and identity encoding", async () => {
  expect(
    (
      await post(opaque(), UUID, {
        "content-type": "TEXT/PLAIN; charset=UTF-8",
        "content-encoding": "identity",
      })
    ).status,
  ).toBe(204);
});

it("retries, unauthenticated overwrites and old-envelope replay remain accepted", async () => {
  for (const body of [opaque(1), opaque(1), opaque(2), opaque(1)]) {
    expect((await post(body)).status).toBe(204);
    expect(await (await exports.default.fetch(endpoint())).text()).toBe(body);
    expect(await count()).toBe(1);
  }
});

it("atomically bounds concurrent registrations but allows updates at capacity", async () => {
  const ids = Array.from({ length: 12 }, () => crypto.randomUUID());
  const responses = await Promise.all(ids.map((id) => post(opaque(), id)));
  expect(responses.filter((r) => r.status === 204)).toHaveLength(3);
  expect(responses.filter((r) => r.status === 503)).toHaveLength(9);
  expect(await count()).toBe(3);
  for (let i = 0; i < responses.length; i++) {
    const response = responses[i]!;
    if (response.status === 503) {
      expect(await response.json()).toEqual({ error: "registry-full" });
      expect(response.headers.get("retry-after")).toBe("60");
    } else {
      expect((await post(opaque(5), ids[i]!)).status).toBe(204);
      expect(
        await (await exports.default.fetch(endpoint(ids[i]!))).text(),
      ).toBe(opaque(5));
    }
  }
  expect(await count()).toBe(3);
});

it("a real SQL write failure leaves the entire old value intact", async () => {
  await post();
  const before = await env.DB.prepare("SELECT * FROM endpoints").all();
  await env.DB.exec(
    "CREATE TRIGGER reject_write BEFORE UPDATE ON endpoints BEGIN SELECT RAISE(ABORT, 'synthetic-private-db-detail'); END;",
  );
  const response = await post(opaque(5));
  expect(response.status).toBe(503);
  expect(response.headers.get("retry-after")).toBe("60");
  expect(await response.json()).toEqual({ error: "storage-unavailable" });
  expect(await (await exports.default.fetch(endpoint())).text()).toBe(opaque());
  expect(
    (await env.DB.prepare("SELECT * FROM endpoints").all()).results,
  ).toEqual(before.results);
});

it("returns bounded errors on D1 outages without leaking or logging exceptions", async () => {
  const log = vi.spyOn(console, "log");
  const errorLog = vi.spyOn(console, "error");
  const warning = vi.spyOn(console, "warn");
  const prepare = vi.spyOn(env.DB, "prepare").mockImplementation(() => {
    throw new Error("synthetic-private-db-detail");
  });
  for (const method of ["GET", "POST"]) {
    const request = new Request(endpoint(), {
      method,
      ...(method === "POST"
        ? { body: opaque(), headers: { "content-type": "text/plain" } }
        : {}),
    });
    const response = await worker.fetch(request, env);
    expect(response.status).toBe(503);
    expect(await response.text()).toBe('{"error":"storage-unavailable"}');
    expect(response.headers.get("cache-control")).toBe("no-store");
  }
  expect(prepare).toHaveBeenCalledTimes(2);
  expect(log).not.toHaveBeenCalled();
  expect(errorLog).not.toHaveBeenCalled();
  expect(warning).not.toHaveBeenCalled();
});

it("never fetches an endpoint or logs a successful request", async () => {
  const outgoing = vi
    .spyOn(globalThis, "fetch")
    .mockRejectedValue(new Error("outbound requests forbidden"));
  const log = vi.spyOn(console, "log");
  const errorLog = vi.spyOn(console, "error");
  const request = new Request(endpoint(), {
    method: "POST",
    headers: { "content-type": "text/plain" },
    body: opaque(),
  });
  expect((await worker.fetch(request, env)).status).toBe(204);
  expect((await worker.fetch(new Request(endpoint()), env)).status).toBe(200);
  expect(outgoing).not.toHaveBeenCalled();
  expect(log).not.toHaveBeenCalled();
  expect(errorLog).not.toHaveBeenCalled();
});

it("rejects invalid capacity configuration without writing", async () => {
  for (const limit of [0, -1, 1.5, 10001, NaN]) {
    const request = new Request(endpoint(), {
      method: "POST",
      headers: { "content-type": "text/plain" },
      body: opaque(),
    });
    const response = await worker.fetch(request, {
      ...env,
      MAX_RECORDS: limit,
    });
    expect(response.status).toBe(500);
    expect(await response.json()).toEqual({ error: "invalid-configuration" });
  }
  expect(await count()).toBe(0);
});
