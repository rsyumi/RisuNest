import { streaming, tokenizer, snapshotRestore } from "./runtimeContracts";
import { runLuaWorkerContract } from "../../tests/browser/luaWorkerContract";
import { installPickerContracts } from "./pickerContracts";
import { invoke } from "@tauri-apps/api/core";
import { join } from "@tauri-apps/api/path";
import { mkdir, readFile, writeFile, exists } from "@tauri-apps/plugin-fs";
import {
  beginIOSGeneration,
  getIOSNativeState,
  installIOSPersistenceLifecycle,
  initializeIOSNative,
} from "../../src/ts/iosNative";
import { isTauriIOS, isTauriDesktop } from "../../src/ts/platform";
import { nativeDataPath } from "../../src/ts/storage/nativePaths";
import { persistence, reload, regex, guard, check, pause } from "./contracts";
import { PersistentBenchmarkMarker } from "./persistentMarker";
import { showNativeHarness } from "../nativeAppLock";
import { failureRecord } from "../failureRecord";

const report = (stage: string, result: unknown) =>
  invoke("ios_bench_report", { stage, result });
async function userStart(label: string, onStart?: () => void) {
  await new Promise<void>((resolve) => {
    const button = document.createElement("button");
    button.textContent = label;
    button.style.cssText = "display:block;padding:24px;margin:16px";
    button.onclick = () => {
      onStart?.();
      button.remove();
      resolve();
    };
    document.getElementById("benchmark")!.append(button);
  });
}
async function resetLegacyMeasurementProfile() {
  await guard();
  const opened = await invoke<{ revision: number }>("pds_open");
  const jobs = await invoke<unknown[]>("native_file_job_list");
  check(jobs.length === 0, "Resolve retained synthetic jobs before resetting the measurement profile");
  const resetId = crypto.randomUUID();
  const { stagingId } = await invoke<{ stagingId: string }>("pds_replace_begin");
  await invoke("pds_replace_put_root", { stagingId, root: { username: "Synthetic memory fixture", botPresetsId: 0,
    pluginCustomStorage: {}, modules: [], loadouts: [], plugins: [], syntheticMeasurementResetId: resetId } });
  await invoke("pds_replace_put_presets", { stagingId, presets: [] });
  const committed = await invoke<{ revision: number }>("pds_replace_commit", { stagingId, expectedRevision: opened.revision });
  const root = await invoke<{ value: { syntheticMeasurementResetId?: string } }>("pds_read_root");
  check(root.value.syntheticMeasurementResetId === resetId, "Synthetic profile reset identity mismatch");
  for (const trash of [false, true]) {
    const page = await invoke<{ items: unknown[]; nextCursor?: string }>("pds_query_characters", {
      query: { order: "configured", trash, limit: 1 },
    });
    check(page.items.length === 0 && !page.nextCursor, "Synthetic profile still contains characters");
  }
  const presets = await invoke<{ items: unknown[] }>("pds_query_presets");
  check(presets.items.length === 0, "Synthetic profile still contains presets");
  return { resetId, resetRevision: committed.revision, resetVerified: true };
}
async function oauthCallbackContract() {
  await userStart("Start OAuth callback");
  const callbackScheme = "risunestoauthtest";
  const expectedCallback =
    "risunestoauthtest://oauth?code=synthetic&state=device";
  const result = await invoke<{
    status: string;
    callbackUrl?: string;
  }>("ios_bench_authenticate");
  check(
    callbackScheme === new URL(expectedCallback).protocol.slice(0, -1),
    "OAuth callback scheme fixture",
  );
  check(result.status === "succeeded", "OAuth session did not succeed");
  check(
    result.callbackUrl === expectedCallback,
    "OAuth callback URL was not returned unchanged",
  );
  const measured = document.createElement("pre");
  measured.textContent = "oauth-result:" + JSON.stringify(result);
  document.getElementById("benchmark")!.append(measured);
  return result;
}
async function main() {
  await guard();
  check(isTauriIOS && !isTauriDesktop, "iOS runtime classification");
  const phase = await invoke<string>("ios_bench_phase");
  if (/^app-lock-(immediate|timed|immediate-mutant|ui)$/.test(phase)) {
    const { appLockFixture } = await import('../appLock/fixture');
    const { initialize } = await import('./contracts');
    await appLockFixture('ios', phase, initialize);
    return;
  }
  if (phase === "sidebar-touch") {
    document.getElementById("app")!.style.display = "none";
    const { mountSidebarTouchFixture } = await import("../sidebar-touch/fixture");
    mountSidebarTouchFixture(document.getElementById("benchmark")!, true);
    await showNativeHarness();
    return;
  }
  if (
    phase !== "app" &&
    phase !== "app-restart" &&
    phase !== "onboarding" &&
    phase !== "settings" &&
    phase !== "sync" &&
    phase !== "sync-restart" &&
    phase !== "sync-publish" &&
    !/^(external-storage|external-sync|backup-file)(-restart)?$/.test(phase)
  ) {
    // Product CSS gives the empty app root a full viewport of height.
    // Keep native-contract status/results visible to older WebKit accessibility.
    document.getElementById("app")!.style.display = "none";
    document.body.style.color = "var(--risu-theme-textcolor)";
    await showNativeHarness();
  }
  await initializeIOSNative();
  const generationReloadKey = "ios-synthetic-generation-reload";
  const obsoleteId = sessionStorage.getItem(generationReloadKey);
  if (phase === "contracts" && obsoleteId !== null) {
    check(
      (await getIOSNativeState()).activeTasks.length === 0,
      "new main document releases obsolete generation",
    );
    await invoke("plugin:ios-native|end", { id: obsoleteId, success: false });
    check(
      (await getIOSNativeState()).activeTasks.length === 0,
      "obsolete generation cleanup remains harmless after reload",
    );
    sessionStorage.removeItem(generationReloadKey);
    await report("generation-reload", { passed: true });
    await report("complete", { passed: true });
    return;
  }
  if (/^legacy-restore-(100|300|600)-(raw|gzip)$/.test(phase)) {
    const [, size, encoding] = /^legacy-restore-(100|300|600)-(raw|gzip)$/.exec(phase)!;
    const reset = await resetLegacyMeasurementProfile();
    const { runLegacyRestoreMeasurement } = await import('../legacy-restore/run');
    const readbackStage = document.createElement('pre');
    document.body.append(readbackStage);
    const readbackStarted = performance.now();
    const showReadbackStage = (progress: { stage: number; index: number; readReturned: number; hashVerified: number; messageCount: number }) => {
      readbackStage.textContent = 'legacy-readback-stage:' + JSON.stringify({
        ...progress, elapsedMs: Math.floor(performance.now() - readbackStarted),
      });
    };
    const showVerifyStage = (stage: 1 | 2 | 3) => showReadbackStage({
      stage, index: 0, readReturned: 0, hashVerified: 0, messageCount: 0,
    });
    const result = await runLegacyRestoreMeasurement({
      megabytes: Number(size) as 100 | 300 | 600,
      encoding: encoding as 'raw' | 'gzip',
      assertIsolatedHarness: guard,
      report: async (event) => {
        await report('legacy-restore', event);
        document.getElementById('benchmark')!.textContent = 'legacy-restore-event:' + JSON.stringify(event);
      },
      sampleMemory: () => invoke('ios_bench_peak_rss'),
      beforeRestore: async (prepared) => {
        const ready = { ...prepared, ...reset };
        await report('legacy-restore-ready', ready);
        document.getElementById('benchmark')!.textContent = 'legacy-restore-ready:' + JSON.stringify(ready);
        await userStart('Start synthetic restore');
      },
      afterRestore: async () => {
        showVerifyStage(1);
        await userStart('Verify synthetic restore', () => showVerifyStage(2));
        showVerifyStage(3);
      },
      onReadbackProgress: showReadbackStage,
    });
    document.getElementById('benchmark')!.textContent = 'legacy-restore-result:' + JSON.stringify(result);
    check(result.phase === 'verified', 'Legacy restore did not verify');
    await report('complete', { passed: true });
    return;
  } else if (phase === "app" || phase === "app-restart") {
    const { productApp } = await import("./productContracts");
    await report(phase, await productApp(phase === "app-restart"));
    await report("complete", { passed: true });
    return;
  }
  if (phase === "onboarding") {
    const { productOnboarding } = await import("./productContracts");
    await report(phase, await productOnboarding());
    return;
  }
  if (phase === "settings") {
    const { productSettings } = await import("./productContracts");
    await report(phase, await productSettings());
    return;
  }
  if (phase === "ui" || phase === "general-files") {
    await installPickerContracts(phase === "general-files");
    return;
  }
  if (phase === "sync" || phase === "sync-restart" || phase === "sync-publish") {
    const { syncContract } = await import("./syncContracts");
    await syncContract(phase);
    return;
  }
  if (/^(external-storage|external-sync|backup-file)(-restart)?$/.test(phase)) {
    const { externalStorageContract } = await import("./externalStorageContracts");
    await externalStorageContract(phase as Parameters<typeof externalStorageContract>[0]);
    return;
  }
  installIOSPersistenceLifecycle(async () => {
    await invoke("pds_checkpoint", { mode: "passive" });
  });
  // Continued processing requires a user action; XCTest performs an actual tap.
  if (phase === "cloud" || phase === "cloud-cancel")
    await userStart("Start live request");
  if (phase === "background-ui") await userStart("Start background work");
  if (phase === "oauth") {
    await report("oauth", await oauthCallbackContract());
    await report("complete", { passed: true });
  } else if (phase === "network") {
    const before = await invoke("ios_bench_network_probe");
    await userStart("Start network background work");
    const lease = await beginIOSGeneration();
    const initialState = await getIOSNativeState();
    let during;
    try {
      during = await invoke("ios_bench_network_probe");
    } finally {
      await lease.dispose();
    }
    const result = {
      before,
      during,
      initialState,
      state: await getIOSNativeState(),
    };
    const measured = document.createElement("pre");
    measured.textContent = "network-result:" + JSON.stringify(result);
    document.getElementById("benchmark")!.append(measured);
    await report("network", result);
    await report("complete", { passed: true });
  } else if (phase === "cloud" || phase === "cloud-cancel") {
    const { cloudContract } = await import("./cloudContracts");
    await report("cloud", await cloudContract(phase === "cloud-cancel"));
    await report("complete", { passed: true });
  } else if (phase === "device-core") {
    const persistenceResult = await persistence();
    document.getElementById("status")!.textContent = "core-regex";
    const regexResult = await regex();
    document.getElementById("status")!.textContent = "core-tokenizer";
    const result = {
      persistence: persistenceResult,
      regex: regexResult,
      tokenizer: await tokenizer(),
    };
    await report("device-core", result);
    const measured = document.createElement("pre");
    measured.textContent = "device-core-result:" + JSON.stringify(result);
    document.getElementById("benchmark")!.append(measured);
    await report("complete", { passed: true });
  } else if (phase === "contracts") {
    await report("persistence", await persistence());
    await report("regex", await regex());
    await report("streaming", await streaming());
    await report("tokenizer", await tokenizer());
    await report("lua-worker", await runLuaWorkerContract());
    const root = await nativeDataPath();
    const folder = await join(root, "ios-file-staging", crypto.randomUUID());
    const path = await join(folder, "synthetic.bin");
    const bytes = Uint8Array.from({ length: 1024 * 1024 }, (_, i) => i % 251);
    await mkdir(folder, { recursive: true });
    await writeFile(path, bytes);
    const read = await readFile(path);
    check(
      bytes.length === read.length &&
        bytes.every((value, i) => read[i] === value),
      "native file binary roundtrip",
    );
    await invoke("plugin:ios-native|discard_file", { path });
    check(
      !(await exists(folder)),
      "discarding a staged file removes the folder that held it",
    );
    let rejected = false;
    try {
      await invoke("plugin:ios-native|discard_file", {
        path: await join(root, "persistent", "store.sqlite"),
      });
    } catch {
      rejected = true;
    }
    check(rejected, "file cleanup must reject paths outside staging");
    await report("files", {
      passed: true,
      bytes: bytes.length,
      rootSuffixMatches: root.endsWith("io.github.rsyumi.risunest.ios.bench"),
    });
    const lease = await beginIOSGeneration();
    check(
      (await getIOSNativeState()).activeTasks.length === 1,
      "native generation assertion acquired",
    );
    await lease.dispose();
    check(
      (await getIOSNativeState()).activeTasks.length === 0,
      "native generation assertion released",
    );
    await report("lifecycle", {
      passed: true,
      state: await getIOSNativeState(),
    });
    await beginIOSGeneration();
    const obsoleteTasks = (await getIOSNativeState()).activeTasks;
    check(obsoleteTasks.length === 1, "obsolete generation assertion acquired");
    await initializeIOSNative();
    const retainedTasks = (await getIOSNativeState()).activeTasks;
    check(
      retainedTasks.length === 1 && retainedTasks[0] === obsoleteTasks[0],
      "same main document retains active generation",
    );
    sessionStorage.setItem(generationReloadKey, obsoleteTasks[0]);
    location.reload();
    return;
  } else if (phase === "reload") {
    await report("reload", await reload());
    await report("complete", { passed: true });
  } else if (phase === "restore") {
    if (!(await snapshotRestore())) return;
    await report("restore", { passed: true });
    await report("complete", { passed: true });
  } else if (phase === "ipad") {
    await invoke("pds_open");
    await report("ipad", {
      passed: true,
      ios: isTauriIOS,
      desktop: isTauriDesktop,
      state: await getIOSNativeState(),
    });
    await report("complete", { passed: true });
  } else if (phase === "background" || phase === "background-ui") {
    const marker = new PersistentBenchmarkMarker();
    await marker.open();
    const lease = await beginIOSGeneration();
    const initialState = await getIOSNativeState();
    await report("background-ready", { state: initialState });
    document.getElementById("status")!.textContent = "background-ready";
    let tick = 0;
    const gapsMs: number[] = [];
    let previous = performance.now();
    const stop = performance.now() + 20_000;
    while (performance.now() < stop && !lease.signal?.aborted) {
      gapsMs.push(performance.now() - previous);
      previous = performance.now();
      lease.progress(Math.min(3, Math.floor(tick / 25) + 1));
      await marker.write(String(++tick));
      await pause(250);
    }
    await lease.dispose();
    await report("background-result", {
      initialState,
      tick,
      gapsMs,
      aborted: lease.signal?.aborted,
      state: await getIOSNativeState(),
    });
    const measured = document.createElement("pre");
    measured.textContent =
      "background-result:" +
      JSON.stringify({
        initialState,
        tick,
        aborted: lease.signal?.aborted,
        maxGapMs: Math.max(...gapsMs),
        state: await getIOSNativeState(),
      });
    document.getElementById("benchmark")!.append(measured);
    await report("complete", { passed: true });
  } else throw new Error("Unknown verification phase");
  document.getElementById("status")!.textContent = "passed";
}
void main().catch(async (error) => {
  const status = document.getElementById("status");
  if (status) status.textContent = "failed";
  const phase = await invoke<string>("ios_bench_phase").catch(() => "unknown");
  const message = phase.startsWith("cloud")
    ? "Live cloud contract failed"
    : error instanceof Error
      ? error.message
      : JSON.stringify(error);
  const detail = document.createElement("pre");
  detail.textContent = "verification-error:" + message;
  document.getElementById("benchmark")?.append(detail);
  await report("failure", {
    // A live cloud error can hold what the provider sent, so it records only the fixed message.
    ...(phase.startsWith("cloud") ? {} : failureRecord(error)),
    message,
    detail: !phase.startsWith("cloud") && error instanceof Error
      ? (error as Error & { detail?: unknown }).detail
      : undefined,
  });
});
