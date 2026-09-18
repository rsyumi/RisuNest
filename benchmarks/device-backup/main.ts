import "../../src/ts/polyfill";
import { invoke as nativeInvoke } from "@tauri-apps/api/core";
import { appDataDir, join } from "@tauri-apps/api/path";
import {
  PluginDeviceKeyspace,
  createNativePluginDeviceBackend,
} from "../../src/ts/plugins/pluginDeviceKeyspace";
import { deviceMaintenanceBeforeBootstrap } from "../../src/ts/storage/deviceBackup/entry";
import type { NativePortableDeviceSection } from "../../src/ts/storage/deviceBackup/selection";
import {
  acknowledgeRecoveredNativeRestores,
  reconcileNativeFileJobsBeforeBootstrap,
} from "../../src/ts/storage/nativeFileJobRecovery";
import {
  runNativeArchiveExport,
  runNativeArchiveRestore,
  type NativeBackupExportDependencies,
  type NativeBlockRestoreRuntime,
  type NativeFileJobStatus,
} from "../../src/ts/storage/nativeFileJobs";
import {
  createNativeHypaEmbeddingCache,
  type HypaEmbeddingEntry,
} from "../../src/ts/storage/hypaEmbeddingCache";
import { createNativeDeviceSettings } from "../../src/ts/storage/nativeDeviceSettings";

const key = "risunest-synthetic-device-smoke";
const faultKey = "risunest-synthetic-device-smoke-fault";
const pluginOwner = "device-backup-smoke";
const settingKey = "risuNestDeviceSettings";
const vectorCount = 2048;
const vectorDimensions = 1024;
const wait = (milliseconds: number) =>
  new Promise<void>((resolve) => setTimeout(resolve, milliseconds));

type FaultPoint = "activating-database";

interface Control {
  stage:
    | "ready"
    | "export"
    | "restart"
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
  peakHeapBytes: number;
  maxNativeProgressBytes: number;
  statusTransitions: string[];
  checks: string[];
  restartCount: number;
  failure?: string;
  fault?: {
    point: FaultPoint;
    reached: boolean;
    oldExpected: string[];
    newExpected: string[];
    outcome?: "old" | "committed";
  };
}

const sections: NativePortableDeviceSection[] = [
  "hypa",
  "local-plugins",
  "local-settings",
];
const hypaKeys = Array.from({ length: vectorCount }, (_, index) =>
  (index + 1).toString(16).padStart(64, "0"),
);
const extraHypaKey = "f".repeat(64);

let control: Control = JSON.parse(
  localStorage.getItem(faultKey) ?? sessionStorage.getItem(key) ?? "null",
) ?? {
  stage: "ready",
  revision: 0,
  peakHeapBytes: 0,
  maxNativeProgressBytes: 0,
  statusTransitions: [],
  checks: [],
  restartCount: 0,
};

function save() {
  sessionStorage.setItem(key, JSON.stringify(control));
  if (control.fault) localStorage.setItem(faultKey, JSON.stringify(control));
}

function check(value: unknown, label: string) {
  if (!value) throw new Error(label);
  if (!control.checks.includes(label)) control.checks.push(label);
  save();
}

function heap() {
  control.peakHeapBytes = Math.max(
    control.peakHeapBytes,
    (performance as Performance & { memory?: { usedJSHeapSize: number } })
      .memory?.usedJSHeapSize ?? 0,
  );
}

async function hash(parts: BlobPart[]) {
  const bytes = await new Blob(parts).arrayBuffer();
  return Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
    (value) => value.toString(16).padStart(2, "0"),
  ).join("");
}

function pluginKeyspace() {
  return new PluginDeviceKeyspace(
    pluginOwner,
    createNativePluginDeviceBackend(),
  );
}

function embeddingEntries(seed: number): HypaEmbeddingEntry[] {
  return hypaKeys.map((entryKey, index) => {
    const vector = new Float32Array(vectorDimensions);
    vector.fill(seed + index / vectorCount);
    return {
      key: entryKey,
      producer: "device-backup-smoke",
      model: "synthetic",
      endpoint: null,
      preprocessVersion: 1,
      dimensions: vectorDimensions,
      vector: vector.buffer,
    };
  });
}

async function seed() {
  localStorage.setItem("outside_smoke_sentinel", "untouched");
  await createNativeHypaEmbeddingCache().write(embeddingEntries(1));
  const plugins = pluginKeyspace();
  await plugins.setItem("string", "text", "synthetic\u0000🙂");
  await plugins.setItem(
    "json",
    "graph",
    JSON.stringify({ nested: [1, 2, 3], enabled: true }),
  );
  await createNativeDeviceSettings().set(settingKey, {
    enabled: true,
    label: "synthetic",
  });
}

async function mutate() {
  await createNativeHypaEmbeddingCache().write([
    ...embeddingEntries(7),
    {
      key: extraHypaKey,
      producer: "device-backup-smoke",
      model: "synthetic",
      endpoint: null,
      preprocessVersion: 1,
      dimensions: 4,
      vector: new Float32Array([9, 8, 7, 6]).buffer,
    },
  ]);
  const plugins = pluginKeyspace();
  await plugins.setItem("string", "text", "replaced");
  await plugins.removeItem("json", "graph");
  await plugins.setItem("json", "extra", JSON.stringify({ remove: true }));
  await createNativeDeviceSettings().set(settingKey, {
    enabled: false,
    label: "replaced",
    extra: true,
  });
}

async function fingerprints() {
  const cache = await createNativeHypaEmbeddingCache().read([
    ...hypaKeys,
    extraHypaKey,
  ]);
  const hypaParts: BlobPart[] = [];
  for (const entryKey of [...hypaKeys, extraHypaKey]) {
    const value = cache.get(entryKey);
    if (value) {
      hypaParts.push(
        JSON.stringify({ key: entryKey, dimensions: value.dimensions }),
      );
      const vectorBytes = new Uint8Array(value.vector.byteLength);
      vectorBytes.set(
        new Uint8Array(
          value.vector.buffer,
          value.vector.byteOffset,
          value.vector.byteLength,
        ),
      );
      hypaParts.push(vectorBytes);
    } else {
      hypaParts.push(JSON.stringify({ key: entryKey, missing: true }));
    }
  }
  const plugins = pluginKeyspace();
  const pluginParts = await Promise.all(
    (["graph", "extra"] as const).map(async (entryKey) =>
      JSON.stringify([
        "json",
        entryKey,
        await plugins.getItem("json", entryKey),
      ]),
    ),
  );
  pluginParts.push(
    JSON.stringify([
      "string",
      "text",
      await plugins.getItem("string", "text"),
    ]),
  );
  const setting = await createNativeDeviceSettings().get(settingKey);
  return [
    await hash(hypaParts),
    await hash(pluginParts),
    await hash([JSON.stringify(setting)]),
  ];
}

function syntheticRuntime(): NativeBlockRestoreRuntime & {
  readonly revision: number;
  flushPendingData(reason: string): Promise<void>;
} {
  return {
    get revision() {
      return control.revision;
    },
    async flushPendingData() {},
    async capturePersistentMutationToken() {
      return { revision: control.revision, mutationGeneration: 0 };
    },
    async acquireDestructiveReplacementFence(expected) {
      return {
        revision: expected.revision,
        async refreshCommittedWorkingSet(revision) {
          if (revision !== control.revision)
            throw new Error("Synthetic device-only restore changed the library revision");
          // The isolated entry has no DBState projection. Device-only jobs must
          // leave the already-current library revision untouched.
          return { kind: "committed", revision, projection: "applied" };
        },
        release() {},
      };
    },
    markCommittedWorkingSetRefreshRequired() {},
    getStorageAuthorityEpoch() {
      return 0;
    },
  };
}

const fileJobDependencies: NativeBackupExportDependencies = {
  isTauri: () => true,
  invoke: (command, args) =>
    args === undefined ? nativeInvoke(command) : nativeInvoke(command, args),
  wait,
  async copyToAndroidSaf() {
    throw new Error("Synthetic smoke uses an app-owned desktopPath destination");
  },
};

function observeStatus(status: NativeFileJobStatus) {
  const transition = `${status.kind}:${status.state}:${status.phase}`;
  if (control.statusTransitions.at(-1) !== transition)
    control.statusTransitions.push(transition);
  control.maxNativeProgressBytes = Math.max(
    control.maxNativeProgressBytes,
    status.progress.completedBytes,
  );
  heap();
  save();
}

async function assertNativeFence(status: NativeFileJobStatus) {
  const label = `native-pds-fence-held:${status.jobId}`;
  if (control.checks.includes(label)) return;
  let rejected = false;
  try {
    await nativeInvoke("pds_open");
  } catch {
    rejected = true;
  }
  check(rejected, label);
}

async function observeRestoreStatus(status: NativeFileJobStatus) {
  observeStatus(status);
  if (status.phase !== "activating-database") return;
  await assertNativeFence(status);
  if (
    control.stage === "fault-restore" &&
    control.fault &&
    !control.fault.reached
  ) {
    control.fault.reached = true;
    save();
    await new Promise<never>(() => {});
  }
}

async function exportArchive(destination: string) {
  const result = await runNativeArchiveExport(
    syntheticRuntime(),
    { type: "desktopPath", path: destination },
    { library: false, deviceSections: sections },
    {
      pollIntervalMs: 20,
      onStarted(jobId) {
        control.jobId = jobId;
        save();
      },
      onStatus: observeStatus,
    },
    fileJobDependencies,
  );
  check(result.sourceBytes > 0, "native-archive-published");
  check(/^[a-f0-9]{64}$/.test(result.sourceSha256), "native-archive-hashed");
  check(
    control.statusTransitions.some((value) =>
      value.startsWith("export-portable-backup:"),
    ),
    "native-export-job-observed",
  );
  return result;
}

async function restore(destination: string) {
  return runNativeArchiveRestore(
    syntheticRuntime(),
    { type: "desktopPath", path: destination },
    {
      pollIntervalMs: 20,
      onStarted(jobId) {
        control.jobId = jobId;
        save();
      },
      onStatus: observeStatus,
      onNativeStatus: observeRestoreStatus,
      async choosePortableSections(preview) {
        check(
          !preview.libraryIncluded &&
            sections.every((section) =>
              preview.deviceSections.includes(section),
            ),
          "native-section-preview-matches",
        );
        return { library: false, deviceSections: sections };
      },
    },
    fileJobDependencies,
  );
}

async function start() {
  check(control.stage === "ready", "synthetic-fresh-start");
  const opened = await nativeInvoke<{ revision: number }>("pds_open");
  control.revision = opened.revision;
  await seed();
  control.expected = await fingerprints();
  control.stage = "export";
  control.startedAt = Date.now();
  control.backupPath = await join(
    await appDataDir(),
    "synthetic-device-backup.risunest",
  );
  save();
  await exportArchive(control.backupPath);
  control.captureMs = Date.now() - control.startedAt;
  control.stage = "restart";
  save();
  location.reload();
}

async function restoreArchive(
  peer: { archivePath: string; expected: string[] },
  faultPoint?: FaultPoint,
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
  await restore(peer.archivePath);
  if (!faultPoint) await completeRestore();
}

async function completeRestore() {
  control.restoreMs = Date.now() - control.startedAt!;
  check(
    control.statusTransitions.some((value) =>
      value.startsWith("restore-portable-backup:"),
    ),
    "native-restore-job-observed",
  );
  check(
    JSON.stringify(control.expected) === JSON.stringify(await fingerprints()),
    "all-native-sections-and-bytes-roundtrip",
  );
  check(
    localStorage.getItem("outside_smoke_sentinel") === "untouched",
    "outside-native-sections-preserved",
  );
  control.stage = "cancel";
  control.cancellationAt = Date.now();
  save();
  const cancellation = new AbortController();
  let cancellationPhase: string | undefined;
  try {
    await runNativeArchiveExport(
      syntheticRuntime(),
      {
        type: "desktopPath",
        path: await join(await appDataDir(), "synthetic-cancelled.risunest"),
      },
      { library: false, deviceSections: ["hypa"] },
      {
        signal: cancellation.signal,
        pollIntervalMs: 10,
        onStarted(jobId) {
          control.jobId = jobId;
          save();
        },
        onStatus(status) {
          observeStatus(status);
          if (
            !cancellation.signal.aborted &&
            status.state === "running" &&
            status.phase === "writing-export"
          ) {
            cancellationPhase = status.phase;
            cancellation.abort();
          }
        },
      },
      fileJobDependencies,
    );
    throw new Error("capture-cancelled-before-publication");
  } catch (error) {
    if (
      !(error instanceof DOMException && error.name === "AbortError") &&
      !(error instanceof Error && error.name === "AbortError")
    )
      throw error;
  }
  control.cancellationMs = Date.now() - control.cancellationAt;
  check(cancellationPhase === "writing-export", "native-job-cancel-observed");
  await nativeInvoke("pds_open");
  check(true, "normal-store-reopens-after-cancellation");
  control.stage = "done";
  save();
}

async function resume() {
  const recovered = await reconcileNativeFileJobsBeforeBootstrap({
    invoke: fileJobDependencies.invoke,
    wait,
  });
  await acknowledgeRecoveredNativeRestores(
    recovered.pendingRestoreAcknowledgements,
    { invoke: fileJobDependencies.invoke, wait },
  );
  if (control.stage === "fault-restore") {
    check(true, "native-startup-recovery-completed");
    check(!!control.fault?.reached, "synthetic-process-kill-point-reached");
    await nativeInvoke("pds_open");
    const actual = await fingerprints();
    const oldState =
      JSON.stringify(actual) === JSON.stringify(control.fault!.oldExpected);
    const committedState =
      JSON.stringify(actual) === JSON.stringify(control.fault!.newExpected);
    check(
      oldState || committedState,
      "process-kill-recovers-complete-section-set",
    );
    control.fault!.outcome = oldState ? "old" : "committed";
    check(
      localStorage.getItem("outside_smoke_sentinel") === "untouched",
      "process-kill-outside-native-sections-preserved",
    );
    control.stage = "fault-recovered";
    save();
  } else if (control.stage === "restart") {
    control.restartCount += 1;
    check(control.restartCount === 1, "renderer-restart-observed");
    check(
      JSON.stringify(control.expected) === JSON.stringify(await fingerprints()),
      "native-fixture-survives-renderer-restart",
    );
    await mutate();
    control.stage = "restore";
    control.startedAt = Date.now();
    save();
    await restore(control.backupPath!);
    await completeRestore();
  } else if (control.stage === "restore") {
    throw new Error("native-restore-wrapper-did-not-settle");
  } else if (["export", "cancel"].includes(control.stage)) {
    throw new Error("native-file-job-wrapper-did-not-settle");
  }
}

(globalThis as typeof globalThis & {
  __deviceBackupSmoke?: {
    state(): Control;
    start(): Promise<void>;
    restoreControl(value: Control): Promise<void>;
    restoreArchive(
      peer: { archivePath: string; expected: string[] },
      faultPoint?: FaultPoint,
    ): Promise<void>;
  };
}).__deviceBackupSmoke = {
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
      control = {
        ...value,
        fault: undefined,
        failure: undefined,
        statusTransitions: [],
        maxNativeProgressBytes: 0,
      };
      localStorage.removeItem(faultKey);
      save();
    }),
  restoreArchive: (peer, faultPoint) =>
    entryCompletion.then(() => restoreArchive(peer, faultPoint)).catch(fail),
};

function fail(error: unknown) {
  control.failure = error instanceof Error ? error.message : String(error);
  save();
  document.getElementById("result")!.textContent =
    "Synthetic device backup smoke failed";
}

const entryCompletion = deviceMaintenanceBeforeBootstrap()
  .then(resume)
  .catch(fail);
