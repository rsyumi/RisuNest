import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { registerMacosLifecycle, type MacosExitRequest } from "../../src/ts/storage/macosLifecycle";
import {
  invokeNativeTokenizerBatch,
  resolveNativeTokenizerRoute,
  type NativeTokenizerId,
} from "../../src/ts/tokenizer/nativeTokenizer";
import corpus from "../tokenizer/native-tokenizer-corpus.json";
import { createNativeTokenizerBenchmarkSeam } from "../tokenizer/nativeTokenizerBenchmark";
import { persistence, reload, regex, guard, check, pause } from "./contracts";
import { showNativeHarness } from "../nativeAppLock";
import { failureRecord } from "../failureRecord";
import { SyncStepError, syncFailureRecord } from "../sync-ui/driver";

const report = (stage: string, result: unknown) =>
  invoke("macos_bench_report", { stage, result });
const expectedKey = "macos-synthetic-expected";
const failureDetail = (error: unknown) =>
  error instanceof Error ? (error as Error & { detail?: unknown }).detail : undefined;
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
        const value = error as {
          code?: string;
          special_token?: string;
          index?: number;
        };
        check(
          value.code === entry.error!.code &&
            value.special_token === entry.error!.token &&
            value.index === 0,
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

async function verifyStored(expected = JSON.parse(localStorage.getItem(expectedKey)!)) {
  check(expected, "previous synthetic result exists");
  const actual = await reload();
  check(
    actual.revision === expected.revision &&
      actual.finalHash === expected.finalHash,
    "exact revision/hash survived reload or restart",
    { expected, actual: { revision: actual.revision, finalHash: actual.finalHash } },
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
    saveLocally: async () => {
      // The harness quits through terminate:, which never ends the session.
      await report("failure", { passed: false, message: "Unexpected session-end quit" });
    },
  });
  await invoke("macos_bench_quit");
  await until(async () => cancelled, "quit failure was not confirmed");
  await pause(500);
  check(await window.isVisible(), "failed quit keeps visible app");
  await report("quit-cancelled", { passed: true });
  await invoke("macos_bench_quit");
}

async function startupAppearance(seed: boolean, theme: "light" | "dark") {
  await guard();
  check(Boolean(document.getElementById("preloading")), "appearance probe requires product HTML build");
  const nativeThemeReadback = async () => {
    let nativeTheme: string | null = null;
    await until(async () => {
      nativeTheme = await getCurrentWindow().theme();
      return nativeTheme === theme;
    }, "product native appearance theme timed out");
    const colorScheme = getComputedStyle(document.documentElement).colorScheme;
    check(colorScheme === theme, "product color scheme must match the expected app theme");
    return { nativeTheme, nativeThemeScope: "app", nativeThemeMatchesApp: true, colorScheme };
  };
  const seedReadback = async () => {
    const marker = localStorage.getItem("appearance-theme");
    const metadata = {
      expectedTheme: theme,
      markerTheme: marker === "light" || marker === "dark" ? marker : null,
      markerPresent: marker !== null, markerLength: marker?.length ?? 0,
      originMatches: location.protocol === "tauri:" && location.hostname === "localhost",
    };
    try {
      const opened = await invoke<{ revision: number }>("pds_open");
      const root = await invoke<{ revision: number; value: {
        colorScheme?: { type?: unknown }; didFirstSetup?: unknown;
      } }>("pds_read_root");
      const schemeType = root.value.colorScheme?.type;
      return {
        ...metadata, readSucceeded: true,
        openedRevision: Number.isSafeInteger(opened.revision) ? opened.revision : null,
        storedSchemeType: schemeType === "light" || schemeType === "dark" ? schemeType : null,
        didFirstSetup: typeof root.value.didFirstSetup === "boolean" ? root.value.didFirstSetup : null,
        readRevision: Number.isSafeInteger(root.revision) ? root.revision : null,
      };
    } catch {
      return { ...metadata, readSucceeded: false, storeReadError: "unavailable" };
    }
  };
  if (seed) {
    const { defaultColorScheme } = await import("../../src/ts/gui/colorscheme");
    const colorScheme = theme === "dark" ? defaultColorScheme : {
      bgcolor: "#ffffff", darkbg: "#f0f0f0", borderc: "#0f172a", selected: "#e0e0e0",
      draculared: "#ff5555", textcolor: "#0f172a", textcolor2: "#64748b",
      darkBorderc: "#d1d5db", darkbutton: "#e5e7eb", type: "light",
    };
    const opened = await invoke<{ revision: number }>("pds_open");
    const committed = await invoke<{ revision: number }>("pds_commit", {
      commit: { expectedRevision: opened.revision, rootMutations: [
        { type: "set", key: "didFirstSetup", value: true },
        { type: "set", key: "colorScheme", value: colorScheme },
      ] }, assetAliases: [],
    });
    localStorage.setItem("tos4", "true");
    localStorage.setItem("appearance-theme", theme);
    const { resolveAppearance, WINDOWS_APPEARANCE_CACHE } = await import("../../src/ts/gui/windowsAppearance");
    const app = await import("../../src/main");
    await app.default;
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const context = canvas.getContext("2d", { willReadFrequently: true });
    check(context, "appearance seed color conversion unavailable");
    const rgba = (color: string): [number, number, number, number] | undefined => {
      if (!color || !CSS.supports("color", color)) return undefined;
      context.clearRect(0, 0, 1, 1);
      context.fillStyle = color;
      context.fillRect(0, 0, 1, 1);
      const data = context.getImageData(0, 0, 1, 1).data;
      return [data[0], data[1], data[2], data[3]];
    };
    const deadline = performance.now() + 60_000;
    while (true) {
      check(performance.now() < deadline, "product appearance seed bootstrap/hint timed out");
      if (performance.getEntriesByName("boot:interactive").length) {
        const style = getComputedStyle(document.documentElement);
        const rendered = resolveAppearance({ ...colorScheme, type: theme }, name => style.getPropertyValue(name), rgba);
        let cached: Record<string, unknown> | null = null;
        try { cached = JSON.parse(localStorage.getItem(WINDOWS_APPEARANCE_CACHE) ?? "null"); } catch {}
        if (cached && typeof cached === "object"
          && cached.background === rendered.background && cached.caption === rendered.caption
          && cached.text === rendered.text && cached.dark === rendered.dark && style.colorScheme === theme) break;
      }
      await pause(50);
    }
    const nativeAppearance = await nativeThemeReadback();
    return {
      seeded: true, theme, startupHintMatchesRenderedPalette: true,
      ...nativeAppearance,
      interactiveMs: performance.getEntriesByName("boot:interactive")[0].startTime,
      openedRevision: Number.isSafeInteger(opened.revision) ? opened.revision : null,
      committedRevision: Number.isSafeInteger(committed.revision) ? committed.revision : null,
      seedReadback: await seedReadback(),
    };
  }
  await report("appearance-app-readback", await seedReadback());
  check(localStorage.getItem("appearance-theme") === theme, "appearance theme seed mismatch");
  const preloaderBackground = getComputedStyle(document.getElementById("preloading")!).backgroundColor;
  const app = await import("../../src/main");
  await app.default;
  const deadline = performance.now() + 60_000;
  while (!performance.getEntriesByName("boot:interactive").length) {
    check(performance.now() < deadline, "product appearance bootstrap timed out");
    await pause(50);
  }
  const style = getComputedStyle(document.documentElement);
  const bounds = document.getElementById("app")!.getBoundingClientRect();
  await pause(1500);
  const nativeAppearance = await nativeThemeReadback();
  return {
    ...nativeAppearance,
    theme, systemDark: matchMedia("(prefers-color-scheme: dark)").matches,
    preloaderBackground, colorScheme: style.colorScheme,
    appBackground: style.getPropertyValue("--risu-theme-bgcolor"),
    bounds: { x: bounds.x, y: bounds.y, width: bounds.width, height: bounds.height },
    interactiveMs: performance.getEntriesByName("boot:interactive")[0].startTime,
    visualReview: "required", contentBackgroundPass: null, titlebarPass: null,
  };
}

async function main() {
  await guard();
  const phase = await invoke<string>("macos_bench_phase");
  if (/^app-lock-(immediate|timed|immediate-mutant)$/.test(phase)) {
    const { appLockFixture } = await import('../appLock/fixture');
    const { initialize } = await import('./contracts');
    await appLockFixture('macos', phase, initialize);
    return;
  }
  if (phase !== "app" && !/^appearance-(seed|app)-(light|dark)$/.test(phase)
    && !/^sync-(publish|receive|pull)$/.test(phase)
    && !/^(external-storage|external-sync|backup-file)(-restart)?$/.test(phase)) {
    await showNativeHarness();
  }
  if (phase.startsWith("session-dispatch-")) {
    await (await import("./terminationDispatch")).terminationDispatch(phase);
  } else if (/^appearance-(seed|app)-(light|dark)$/.test(phase)) {
    const [, action, theme] = phase.split("-");
    await report(phase, await startupAppearance(action === "seed", theme as "light" | "dark"));
    await invoke("macos_bench_quit");
  } else if (phase === "session-deadline" || phase === "session-upgrade") {
    // Deliberately install no renderer exit listener. Native expiry must release AppKit.
    await invoke("macos_lifecycle_ready");
    await report("session-deadline-started", { passed: true, rendererResponds: false });
    await invoke("macos_bench_session_quit");
  } else if (phase === "termination-probe") {
    const settle = async (attempt: number) => {
      await invoke("macos_bench_modal_ack", { attempt, approve: attempt === 3 });
      if (attempt !== 3) {
        await until(async () => !(await invoke<{ pending: boolean }>("macos_bench_modal_status")).pending,
          "native termination reply did not settle");
        void invoke("macos_bench_modal_begin");
      }
    };
    window.addEventListener("termination-probe", event => {
      const attempt = (event as CustomEvent<number>).detail;
      if (attempt === 2) {
        location.reload();
      } else {
        void settle(attempt);
      }
    });
    const state = await invoke<{ attempt: number; pending: boolean }>("macos_bench_modal_status");
    if (state.attempt === 0 && !state.pending) {
      void invoke("macos_bench_modal_begin");
    } else {
      check(state.attempt === 2 && state.pending, "Reloaded document must resume native termination attempt 2");
      await settle(2);
    }
  } else if (phase === "contracts") {
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
    // The controller hands over the quit save; WebKit may drop storage written this close to the quit.
    const stored = await invoke<string | null>("macos_bench_expected");
    await report("restart", { passed: true, ...(await verifyStored(stored ? JSON.parse(stored) : null)) });
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
    const marker = "macos-synthetic-ui-edit 🐿️";
    const runtime = getPersistentDataRuntime();
    const editLastMessage = async (value: string) => {
      const lease = await runtime.acquireCompleteConversation("edit-message");
      try {
        const { captureChatMessageTarget, saveCapturedChatMessage } =
          await import("../../src/ts/chatMessageUi");
        const context = {
          captureCurrent: () => {
            const character = DBState.db.characters[index];
            return {
              character,
              conversation: character.chats[character.chatPage],
            };
          },
          getCurrentSession: () => runtime.getActiveConversationSession(),
        };
        const target = captureChatMessageTarget({
          ...context,
          absoluteIndex: lease.session.totalMessages - 1,
        });
        check(target, "capture product message edit target");
        check(
          saveCapturedChatMessage(target, context, value).saved,
          "product message edit accepted",
        );
        return lease.session.conversationId;
      } finally {
        lease.release();
      }
    };
    const conversationId = await editLastMessage(marker);
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
      conversationId,
    });
    check(
      persisted.value.message.at(-1)?.data === marker,
      "product working-set edit persisted through Rust",
      { revision: persisted.revision, last: persisted.value.message.at(-1)?.data },
    );
    await report("app", {
      passed: true,
      renderedTextLength: document.getElementById("app")!.textContent!.length,
      revision: persisted.revision,
    });
    await pause(2000);
    const nativeEvents = await invoke<string[]>("macos_bench_events");
    const replies = nativeEvents.filter(event => event.startsWith("native-reply-"));
    check(
      replies.length === 0 || (replies.length === 1 && replies[0] === "native-reply-no"),
      "product document must start or resume after one native reload cancellation",
    );
    check(!nativeEvents.includes("quit"), "native product quit must not request a runtime exit");
    const attempt = replies.length + 1;
    if (attempt === 2) {
      await report("app-native-reload-cancelled", { passed: true, replyCount: 1 });
      const token = localStorage.getItem("macos-app-departed-token");
      check(token, "departed product request token was observed");
      let rejected = false;
      try {
        await invoke("macos_exit_response", { token, exit: true });
      } catch (error) {
        rejected = String(error) === "No matching macOS quit request";
      }
      check(rejected, "departed product token must reject late approval");
      const after = await invoke<string[]>("macos_bench_events");
      check(
        after.filter(event => event.startsWith("native-reply-")).length === 1 && !after.includes("quit"),
        "stale product response must not reply or request runtime exit",
      );
      await report("app-native-stale-rejected", { passed: true, replyCount: 1 });
    }
    const { get } = await import("svelte/store");
    const { syncExitDialogState } = await import("../../src/ts/storage/syncExitProduction");
    const flush = runtime.flushPendingDataLocally.bind(runtime);
    let release!: () => void;
    const gate = new Promise<void>(resolve => { release = resolve; });
    let flushEntered = false;
    let flushCalls = 0;
    const nativeMarker = "macos-synthetic-native-quit-saved 🐿️";
    runtime.flushPendingDataLocally = async reason => {
      if (reason !== "normal-exit") return flush(reason);
      try {
        check(++flushCalls === 1, "one coordinated local flush per native product request");
        flushEntered = true;
        await gate;
        await flush(reason);
        const saved = await invoke<{
          revision: number;
          value: { message: { data: string }[] };
        }>("pds_read_conversation", { characterId: "char-a", conversationId });
        const read = { previous: persisted.revision, revision: saved.revision, last: saved.value.message.at(-1)?.data };
        check(read.last === nativeMarker, "native quit saves the fresh product edit", read);
        check(Number.isSafeInteger(saved.revision) && saved.revision > persisted.revision,
          "native quit advances the saved revision", read);
        // The controller hands this to the restart; WebKit may drop storage written this close to the quit.
        await report("app-native-saved", {
          passed: true, revision: saved.revision, conversationId, marker: nativeMarker,
        });
      } catch (error) {
        await report("failure", { passed: false, ...failureRecord(error), message: String(error), detail: failureDetail(error) });
        throw error;
      }
    };
    let nativeRequests = 0;
    const unlisten = await listen<MacosExitRequest>("risu-macos-exit-requested", ({ payload }) => {
      nativeRequests++;
      if (attempt === 1) localStorage.setItem("macos-app-departed-token", payload.token);
    });
    await invoke("macos_bench_native_quit");
    await until(async () => flushEntered && get(syncExitDialogState).phase === "saving"
      && !!document.querySelector('[data-testid="sync-exit-dialog"]'),
      "real product saving decision UI did not render during native termination");
    check(nativeRequests === 1, "one product event per native termination transaction");
    check(await getCurrentWindow().isVisible(), "native decision UI keeps the main window visible");
    await report("app-native-saving", { passed: true, attempt, nativeRequests, flushCalls });
    if (attempt === 1) {
      unlisten();
      location.reload();
      return;
    }
    check(await editLastMessage(nativeMarker) === conversationId, "native save uses the same synthetic conversation");
    await tick();
    await until(async () => document.getElementById("app")!.textContent!.includes(nativeMarker),
      "fresh native quit edit did not render");
    release();
  } else if (phase === "app-restart") {
    await invoke("pds_open");
    const stored = await invoke<string | null>("macos_bench_expected");
    const expected = stored ? JSON.parse(stored) : null;
    check(expected, "native quit expectation was handed over", { stored });
    const saved = await invoke<{ revision: number; value: { message: { data: string }[] } }>(
      "pds_read_conversation",
      { characterId: "char-a", conversationId: expected.conversationId },
    );
    const actual = {
      revision: saved.revision,
      messages: saved.value.message.length,
      last: saved.value.message.at(-1)?.data,
    };
    check(
      actual.last === expected.marker,
      "product UI edit survives quit and restart",
      { stored, actual },
    );
    check(Number.isSafeInteger(expected.revision) && saved.revision === expected.revision,
      "native quit saved revision survives restart exactly", { stored, actual });
    await report("app-restart", { passed: true, revision: saved.revision });
    await invoke("macos_bench_quit");
  } else if (phase === "quit-escape") {
    // The document never acknowledges, as a hung renderer would not.
    const requests: unknown[] = [];
    await listen("risu-macos-exit-requested", ({ payload }) => {
      requests.push(payload);
    });
    // As in the product, the page-start reset queued for the main thread runs before the document is ready.
    await invoke("macos_bench_main_thread_settled");
    await invoke("macos_lifecycle_ready");
    await report("quit-escape-ready", { passed: true });
    await invoke("macos_bench_native_quit");
    await until(async () => requests.length === 1, "native quit did not reach the document");
    await report("quit-escape-delivered", { passed: true });
    // AppKit ends a quit repeated while the first one waits, without asking the product again.
    await invoke("macos_bench_repeat_native_quit");
    await pause(15_000);
    throw new Error("a repeated native quit did not end the app");
  } else if (/^sync-(publish|receive|pull)$/.test(phase)) {
    await (await import("./sync")).syncPhase(phase);
  } else if (/^(external-storage|external-sync|backup-file)(-restart)?$/.test(phase)) {
    await (await import("./externalStorage")).externalStoragePhase(phase);
  } else if (phase === "streaming") {
    document.getElementById("benchmark")!.remove();
    await import("../streaming/main");
    const api = (
      window as unknown as {
        __streamingSmoke: {
          run(): Promise<{
            passed: boolean;
            assertion?: string;
            diagnostics?: Record<string, unknown>;
          }>;
        };
      }
    ).__streamingSmoke;
    const result = await api.run();
    check(
      result.passed,
      `streaming suite: ${result.assertion ?? "failed"} ${JSON.stringify(result.diagnostics ?? {})}`,
    );
    await report("streaming", result);
    await invoke("macos_bench_quit");
  } else {
    throw new Error("Unknown isolated Mac harness phase");
  }
}
void main().catch(async (error) => {
  await report("failure", {
    // A Sync step keeps the error it wraps, which is redacted as the step's own message is.
    ...(error instanceof SyncStepError ? syncFailureRecord(error) : failureRecord(error)),
    message:
      error instanceof Error
        ? error.message
        : typeof error === "string"
          ? error
          : JSON.stringify(error),
    detail: failureDetail(error),
  });
  // The controller records the failure and terminates this isolated process.
});
