import { invoke } from "@tauri-apps/api/core";
import { SyncDriver, SyncStepError, redact, syncFailureRecord } from "../sync-ui/driver";
import { publishLibrary, receiveAndPush, reconnectAndPull } from "../sync-ui/roundTrip";
import { initialize } from "./contracts";

interface SyncInput {
  registration?: string;
  expect?: string;
  marker: string;
}

const report = (stage: string, result: unknown) => invoke("ios_bench_report", { stage, result });

/**
 * Publishes a seeded library to an empty server (`sync-publish`), joins the library behind the
 * supplied registration (`sync`), or reconnects from the stored credential after a relaunch
 * (`sync-restart`). XCTest reads the outcome from a fixed overlay, since the product replaces
 * the harness page.
 */
export async function syncContract(phase: "sync" | "sync-restart" | "sync-publish") {
  const overlay = document.createElement("pre");
  overlay.style.cssText = "position:fixed;left:0;right:0;bottom:0;z-index:2147483647;margin:0;padding:4px 8px;"
    + "font:12px monospace;white-space:pre-wrap;pointer-events:none;background:#000;color:#fff";
  document.body.append(overlay);
  const driver = new SyncDriver(phase, report, (step) => { overlay.textContent = `sync-step:${phase}:${step}`; });
  try {
    let input: SyncInput;
    try {
      input = await invoke<SyncInput>("ios_bench_sync_input");
    } catch {
      throw new SyncStepError("sync-env", "input", "sync inputs were not supplied");
    }
    const result = phase === "sync"
      ? await receiveAndPush(driver, { seed: initialize, registration: input.registration!, expect: input.expect!, marker: input.marker })
      : phase === "sync-publish"
        ? await publishLibrary(driver, { seed: initialize, registration: input.registration!, marker: input.marker, characterId: "char-a" })
        : await reconnectAndPull(driver, { expect: input.marker });
    await report(phase, { passed: true, label: "sync-result", ...result });
    overlay.textContent = `sync-result:passed:${JSON.stringify({ phase, ...result })}`;
  } catch (error) {
    const detail = error instanceof SyncStepError ? error.detail : { label: "sync-result", step: "unknown" };
    const message = error instanceof Error ? redact(error.message) : "failed";
    await report("failure", { passed: false, ...syncFailureRecord(error), message, detail }).catch(() => {});
    overlay.textContent = `sync-result:failed:${JSON.stringify({ phase, ...detail, message })}`;
  }
}
