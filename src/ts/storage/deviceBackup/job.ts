import {
  AndroidSafDestinationError,
  acknowledgeAndroidSafExport,
  copyNativeExportToAndroidSaf,
  getAndroidSafExportSourceId,
  getAndroidSafExportStatus,
  listenAndroidSafDestinationEvents,
  type AndroidSafDestinationEvent,
  type AndroidSafJavascriptBridge,
} from "../androidSafBridge";
import type { NativeFileJobResult } from "../nativeFileJobs";
import {
  requestDeviceMaintenanceRestart,
  type DeviceMaintenanceBootstrap,
} from "./maintenance";
import type { DeviceNativeInvoke } from "./nativeSpool";

export interface PortableJobStatus {
  jobId: string;
  kind: "export-portable-backup" | "restore-portable-backup";
  state:
    | "queued"
    | "running"
    | "waitingForInput"
    | "cancelling"
    | "succeeded"
    | "failed"
    | "cancelled";
  phase: string;
  expectedRevision?: number;
  deviceSessionId?: string;
  result?: NativeFileJobResult;
  error?: { code: string; message: string };
}

export interface PortableMaintenanceOwnership {
  /** Revision flushed under the caller's operation ownership, before starting the native job. */
  flushedRevision: number;
  /** Throw if the initiating operation no longer owns the current runtime. */
  assertHeld(): void;
}

/** Call from the portable job poll loop. A return of false means keep polling. */
export async function handlePortableDeviceMaintenanceStatus(
  status: PortableJobStatus,
  ownership: PortableMaintenanceOwnership,
  dependencies: { invoke: DeviceNativeInvoke; reload?: () => void },
): Promise<false> {
  if (
    status.state !== "waitingForInput" ||
    status.phase !== "awaiting-device-maintenance"
  )
    return false;
  ownership.assertHeld();
  if (
    !status.deviceSessionId ||
    (status.expectedRevision !== undefined &&
      status.expectedRevision !== ownership.flushedRevision)
  )
    throw new Error(
      "Portable backup maintenance ownership does not match the flushed runtime",
    );
  const bootstrap = await dependencies.invoke<DeviceMaintenanceBootstrap>(
    "native_device_backup_bootstrap",
  );
  ownership.assertHeld();
  if (
    bootstrap.mode !== "maintenance" ||
    bootstrap.session?.sessionId !== status.deviceSessionId ||
    bootstrap.session.jobId !== status.jobId
  )
    throw new Error(
      "Portable backup device session does not belong to this file job",
    );
  return requestDeviceMaintenanceRestart(
    dependencies.invoke,
    dependencies.reload,
  );
}

const pendingKey = "risuNestPortableExportIntent";
export interface PendingPortableExport {
  schema: "risunest.portable-export-intent/v1";
  jobId: string;
  publication: "desktop" | "android-saf";
  suggestedName?: string;
  phase: "waiting-native" | "publishing" | "published";
  requestId?: string;
  publicationWarningCodes?: string[];
}
export interface PortableExportIntentStore {
  read(): PendingPortableExport | null;
  write(intent: PendingPortableExport): void;
  clear(jobId: string): void;
}

function validateIntent(
  value: unknown,
): asserts value is PendingPortableExport {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Invalid pending portable export intent");
  const intent = value as Partial<PendingPortableExport>;
  if (
    intent.schema !== "risunest.portable-export-intent/v1" ||
    typeof intent.jobId !== "string" ||
    !/^[A-Za-z0-9_-]{1,128}$/.test(intent.jobId) ||
    !["desktop", "android-saf"].includes(intent.publication) ||
    !["waiting-native", "publishing", "published"].includes(intent.phase) ||
    (intent.publication === "android-saf" &&
      (typeof intent.suggestedName !== "string" ||
        !intent.suggestedName.endsWith(".risunest") ||
        intent.suggestedName.length > 255 ||
        /[\\/\0]/.test(intent.suggestedName))) ||
    (intent.requestId !== undefined &&
      !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(
        intent.requestId,
      )) ||
    (intent.phase === "publishing" && !intent.requestId) ||
    (intent.publicationWarningCodes !== undefined &&
      (!Array.isArray(intent.publicationWarningCodes) ||
        intent.publicationWarningCodes.length > 64 ||
        !intent.publicationWarningCodes.every(
          (code) => typeof code === "string" && /^[a-z0-9-]{1,128}$/.test(code),
        ))) ||
    (intent.publication === "android-saf" &&
      intent.phase === "published" &&
      (!intent.requestId || !intent.publicationWarningCodes))
  )
    throw new Error("Invalid pending portable export intent");
}

/** App-owned handoff state. This key is outside every portable device-data section. */
export function createPortableExportIntentStore(
  storage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = localStorage,
): PortableExportIntentStore {
  const read = () => {
    const text = storage.getItem(pendingKey);
    if (text === null) return null;
    const value: unknown = JSON.parse(text);
    validateIntent(value);
    return value;
  };
  return {
    read,
    write(intent) {
      validateIntent(intent);
      const current = read();
      if (current && current.jobId !== intent.jobId)
        throw new Error("Another portable export still needs acknowledgement");
      const text = JSON.stringify(intent);
      storage.setItem(pendingKey, text);
      if (storage.getItem(pendingKey) !== text)
        throw new Error("Portable export intent could not be persisted");
    },
    clear(jobId) {
      if (read()?.jobId === jobId) storage.removeItem(pendingKey);
    },
  };
}

export function rememberPortableExport(
  jobId: string,
  destination:
    { type: "desktopPath" } | { type: "androidSaf"; suggestedName: string },
  store: PortableExportIntentStore = createPortableExportIntentStore(),
): void {
  store.write({
    schema: "risunest.portable-export-intent/v1",
    jobId,
    publication: destination.type === "androidSaf" ? "android-saf" : "desktop",
    ...(destination.type === "androidSaf"
      ? { suggestedName: destination.suggestedName }
      : {}),
    phase: "waiting-native",
  });
}

export class PortableExportNeedsAttention extends Error {
  constructor(
    public readonly code: string,
    public readonly jobId: string,
    public readonly committedResult?: NativeFileJobResult,
    public readonly warningCodes: string[] = [],
  ) {
    super(`Portable backup export needs attention: ${code}`);
    this.name = "PortableExportNeedsAttention";
  }
}

export interface PortableExportResumeDependencies {
  invoke: DeviceNativeInvoke;
  store: PortableExportIntentStore;
  wait(milliseconds: number): Promise<void>;
  publishAndroid(
    intent: PendingPortableExport,
    result: NativeFileJobResult,
  ): Promise<AndroidSafDestinationEvent>;
  resumeAndroid(
    intent: PendingPortableExport,
    result: NativeFileJobResult,
  ): Promise<AndroidSafDestinationEvent>;
  acknowledgeAndroid(requestId: string): boolean;
  androidAcknowledgementPending?(requestId: string): boolean;
  cleanupHandoff(path: string): Promise<void>;
  onStatus?(status: PortableJobStatus): void;
  onResult?(result: NativeFileJobResult): void | Promise<void>;
}

/** Resume after normal bootstrap. Generic file-job recovery must leave this job retained. */
export async function resumePendingPortableExport(
  dependencies: PortableExportResumeDependencies,
): Promise<NativeFileJobResult | null> {
  let intent = dependencies.store.read();
  if (!intent) return null;
  let status: PortableJobStatus;
  while (true) {
    status = await dependencies.invoke<PortableJobStatus>(
      "native_file_job_status",
      { jobId: intent.jobId },
    );
    if (
      status.jobId !== intent.jobId ||
      status.kind !== "export-portable-backup"
    )
      throw new PortableExportNeedsAttention(
        "job-identity-mismatch",
        intent.jobId,
      );
    dependencies.onStatus?.(status);
    if (["succeeded", "failed", "cancelled"].includes(status.state)) break;
    await dependencies.wait(100);
  }
  if (status.state !== "succeeded")
    throw new PortableExportNeedsAttention(
      status.error?.code ?? status.state,
      intent.jobId,
    );
  if (!status.result)
    throw new PortableExportNeedsAttention(
      "missing-native-result",
      intent.jobId,
    );
  let result = status.result;
  if (intent.publicationWarningCodes)
    result = {
      ...result,
      warningCodes: [
        ...new Set([...result.warningCodes, ...intent.publicationWarningCodes]),
      ],
    };
  if (intent.publication === "android-saf" && intent.phase !== "published") {
    if (!result.handoffPath)
      throw new PortableExportNeedsAttention(
        "missing-android-handoff",
        intent.jobId,
      );
    const continuing = intent.phase === "publishing";
    if (!continuing) {
      intent = {
        ...intent,
        phase: "publishing",
        requestId: crypto.randomUUID(),
      };
      dependencies.store.write(intent);
    }
    let terminal: AndroidSafDestinationEvent;
    try {
      terminal = await (continuing
        ? dependencies.resumeAndroid(intent, result)
        : dependencies.publishAndroid(intent, result));
    } catch (error) {
      if (error instanceof AndroidSafDestinationError)
        throw new PortableExportNeedsAttention(
          error.code,
          intent.jobId,
          undefined,
          error.warningCodes,
        );
      throw error;
    }
    if (
      terminal.requestId !== intent.requestId ||
      terminal.state !== "succeeded" ||
      terminal.bytes !== result.sourceBytes
    )
      throw new PortableExportNeedsAttention(
        terminal.state === "succeeded"
          ? "destination-length-mismatch"
          : (terminal.code ?? terminal.state),
        intent.jobId,
        undefined,
        terminal.warningCodes,
      );
    result = {
      ...result,
      warningCodes: [
        ...new Set([...result.warningCodes, ...terminal.warningCodes]),
      ],
    };
    intent = {
      ...intent,
      phase: "published",
      publicationWarningCodes: terminal.warningCodes,
    };
    dependencies.store.write(intent);
    if (
      !dependencies.acknowledgeAndroid(intent.requestId) &&
      dependencies.androidAcknowledgementPending?.(intent.requestId) !== false
    )
      throw new PortableExportNeedsAttention(
        "destination-acknowledgement-pending",
        intent.jobId,
        result,
      );
  } else if (intent.publication === "android-saf" && intent.requestId) {
    // A previous acknowledgement can have succeeded before the WebView disappeared.
    // The native job keeps the verified handoff until its final forget operation.
    if (
      !dependencies.acknowledgeAndroid(intent.requestId) &&
      dependencies.androidAcknowledgementPending?.(intent.requestId) !== false
    )
      throw new PortableExportNeedsAttention(
        "destination-acknowledgement-pending",
        intent.jobId,
        result,
      );
  }
  try {
    if (result.handoffPath)
      await dependencies.cleanupHandoff(result.handoffPath);
    await dependencies.invoke("native_file_job_forget", {
      jobId: intent.jobId,
    });
  } catch {
    throw new PortableExportNeedsAttention(
      "cleanup-failed",
      intent.jobId,
      result,
    );
  }
  dependencies.store.clear(intent.jobId);
  await dependencies.onResult?.(result);
  return result;
}

function productionAndroidBridge(): AndroidSafJavascriptBridge {
  const bridge = (
    window as Window & { RisuSafBridge?: AndroidSafJavascriptBridge }
  ).RisuSafBridge;
  if (!bridge) throw new Error("Android SAF bridge is unavailable");
  return bridge;
}

/** Reuses the native SAF receipt after renderer loss; never starts a second copy. */
async function resumeAndroidPortablePublication(
  intent: PendingPortableExport,
  result: NativeFileJobResult,
): Promise<AndroidSafDestinationEvent> {
  const bridge = productionAndroidBridge();
  const read = () => {
    const text = getAndroidSafExportStatus(bridge);
    if (!text) return null;
    const event: AndroidSafDestinationEvent = JSON.parse(text);
    if (event.requestId !== intent.requestId)
      throw new PortableExportNeedsAttention(
        "another-android-publication-is-pending",
        intent.jobId,
      );
    return event;
  };
  const terminal = read();
  if (terminal) return terminal;
  const active = getAndroidSafExportSourceId(bridge);
  if (!active || !result.handoffPath?.includes(active))
    throw new PortableExportNeedsAttention(
      "android-publication-retry-required",
      intent.jobId,
    );
  return new Promise((resolve, reject) => {
    const dispose = listenAndroidSafDestinationEvents((event) => {
      if (event.requestId === intent.requestId) {
        dispose();
        resolve(event);
      }
    });
    try {
      const completed = read();
      if (completed) {
        dispose();
        resolve(completed);
      }
    } catch (error) {
      dispose();
      reject(error);
    }
  });
}

export function portableAndroidPublicationDependencies(): Pick<
  PortableExportResumeDependencies,
  | "publishAndroid"
  | "resumeAndroid"
  | "acknowledgeAndroid"
  | "androidAcknowledgementPending"
> {
  return {
    async publishAndroid(intent, result) {
      const copied = await copyNativeExportToAndroidSaf(
        {
          sourcePath: result.handoffPath!,
          suggestedName: intent.suggestedName!,
          deferAcknowledgement: true,
        },
        {
          createRequestId: () => intent.requestId!,
          bridge: productionAndroidBridge(),
          addEventListener: (name, listener) =>
            window.addEventListener(name, listener),
          removeEventListener: (name, listener) =>
            window.removeEventListener(name, listener),
        },
      );
      return {
        requestId: intent.requestId!,
        state: "succeeded",
        bytes: copied.bytes,
        warningCodes: copied.warningCodes,
      };
    },
    resumeAndroid: resumeAndroidPortablePublication,
    acknowledgeAndroid: acknowledgeAndroidSafExport,
    androidAcknowledgementPending() {
      const bridge = productionAndroidBridge();
      return (
        getAndroidSafExportStatus(bridge) !== null ||
        getAndroidSafExportSourceId(bridge) !== null
      );
    },
  };
}
