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
    (await getIdentifier()) === "io.github.rsyumi.risunest.ios.bench",
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
  const samples: Record<string, unknown>[] = [];
  const commands: string[] = [];
  const transport = new NativeCommitTransport({
    windows: () => false,
    ios: () => platform() === "ios",
    invoke: (command, args) => {
      commands.push(command);
      return invoke(command, args);
    },
    encode: encodeNativeCommit,
    shared: () => undefined,
  });
  for (const size of [1024 * 1024, 6 * 1024 * 1024]) {
    const unit = '합성🐿️\\"\n cafe\u0301 A\u030a \u1100\u1161';
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
                    chatId: "ios-synthetic-message",
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
  const final = await readMessage();
  return {
    passed: true,
    samples,
    revision,
    finalHash: await digest(final.value.message[0].data),
  };
}
async function reload() {
  await guard();
  await invoke("pds_open");
  const final = await readMessage();
  return {
    revision: final.revision,
    finalHash: await digest(final.value.message[0].data),
  };
}
async function regex() {
  await guard();
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

export { persistence, reload, regex, guard, check, pause, initialize };
