import { mount, settled as domSettled, unmount } from "svelte";
import SummaryItem from "../../src/lib/Others/HypaV3Modal/modal-summary-item.svelte";
import type { SerializableHypaV3Data, SerializableSummary } from "../../src/ts/process/memory/hypav3";
import type { SummaryItemState } from "../../src/lib/Others/HypaV3Modal/types";
import { DBState, selectedCharID } from "../../src/ts/stores.svelte";
import type { Database } from "../../src/ts/storage/database.svelte";
import { getPersistentDataRuntime } from "../../src/ts/storage/persistentDataRuntime.svelte";
import { isMetadataOnlySelectedConversation } from "../../src/ts/storage/selectedConversationLifecycle";
import { isTauri } from "../../src/ts/platform";

const MESSAGE_COUNT = 2000;
const SUMMARY_COUNT = 100;
const MEMOS_PER_SUMMARY = 40;
const REPEATS = 5;
const DEADLINE_MS = 60000;
const pause = () => new Promise<void>((resolve) => setTimeout(resolve, 10));
const median = (values: number[]) => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)];

class ProbeAssertion extends Error {}

function requireProbe(condition: unknown, id: string): asserts condition {
  if (!condition) throw new ProbeAssertion(id);
}

interface Counters {
  acquireRevision: number;
  readConversationWindow: number;
  released: number;
  failures: number;
  pending: number;
  active: number;
  peakActive: number;
}

/** Run only in the isolated streaming harness on a disposable synthetic app profile. */
export async function runHypaSummaryProbe(stopStreamingFixture: () => Promise<void>) {
  const cases: Record<string, unknown>[] = [];
  let phase = "hypa-summary-preflight";
  const progress = (next: string) => {
    phase = next;
    (window as unknown as { __streamingSmokeProgress?: (value: string) => void })
      .__streamingSmokeProgress?.(next);
  };
  let cleanup = async () => {};
  try {
    requireProbe(isTauri, "native-profile-required");
    requireProbe(document.querySelector('[data-streaming-smoke="synthetic-v1"]'), "isolated-fixture-required");
    requireProbe(DBState.db.characters.length === 1 && DBState.db.characters[0].chaId === "streaming-synthetic", "synthetic-fixture-required");
    const database = JSON.parse(JSON.stringify(DBState.db)) as Database;
    const character = database.characters[0];
    requireProbe(character.chats.length === 1 && character.chats[0].message.length === 0, "fresh-fixture-required");
    character.chats[0].message = Array.from({ length: MESSAGE_COUNT }, (_, index) => ({
      role: index % 2 ? "char" as const : "user" as const,
      data: "Synthetic Hypa message. ".repeat(8),
      chatId: `hypa-synthetic-${index}`,
      time: index,
    }));
    const hypaV3Data: SerializableHypaV3Data = {
      summaries: Array.from({ length: SUMMARY_COUNT }, (_, index) => ({
        text: "Synthetic summary.",
        isImportant: false,
        // Adjacent summaries share half their memos; the last wraps to the first.
        chatMemos: Array.from({ length: MEMOS_PER_SUMMARY }, (_, offset) =>
          `hypa-synthetic-${(index * 20 + offset) % MESSAGE_COUNT}`),
      })),
    };
    await stopStreamingFixture();
    const runtime = getPersistentDataRuntime();
    const store = runtime.store;
    await store.open();
    let revision = (await store.readRoot()).revision;
    progress("hypa-summary-seed");
    revision = (await store.replaceFromDatabase(database, revision)).revision;
    let saveSequence = 0;
    const save = async () => {
      const started = performance.now();
      revision = (await store.commit({
        expectedRevision: revision,
        rootMutations: [{ type: "set", key: "username", value: `Synthetic User ${++saveSequence}` }],
      })).revision;
      return performance.now() - started;
    };
    const prepareSelection = async () => {
      DBState.db = JSON.parse(JSON.stringify(database)) as Database;
      selectedCharID.set(0);
      await runtime.initializeActiveWorkingSet(DBState.db);
      requireProbe(runtime.tryDemoteSelectedConversation(), "metadata-demotion-required");
      requireProbe(isMetadataOnlySelectedConversation(DBState.db.characters[0].chats[0]), "metadata-only-selection-required");
      await domSettled();
    };
    const measure = async (concurrentSave: boolean) => {
      await prepareSelection();
      const counters: Counters = { acquireRevision: 0, readConversationWindow: 0, released: 0, failures: 0, pending: 0, active: 0, peakActive: 0 };
      const acquire = store.acquireRevision;
      let savePromise: Promise<number> | undefined;
      let readsAtSaveStart = 0;
      let activeAtSaveStart = 0;
      let saveFinishedBeforeReady = false;
      const host = document.createElement("section");
      host.dataset.hypaSummaryProbe = "synthetic-v1";
      document.querySelector("#app")!.append(host);
      const components: ReturnType<typeof mount>[] = [];
      store.acquireRevision = async (requestedRevision) => {
        counters.acquireRevision++;
        counters.pending++;
        try {
          const lease = await acquire.call(store, requestedRevision);
          counters.active++;
          counters.peakActive = Math.max(counters.peakActive, counters.active);
          const read = lease.readConversationWindow.bind(lease);
          const release = lease.release.bind(lease);
          lease.readConversationWindow = async (query) => {
            counters.readConversationWindow++;
            const result = read(query);
            if (concurrentSave && !savePromise) {
              readsAtSaveStart = counters.readConversationWindow;
              activeAtSaveStart = counters.active;
              savePromise = save().then((elapsed) => {
                saveFinishedBeforeReady = true;
                return elapsed;
              });
              // Keep a rejected save handled while the DOM readiness wait runs.
              void savePromise.catch(() => {});
            }
            try { return await result; }
            catch (error) { counters.failures++; throw error; }
          };
          let released = false;
          lease.release = async () => {
            if (released) return;
            await release();
            released = true;
            counters.released++;
            counters.active--;
          };
          return lease;
        } catch (error) {
          counters.failures++;
          throw error;
        } finally { counters.pending--; }
      };
      cleanup = async () => {
        await Promise.all(components.map((component) => unmount(component)));
        const deadline = performance.now() + DEADLINE_MS;
        while ((counters.pending || counters.active) && performance.now() < deadline) await pause();
        store.acquireRevision = acquire;
        host.remove();
        requireProbe(counters.pending === 0 && counters.active === 0, "lease-drain-timeout");
      };
      const started = performance.now();
      const state = new WeakMap<SerializableSummary, SummaryItemState>();
      for (let index = 0; index < SUMMARY_COUNT; index++) {
        const target = document.createElement("div");
        host.append(target);
        components.push(mount(SummaryItem, {
          target,
          props: { summaryIndex: index, hypaV3Data, summaryItemStateMap: state,
            expandedMessageState: null, searchState: null, filterSelected: false, categories: [] },
        }));
      }
      await domSettled();
      const buttons = [...host.querySelectorAll<SVGElement>("svg.lucide-refresh-cw")]
        .map((icon) => icon.closest("button")!);
      requireProbe(buttons.length === SUMMARY_COUNT, "reroll-controls-required");
      let enabled = 0;
      while (performance.now() - started < DEADLINE_MS) {
        await domSettled();
        enabled = buttons.filter((button) => !button.disabled).length;
        if (enabled === SUMMARY_COUNT) break;
        await pause();
      }
      const readinessMs = performance.now() - started;
      const commitCompletedBeforeReadiness = saveFinishedBeforeReady;
      requireProbe(enabled === SUMMARY_COUNT, "reroll-readiness-timeout");
      requireProbe(counters.acquireRevision > 0 && counters.readConversationWindow > 0, "persistent-lookups-required");
      requireProbe(!concurrentSave || (savePromise && activeAtSaveStart > 0), "save-must-overlap-lookups");
      const commitMs = savePromise ? await savePromise : null;
      const dispose = cleanup;
      cleanup = async () => {};
      await dispose();
      requireProbe(counters.failures === 0 && counters.released === counters.acquireRevision, "lookup-or-lease-failure");
      return { readinessMs, commitMs, readsAtSaveStart, activeAtSaveStart, commitCompletedBeforeReadiness, ...counters };
    };
    progress("hypa-summary-readiness");
    const readiness = await measure(false);
    cases.push({ id: "hypa-summary-readiness", passed: true, ...readiness });
    const controls: number[] = [];
    const loaded: number[] = [];
    const pairedDelays: number[] = [];
    for (let index = 0; index < REPEATS; index++) {
      progress(`hypa-summary-save-${index + 1}`);
      const before = await save();
      const measured = await measure(true);
      const after = await save();
      const control = (before + after) / 2;
      controls.push(control);
      loaded.push(measured.commitMs!);
      pairedDelays.push(measured.commitMs! - control);
      cases.push({ id: `hypa-summary-save-${index + 1}`, passed: true,
        controlBeforeMs: before, controlAfterMs: after, controlMs: control, ...measured });
    }
    const controlMedianMs = median(controls);
    const concurrentMedianMs = median(loaded);
    const pairedDelayMedianMs = median(pairedDelays);
    const readinessMaxMs = Math.max(...cases.map((item) => item.readinessMs as number));
    // A frame plus 20% above control in at least four pairs avoids flagging timer noise.
    const delayThresholdMs = Math.max(16, controlMedianMs * 0.2);
    const measurableCommitDelay = pairedDelayMedianMs > delayThresholdMs &&
      pairedDelays.filter((value) => value > delayThresholdMs).length >= 4;
    return { passed: true, profile: "hypa-summary", messageCount: MESSAGE_COUNT,
      summaryCount: SUMMARY_COUNT, memosPerSummary: MEMOS_PER_SUMMARY,
      distinctMemos: MESSAGE_COUNT, requestedMemoReferences: SUMMARY_COUNT * MEMOS_PER_SUMMARY,
      repeats: REPEATS, metadataOnly: true, controlMedianMs, concurrentMedianMs, pairedDelayMedianMs, readinessMaxMs,
      gate: { readinessThresholdMs: 1000, delayThresholdMs,
        slowReadiness: readinessMaxMs > 1000, measurableCommitDelay,
        optimizationRequired: readinessMaxMs > 1000 || measurableCommitDelay }, cases };
  } catch (error) {
    return { passed: false, profile: "hypa-summary", phase,
      assertion: error instanceof ProbeAssertion ? error.message : "hypa-summary-probe-failed", cases };
  } finally {
    await cleanup();
  }
}
