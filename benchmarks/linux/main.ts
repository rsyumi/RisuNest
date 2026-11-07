import { showNativeHarness } from "../nativeAppLock";
import { failureRecord } from "../failureRecord";
import { runUnicodePersistenceProbe, verifyUnicodePersistenceProbe } from "../unicodePersistenceProbe";
import { invoke } from "@tauri-apps/api/core";
import { getIdentifier } from "@tauri-apps/api/app";
import { platform } from "@tauri-apps/plugin-os";
import {
  NativeCommitTransport,
  encodeNativeCommit,
  type CommitEnvelope,
} from "../../src/ts/storage/nativeCommitTransport";
import { executeNativeRegexBatch } from "../../src/ts/process/nativeRegexBatch";
import { classifyRegexSafePlan } from "../../src/ts/process/regexSafePlan";
import { getRegexExecutionPlan } from "../../src/ts/process/regexExecutionPlan";
import { makeRegexFixture } from "../../src/ts/process/tests/phase1Fixtures";
import fixture from "../../src-tauri/fixtures/persistent-fixture.json";

const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
function check(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}
async function guard() {
  check(
    (await getIdentifier()) === "io.github.rsyumi.risunest.linux.bench",
    "isolated benchmark identifier required",
  );
}
async function initialize() {
  await guard();
  const opened = await invoke<{ revision: number }>("pds_open");
  const { stagingId } = await invoke<{ stagingId: string }>(
    "pds_replace_begin",
  );
  const { characters, botPresets, ...root } = fixture;
  await invoke("pds_replace_put_root", { stagingId, root });
  await invoke("pds_replace_put_presets", { stagingId, presets: botPresets });
  await invoke("pds_replace_add_characters", { stagingId, characters });
  const revision = (
    await invoke<{ revision: number }>("pds_replace_commit", {
      stagingId,
      expectedRevision: opened.revision,
    })
  ).revision;
  for (const body of [new TextEncoder().encode("{"), {}]) {
    let rejected = false;
    try {
      await invoke("pds_commit_raw", body);
    } catch {
      rejected = true;
    }
    check(rejected, "malformed raw request must fail");
    check(
      (await invoke<{ revision: number }>("pds_open")).revision === revision,
      "invalid raw request mutated store",
    );
  }
  return revision;
}
async function readMessage() {
  return invoke<{ revision: number; value: { message: { data: string }[] } }>(
    "pds_read_conversation",
    { characterId: "char-b", conversationId: "conv-beta" },
  );
}
async function digest(value: string) {
  return Array.from(
    new Uint8Array(
      await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)),
    ),
    (x) => x.toString(16).padStart(2, "0"),
  ).join("");
}
async function persistence() {
  let revision = await initialize();
  await showNativeHarness();
  const samples: Record<string, unknown>[] = [];
  const commands: string[] = [];
  const transport = new NativeCommitTransport({
    windows: () => false,
    linux: () => platform() === "linux",
    invoke: (command, args) => {
      commands.push(command);
      return invoke(command, args);
    },
    encode: encodeNativeCommit,
    shared: () => undefined,
  });
  for (const size of [1024 * 1024, 6 * 1024 * 1024]) {
    const unit = '합성🐿️\\"\n';
    const body = unit.repeat(
      Math.floor(size / new TextEncoder().encode(unit).length),
    );
    // Two warmups per mode, then alternating order for twenty measured samples.
    for (let repeat = -2; repeat < 20; repeat++) {
      for (const mode of repeat % 2
        ? ["optimized", "json"]
        : ["json", "optimized"]) {
        const text = `${body}-${repeat}-${mode}`;
        const input: CommitEnvelope = {
          assetAliases: [],
          commit: {
            expectedRevision: revision,
            conversations: [
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
                    chatId: "linux-synthetic-message",
                  },
                ],
              },
            ],
          },
        };
        const frameGaps: number[] = [];
        let previous = performance.now();
        let running = true;
        const frame = (now: number) => {
          if (!running) return;
          frameGaps.push(now - previous);
          previous = now;
          document.getElementById("motion")!.style.transform =
            `translateX(${now % 200}px)`;
          if (running) requestAnimationFrame(frame);
        };
        document.getElementById("status")!.textContent =
          `${size}/${mode}/${repeat}`;
        await pause(40);
        previous = performance.now();
        requestAnimationFrame(frame);
        await pause(40);
        commands.length = 0;
        const start = performance.now();
        const result =
          mode === "json"
            ? await invoke<{ revision: number }>("pds_commit", { ...input })
            : await transport.commit(input);
        const elapsedMs = performance.now() - start;
        await pause(40);
        running = false;
        check(result.revision === revision + 1, "one revision per commit");
        revision = result.revision;
        const read = await readMessage();
        check(
          read.revision === revision && read.value.message[0].data === text,
          "exact Unicode roundtrip",
        );
        if (repeat >= 0)
          samples.push({
            size,
            mode,
            repeat,
            commands: mode === "json" ? ["pds_commit"] : [...commands],
            elapsedMs,
            frameMaxMs: Math.max(...frameGaps),
            frameGaps,
          });
        if (repeat === 19 && mode === "optimized") {
          let rejected = false;
          try {
            await transport.commit(input);
          } catch {
            rejected = true;
          }
          check(rejected, "stale revision replay rejected");
          check(
            (await readMessage()).revision === revision,
            "replay changed revision",
          );
        }
      }
    }
  }
  const unicode = await runUnicodePersistenceProbe(revision);
  revision = unicode.revision;
  const final = await readMessage();
  return {
    passed: true,
    samples,
    unicode,
    revision,
    finalHash: await digest(final.value.message[0].data),
  };
}
async function reload() {
  await guard();
  await showNativeHarness();
  await invoke("pds_open");
  const unicode = await verifyUnicodePersistenceProbe();
  const final = await readMessage();
  return {
    unicode,
    revision: final.revision,
    finalHash: await digest(final.value.message[0].data),
  };
}
async function regex() {
  await guard();
  await showNativeHarness();
  const data = makeRegexFixture(500, 256 * 1024);
  const plan = getRegexExecutionPlan(data.scripts, "editoutput");
  const safe = classifyRegexSafePlan(plan, data.input);
  check(safe.accepted, "fixture must use safe regex plan");
  const samples = [];
  for (let repeat = -2; repeat < 20; repeat++) {
    let expected = data.input;
    const startJs = performance.now();
    for (const entry of plan.entries)
      expected = expected.replace(
        new RegExp(entry.pattern, entry.flags),
        entry.replacement,
      );
    const jsMs = performance.now() - startJs;
    const startNative = performance.now();
    const result = await executeNativeRegexBatch(safe.plan, data.input);
    const nativeMs = performance.now() - startNative;
    check(
      result.data === expected && !result.errors.length,
      "regex oracle mismatch",
    );
    if (repeat >= 0) samples.push({ jsMs, nativeMs });
  }
  return { passed: true, samples };
}
async function startupAppearance(seed: boolean, theme: "light" | "dark") {
  await guard();
  check(Boolean(document.getElementById("preloading")), "appearance probe requires product HTML build");
  if (seed) {
    const { defaultColorScheme } = await import("../../src/ts/gui/colorscheme");
    const colorScheme = theme === "dark" ? defaultColorScheme : {
      bgcolor: "#ffffff", darkbg: "#f0f0f0", borderc: "#0f172a", selected: "#e0e0e0",
      draculared: "#ff5555", textcolor: "#0f172a", textcolor2: "#64748b",
      darkBorderc: "#d1d5db", darkbutton: "#e5e7eb", type: "light",
    };
    const opened = await invoke<{ revision: number }>("pds_open");
    await invoke("pds_commit", {
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
    return {
      seeded: true, theme, startupHintMatchesRenderedPalette: true,
      interactiveMs: performance.getEntriesByName("boot:interactive")[0].startTime,
    };
  }
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
  return {
    theme, systemDark: matchMedia("(prefers-color-scheme: dark)").matches,
    preloaderBackground, colorScheme: style.colorScheme,
    appBackground: style.getPropertyValue("--risu-theme-bgcolor"),
    bounds: { x: bounds.x, y: bounds.y, width: bounds.width, height: bounds.height },
    interactiveMs: performance.getEntriesByName("boot:interactive")[0].startTime,
    visualReview: "required", contentBackgroundPass: null, titlebarPass: null,
  };
}

async function until(predicate: () => boolean | Promise<boolean>, message: string) {
  const deadline = performance.now() + 60_000;
  while (!(await predicate())) {
    check(performance.now() < deadline, message);
    await pause(100);
  }
}

const productMarker = "linux-synthetic-ui-edit 🐿️";

// The product mounts into #app; the synthetic profile has already accepted the terms.
async function mountProduct() {
  const { createNativeDeviceSettings } = await import("../../src/ts/storage/nativeDeviceSettings");
  await createNativeDeviceSettings().set("risunest_tos_v1", "true");
  document.getElementById("benchmark")!.remove();
  const product = await import("../../src/main");
  await product.default;
  const { getPersistentDataRuntime } = await import(
    "../../src/ts/storage/persistentDataRuntime.svelte"
  );
  await until(() => {
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
  const index = DBState.db.characters.findIndex(
    (character) => character.chaId === "char-a",
  );
  check(
    index >= 0 && (await changeChar(index)),
    "open synthetic character in product UI",
  );
  return { runtime: getPersistentDataRuntime(), DBState, index };
}

async function app() {
  await initialize();
  const opened = await invoke<{ revision: number }>("pds_open");
  await invoke("pds_commit", {
    commit: {
      expectedRevision: opened.revision,
      rootMutations: [{ type: "set", key: "didFirstSetup", value: true }],
    },
    assetAliases: [],
  });
  const { runtime, DBState, index } = await mountProduct();
  const { tick } = await import("svelte");
  const lease = await runtime.acquireCompleteConversation("edit-message");
  let conversationId: string;
  try {
    const { captureChatMessageTarget, saveCapturedChatMessage } = await import(
      "../../src/ts/chatMessageUi"
    );
    const context = {
      captureCurrent: () => {
        const character = DBState.db.characters[index];
        return { character, conversation: character.chats[character.chatPage] };
      },
      getCurrentSession: () => runtime.getActiveConversationSession(),
    };
    conversationId = lease.session.conversationId;
    const target = captureChatMessageTarget({
      ...context,
      absoluteIndex: lease.session.totalMessages - 1,
    });
    check(target, "capture product message edit target");
    check(
      saveCapturedChatMessage(target, context, productMarker).saved,
      "product message edit accepted",
    );
  } finally {
    lease.release();
  }
  await tick();
  await runtime.flushPendingData("linux-ui-smoke");
  await until(
    () => document.getElementById("app")!.textContent!.includes(productMarker),
    "edited synthetic message did not render in product chat",
  );
  const persisted = await invoke<{
    revision: number;
    value: { message: { data: string }[] };
  }>("pds_read_conversation", { characterId: "char-a", conversationId });
  check(
    persisted.value.message.at(-1)?.data === productMarker,
    "product edit persisted through Rust: " +
      JSON.stringify({ revision: persisted.revision, last: persisted.value.message.at(-1)?.data }),
  );
  return {
    passed: true,
    conversationId,
    revision: persisted.revision,
    renderedTextLength: document.getElementById("app")!.textContent!.length,
  };
}

// The controller hands over the saved result; WebKit may drop storage written just before the process ends.
async function appRestart(expected: { conversationId: string; revision: number }) {
  await guard();
  await invoke("pds_open");
  const saved = await invoke<{ revision: number; value: { message: { data: string }[] } }>(
    "pds_read_conversation",
    { characterId: "char-a", conversationId: expected.conversationId },
  );
  const actual = { revision: saved.revision, messages: saved.value.message.length, last: saved.value.message.at(-1)?.data };
  check(
    actual.last === productMarker && Number.isSafeInteger(saved.revision) && saved.revision >= expected.revision,
    "product edit survives process restart: " + JSON.stringify({ expected, actual }),
  );
  await mountProduct();
  await until(
    () => document.getElementById("app")!.textContent!.includes(productMarker),
    "restored synthetic edit did not render in product chat",
  );
  return { passed: true, revision: saved.revision };
}

async function streaming() {
  await guard();
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
  return result;
}

async function appLock(phase: string) {
  await guard();
  const events: { stage: string; result: unknown }[] = [];
  Object.assign(window, { __linuxAppLockEvents: events });
  const { appLockFixture } = await import("../appLock/fixture");
  await appLockFixture("linux", phase, initialize, async (stage, result) => {
    events.push({ stage, result });
  });
  return { passed: true };
}

Object.assign(window, {
  __RISUNEST_LINUX_BENCHMARK__: { persistence, reload, regex, startupAppearance, app, appRestart, streaming, appLock, failureRecord },
});
