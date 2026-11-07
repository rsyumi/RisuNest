import { invoke } from "@tauri-apps/api/core";
import { SyncDriver, SyncStepError, redact, syncFailureRecord } from "../sync-ui/driver";
import { webDavAfterRelaunch, webDavBackupRestore, type WebDavInput } from "../sync-ui/externalStorage";
import { webDavSyncAfterRelaunch, webDavSyncPublish } from "../sync-ui/externalSync";
import { backupFileAfterRelaunch, backupFileRestore } from "../sync-ui/portableBackup";
import { initialize } from "./contracts";

interface ExternalInput {
  webdav?: WebDavInput;
  before: string;
  after?: string;
}

const report = (stage: string, result: unknown) => invoke("ios_bench_report", { stage, result });

export type ExternalPhase = "external-storage" | "external-storage-restart" | "external-sync" | "external-sync-restart"
  | "backup-file" | "backup-file-restart";

/**
 * Backs up to a new WebDAV repository and restores it (`external-storage`), publishes to a new WebDAV
 * sync repository (`external-sync`) or restores a RisuNest backup file (`backup-file`), and checks the
 * result after a relaunch (`-restart`). XCTest reads the outcome from the same fixed overlay the Sync
 * phases use.
 */
export async function externalStorageContract(phase: ExternalPhase) {
  const overlay = document.createElement("pre");
  overlay.style.cssText = "position:fixed;left:0;right:0;bottom:0;z-index:2147483647;margin:0;padding:4px 8px;"
    + "font:12px monospace;white-space:pre-wrap;pointer-events:none;background:#000;color:#fff";
  document.body.append(overlay);
  const driver = new SyncDriver(phase, report, (step) => { overlay.textContent = `sync-step:${phase}:${step}`; });
  try {
    let input: ExternalInput;
    try {
      input = await invoke<ExternalInput>("ios_bench_external_input");
    } catch {
      throw new SyncStepError("sync-env", "input", "external storage inputs were not supplied");
    }
    const result = phase === "external-storage"
      ? await webDavBackupRestore(driver, {
        seed: initialize, platform: "ios", characterId: "char-a", webdav: input.webdav!, before: input.before, after: input.after!,
      })
      : phase === "external-storage-restart"
        ? await webDavAfterRelaunch(driver, { before: input.before })
        : phase === "external-sync"
          ? await webDavSyncPublish(driver, { seed: initialize, platform: "ios", characterId: "char-a", webdav: input.webdav!, first: input.before })
          : phase === "external-sync-restart"
            ? await webDavSyncAfterRelaunch(driver, { characterId: "char-a", first: input.before, second: input.after! })
            : phase === "backup-file"
              ? await backupFileRestore(driver, { seed: initialize, characterId: "char-a", before: input.before, after: input.after! })
              : await backupFileAfterRelaunch(driver, { before: input.before, after: input.after! });
    await report(phase, { passed: true, label: "sync-result", ...result });
    overlay.textContent = `sync-result:passed:${JSON.stringify({ phase, ...result })}`;
  } catch (error) {
    const detail = error instanceof SyncStepError ? error.detail : { label: "sync-result", step: "unknown" };
    const message = error instanceof Error ? redact(error.message) : "failed";
    await report("failure", { passed: false, ...syncFailureRecord(error), message, detail }).catch(() => {});
    overlay.textContent = `sync-result:failed:${JSON.stringify({ phase, ...detail, message })}`;
  }
}
