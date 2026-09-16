import {
  invoke,
  type InvokeArgs,
  type InvokeOptions,
} from "@tauri-apps/api/core";
import {
  NativeCommitTransport,
  encodeNativeCommit,
  type CommitEnvelope,
} from "../../src/ts/storage/nativeCommitTransport";
import { ANDROID_COMMIT_CHUNK_BYTES } from "../../src/ts/storage/androidCommitTransport";
import {
  ANDROID_BINARY_CHUNK_BYTES,
  binaryCommitSender,
  getAndroidBinaryCommitBridge,
} from "../../src/ts/storage/androidBinaryCommitBridge";
import syntheticDatabase from "../../src-tauri/fixtures/persistent-fixture.json";

/** Plugin storage is per owner, so the harness writes and reads as one. */
const benchmarkPluginOwner = "streaming-persistence-benchmark";

const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export async function runPersistenceSpike() {
  const cases: Record<string, unknown>[] = [];
  const encoder = new TextEncoder();
  for (const multilingual of [false, true]) {
    for (const size of [0, 256 * 1024, 1024 * 1024, 6 * 1024 * 1024]) {
      const unit = multilingual ? '합성🐿️\\"\n' : 'abc\\"\n';
      const unitBytes = encoder.encode(unit).length;
      const value = unit.repeat(Math.floor(size / unitBytes));
      const bytes = encoder.encode(value);
      for (const mode of ["json", "raw", "chunks"] as const) {
        // Size accounting stays outside the measured interval and observer.
        let wireBytes =
          mode === "raw"
            ? encoder.encode(JSON.stringify({ value: Array.from(bytes) }))
                .length
            : mode === "json"
              ? encoder.encode(JSON.stringify({ value })).length
              : 0;
        if (mode === "chunks") {
          for (let offset = 0; offset < value.length; offset += 16 * 1024)
            wireBytes += encoder.encode(
              JSON.stringify({
                value: value.slice(offset, offset + 16 * 1024),
              }),
            ).length;
          if (!value.length) wireBytes = 12;
        }
        await pause(50);
        const frames: number[] = [];
        const longTasks: number[] = [];
        let previous = performance.now();
        let active = true;
        const frame = (now: number) => {
          frames.push(now - previous);
          previous = now;
          if (active) requestAnimationFrame(frame);
        };
        const supported =
          PerformanceObserver.supportedEntryTypes.includes("longtask");
        const observer = supported
          ? new PerformanceObserver((list) => {
              longTasks.push(
                ...list.getEntries().map((entry) => entry.duration),
              );
            })
          : undefined;
        observer?.observe({ entryTypes: ["longtask"] });
        requestAnimationFrame(frame);
        await pause(50);
        const submits: number[] = [];
        const started = performance.now();
        const submit = async (payload: Record<string, unknown>) => {
          const t = performance.now();
          const pending = invoke<string>("plugin:app|version", payload);
          submits.push(performance.now() - t);
          if (typeof (await pending) !== "string")
            throw new Error("spike-response");
        };
        if (mode === "json") await submit({ value });
        else if (mode === "raw") {
          // Model Tauri's installed Uint8Array JSON replacer, not native raw support.
          const t = performance.now();
          const pending = invoke<string>("plugin:app|version", {
            value: bytes,
          });
          submits.push(performance.now() - t);
          await pending;
        } else {
          for (let offset = 0; offset < value.length; offset += 16 * 1024) {
            await submit({ value: value.slice(offset, offset + 16 * 1024) });
          }
          if (!value.length) await submit({ value: "" });
        }
        const elapsedMs = performance.now() - started;
        await pause(50);
        active = false;
        observer?.disconnect();
        cases.push({
          id: `${multilingual ? "unicode" : "ascii"}-${size}-${mode}`,
          utf16Units: value.length,
          utf8Bytes: bytes.length,
          wireBytes,
          submitMaxMs: Math.max(...submits),
          elapsedMs,
          frameMaxMs: Math.max(...frames),
          frames: frames.length,
          longTasks: supported ? longTasks : null,
        });
      }
    }
  }
  return { passed: true, cases };
}

function check(ok: unknown, id: string): asserts ok {
  if (!ok) throw new Error(id);
}

export async function runPersistenceSuite(api: {
  configure(mode: "recent", defer: boolean): Promise<void>;
  publish(source: string, active?: boolean): Promise<void>;
  sourceMatches(source: string): boolean;
}) {
  const cases: Record<string, unknown>[] = [];
  let phase = "persistence-init";
  const progress = (id: string) => {
    phase = id;
    (window as any).__streamingSmokeProgress?.(id);
  };
  try {
    const opened = await invoke<{ revision: number }>("pds_open");
    const stage = await invoke<{ stagingId: string }>("pds_replace_begin");
    const { characters, botPresets, ...root } = syntheticDatabase;
    await invoke("pds_replace_put_root", { stagingId: stage.stagingId, root });
    await invoke("pds_replace_put_presets", {
      stagingId: stage.stagingId,
      presets: botPresets,
    });
    await invoke("pds_replace_add_characters", {
      stagingId: stage.stagingId,
      characters,
    });
    let { revision } = await invoke<{ revision: number }>(
      "pds_replace_commit",
      { stagingId: stage.stagingId, expectedRevision: opened.revision },
    );
    await api.configure("recent", true);
    const encoder = new TextEncoder();
    for (const scope of ["root", "plugin", "conversation"] as const) {
      for (const multilingual of [false, true]) {
        for (const size of [0, 256 * 1024, 1024 * 1024, 6 * 1024 * 1024]) {
          const unit = multilingual ? '합성🐿️\\"\n' : 'abc\\"\n';
          const value = unit.repeat(
            Math.floor(size / encoder.encode(unit).length),
          );
          for (let repeat = 0; repeat < 2; repeat++) {
            for (const mode of repeat % 2
              ? ["optimized", "strings", "json"]
              : ["json", "strings", "optimized"]) {
              progress(
                `persistence-${scope}-${multilingual ? "unicode" : "ascii"}-${size}-${repeat}-${mode}`,
              );
              const commit: CommitEnvelope["commit"] = {
                expectedRevision: revision,
              };
              // Alternate a small suffix so every sample is a real mutation.
              const text = value + `-${repeat}-${mode}`;
              if (scope === "root")
                commit.rootMutations = [
                  {
                    type: "set",
                    key: "username",
                    value: { nested: [text] } as any,
                  },
                ];
              if (scope === "plugin")
                commit.pluginStorage = [
                  {
                    type: "set",
                    owner: benchmarkPluginOwner,
                    key: "android-persistence",
                    value: { nested: [text] },
                  },
                ];
              if (scope === "conversation")
                commit.conversations = [
                  {
                    type: "replace-range",
                    characterId: "char-b",
                    conversationId: "conv-beta",
                    start: 0,
                    deleteCount: 1,
                    messages: [
                      {
                        role: "char",
                        data: text,
                        chatId: "android-persistence-message",
                      },
                    ],
                  },
                ];
              const input: CommitEnvelope = { commit, assetAliases: [] };
              const encoded = encoder.encode(JSON.stringify(input)).length; // outside measured work
              const submits: number[] = [];
              const commands: Record<string, number> = {};
              const chunks: number[] = [];
              let binaryChunks = 0;
              let binaryMaxBytes = 0;
              const binary =
                mode === "optimized"
                  ? getAndroidBinaryCommitBridge()
                  : undefined;
              const measuredBinary = binary
                ? {
                    get onmessage() {
                      return binary.onmessage;
                    },
                    set onmessage(listener) {
                      binary.onmessage = listener;
                    },
                    postMessage(packet: ArrayBuffer) {
                      binaryChunks++;
                      binaryMaxBytes = Math.max(
                        binaryMaxBytes,
                        packet.byteLength - 40,
                      );
                      const t = performance.now();
                      binary.postMessage(packet);
                      submits.push(performance.now() - t);
                    },
                  }
                : null;
              const measuredInvoke = <T>(
                command: string,
                args?: InvokeArgs,
                options?: InvokeOptions,
              ): Promise<T> => {
                commands[command] = (commands[command] ?? 0) + 1;
                if (command === "pds_commit_android_chunk")
                  chunks.push((args as any).chunk.length);
                const t = performance.now();
                const pending = invoke<T>(command, args, options);
                submits.push(performance.now() - t);
                return pending;
              };
              const transport = new NativeCommitTransport({
                windows: () => false,
                android: () => true,
                androidBinary: () => measuredBinary,
                invoke: measuredInvoke,
                encode: encodeNativeCommit,
                shared: () => undefined,
              });
              const frames: number[] = [];
              const longTasks: number[] = [];
              let active = true;
              let last = performance.now();
              const frame = (now: number) => {
                frames.push(now - last);
                last = now;
                if (active) requestAnimationFrame(frame);
              };
              const supported =
                PerformanceObserver.supportedEntryTypes.includes("longtask");
              const observer = supported
                ? new PerformanceObserver((list) =>
                    longTasks.push(...list.getEntries().map((e) => e.duration)),
                  )
                : undefined;
              observer?.observe({ entryTypes: ["longtask"] });
              requestAnimationFrame(frame);
              await pause(50);
              const start = performance.now();
              let publications = 0;
              let publishPending: Promise<void> = Promise.resolve();
              let publishing = false;
              const publish = () => {
                if (publishing) return;
                publishing = true;
                publications++;
                publishPending = api
                  .publish(
                    `<Thoughts>합성 추론 ${publications}</Thoughts>\nAnswer ${publications}`,
                  )
                  .finally(() => {
                    publishing = false;
                  });
              };
              const timer = setInterval(publish, 33);
              let saved: { revision: number };
              try {
                saved = await (mode === "json"
                  ? measuredInvoke("pds_commit", input as any)
                  : transport.commit(input));
              } finally {
                clearInterval(timer);
                await publishPending;
              }
              const elapsedMs = performance.now() - start;
              await pause(50);
              active = false;
              observer?.disconnect();
              check(saved!.revision === revision + 1, "single-revision");
              revision = saved!.revision;
              const read =
                scope === "root"
                  ? await invoke<any>("pds_read_root")
                  : scope === "plugin"
                    ? await invoke<any>("pds_read_plugin_storage", {
                        owner: benchmarkPluginOwner,
                        key: "android-persistence",
                      })
                    : await invoke<any>("pds_read_conversation", {
                        characterId: "char-b",
                        conversationId: "conv-beta",
                      });
              const actual =
                scope === "root"
                  ? read.value.username.nested[0]
                  : scope === "plugin"
                    ? read.value.nested[0]
                    : read.value.message[0].data;
              check(
                actual === text && read.revision === revision,
                "exact-saved-source",
              );
              cases.push({
                id: phase,
                scope,
                mode,
                repeat,
                utf8PayloadBytes: encoded,
                utf16SourceUnits: text.length,
                elapsedMs,
                submitMaxMs: Math.max(...submits),
                frameMaxMs: Math.max(...frames),
                frameP95Ms: frames.sort((a, b) => a - b)[
                  Math.ceil(frames.length * 0.95) - 1
                ],
                frames: frames.length,
                publications,
                commands,
                chunkMaxUtf16: chunks.length ? Math.max(...chunks) : null,
                chunkByteLimit:
                  mode === "json"
                    ? null
                    : binary
                      ? ANDROID_BINARY_CHUNK_BYTES
                      : ANDROID_COMMIT_CHUNK_BYTES,
                binaryChunks,
                binaryMaxBytes,
                longTasks: supported ? longTasks : null,
                exact: true,
                jsHeapBytes:
                  (performance as any).memory?.usedJSHeapSize ?? null,
              });
            }
          }
        }
      }
    }
    progress("persistence-protocol");
    const before = await invoke<any>("pds_read_root");
    const id = crypto.randomUUID();
    await invoke("pds_commit_android_open", {
      id,
      totalBytes: 2,
      binary: false,
    });
    const reject = async (command: string, args: Record<string, unknown>) => {
      let failed = false;
      try {
        await invoke(command, args);
      } catch {
        failed = true;
      }
      check(failed, "invalid-transfer-rejected");
    };
    await reject("pds_commit_android_open", {
      id: crypto.randomUUID(),
      totalBytes: 2,
      binary: false,
    });
    await reject("pds_commit_android_chunk", { id, offset: 1, chunk: "{}" });
    await reject("pds_commit_android_chunk", {
      id: crypto.randomUUID(),
      offset: 0,
      chunk: "{}",
    });
    await reject("pds_commit_android_finish", { id });
    await invoke("pds_commit_android_cancel", { id });
    await reject("pds_commit_android_finish", { id });
    const bridge = getAndroidBinaryCommitBridge();
    if (bridge) {
      const binaryId = crypto.randomUUID();
      await invoke("pds_commit_android_open", {
        id: binaryId,
        totalBytes: 2,
        binary: true,
      });
      const sender = binaryCommitSender(bridge, binaryId);
      try {
        let rejected = false;
        try {
          await sender.append(1, new Uint8Array([123]));
        } catch {
          rejected = true;
        }
        check(rejected, "binary-offset-rejected");
        await reject("pds_commit_android_chunk", {
          id: binaryId,
          offset: 0,
          chunk: "{}",
        });
        await sender.append(0, new Uint8Array([123]));
        await reject("pds_commit_android_finish", { id: binaryId });
        await invoke("pds_commit_android_cancel", { id: binaryId });
        rejected = false;
        try {
          await sender.append(1, new Uint8Array([125]));
        } catch {
          rejected = true;
        }
        check(rejected, "binary-cancelled-rejected");
      } finally {
        sender.close();
        await invoke("pds_commit_android_cancel", { id: binaryId });
      }
    }
    const stale: CommitEnvelope = {
      commit: {
        expectedRevision: revision - 1,
        rootMutations: [
          { type: "set", key: "username", value: "x".repeat(256 * 1024) },
        ],
      },
      assetAliases: [],
    };
    const transport = new NativeCommitTransport({
      windows: () => false,
      android: () => true,
      invoke,
      encode: encodeNativeCommit,
      shared: () => undefined,
    });
    let conflict = false;
    try {
      await transport.commit(stale);
    } catch (error: any) {
      conflict = error?.code === "revision-conflict";
    }
    check(conflict, "stale-revision");
    check(
      JSON.stringify(await invoke("pds_read_root")) === JSON.stringify(before),
      "failed-save-is-atomic",
    );
    const final = "Synthetic final persisted answer 한글 🐿️";
    const result = await transport.commit({
      commit: {
        expectedRevision: revision,
        rootMutations: [{ type: "set", key: "username", value: final }],
      },
      assetAliases: [],
    });
    await api.publish(final, false);
    check(api.sourceMatches(final), "final-display-source");
    check(
      (await invoke<any>("pds_read_root")).value.username === final,
      "final-persisted-source",
    );
    // The runner reloads this page and confirms a stale partial producer was released.
    const interrupted = crypto.randomUUID();
    await invoke("pds_commit_android_open", {
      id: interrupted,
      totalBytes: 3,
      binary: Boolean(bridge),
    });
    if (bridge) {
      const sender = binaryCommitSender(bridge, interrupted);
      try {
        await sender.append(0, new Uint8Array([97]));
      } finally {
        sender.close();
      }
    } else
      await invoke("pds_commit_android_chunk", {
        id: interrupted,
        offset: 0,
        chunk: "a",
      });
    cases.push({
      id: "protocol-and-final-save",
      exact: true,
      revision: result.revision,
    });
    return { passed: true, cases };
  } catch (error) {
    return {
      passed: false,
      phase,
      assertion:
        error instanceof Error ? error.message : "native-command-failed",
      cases,
    };
  }
}

export async function checkPersistenceReload() {
  await invoke("pds_open");
  const root = await invoke<any>("pds_read_root");
  check(
    root.value.username === "Synthetic final persisted answer 한글 🐿️",
    "reload-durable-source",
  );
  const id = crypto.randomUUID();
  await invoke("pds_commit_android_open", {
    id,
    totalBytes: 1,
    binary: Boolean(getAndroidBinaryCommitBridge()),
  });
  await invoke("pds_commit_android_cancel", { id });
  return { passed: true, id: "reload-releases-partial-and-retains-save" };
}
