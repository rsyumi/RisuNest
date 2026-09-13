import { invoke as nativeInvoke } from "@tauri-apps/api/core";
import { pluginDeviceStorage } from "../../src/ts/plugins/pluginDeviceStorage";
import {
  captureDeviceSection,
  databaseSectionId,
  type DeviceSpool,
  type DeviceSectionId,
} from "../../src/ts/storage/deviceBackup/scopes";
import {
  runDeviceMaintenance,
  type DeviceMaintenanceBootstrap,
} from "../../src/ts/storage/deviceBackup/maintenance";
import type { DeviceNativeInvoke } from "../../src/ts/storage/deviceBackup/nativeSpool";

const key = "risunest-synthetic-device-smoke";
const faultKey = "risunest-synthetic-device-smoke-fault";
const wait = (milliseconds: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, milliseconds));
interface Control {
  stage:
    | "ready"
    | "export"
    | "restore"
    | "cancel"
    | "done"
    | "fault-restore"
    | "fault-recovered";
  jobId?: string;
  backupPath?: string;
  expected?: string[];
  revision: number;
  captureMs?: number;
  restoreMs?: number;
  startedAt?: number;
  cancellationAt?: number;
  cancellationMs?: number;
  maxIpcBytes: number;
  totalIpcBytes: number;
  peakHeapBytes: number;
  checks: string[];
  failure?: string;
  fault?: {
    point: "before-commit" | "after-device-marker";
    reached: boolean;
    oldExpected: string[];
    newExpected: string[];
  };
}
const sections: DeviceSectionId[] = [
  "local-storage",
  "localforage",
  databaseSectionId("safe_plugin_smoke_records"),
  databaseSectionId("safe_plugin_smoke_writers"),
];
let control: Control = JSON.parse(
  localStorage.getItem(faultKey) ?? sessionStorage.getItem(key) ?? "null",
) ?? {
  stage: "ready",
  revision: 0,
  maxIpcBytes: 0,
  totalIpcBytes: 0,
  peakHeapBytes: 0,
  checks: [],
};
function save() {
  sessionStorage.setItem(key, JSON.stringify(control));
  if (control.fault) localStorage.setItem(faultKey, JSON.stringify(control));
}
function check(value: unknown, label: string) {
  if (!value) throw new Error(label);
  control.checks.push(label);
  save();
}
function heap() {
  control.peakHeapBytes = Math.max(
    control.peakHeapBytes,
    (performance as Performance & { memory?: { usedJSHeapSize: number } })
      .memory?.usedJSHeapSize ?? 0,
  );
}
async function hash(body: Blob | ArrayBuffer) {
  const bytes = body instanceof Blob ? await body.arrayBuffer() : body;
  return Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
    (value) => value.toString(16).padStart(2, "0"),
  ).join("");
}
function environment(signal?: AbortSignal) {
  return {
    localStorage,
    localforage: pluginDeviceStorage,
    indexedDB,
    keyRange: IDBKeyRange,
    signal,
    barrier: { assertHeld() {} },
    estimateStorage: () => navigator.storage.estimate(),
  };
}
async function fingerprints() {
  const results: string[] = [];
  for (const sectionId of sections) {
    let parts: string[] = [];
    const spool: DeviceSpool = {
      async beginSection(_id, metadata) {
        parts = [JSON.stringify(metadata)];
      },
      async appendRow(_id, row) {
        parts.push(JSON.stringify(row));
      },
      async finishSection(id) {
        results.push(
          await hash(new TextEncoder().encode(parts.join("\n")).buffer),
        );
        return {
          sectionId: id,
          metadata: JSON.parse(parts[0]),
          recordCount: parts.length - 1,
          digest: results.at(-1)!,
        };
      },
      async sections() {
        throw new Error("comparison sections unused");
      },
      async *rows() {
        throw new Error("comparison rows unused");
      },
      putBinary: hash,
      async getBinary() {
        throw new Error("comparison object read unused");
      },
    };
    await captureDeviceSection(sectionId, spool, environment());
  }
  return results;
}
function request<T>(value: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    value.onsuccess = () => resolve(value.result);
    value.onerror = () => reject(value.error);
  });
}
async function database(
  name: string,
  version?: number,
  upgrade?: (db: IDBDatabase) => void,
) {
  const opening = indexedDB.open(name, version);
  if (upgrade) opening.onupgradeneeded = () => upgrade(opening.result);
  return request(opening);
}
async function transaction(
  db: IDBDatabase,
  store: string,
  write: (store: IDBObjectStore) => void,
) {
  await new Promise<void>((resolve, reject) => {
    const tx = db.transaction(store, "readwrite");
    tx.oncomplete = () => resolve();
    tx.onabort = () => reject(tx.error);
    tx.onerror = () => {};
    write(tx.objectStore(store));
  });
}
async function seed() {
  localStorage.setItem("safe_plugin_smoke_text", "synthetic\u0000\ud800🙂");
  localStorage.setItem("outside_smoke_sentinel", "untouched");
  const buffer = new Uint8Array(8 * 1024 * 1024);
  buffer.fill(53);
  const graph: any = {
    array: [, undefined, NaN, -0, Infinity],
    map: new Map(),
    set: new Set([1n, "synthetic"]),
    blob: new Blob([buffer]),
    view: new Uint16Array(buffer.buffer, 4, 8),
  };
  Object.defineProperty(graph, "__proto__", {
    value: { synthetic: true },
    enumerable: true,
    writable: true,
    configurable: true,
  });
  graph.self = graph;
  graph.map.set(graph, graph.view);
  await pluginDeviceStorage.setItem("safe_plugin_smoke_graph", graph);
  await pluginDeviceStorage.setItem("outside_smoke_sentinel", "untouched");
  const db = await database("safe_plugin_smoke_records", 3, (value) => {
    const store = value.createObjectStore("records", { autoIncrement: true });
    store.createIndex("tag", "tag");
    value.createObjectStore("inline", { keyPath: "id", autoIncrement: true });
  });
  await transaction(db, "records", (store) => {
    store.add({ tag: "synthetic", nested: graph }, 1);
    store.add({ tag: "deleted" }, 41);
    store.delete(41);
  });
  await transaction(db, "inline", (store) => {
    store.add({ id: 7, text: "synthetic" });
  });
  // Keep a raw connection alive intentionally, proving that navigation closes it.
  (globalThis as any).__smokeRawConnection = db;
  const writers = await database("safe_plugin_smoke_writers", 1, (value) =>
    value.createObjectStore("counters"),
  );
  await transaction(writers, "counters", (store) => {
    store.put(0, "worker");
    store.put(0, "iframe");
  });
  writers.close();
  const writerCode = (entry: string) =>
    `const r=indexedDB.open('safe_plugin_smoke_writers');r.onsuccess=()=>{const db=r.result;setInterval(()=>{const t=db.transaction('counters','readwrite');const s=t.objectStore('counters');const r=s.get('${entry}');r.onsuccess=()=>s.put((r.result||0)+1,'${entry}');},10);};`;
  (globalThis as any).__smokeWorker = new Worker(
    URL.createObjectURL(
      new Blob([writerCode("worker")], { type: "application/javascript" }),
    ),
  );
  const iframe = document.createElement("iframe");
  iframe.srcdoc = `<script>${writerCode("iframe")}</script>`;
  document.body.append(iframe);
  await wait(200);
}
async function writerValues() {
  const db = await database("safe_plugin_smoke_writers");
  const tx = db.transaction("counters", "readonly");
  const store = tx.objectStore("counters");
  const values = await Promise.all([
    request(store.get("worker")),
    request(store.get("iframe")),
  ]);
  db.close();
  return values;
}
async function mutate() {
  localStorage.setItem("safe_plugin_smoke_text", "replaced");
  localStorage.setItem("safe_plugin_smoke_extra", "remove");
  await pluginDeviceStorage.removeItem("safe_plugin_smoke_graph");
  await pluginDeviceStorage.setItem("safe_plugin_smoke_extra", "remove");
  for (const name of [
    "safe_plugin_smoke_records",
    "safe_plugin_smoke_writers",
  ]) {
    await request(indexedDB.deleteDatabase(name));
    const db = await database(name, 9, (value) =>
      value.createObjectStore("wrong"),
    );
    db.close();
  }
}
async function jobStatus() {
  return nativeInvoke<any>("native_file_job_status", { jobId: control.jobId });
}
async function toMaintenance() {
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    const status = await jobStatus();
    if (status.phase === "awaiting-device-maintenance") {
      let rejected = false;
      try {
        await nativeInvoke("pds_open");
      } catch {
        rejected = true;
      }
      check(rejected, "native-pds-fence-held");
      save();
      location.reload();
      return;
    }
    if (["failed", "cancelled"].includes(status.state))
      throw new Error(`job-start-${status.error?.code ?? status.state}`);
    await wait(20);
  }
  throw new Error("maintenance-wait-timeout");
}
async function terminal() {
  const deadline = Date.now() + 120000;
  while (Date.now() < deadline) {
    const status = await jobStatus();
    if (["succeeded", "failed", "cancelled"].includes(status.state))
      return status;
    await wait(20);
  }
  throw new Error("terminal-wait-timeout");
}
async function start() {
  check(control.stage === "ready", "synthetic-fresh-start");
  const opened = await nativeInvoke<{ revision: number }>("pds_open");
  control.revision = opened.revision;
  await seed();
  let writers = await writerValues();
  const writerDeadline = Date.now() + 4000;
  while (writers.some((value) => value === 0) && Date.now() < writerDeadline) {
    await wait(100);
    writers = await writerValues();
  }
  check(
    writers.every((value) => value > 0),
    "synthetic-worker-and-iframe-writing",
  );
  control.stage = "export";
  control.startedAt = Date.now();
  save();
  const started = await nativeInvoke<{ jobId: string }>(
    "native_file_job_start",
    {
      request: {
        kind: "export-portable-backup",
        expectedRevision: control.revision,
        selection: { library: false, deviceSections: sections },
      },
    },
  );
  control.jobId = started.jobId;
  save();
  await toMaintenance();
}

async function restoreArchive(
  peer: { archivePath: string; expected: string[] },
  faultPoint?: "before-commit" | "after-device-marker",
) {
  check(
    control.stage === "done" || control.stage === "fault-recovered",
    "synthetic-completed-fixture-before-exchange",
  );
  check(
    typeof peer.archivePath === "string" &&
      peer.archivePath.length > 0 &&
      peer.expected.length === sections.length &&
      peer.expected.every((value) => /^[a-f0-9]{64}$/.test(value)),
    "synthetic-peer-descriptor-valid",
  );
  const jobs = await nativeInvoke<Array<{ jobId: string; state: string }>>(
    "native_file_job_list",
  );
  if (jobs.some((job) => job.jobId === control.jobId)) {
    check(
      jobs.some(
        (job) =>
          job.jobId === control.jobId &&
          ["succeeded", "failed", "cancelled"].includes(job.state),
      ),
      "synthetic-previous-job-terminal",
    );
    await nativeInvoke("native_file_job_forget", { jobId: control.jobId });
  }
  await nativeInvoke("pds_open");
  await mutate();
  control.failure = undefined;
  control.expected = peer.expected;
  control.backupPath = peer.archivePath;
  control.stage = faultPoint ? "fault-restore" : "restore";
  control.fault = faultPoint
    ? {
        point: faultPoint,
        reached: false,
        oldExpected: await fingerprints(),
        newExpected: peer.expected,
      }
    : undefined;
  localStorage.removeItem(faultKey);
  control.startedAt = Date.now();
  save();
  const started = await nativeInvoke<{ jobId: string }>(
    "native_file_job_start",
    {
      request: {
        kind: "restore-portable-backup",
        source: { type: "desktopPath", path: peer.archivePath },
        expectedRevision: control.revision,
        selection: { library: false, deviceSections: sections },
      },
    },
  );
  control.jobId = started.jobId;
  save();
  await toMaintenance();
}

async function resume() {
  const bootstrap = await nativeInvoke<DeviceMaintenanceBootstrap>(
    "native_device_backup_bootstrap",
    { freshBootstrap: true },
  );
  if (bootstrap.mode === "maintenance") {
    const cancellation = new AbortController();
    const invoke: DeviceNativeInvoke = async (command, args) => {
      const size = Array.isArray(args?.bytes) ? args.bytes.length : 0;
      control.maxIpcBytes = Math.max(control.maxIpcBytes, size);
      control.totalIpcBytes += size;
      heap();
      const result = await nativeInvoke(command, args);
      if (
        control.stage === "fault-restore" &&
        control.fault &&
        !control.fault.reached &&
        ((control.fault.point === "before-commit" &&
          command === "native_device_backup_section_complete" &&
          args?.rollback === false) ||
          (control.fault.point === "after-device-marker" &&
            command === "native_device_backup_finish_device"))
      ) {
        // The runner kills only this synthetic native process after observing
        // this durable boundary. No product command or module is instrumented.
        control.fault.reached = true;
        save();
        await new Promise<never>(() => {});
      }
      if (
        control.stage === "cancel" &&
        command === "native_device_backup_blob_append" &&
        !cancellation.signal.aborted
      ) {
        control.cancellationAt = Date.now();
        save();
        await nativeInvoke("native_file_job_cancel", { jobId: control.jobId });
        cancellation.abort();
      }
      return result as never;
    };
    await runDeviceMaintenance(bootstrap, {
      invoke,
      environment: environment(cancellation.signal),
      wait,
      async assertExclusiveWriters() {
        if (
          navigator.serviceWorker?.controller ||
          (await navigator.serviceWorker?.getRegistrations())?.length
        )
          throw new Error("unexpected-synthetic-service-worker");
      },
      view: {
        progress() {
          heap();
        },
        async reviewReplacement(plan) {
          check(
            plan.some((item) => item.deletionCount > 0),
            "deletion-plan-before-restore",
          );
          return true;
        },
        async completed() {
          control.checks.push("result-acknowledged-before-normal-bootstrap");
          save();
        },
        failed() {
          throw new Error("maintenance-recovery-blocked");
        },
      },
    });
  }
  if (control.stage === "fault-restore") {
    check(!!control.fault?.reached, "synthetic-process-kill-point-reached");
    const current = await nativeInvoke<DeviceMaintenanceBootstrap>(
      "native_device_backup_bootstrap",
    );
    check(current.mode === "normal", "recovery-before-normal-bootstrap");
    await nativeInvoke("pds_open");
    const expected =
      control.fault!.point === "before-commit"
        ? control.fault!.oldExpected
        : control.fault!.newExpected;
    check(
      JSON.stringify(await fingerprints()) === JSON.stringify(expected),
      control.fault!.point === "before-commit"
        ? "process-kill-recovers-entire-old-state"
        : "process-kill-recovers-entire-committed-state",
    );
    check(
      localStorage.getItem("outside_smoke_sentinel") === "untouched" &&
        (await pluginDeviceStorage.getItem("outside_smoke_sentinel")) ===
          "untouched",
      "process-kill-outside-prefix-preserved",
    );
    control.stage = "fault-recovered";
    save();
  } else if (control.stage === "export") {
    const status = await terminal();
    check(
      status.state === "succeeded",
      `capture-${status.error?.code ?? status.state}`,
    );
    await nativeInvoke("pds_open");
    control.captureMs = Date.now() - control.startedAt!;
    control.backupPath = status.result.handoffPath;
    check(typeof control.backupPath === "string", "managed-archive-produced");
    const first = await writerValues();
    await wait(250);
    check(
      JSON.stringify(first) === JSON.stringify(await writerValues()),
      "worker-and-iframe-stopped-after-navigation",
    );
    control.expected = await fingerprints();
    save();
    await mutate();
    await nativeInvoke("native_file_job_forget", { jobId: control.jobId });
    control.stage = "restore";
    control.startedAt = Date.now();
    save();
    const started = await nativeInvoke<{ jobId: string }>(
      "native_file_job_start",
      {
        request: {
          kind: "restore-portable-backup",
          source: { type: "desktopPath", path: control.backupPath },
          expectedRevision: control.revision,
          selection: { library: false, deviceSections: sections },
        },
      },
    );
    control.jobId = started.jobId;
    save();
    await toMaintenance();
  } else if (control.stage === "restore") {
    const status = await terminal();
    check(
      status.state === "succeeded",
      `restore-${status.error?.code ?? status.state}`,
    );
    await nativeInvoke("pds_open");
    control.restoreMs = Date.now() - control.startedAt!;
    check(
      JSON.stringify(control.expected) === JSON.stringify(await fingerprints()),
      "all-selected-graphs-schema-generator-and-bytes-roundtrip",
    );
    check(
      localStorage.getItem("outside_smoke_sentinel") === "untouched" &&
        (await pluginDeviceStorage.getItem("outside_smoke_sentinel")) ===
          "untouched",
      "outside-prefix-preserved",
    );
    await nativeInvoke("native_file_job_forget", { jobId: control.jobId });
    control.stage = "cancel";
    save();
    const started = await nativeInvoke<{ jobId: string }>(
      "native_file_job_start",
      {
        request: {
          kind: "export-portable-backup",
          expectedRevision: control.revision,
          selection: { library: false, deviceSections: ["localforage"] },
        },
      },
    );
    control.jobId = started.jobId;
    save();
    await toMaintenance();
  } else if (control.stage === "cancel") {
    const status = await terminal();
    check(status.state !== "succeeded", "capture-cancelled-before-publication");
    control.cancellationMs = Date.now() - control.cancellationAt!;
    check(control.maxIpcBytes <= 256 * 1024, "bounded-ipc-chunks");
    await nativeInvoke("pds_open");
    check(true, "normal-store-reopens-after-cancellation");
    control.stage = "done";
    save();
  }
}
(globalThis as any).__deviceBackupSmoke = {
  state: () => ({ ...control }),
  start: () =>
    entryCompletion
      .then(() => {
        if (control.failure) throw new Error(control.failure);
        return start();
      })
      .catch(fail),
  restoreControl: (value: Control) =>
    entryCompletion.then(() => {
      if (control.failure) throw new Error(control.failure);
      if (
        !["ready", "done", "fault-recovered"].includes(control.stage) ||
        !["done", "fault-recovered"].includes(value.stage)
      )
        throw new Error("Synthetic fixture cannot be rehydrated while active");
      control = { ...value, fault: undefined, failure: undefined };
      localStorage.removeItem(faultKey);
      save();
    }),
  restoreArchive: (
    peer: { archivePath: string; expected: string[] },
    faultPoint?: "before-commit" | "after-device-marker",
  ) => entryCompletion.then(() => restoreArchive(peer, faultPoint)).catch(fail),
};
function fail(error: unknown) {
  control.failure = error instanceof Error ? error.message : String(error);
  save();
  document.getElementById("result")!.textContent =
    "Synthetic device backup smoke failed";
}
const entryCompletion = resume().catch(fail);
