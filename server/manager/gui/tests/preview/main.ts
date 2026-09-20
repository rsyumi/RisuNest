import { mount } from "svelte";
import App from "../../src/App.svelte";
import "../../src/styles.css";
import type { Backend, Status } from "../../src/api";

// Separate verification entry. Never imported by the product frontend.
let counter = 0;
const status: Status = {
  listener: "127.0.0.1:14319",
  revision: "synthetic:0",
  uptimeSeconds: 7200,
  connection: {
    endpoint: "https://synthetic.example.com",
    cloudflared: null,
    registryUrl: "https://registry.example.com",
    registryEnabled: true,
    uuid: "00000000-0000-4000-8000-000000000001",
  },
  connectionState: { mode: "fixed", publication: "published" },
  tunnel: { phase: "external", endpoint: null, error: null, logs: [] },
  publication: { phase: "published", error: null },
  storage: {
    measuredAt: Math.floor(Date.now() / 1000),
    totalBytes: 1374389534,
    availableBytes: 255550554112,
    dataBytes: 1000000000,
    databaseBytes: 340000000,
    temporaryBytes: 30000000,
    otherBytes: 4389534,
    error: null,
  },
  devices: [
    {
      id: "synthetic-device-1",
      name: "시험 기기",
      revoked: false,
      pending: false,
      registrationRequest: null,
    },
  ],
  defaultRegistryUrl: "https://registry.example.com",
};
const backend: Backend = {
  status: async () => structuredClone(status),
  environment: async () => ({
    network: { schema: 1, address: "127.0.0.1", port: 14319 },
    platform: "windows",
    cloudflared: "C:\\synthetic\\cloudflared.exe",
    dataDir: "synthetic",
    startup: { registered: true, enabled: true, actionMatches: true },
    startupError: null,
    trayStartup: false,
    updateSettings: { schema: "risunest-sync-update-settings/v1", policy: "off" },
    updateStatus: { schema: "risunest-sync-update-status/v1", phase: "idle", targetVersion: null, lastCheckedAt: null, lastCompletedAt: null, deferredUntil: null, reason: null, lastFailedVersion: null },
    updateSchedule: { registered: false, enabled: false, actionMatches: true },
    updateScheduleError: null,
  }),
  mutate: async (path, body) => {
    if (body.revision !== status.revision) throw "management-stale-state";
    status.revision = `synthetic:${++counter}`;
    if (path === "connection")
      status.connection = { ...status.connection, ...(body.options as object) };
    return {};
  },
  start: async () => {},
  network: async () => {},
  startup: async () => ({ registered: true, enabled: true, actionMatches: true }),
  updatePolicy: async () => {},
  updateCheck: async () => ({ result: "current" }),
  trayStartup: async () => {},
  requestId: async () => "a".repeat(64),
  qr: async () => "",
};
mount(App, { target: document.getElementById("app")!, props: { backend } });
