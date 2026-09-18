import { streaming, tokenizer, snapshotRestore } from "./runtimeContracts";
import { installPickerContracts } from "./pickerContracts";
import { invoke } from "@tauri-apps/api/core";
import { appDataDir, join } from "@tauri-apps/api/path";
import { mkdir, readFile, writeFile, remove } from "@tauri-apps/plugin-fs";
import {
  beginIOSGeneration,
  getIOSNativeState,
  installIOSPersistenceLifecycle,
  initializeIOSNative,
} from "../../src/ts/iosNative";
import { isTauriIOS, isTauriDesktop } from "../../src/ts/platform";
import { persistence, reload, regex, guard, check, pause } from "./contracts";
import { PersistentBenchmarkMarker } from "./persistentMarker";

const report = (stage: string, result: unknown) =>
  invoke("ios_bench_report", { stage, result });
async function userStart(label: string) {
  await new Promise<void>((resolve) => {
    const button = document.createElement("button");
    button.textContent = label;
    button.style.cssText = "display:block;padding:24px;margin:16px";
    button.onclick = () => {
      button.remove();
      resolve();
    };
    document.getElementById("benchmark")!.append(button);
  });
}
async function main() {
  await guard();
  check(isTauriIOS && !isTauriDesktop, "iOS runtime classification");
  const phase = await invoke<string>("ios_bench_phase");
  if (phase !== "app" && phase !== "app-restart") {
    // Product CSS gives the empty app root a full viewport of height.
    // Keep native-contract status/results visible to older WebKit accessibility.
    document.getElementById("app")!.style.display = "none";
    document.body.style.color = "var(--risu-theme-textcolor)";
  }
  await initializeIOSNative();
  if (phase === "app" || phase === "app-restart") {
    const { productApp } = await import("./productContracts");
    await report(phase, await productApp(phase === "app-restart"));
    await report("complete", { passed: true });
    return;
  }
  if (phase === "ui") {
    await installPickerContracts();
    return;
  }
  installIOSPersistenceLifecycle(async () => {
    await invoke("pds_checkpoint", { mode: "passive" });
  });
  // Continued processing requires a user action; XCTest performs an actual tap.
  if (phase === "cloud" || phase === "cloud-cancel")
    await userStart("Start live request");
  if (phase === "background-ui") await userStart("Start background work");
  if (phase === "network") {
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
    const root = await appDataDir();
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
    await remove(folder);
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
    const obsolete = await beginIOSGeneration();
    await initializeIOSNative();
    check(
      (await getIOSNativeState()).activeTasks.length === 0,
      "new main document releases obsolete generation",
    );
    await obsolete.dispose();
    await report("complete", { passed: true });
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
  await report("failure", { message });
});
