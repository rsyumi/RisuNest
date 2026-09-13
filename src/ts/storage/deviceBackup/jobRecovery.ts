import { invoke } from "@tauri-apps/api/core";
import { isTauriIOS } from "../../platform";
import { alertNormal, alertSelect } from "../../alert";
import {
  createPortableExportIntentStore,
  portableAndroidPublicationDependencies,
  PortableExportNeedsAttention,
  rememberPortableExport,
  resumePendingPortableExport,
  type PendingPortableExport,
  type PortableJobStatus,
} from "./job";

let running: Promise<void> | undefined;

async function recover(): Promise<void> {
  if (
    !(window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
  )
    return;
  const store = createPortableExportIntentStore();
  const jobs = await invoke<PortableJobStatus[]>("native_file_job_list");
  const portable = jobs.filter((job) => job.kind === "export-portable-backup");
  const stored = store.read();
  if (stored && !portable.some((job) => job.jobId === stored.jobId)) {
    if (stored.phase === "published") store.clear(stored.jobId);
    else {
      const choice = await alertSelect(
        ["Dismiss this attempt", "Keep for later"],
        "The previous portable backup job is no longer available. Its export could not be confirmed.",
      );
      if (choice !== "0") return;
      store.clear(stored.jobId);
    }
  }
  for (const job of portable) {
    const existing = store.read();
    if (existing && existing.jobId !== job.jobId) continue;
    if (!existing) {
      const choice = await alertSelect(
        ["Review this backup", "Keep for later"],
        "A previous portable backup task needs review.",
      );
      if (choice !== "0") continue;
      let status = await invoke<PortableJobStatus>("native_file_job_status", {
        jobId: job.jobId,
      });
      while (!["succeeded", "failed", "cancelled"].includes(status.state)) {
        await new Promise((resolve) => setTimeout(resolve, 100));
        status = await invoke<PortableJobStatus>("native_file_job_status", {
          jobId: job.jobId,
        });
      }
      rememberPortableExport(
        job.jobId,
        status.result?.handoffPath
          ? {
              type: isTauriIOS ? "iosFiles" : "androidSaf",
              suggestedName: "RisuNest backup.risunest",
            }
          : { type: "desktopPath" },
        store,
      );
    }
    while (store.read()?.jobId === job.jobId) {
      try {
        await resumePendingPortableExport({
          invoke,
          store,
          wait: (milliseconds) =>
            new Promise((resolve) => setTimeout(resolve, milliseconds)),
          ...portableAndroidPublicationDependencies(),
          cleanupHandoff: async (path) => {
            await invoke("native_portable_handoff_cleanup", { path });
          },
          onResult: (result) =>
            alertNormal(
              result.warningCodes.length
                ? "The portable backup was saved. Review the task warnings before moving the backup."
                : "The portable backup was saved.",
            ),
        });
      } catch (error) {
        const intent = store.read();
        if (!intent) throw error;
        const status = await invoke<PortableJobStatus>(
          "native_file_job_status",
          { jobId: job.jobId },
        );
        const published =
          error instanceof PortableExportNeedsAttention &&
          !!error.committedResult;
        const partial =
          error instanceof PortableExportNeedsAttention &&
          error.warningCodes.includes("partial-destination-may-remain");
        const canRetry = status.state === "succeeded";
        const labels = canRetry
          ? [
              published ? "Retry cleanup" : "Retry saving backup",
              "Discard this export",
              "Keep for later",
            ]
          : ["Dismiss failed attempt", "Keep for later"];
        const code =
          error instanceof PortableExportNeedsAttention
            ? error.code
            : "portable-export-recovery-failed";
        const choice = await alertSelect(
          labels,
          `${published ? "The backup was saved, but cleanup needs attention." : "The previous backup export needs attention."} (${code})${partial ? " A partial file may remain at the selected destination." : ""}`,
        );
        if (choice === "0" && canRetry) {
          if (!published) {
            ensureAndroidReceiptSettled(intent);
            // User explicitly chose another save attempt. The native archive is
            // retained and no old destination URI is replayed from backup data.
            store.write({
              ...intent,
              phase: "waiting-native",
              requestId: undefined,
            });
          }
          continue;
        }
        if ((choice === "1" && canRetry) || (choice === "0" && !canRetry)) {
          await discard(intent, status);
          store.clear(intent.jobId);
          break;
        }
        return;
      }
    }
  }
}

async function discard(
  intent: PendingPortableExport,
  status: PortableJobStatus,
): Promise<void> {
  ensureAndroidReceiptSettled(intent);
  if (status.result?.handoffPath)
    await invoke("native_portable_handoff_cleanup", {
      path: status.result.handoffPath,
    });
  await invoke("native_file_job_forget", { jobId: intent.jobId });
}

function ensureAndroidReceiptSettled(intent: PendingPortableExport): void {
  if (intent.publication !== "android-saf" || !intent.requestId) return;
  const publication = portableAndroidPublicationDependencies();
  if (
    publication.androidAcknowledgementPending(intent.requestId) &&
    !publication.acknowledgeAndroid(intent.requestId)
  )
    throw new PortableExportNeedsAttention(
      "android-publication-still-active",
      intent.jobId,
    );
}

/** Runs only after normal bootstrap; concurrent callers share one recovery flow. */
export function resumePortableExportsAfterBootstrap(): Promise<void> {
  if (!running)
    running = recover()
      .catch(async () => {
        await alertSelect(
          ["Keep for later"],
          "Portable backup recovery could not finish. The pending task and its source file have been retained.",
        );
      })
      .finally(() => {
        running = undefined;
      });
  return running;
}
