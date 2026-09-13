import { invoke } from "@tauri-apps/api/core";
import { Ollama } from "ollama/dist/browser.mjs";
import { fetchTauriHttpStream } from "../../src/ts/network/tauriHttpStream";
import { createRequestAbortScope } from "../../src/ts/network/requestAbortScope";
import { beginIOSGeneration, getIOSNativeState } from "../../src/ts/iosNative";
import { check } from "./contracts";

/** Optional live transport check. Never display credentials or provider output. */
export async function cloudContract(cancelExpected: boolean) {
  const key = await invoke<string>("ios_bench_cloud_key");
  const controller = new AbortController();
  const lease = await beginIOSGeneration(controller.signal);
  const requestScope = createRequestAbortScope(lease.signal);
  const initialState = await getIOSNativeState();
  const started = performance.now();
  const events: { event: string; ms: number }[] = [];
  const visibility = () =>
    events.push({
      event: document.hidden ? "hidden" : "visible",
      ms: performance.now() - started,
    });
  document.addEventListener("visibilitychange", visibility);
  const cancel = document.createElement("button");
  cancel.textContent = "Cancel live request";
  cancel.onclick = () => controller.abort();
  document.getElementById("benchmark")!.append(cancel);
  const watchdog = setTimeout(() => controller.abort("timeout"), 180_000);
  let text = "";
  let chunks = 0;
  let hiddenChunks = 0;
  let maxGapMs = 0;
  let previous = started;
  let finished = false;
  let transportFinished = false;
  let outcome = "failed";
  const client = new Ollama({
    host: "https://ollama.com",
    headers: { Authorization: "Bearer " + key },
    fetch: requestScope.fetch(
      async (input: RequestInfo | URL, init: RequestInit = {}) => {
        // Same native transport and Ollama SDK parser used by the product.
        const response = await fetchTauriHttpStream({
          url: String(input),
          method: init.method ?? "POST",
          headers: Object.fromEntries(new Headers(init.headers).entries()),
          body: new TextEncoder().encode(String(init.body)),
          signal: init.signal ?? undefined,
          onFinish: () => {
            transportFinished = true;
          },
        });
        check(response.ok, "Cloud HTTP request rejected");
        return response;
      },
    ),
  });
  try {
    await invoke("pds_open");
    const stream = await client.chat({
      model: "deepseek-v4.1-flash",
      stream: true,
      think: false,
      messages: [
        {
          role: "user",
          content:
            "For a synthetic mobile streaming test, write 300 numbered sentences about an imaginary squirrel sorting colored wooden blocks. Each sentence must have at least 15 words. Do not summarize or stop early.",
        },
      ],
      options: { num_predict: 8192 },
    });
    for await (const chunk of stream) {
      const now = performance.now();
      maxGapMs = Math.max(maxGapMs, now - previous);
      previous = now;
      chunks++;
      if (document.hidden) hiddenChunks++;
      text += chunk.message?.content ?? "";
      finished ||= chunk.done === true;
      lease.progress(2);
      await invoke("pds_set_app_kv", {
        key: "ios-synthetic-cloud",
        value: text,
      });
      if (text.length)
        document.getElementById("status")!.textContent = "cloud-streaming";
    }
    outcome = controller.signal.aborted
      ? controller.signal.reason === "timeout"
        ? "timeout"
        : "cancelled"
      : finished
        ? "complete"
        : lease.signal?.aborted
          ? "expired"
          : "incomplete";
  } catch {
    // SDK/HTTP errors may contain request details. Report only a fixed category.
    outcome = controller.signal.aborted
      ? controller.signal.reason === "timeout"
        ? "timeout"
        : "cancelled"
      : lease.signal?.aborted
        ? "expired"
        : "request-failed";
  } finally {
    clearTimeout(watchdog);
    document.removeEventListener("visibilitychange", visibility);
    // The SDK stops at done:true without necessarily consuming HTTP EOF.
    controller.abort();
    requestScope.dispose();
    await lease.dispose(finished);
    client.abort();
  }
  const state = await getIOSNativeState();
  const saved = await invoke<string>("pds_get_app_kv", {
    key: "ios-synthetic-cloud",
  });
  const result = {
    model: "deepseek-v4.1-flash",
    outcome,
    chunks,
    hiddenChunks,
    characters: text.length,
    maxGapMs,
    durationMs: performance.now() - started,
    initialState,
    state,
    events,
    transportFinished,
    savedExact: saved === text,
  };
  const measured = document.createElement("pre");
  measured.textContent = "cloud-result:" + JSON.stringify(result);
  document.getElementById("benchmark")!.append(measured);
  check(
    chunks > 0 && text.length > 0 && saved === text,
    "Cloud stream and saved output contract",
  );
  check(
    state.activeTasks.length === 0 && transportFinished,
    "Cloud request cleanup contract",
  );
  check(
    cancelExpected
      ? outcome === "cancelled"
      : ["complete", "expired"].includes(outcome),
    "Cloud completion contract",
  );
  return result;
}
