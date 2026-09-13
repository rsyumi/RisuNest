import {
  createNativeDeviceSpool,
  type DeviceNativeInvoke,
} from "./nativeSpool";
import { cleanupPluginDatabaseStages } from "./indexedDb";
import { deviceSectionLabel } from "./selection";
import { CloneGraphError } from "./cloneGraph";
import { verifyCurrentDeviceSection } from "./verify";
import {
  captureDeviceSection,
  stageDeviceSection,
  type DeviceSectionId,
  type DeviceSpool,
  type DeviceStorageEnvironment,
  type StagedDeviceSection,
} from "./scopes";

export interface DeviceMaintenanceSession {
  sessionId: string;
  jobId: string;
  operation: "capture" | "restore";
  phase: string;
  includesLibrary: boolean;
  failureCode?: string | null;
  failureDetail?: {
    sectionId: DeviceSectionId;
    valueType: string;
    location: string;
  } | null;
  selectedSections: DeviceSectionId[];
  action:
    | "capture"
    | "prepare"
    | "rollback"
    | "reapply-source"
    | "recovery-required"
    | "complete"
    | "continue"
    | "await-library"
    | "await-capture"
    | "await-native-preparation"
    | "await-source"
    | "await-navigation";
}
export interface DeviceMaintenanceBootstrap {
  mode: "normal" | "maintenance";
  session: DeviceMaintenanceSession | null;
}
export interface DeviceMaintenanceView {
  progress(message: string): void;
  reviewReplacement(
    sections: { label: string; deletionCount: number }[],
  ): Promise<boolean>;
  completed(message: string): Promise<void>;
  failed(message: string): void;
}
export interface DeviceMaintenanceDependencies {
  invoke: DeviceNativeInvoke;
  environment: Omit<DeviceStorageEnvironment, "barrier">;
  view: DeviceMaintenanceView;
  wait(milliseconds: number): Promise<void>;
  spool?: (sessionId: string, kind: "source" | "rollback") => DeviceSpool;
  assertExclusiveWriters?: () => Promise<void>;
}

export class DeviceWriterBarrierError extends Error {
  constructor() {
    super("A service worker can still write device storage");
    this.name = "DeviceWriterBarrierError";
  }
}

function describeDeviceFailure(session: DeviceMaintenanceSession): string {
  const detail = session.failureDetail;
  if (session.failureCode === "device-writer-active")
    return " An active service worker prevents a consistent device-storage capture. Stop it before retrying.";
  if (detail)
    return ` ${deviceSectionLabel(detail.sectionId)} could not preserve ${detail.valueType} at ${detail.location}. Exclude that area or resolve the unsupported data before retrying.`;
  return session.failureCode === "cancelled"
    ? " The operation was cancelled."
    : session.failureCode
      ? " Review the backup task failure before retrying."
      : "";
}

/** Called before any normal app module import. Never releases a failed recovery. */
export async function runDeviceMaintenance(
  initial: DeviceMaintenanceBootstrap,
  dependencies: DeviceMaintenanceDependencies,
): Promise<void> {
  let bootstrap = initial;
  const { invoke, view } = dependencies;
  let barrierHeld = bootstrap.mode === "maintenance";
  let stagesCleaned = false;
  let processedBytes = 0;
  let processedRecords = 0;
  let recovering = false;
  let verifiedAppliedSession: string | undefined;
  const displayProgress = () =>
    view.progress(
      `Processing device storage: ${processedRecords} records, ${processedBytes} bytes transferred`,
    );
  const environment: DeviceStorageEnvironment = {
    ...dependencies.environment,
    barrier: {
      assertHeld() {
        if (!barrierHeld)
          throw new Error("Native device maintenance barrier is not held");
      },
    },
    onProgress(progress) {
      processedRecords++;
      displayProgress();
      dependencies.environment.onProgress?.(progress);
    },
  };
  const holdAutomaticSyncAfterCommit = (session: DeviceMaintenanceSession) => {
    if (
      session.operation === "restore" &&
      session.includesLibrary &&
      session.phase === "committed"
    )
      environment.localStorage.setItem("risuNestServerSyncRestoreHold", "true");
  };
  const spoolFor =
    dependencies.spool ??
    ((sessionId, kind) =>
      createNativeDeviceSpool(
        sessionId,
        kind,
        invoke,
        (bytes) => {
          processedBytes += bytes;
          displayProgress();
        },
        () => {
          if (!recovering) environment.signal?.throwIfAborted();
        },
      ));
  const refresh = () =>
    invoke<DeviceMaintenanceBootstrap>("native_device_backup_bootstrap");
  sessionLoop: while (bootstrap.mode === "maintenance") {
    const session = bootstrap.session;
    if (!session) throw new Error("Native maintenance session is missing");
    recovering =
      session.action === "rollback" || session.action === "reapply-source";
    const args = { sessionId: session.sessionId };
    const source = spoolFor(session.sessionId, "source");
    const rollback = spoolFor(session.sessionId, "rollback");
    let staged: StagedDeviceSection[] = [];
    let activeSection: DeviceSectionId | undefined;
    try {
      if (session.action === "recovery-required")
        throw new Error(
          "Device storage recovery requires retry. Normal app startup remains blocked.",
        );
      if (
        !stagesCleaned &&
        ["capture", "prepare", "rollback", "reapply-source"].includes(
          session.action,
        )
      ) {
        await dependencies.assertExclusiveWriters?.();
        await cleanupPluginDatabaseStages({
          ...environment,
          signal: undefined,
        });
        stagesCleaned = true;
      }
      if (session.action === "capture") {
        view.progress("Capturing device plugin storage. Plugins are paused.");
        for (const sectionId of session.selectedSections) {
          activeSection = sectionId;
          await captureDeviceSection(sectionId, source, environment);
        }
        await invoke("native_device_backup_finish_device", args);
      } else if (session.action === "prepare") {
        view.progress(
          "Validating selected device storage and preserving rollback data.",
        );
        const sections = await source.sections();
        for (const sectionId of session.selectedSections) {
          activeSection = sectionId;
          const section = sections.find(
            (value) => value.sectionId === sectionId,
          );
          if (!section)
            throw new Error(
              "Selected device section is missing from the backup",
            );
          staged.push(await stageDeviceSection(section, source, environment));
        }
        for (const sectionId of session.selectedSections) {
          activeSection = sectionId;
          await captureDeviceSection(sectionId, rollback, environment);
        }
        const accepted = await view.reviewReplacement(
          staged.map((section) => ({
            label: deviceSectionLabel(section.sectionId),
            deletionCount: section.deletionCount,
          })),
        );
        if (!accepted)
          throw new DOMException(
            "Device restore cancelled before application",
            "AbortError",
          );
        await invoke("native_device_backup_prepared", args);
        // Wait until the native job has validated and staged the incoming library.
        // Rollback spools remain protected by the maintenance barrier.
        while (true) {
          const decision = await refresh();
          if (decision.session?.sessionId !== session.sessionId)
            throw new Error(
              "Native restore ownership changed before application",
            );
          if (decision.session.phase === "prepared") break;
          if (decision.session.action !== "await-native-preparation") {
            bootstrap = decision;
            continue sessionLoop;
          }
          environment.signal?.throwIfAborted();
          view.progress("Preparing the selected backup before replacing data.");
          await dependencies.wait(100);
        }
        for (const section of staged) {
          activeSection = section.sectionId;
          environment.signal?.throwIfAborted();
          await invoke("native_device_backup_section_intent", {
            ...args,
            sectionId: section.sectionId,
            rollback: false,
          });
          await section.apply();
          const digest = sections.find(
            (value) => value.sectionId === section.sectionId,
          )!.digest;
          await invoke("native_device_backup_section_complete", {
            ...args,
            sectionId: section.sectionId,
            rollback: false,
            digest,
          });
        }
        await invoke("native_device_backup_finish_device", args);
        verifiedAppliedSession = session.sessionId;
      } else if (
        session.action === "rollback" ||
        session.action === "reapply-source"
      ) {
        const rollingBack = session.action === "rollback";
        view.progress(
          rollingBack
            ? "Recovering the previous device storage state."
            : "Verifying the committed device storage restore.",
        );
        // This document already verified every section before its commit. A
        // fresh maintenance entry must reconstruct that proof from the spool.
        if (rollingBack || verifiedAppliedSession !== session.sessionId) {
          const spool = rollingBack ? rollback : source;
          const sections = await spool.sections();
          // Cancellation cannot interrupt recovery of an already committed restore.
          const recoveryEnvironment = { ...environment, signal: undefined };
          for (const sectionId of session.selectedSections) {
            activeSection = sectionId;
            const section = sections.find(
              (value) => value.sectionId === sectionId,
            );
            if (!section)
              throw new Error("Device recovery spool is incomplete");
            const matches = await verifyCurrentDeviceSection(
              section,
              spool,
              recoveryEnvironment,
            );
            const stagedSection = matches
              ? undefined
              : await stageDeviceSection(section, spool, recoveryEnvironment);
            if (stagedSection) staged.push(stagedSection);
            await invoke("native_device_backup_section_intent", {
              ...args,
              sectionId,
              rollback: rollingBack,
            });
            await stagedSection?.apply();
            await invoke("native_device_backup_section_complete", {
              ...args,
              sectionId,
              rollback: rollingBack,
              digest: section.digest,
            });
          }
        }
        holdAutomaticSyncAfterCommit(session);
        await view.completed(
          rollingBack
            ? `The previous storage state has been restored.${describeDeviceFailure(session)}`
            : "The selected device storage has been restored. Plugins can start after you continue.",
        );
        await invoke("native_device_backup_recovery_complete", args);
      } else if (session.action === "complete") {
        holdAutomaticSyncAfterCommit(session);
        if (session.operation === "restore" || session.failureCode)
          await view.completed(
            session.operation === "capture"
              ? `Device storage capture did not complete.${describeDeviceFailure(session)}`
              : session.phase === "rolled-back"
                ? `The restore did not commit. The previous storage state is preserved.${describeDeviceFailure(session)}`
                : "The selected data has been restored. Plugins can start after you continue.",
          );
        await invoke("native_device_backup_recovery_complete", args);
      } else {
        view.progress("Finishing the native backup transaction.");
        await dependencies.wait(100);
      }
    } catch (error) {
      const pending =
        error && typeof error === "object"
          ? (error as { pendingOperation?: Promise<void> }).pendingOperation
          : undefined;
      if (pending) {
        view.progress(
          "Waiting for a blocked database operation before recovery. Plugins remain paused.",
        );
        await pending.catch(() => {});
      }
      await invoke("native_device_backup_fail", {
        ...args,
        code:
          environment.signal?.aborted ||
          (error instanceof DOMException && error.name === "AbortError")
            ? "cancelled"
            : error instanceof CloneGraphError && error.code === "unsupported"
              ? "device-clone-unsupported"
              : error instanceof DeviceWriterBarrierError
                ? "device-writer-active"
                : "device-maintenance-failed",
        detail:
          error instanceof CloneGraphError && activeSection
            ? {
                sectionId: activeSection,
                valueType: error.valueType,
                location: error.location,
              }
            : null,
      });
      const next = await refresh();
      if (
        next.session?.action === "rollback" ||
        next.session?.action === "complete"
      )
        bootstrap = next;
      else {
        view.failed(
          "Device storage recovery could not finish. Restart to retry. Plugins and normal app startup remain blocked.",
        );
        throw error;
      }
    } finally {
      for (const section of staged) {
        try {
          await section.cleanup();
        } catch {
          view.progress(
            "Temporary storage cleanup will be retried after recovery.",
          );
        }
      }
    }
    bootstrap = await refresh();
  }
  barrierHeld = false;
}

/** The caller must already own the native job, flush PDS, and establish its native fence. */
export async function requestDeviceMaintenanceRestart(
  invoke: DeviceNativeInvoke,
  reload: () => void = () => location.reload(),
): Promise<never> {
  const state = await invoke<DeviceMaintenanceBootstrap>(
    "native_device_backup_bootstrap",
  );
  if (state.mode !== "maintenance" || !state.session)
    throw new Error("Native device maintenance has not been prepared");
  reload();
  // Keep the caller's JS ownership alive until the WebView is destroyed.
  return new Promise<never>(() => {});
}
