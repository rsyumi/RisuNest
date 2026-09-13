import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { registerMacosLifecycle } from "../../src/ts/storage/macosLifecycle";
import {
  invokeNativeTokenizerBatch,
  resolveNativeTokenizerRoute,
  type NativeTokenizerId,
} from "../../src/ts/tokenizer/nativeTokenizer";
import corpus from "../tokenizer/native-tokenizer-corpus.json";
import { createNativeTokenizerBenchmarkSeam } from "../tokenizer/nativeTokenizerBenchmark";
import { persistence, reload, regex, guard, check, pause } from "./contracts";

const report = (stage: string, result: unknown) =>
  invoke("macos_bench_report", { stage, result });
const expectedKey = "macos-synthetic-expected";
async function until(predicate: () => Promise<boolean>, message: string) {
  const deadline = performance.now() + 60_000;
  while (!(await predicate())) {
    check(performance.now() < deadline, message);
    await pause(100);
  }
}

async function tokenizer() {
  let ids = 0;
  let errors = 0;
  for (const entry of corpus.cases) {
    const route = resolveNativeTokenizerRoute(
      entry.tokenizerId as NativeTokenizerId,
      true,
      true,
    );
    check(route.kind === "native-tiktoken", "native tokenizer route");
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
        `${entry.name}: exact token IDs`,
      );
      const count = await invokeNativeTokenizerBatch(route, [text], "count");
      check(
        count.mode === "count" && count.counts[0] === entry.ids!.length,
        `${entry.name}: count`,
      );
      ids++;
    } else {
      let rejected = false;
      try {
        await invokeNativeTokenizerBatch(route, [text], "ids");
      } catch (error) {
        const value = error as { code?: string };
        check(
          value.code === entry.error!.code,
          `${entry.name}: special token error category`,
        );
        rejected = true;
      }
      check(rejected, `${entry.name}: special token must reject`);
      errors++;
    }
  }
  const seam = createNativeTokenizerBenchmarkSeam();
  const samples = [];
  try {
    for (const tokenizerId of ["cl100k_base", "o200k_base"] as const) {
      seam.initialize(tokenizerId);
      seam.verifyJavaScriptCorpus(tokenizerId);
      for (const mode of ["count", "ids"] as const) {
        for (const itemCount of [100, 1000]) {
          const request = {
            tokenizerId,
            mode,
            texts: Array.from(
              { length: itemCount },
              (_, index) =>
                `합성 chat ${index}: hello world 🐿️ ${"text ".repeat(16)}`,
            ),
          };
          for (let repeat = -2; repeat < 20; repeat++) {
            const outputs = [];
            for (const implementation of repeat % 2
              ? (["native", "javascript"] as const)
              : (["javascript", "native"] as const)) {
              const measured = await seam.measure(implementation, request, 1);
              outputs.push(measured.result);
              if (repeat >= 0)
                samples.push({
                  tokenizerId,
                  mode,
                  itemCount,
                  repeat,
                  implementation,
                  elapsedMs: measured.durationsMs[0],
                });
            }
            check(
              JSON.stringify(outputs[0]) === JSON.stringify(outputs[1]),
              "tokenizer benchmark output oracle",
            );
          }
        }
      }
    }
  } finally {
    seam.dispose();
  }
  return { passed: true, ids, errors, samples };
}

async function verifyStored() {
  const expected = JSON.parse(localStorage.getItem(expectedKey)!);
  check(expected, "previous synthetic result exists");
  const actual = await reload();
  check(
    actual.revision === expected.revision &&
      actual.finalHash === expected.finalHash,
    "exact revision/hash survived reload or restart",
  );
  return actual;
}

async function lifecycle() {
  const window = getCurrentWindow();
  await window.close();
  await until(
    async () => !(await window.isVisible()),
    "main window did not hide",
  );
  await report("closed", { passed: true });
  await until(
    () => window.isVisible(),
    "Dock did not restore hidden main window",
  );
  check(
    (await invoke<string[]>("macos_bench_events")).includes("reopen"),
    "real NSApplication Reopen event required",
  );
  await report("reopened", { passed: true });
  const files: string[] = [];
  await until(async () => {
    files.push(...(await invoke<string[]>("opened_files_take")));
    return files.length >= 2;
  }, "Finder file open delivery timed out");
  check(files.length === 2, "each Finder file delivered once");
  check(
    files.some((file) => file.endsWith("synthetic 한글 # %.risup")) &&
      files.some((file) => file.endsWith("synthetic-two.risum")),
    "encoded Finder filenames preserved",
  );
  check(
    (await invoke<string[]>("opened_files_take")).length === 0,
    "file queue drains exactly once",
  );
  check(
    (await invoke<string[]>("macos_bench_events")).includes("opened"),
    "real NSApplication Opened event required",
  );
  await report("finder", { passed: true, count: files.length });
  let attempt = 0;
  let cancelled = false;
  await registerMacosLifecycle({
    flush: async () => {
      if (++attempt === 1)
        throw new Error("Synthetic save failure for quit cancellation");
      const previous = await verifyStored();
      const result = await invoke<{ revision: number }>("pds_commit", {
        commit: {
          expectedRevision: previous.revision,
          rootMutations: [
            {
              type: "set",
              key: "username",
              value: "macos-synthetic-quit-saved",
            },
          ],
        },
        assetAliases: [],
      });
      check(
        result.revision === previous.revision + 1,
        "quit flush commits exactly once",
      );
      localStorage.setItem(
        expectedKey,
        JSON.stringify({ ...previous, revision: result.revision }),
      );
    },
    checkpoint: async () => {
      await invoke("pds_checkpoint", { mode: "truncate" });
      await report("quit-saved", { passed: true, ...(await verifyStored()) });
    },
    confirmExitWithoutSaving: async () => {
      cancelled = true;
      return false;
    },
    sync: {
      isSyncActive: () => false,
      hasPendingSync: () => false,
      confirmExit: async () => false,
    },
  });
  await invoke("macos_bench_quit");
  await until(async () => cancelled, "quit failure was not confirmed");
  await pause(500);
  check(await window.isVisible(), "failed quit keeps visible app");
  await report("quit-cancelled", { passed: true });
  await invoke("macos_bench_quit");
}

async function main() {
  await guard();
  const phase = await invoke<string>("macos_bench_phase");
  if (phase === "contracts") {
    if (sessionStorage.getItem("macos-contract-reload")) {
      await report("reload", { passed: true, ...(await verifyStored()) });
      await lifecycle();
      return;
    }
    const result = await persistence();
    localStorage.setItem(expectedKey, JSON.stringify(result));
    await report("persistence", result);
    await report("regex", await regex());
    await report("tokenizer", await tokenizer());
    sessionStorage.setItem("macos-contract-reload", "true");
    location.reload();
  } else if (phase === "restart") {
    await report("restart", { passed: true, ...(await verifyStored()) });
    await invoke("macos_bench_quit");
  } else if (phase === "app") {
    document.getElementById("benchmark")!.remove();
    // This profile is entirely synthetic; preseed completed local onboarding.
    const opened = await invoke<{ revision: number }>("pds_open");
    await invoke("pds_commit", {
      commit: {
        expectedRevision: opened.revision,
        rootMutations: [{ type: "set", key: "didFirstSetup", value: true }],
      },
      assetAliases: [],
    });
    localStorage.setItem("tos4", "true");
    const app = await import("../../src/main");
    await app.default;
    const { getPersistentDataRuntime } = await import(
      "../../src/ts/storage/persistentDataRuntime.svelte"
    );
    await until(async () => {
      try {
        return (
          Boolean(getPersistentDataRuntime().store) &&
          performance.getEntriesByName("boot:interactive").length > 0 &&
          document.getElementById("app")!.textContent!.length > 100
        );
      } catch {
        return false;
      }
    }, "product Svelte app did not initialize");
    const { DBState } = await import("../../src/ts/stores.svelte");
    const { changeChar } = await import("../../src/ts/characters");
    const { tick } = await import("svelte");
    const index = DBState.db.characters.findIndex(
      (character) => character.chaId === "char-a",
    );
    check(
      index >= 0 && (await changeChar(index)),
      "open synthetic character in product UI",
    );
    const character = DBState.db.characters[index];
    const chat = character.chats[character.chatPage];
    const last = chat.message.at(-1)!;
    const marker = "macos-synthetic-ui-edit 🐿️";
    last.data = marker;
    await tick();
    await getPersistentDataRuntime().flushPendingData("macos-ui-smoke");
    await until(
      async () => document.getElementById("app")!.textContent!.includes(marker),
      "edited synthetic message did not render in product chat",
    );
    const persisted = await invoke<{
      revision: number;
      value: { message: { data: string }[] };
    }>("pds_read_conversation", {
      characterId: "char-a",
      conversationId: chat.id,
    });
    check(
      persisted.value.message.at(-1)?.data === marker,
      "product working-set edit persisted through Rust",
    );
    localStorage.setItem(
      "macos-app-expected",
      JSON.stringify({ conversationId: chat.id, marker }),
    );
    await report("app", {
      passed: true,
      renderedTextLength: document.getElementById("app")!.textContent!.length,
      revision: persisted.revision,
    });
    await pause(2000);
    await invoke("macos_bench_quit");
  } else if (phase === "app-restart") {
    await invoke("pds_open");
    const expected = JSON.parse(localStorage.getItem("macos-app-expected")!);
    const saved = await invoke<{ value: { message: { data: string }[] } }>(
      "pds_read_conversation",
      { characterId: "char-a", conversationId: expected.conversationId },
    );
    check(
      saved.value.message.at(-1)?.data === expected.marker,
      "product UI edit survives quit and restart",
    );
    await report("app-restart", { passed: true });
    await invoke("macos_bench_quit");
  } else {
    throw new Error("Unknown isolated Mac harness phase");
  }
}
void main().catch(async (error) => {
  await report("failure", {
    message: error instanceof Error ? error.message : String(error),
  });
  // The controller records the failure and terminates this isolated process.
});
