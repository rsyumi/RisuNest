// A private, synthetic TLS front end. No certificate is installed in OS trust.
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL(".local/", import.meta.url));
const data = mkdtempSync(root + "tls-head-");
const binary =
  process.env.CARGO_TARGET_DIR + "/release/risunest-sync-server.exe";
execFileSync(
  process.env.OPENSSL_EXE || "openssl",
  [
    "req",
    "-new",
    "-newkey",
    "rsa:2048",
    "-x509",
    "-nodes",
    "-days",
    "1",
    "-subj",
    "/CN=localhost",
    "-addext",
    "subjectAltName=IP:127.0.0.1,DNS:localhost",
    "-keyout",
    data + "/key.pem",
    "-out",
    data + "/cert.pem",
  ],
  { windowsHide: true, stdio: "ignore" },
);
const cli = (...args) =>
  execFileSync(binary, [...args, "--data-dir", data + "/server"], {
    windowsHide: true,
    encoding: "utf8",
  });
cli("init");
const credential = JSON.parse(cli("device", "add"));
const daemon = spawn(
  binary,
  ["serve", "--data-dir", data + "/server", "--listen", "127.0.0.1:19427"],
  { windowsHide: true, stdio: "ignore" },
);
const sockets = new Set();
function pipe(socket, port) {
  const upstream = net.connect(port, "127.0.0.1");
  sockets.add(socket);
  sockets.add(upstream);
  socket.on("error", () => upstream.destroy());
  upstream.on("error", () => socket.destroy());
  socket.on("close", () => sockets.delete(socket));
  upstream.on("close", () => sockets.delete(upstream));
  socket.pipe(upstream).pipe(socket);
}
const cert = readFileSync(data + "/cert.pem");
const tlsProxy = tls.createServer(
  { cert, key: readFileSync(data + "/key.pem") },
  (socket) => pipe(socket, 19427),
);
let meter = { bytes: 0 };
let destination = 19427;
const counted = net.createServer((socket) => {
  const measurement = meter;
  const upstream = net.connect(destination, "127.0.0.1");
  sockets.add(socket);
  sockets.add(upstream);
  socket.on("data", (chunk) => {
    measurement.bytes += chunk.length;
  });
  upstream.on("data", (chunk) => {
    measurement.bytes += chunk.length;
  });
  socket.on("error", () => upstream.destroy());
  upstream.on("error", () => socket.destroy());
  socket.on("close", () => sockets.delete(socket));
  upstream.on("close", () => sockets.delete(upstream));
  socket.pipe(upstream).pipe(socket);
});
const listen = (server, port) =>
  new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
async function sample(encrypted, reconnect) {
  meter = { bytes: 0 };
  let protocol = "HTTP";
  destination = encrypted ? 19428 : 19427;
  const module = encrypted ? https : http;
  const agent = new module.Agent({
    keepAlive: !reconnect,
    maxSockets: 1,
    maxCachedSessions: 0,
  });
  const get = (etag) =>
    new Promise((resolve, reject) => {
      const req = module.request(
        {
          hostname: "127.0.0.1",
          port: 19429,
          path: "/head",
          agent,
          ca: cert,
          headers: {
            authorization: `Bearer ${credential.token}`,
            "x-risu-library": credential.libraryId,
            ...(etag ? { "if-none-match": etag } : {}),
          },
        },
        (res) => {
          if (encrypted) protocol = res.socket.getProtocol();
          let length = 0;
          res.on("data", (b) => {
            length += b.length;
          });
          res.on("error", reject);
          res.on("end", () =>
            resolve({ status: res.statusCode, etag: res.headers.etag, length }),
          );
        },
      );
      req.setTimeout(10000, () =>
        req.destroy(new Error("Synthetic TLS timeout")),
      );
      req.on("error", reject);
      req.end();
    });
  try {
    const head = await get();
    assert.equal(head.status, 200);
    assert(head.length <= 1024);
    assert(head.etag);
    meter.bytes = 0;
    const times = [];
    for (let i = 0; i < 20; i++) {
      const start = performance.now();
      const response = await get(head.etag);
      times.push(performance.now() - start);
      assert.equal(response.status, 304);
      assert.equal(response.length, 0);
    }
    times.sort((a, b) => a - b);
    return {
      transport: protocol,
      reconnect,
      sessionResumption: false,
      requests: 20,
      wireBytes: meter.bytes,
      meanMs: times.reduce((a, b) => a + b, 0) / times.length,
      p95Ms: times[18],
      bodyBytes: 0,
    };
  } finally {
    agent.destroy();
  }
}
try {
  await listen(tlsProxy, 19428);
  await listen(counted, 19429);
  await new Promise((r) => setTimeout(r, 500));
  const results = [];
  for (const encrypted of [false, true])
    for (const reconnect of [false, true]) {
      const result = await sample(encrypted, reconnect);
      results.push(result);
      console.log(JSON.stringify(result));
    }
  writeFileSync(root + "tls-head.json", JSON.stringify(results, null, 2));
} finally {
  for (const socket of sockets) socket.destroy();
  await Promise.all([
    new Promise((r) => tlsProxy.close(r)),
    new Promise((r) => counted.close(r)),
  ]);
  daemon.kill();
}
