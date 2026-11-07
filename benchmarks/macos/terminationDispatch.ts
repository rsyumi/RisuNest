import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { mount, tick } from "svelte";
import SyncExitDialog from "../../src/lib/Others/SyncExitDialog.svelte";
import { createMacosExitHandler, type MacosExitRequest } from "../../src/ts/storage/macosLifecycle";
import { SaveCoordinator } from "../../src/ts/storage/saveCoordinator";
import { SqlitePersistentDataStore } from "../../src/ts/storage/sqlitePersistentDataStore";
import { createSyncExitCoordinator, type SyncExitDrainResult } from "../../src/ts/storage/syncExitCoordinator";
import { configureSyncExitCoordinator } from "../../src/ts/storage/syncExitProduction";
import { check, pause } from "./contracts";
import { failureRecord } from "../failureRecord";

const report = (stage: string, result: unknown) => invoke("macos_bench_report", { stage, result });

export async function terminationDispatch(phase: string) {
  const scenario = phase.slice("session-dispatch-".length);
  const store = new SqlitePersistentDataStore();
  await store.open();
  const initial = await store.readRoot();
  let root = initial.value;
  let commits = 0;
  let activeCommits = 0;
  let maximumActiveCommits = 0;
  let checkpoints = 0;
  let flushCalls = 0;
  let coalesced = false;
  let remoteStarts = 0;
  let remoteCancels = 0;
  let rendererRequests = 0;
  let sessionRequests = 0;
  let rendererReplies = 0;
  const started = performance.now();
  const timeline: { stage: string; elapsedMillis: number }[] = [];
  const note = (stage: string) => {
    timeline.push({ stage, elapsedMillis: Math.round(performance.now() - started) });
  };
  let releaseCommit!: () => void;
  const commitGate = new Promise<void>(resolve => { releaseCommit = resolve; });
  const commit = store.commit.bind(store);
  store.commit = async input => {
    commits++;
    activeCommits++;
    maximumActiveCommits = Math.max(maximumActiveCommits, activeCommits);
    note("commit-enter");
    try {
      if (scenario === "local") await commitGate;
      return await commit(input);
    } finally {
      activeCommits--;
      note("commit-settled");
    }
  };
  const save = new SaveCoordinator({
    store,
    captureRoot: () => root,
    captureSelectedCharacter: () => null,
    captureCharacter: () => null,
    replaceDatabase: () => { throw new Error("Unexpected database replacement"); },
  });
  save.initialize(initial.revision);
  root = { ...root, username: `synthetic-session-dispatch-${scenario}` };
  save.markPersistentDataDirty(128);
  let firstFlush: Promise<void> | undefined;
  const flushLocal = () => {
    flushCalls++;
    note("flush-enter");
    const pending = save.flushPendingDataLocally("session-dispatch");
    if (firstFlush) coalesced ||= pending === firstFlush;
    else firstFlush = pending;
    return pending;
  };
  const checkpointLocal = async () => {
    checkpoints++;
    note("checkpoint-enter");
    await invoke("pds_checkpoint", { mode: "truncate" });
    note("checkpoint-settled");
  };
  let settleDrain!: (value: SyncExitDrainResult) => void;
  const coordinator = createSyncExitCoordinator({
    flushLocal,
    checkpointLocal,
    acquireEditFence: async () => ({ release: () => note("fence-released") }),
    captureTarget: async () => ({
      revision: save.revision, libraryEpoch: "synthetic-library",
      selectionEpoch: "synthetic-selection", selectionId: "synthetic-remote",
    }),
    selectedDrain: () => scenario === "initial" ? null : ({
      id: "synthetic-remote",
      drain: async (_target, signal) => {
        remoteStarts++;
        note("remote-start");
        signal.addEventListener("abort", () => note("remote-aborted"), { once: true });
        return new Promise<SyncExitDrainResult>(resolve => { settleDrain = resolve; });
      },
      cancel: async () => { remoteCancels++; note("remote-cancelled"); settleDrain?.({ kind: "complete" }); },
    }),
    softWaitMillis: scenario === "dialog" ? 50 : 30_000,
  });
  configureSyncExitCoordinator(coordinator);
  const dialogTarget = document.createElement("div");
  document.body.append(dialogTarget);
  mount(SyncExitDialog, { target: dialogTarget });
  coordinator.subscribe(state => note(`coordinator-${state.phase}`));
  const snapshot = () => ({
    scenario, route: "self-targeted-apple-event", coordinator: coordinator.snapshot().phase,
    dialogOpen: !!dialogTarget.querySelector('[role="dialog"]'),
    choiceButtons: dialogTarget.querySelectorAll("button").length,
    commits, activeCommits, maximumActiveCommits, checkpoints, flushCalls, coalesced,
    remoteStarts, remoteCancels, rendererRequests, sessionRequests, rendererReplies, timeline,
  });
  const handler = createMacosExitHandler({
    coordinator,
    saveLocally: async () => { await flushLocal(); await checkpointLocal(); },
    respond: async (token, exit) => {
      const saved = await store.readRoot();
      check(saved.value.username === root.username && saved.revision === save.revision,
        "session response must follow the real local commit");
      await tick();
      rendererReplies++;
      note(`renderer-response-${exit}`);
      await report("session-dispatch-renderer-response", { ...snapshot(), exit });
      await invoke("macos_exit_response", { token, exit });
    },
    reportError: error => { void report("failure", { stage: "renderer", ...failureRecord(error), message: String(error) }); },
  });
  await listen<MacosExitRequest>("risu-macos-exit-requested", ({ payload }) => {
    rendererRequests++;
    if (payload.sessionEnd) sessionRequests++;
    note(payload.sessionEnd ? "renderer-session-request" : "renderer-normal-request");
    void report("session-dispatch-renderer-request", {
      ...snapshot(), sessionEnd: payload.sessionEnd,
      hasDeadline: typeof payload.deadlineUnixMillis === "number" && Number.isFinite(payload.deadlineUnixMillis),
    }).then(() => handler(payload)).catch(error => report("failure", { ...failureRecord(error), message: String(error) }));
  });
  await invoke("macos_bench_main_thread_settled");
  await invoke("macos_lifecycle_ready");
  if (scenario !== "initial") {
    await invoke("macos_bench_native_quit");
    const expected = scenario === "local" ? "saving" : scenario === "drain" ? "syncing" : "remote-delayed";
    const deadline = performance.now() + 15_000;
    while (coordinator.snapshot().phase !== expected || (scenario === "local" && activeCommits !== 1)) {
      check(performance.now() < deadline, "native ordinary quit did not reach the requested coordinator stage");
      await pause(10);
    }
    await tick();
    check(!!dialogTarget.querySelector('[role="dialog"]'), "product exit dialog must be mounted");
    check(dialogTarget.querySelectorAll("button").length === (scenario === "dialog" ? 3 : 0),
      "the confirmation stage must expose the real product choices");
  }
  note("send-session-event");
  await report("session-dispatch-ready", { ...snapshot(), passed: true });
  await invoke("macos_bench_dispatch_session_event");
  const sentAt = performance.now();
  await pause(250);
  note("release-local-save");
  releaseCommit();
  await pause(250);
  await report("session-dispatch-after-delivery", snapshot());
  while (performance.now() - sentAt < 6_000) {
    await pause(Math.max(1, 6_000 - (performance.now() - sentAt)));
  }
  if (scenario !== "initial" && sessionRequests === 0 && rendererRequests === 1 && rendererReplies === 0) {
    const saved = await store.readRoot();
    check(saved.value.username === root.username && saved.revision === save.revision,
      "the pending ordinary quit must complete the real local commit");
    note("observation-complete");
    await report("session-dispatch-observation-complete", {
      ...snapshot(), observationMillis: Math.round(performance.now() - sentAt),
      outcome: "pending-session-event-not-delivered-within-window",
    });
    await invoke("macos_bench_repeat_native_quit");
    await pause(15_000);
    throw new Error("Cleanup repeated terminate did not end the isolated app");
  }
  await report("failure", { ...snapshot(), message: "Session event did not terminate the isolated app" });
  await invoke("macos_bench_quit");
}
