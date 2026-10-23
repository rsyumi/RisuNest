import "../../src/ts/polyfill";
import { invoke as nativeInvoke } from "@tauri-apps/api/core";
import { join } from "@tauri-apps/api/path";
import { nativeDataPath } from "../../src/ts/storage/nativePaths";
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
  type NativeFileJobStatus,
} from "../../src/ts/storage/nativeFileJobs";
import {
  createNativeHypaEmbeddingCache,
  type HypaEmbeddingEntry,
} from "../../src/ts/storage/hypaEmbeddingCache";
import { createNativeDeviceSettings } from "../../src/ts/storage/nativeDeviceSettings";
import { getDeviceMarkers, initializeDeviceMarkers } from '../../src/ts/storage/deviceMarkers';
import { updateDeviceSettings } from '../../src/ts/storage/deviceSettings';
import { normalizeDatabaseDefaults, setDatabase } from '../../src/ts/storage/database.svelte';
import { pluginValueMeasurement } from './plugin-values';
import { pluginPerformance } from './plugin-performance';
import {
  capturePersistentRoot,
  createPersistentDataRuntime,
  type PersistentDataRuntime,
} from "../../src/ts/storage/persistentDataRuntime";
import { SqlitePersistentDataStore } from "../../src/ts/storage/sqlitePersistentDataStore";
import { projectScalableWorkingSetAtRevision } from "../../src/ts/storage/workingSetCatalog";

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
  updateDeviceSettings({
    messageSendKey: 'button',
    generationHistoryLimitEnabled: true,
    generationHistoryLimitMultiplier: 3,
  });
  await getDeviceMarkers().flush();
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
  updateDeviceSettings({
    messageSendKey: 'ctrl-shift-enter',
    generationHistoryLimitEnabled: false,
    generationHistoryLimitMultiplier: 4,
  });
  await getDeviceMarkers().flush();
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

let runtimePromise: Promise<PersistentDataRuntime> | undefined;
function syntheticRuntime(): Promise<PersistentDataRuntime> {
  return runtimePromise ??= (async () => {
    const store = new SqlitePersistentDataStore();
    await store.open();
    const root = await store.readRoot();
    let database = normalizeDatabaseDefaults(await projectScalableWorkingSetAtRevision(store, root.revision, {
      selectedCharacterId: null,
      selectedConversationId: null,
      activeCharacterIds: new Set(),
    }));
    setDatabase(database);
    const runtime = createPersistentDataRuntime({
      store,
      state: {
        captureRoot: () => capturePersistentRoot(database),
        capturePresets: () => null,
        capturePluginStorage: () => null,
        captureSelectedCharacter: () => null,
        captureCharacter: (id) => database.characters.find((value) => value.chaId === id) ?? null,
        getSelectedCharacterId: () => null,
        getSelectedConversationId: () => null,
        captureWorkingSetDatabase: () => database,
        replaceDatabase: (next) => { database = next; setDatabase(database); },
        publishCharacter(value) {
          const index = database.characters.findIndex((candidate) => candidate.chaId === value.chaId);
          if (index < 0) database.characters.push(value);
          else database.characters[index] = value;
        },
        publishConversation(characterId, conversation, nextCharacter) {
          const character = database.characters.find((value) => value.chaId === characterId);
          if (!character) throw new Error("Synthetic projection parent missing");
          if (nextCharacter) Object.assign(character, nextCharacter);
          const index = character.chats.findIndex((value) => value.id === conversation.id);
          if (index < 0) character.chats.push(conversation);
          else character.chats[index] = conversation;
        },
      },
      prepareDatabase: async (value) => normalizeDatabaseDefaults(structuredClone(value)),
      onLocalRevision(revision) { control.revision = revision; save(); },
    });
    await runtime.initializeActiveWorkingSet(database);
    check(runtime.revision === root.revision, "real-native-runtime-initialized");
    return runtime;
  })();
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
    await syntheticRuntime(),
    { type: "desktopPath", path: destination },
    { library: true, deviceSections: sections },
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
    await syntheticRuntime(),
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
          preview.libraryIncluded &&
            sections.every((section) =>
              preview.deviceSections.includes(section),
            ),
          "native-section-preview-matches",
        );
        return { library: true, deviceSections: sections };
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
    await nativeDataPath(),
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
      await syntheticRuntime(),
      {
        type: "desktopPath",
        path: await join(await nativeDataPath(), "synthetic-cancelled.risunest"),
      },
      { library: true, deviceSections: sections },
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
  await nativeInvoke('pds_open');
  const recovered = await reconcileNativeFileJobsBeforeBootstrap({
    invoke: fileJobDependencies.invoke,
    wait,
  });
  await acknowledgeRecoveredNativeRestores(
    recovered.pendingRestoreAcknowledgements,
    { invoke: fileJobDependencies.invoke, wait },
  );
  await nativeInvoke('pds_open');
  await initializeDeviceMarkers();
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

async function verifySyntheticRoot() {
  const expected = import.meta.env.VITE_DEVICE_SMOKE_ROOT;
  if (!expected && navigator.userAgent.includes('Windows')) throw new Error('Synthetic Windows root is required');
  if (expected) {
    const actual = await nativeDataPath();
    const normalized = (value: string) => value.replaceAll('\\', '/').toLowerCase();
    if (normalized(actual) !== normalized(expected)) throw new Error('Synthetic native root mismatch');
  }
}

const entryCompletion = verifySyntheticRoot()
  .then(() => deviceMaintenanceBeforeBootstrap())
  .then(resume)
  .catch(fail);

(globalThis as typeof globalThis & { __pluginValueMeasurement?: ReturnType<typeof pluginValueMeasurement> }).__pluginValueMeasurement = pluginValueMeasurement({
  async ready() {
    await entryCompletion;
    if (control.failure || control.stage !== 'ready') throw new Error(control.failure ?? 'Fresh synthetic harness required');
    await nativeInvoke('pds_open');
  },
  async backup() {
    const destination = await join(await nativeDataPath(), 'synthetic-plugin-values.risunest');
    await exportArchive(destination);
    return destination;
  },
  restore,
});

let legacyMeasurement: Record<string, unknown> = { phase: 'idle' };
(globalThis as typeof globalThis & { __pluginPerformance?: ReturnType<typeof pluginPerformance> }).__pluginPerformance = pluginPerformance(async () => {
  await entryCompletion;
  if (control.failure || control.stage !== 'ready') throw new Error(control.failure ?? 'Fresh synthetic harness required');
  await nativeInvoke('pds_open');
});

(globalThis as typeof globalThis & { __legacyRestoreMeasurement?: {
  state(): Record<string, unknown>;
  start(megabytes: 100 | 300 | 600, encoding: 'raw' | 'gzip'): Promise<void>;
} }).__legacyRestoreMeasurement = {
  state: () => legacyMeasurement,
  async start(megabytes, encoding) {
    if (legacyMeasurement.phase !== 'idle') throw new Error('Restart the synthetic harness for each memory case');
    legacyMeasurement = { phase: 'preparing', megabytes, encoding };
    try {
      await entryCompletion;
      if (control.failure || control.stage !== 'ready') throw new Error('Fresh synthetic harness required');
      const { runLegacyRestoreMeasurement } = await import('../legacy-restore/run');
      await runLegacyRestoreMeasurement({
        megabytes, encoding,
        assertIsolatedHarness: async () => {
          const { getIdentifier } = await import('@tauri-apps/api/app');
          if (await getIdentifier() !== 'io.github.rsyumi.risunest' || document.title !== 'RisuNest synthetic device backup smoke')
            throw new Error('Synthetic Android benchmark identity required');
        },
        report: async (event) => { legacyMeasurement = event; },
      });
    } catch (error) {
      legacyMeasurement = { ...legacyMeasurement, phase: 'failed', code: error instanceof Error ? error.message : String(error) };
    }
  },
};
