import { invoke } from "@tauri-apps/api/core";
import { fetchTauriHttpStream } from "../../src/ts/network/tauriHttpStream";
import {
  invokeNativeTokenizerBatch,
  resolveNativeTokenizerRoute,
  type NativeTokenizerId,
} from "../../src/ts/tokenizer/nativeTokenizer";
import corpus from "../tokenizer/native-tokenizer-corpus.json";
import { check } from "./contracts";

export async function streaming() {
  const base = await invoke<string>("ios_bench_stream_url");
  const response = await fetchTauriHttpStream({
    url: base + "/stream",
    method: "GET",
    headers: {},
  });
  const body = await response.text();
  const expected = 'data: {"text":"합성🐿️"}\n\n'.repeat(8);
  check(
    response.status === 200 && body === expected,
    "exact native UTF-8 SSE transport",
  );
  const controller = new AbortController();
  const slow = await fetchTauriHttpStream({
    url: base + "/slow",
    method: "GET",
    headers: {},
    signal: controller.signal,
  });
  const reader = slow.body!.getReader();
  check(!(await reader.read()).done, "first streaming chunk");
  controller.abort();
  let stopped = false;
  try {
    stopped = (await reader.read()).done === true;
  } catch {
    stopped = true;
  }
  check(stopped, "native stream cancellation");
  return { passed: true, bytes: new TextEncoder().encode(body).length };
}

export async function tokenizer() {
  let verified = 0;
  for (const entry of corpus.cases) {
    const route = resolveNativeTokenizerRoute(
      entry.tokenizerId as NativeTokenizerId,
      true,
      true,
    );
    check(route.kind === "native-tiktoken", "native tokenizer route");
    if (route.kind !== "native-tiktoken")
      throw new Error("Native tokenizer unavailable");
    const input = entry.input as {
      kind: string;
      value?: string;
      count?: number;
      codeUnits?: number[];
    };
    const text =
      input.kind === "text"
        ? input.value!
        : input.kind === "repeat"
          ? input.value!.repeat(input.count!)
          : String.fromCharCode(...input.codeUnits!);
    if ("ids" in entry) {
      const result = await invokeNativeTokenizerBatch(route, [text], "ids");
      check(
        result.mode === "ids" &&
          JSON.stringify(result.ids[0]) === JSON.stringify(entry.ids),
        "exact native tokenizer IDs",
      );
    } else {
      let rejected = false;
      try {
        await invokeNativeTokenizerBatch(route, [text], "ids");
      } catch (error) {
        rejected = (error as { code?: string }).code === entry.error!.code;
      }
      check(rejected, "native tokenizer error category");
    }
    verified++;
  }
  return { passed: true, verified };
}

export async function snapshotRestore(): Promise<boolean> {
  await invoke("pds_open");
  const key = "ios-synthetic-restore";
  if (localStorage.getItem(key)) {
    check(
      (await invoke("pds_get_app_kv", { key })) === "before",
      "snapshot reopened the native SQLite state",
    );
    localStorage.removeItem(key);
    return true;
  }
  await invoke("pds_set_app_kv", { key, value: "before" });
  const snapshot = await invoke<{ id: string }>("pds_snapshot_create", {
    reason: "ios-synthetic",
  });
  await invoke("pds_set_app_kv", { key, value: "after" });
  await invoke("pds_snapshot_restore_request", { id: snapshot.id });
  localStorage.setItem(key, "pending");
  await invoke("ios_prepare_restart");
  location.reload();
  return false;
}
