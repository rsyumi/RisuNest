import { mount, settled as domSettled, unmount } from "svelte";
import ViewportFixture from "./ViewportFixture.svelte";
import { DBState, selectedCharID } from "../../src/ts/stores.svelte";
import type { character, Message } from "../../src/ts/storage/database.svelte";
import { ActiveConversationSession } from "../../src/ts/storage/activeConversationSession";
import { SynchronousSessionConversationViewportSource } from "../../src/ts/conversationViewportSource";
import type { LiveChatParserProjectionResolver } from "../../src/ts/selectedConversationLiveParserProjection";
import { setRuntimePerformanceProfile } from "../../src/ts/runtimePerformanceProfile";
import DefaultChatScreen from "../../src/lib/ChatScreens/DefaultChatScreen.svelte";
import type { Database } from "../../src/ts/storage/database.svelte";
import { getPersistentDataRuntime } from "../../src/ts/storage/persistentDataRuntime.svelte";
import { clearStreamingConversation, setStreamingConversation } from "../../src/ts/storage/streamingConversationRegistry";

const metrics = { active: 0, calls: [] as { index: number; durationMs: number }[] };
type ViewportControls = { jumpTo(index: number): Promise<boolean> | undefined; setInputBlocked(value: boolean): void };
let fixture: (ReturnType<typeof mount> & ViewportControls) | undefined;
let source: SynchronousSessionConversationViewportSource | undefined;
let session: ActiveConversationSession;
let currentCharacter: character;
let host: HTMLElement;
let composition = { start: 0, update: 0, end: 0, input: 0, focusin: 0, focusout: 0 };
let samples: { at: number; top: number }[] = [];
let corrections = 0;
let frame = 0;
let restoreScrollBy = () => {};
const scroll = () => host.querySelector<HTMLElement>("[data-viewport-scroll]")!;
const editor = () => host.querySelector<HTMLTextAreaElement>("textarea.message-edit-area");
const rows = () => [...host.querySelectorAll<HTMLElement>("[data-chat-render-key]")];
const pause = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
async function until(test: () => boolean, label: string) {
  const deadline = performance.now() + 15000;
  while (!test()) {
    if (performance.now() > deadline) throw new Error(label);
    await pause();
  }
}
async function ready() {
  await domSettled();
  await until(() => rows().length > 0 && !host.querySelector("[data-chat-mount-pending]") && metrics.active === 0, "viewport-not-ready");
  await pause();
  await pause();
}
function check(value: unknown, label: string): asserts value {
  if (!value) throw new Error(label);
}
function visibleIndices() {
  const bounds = scroll().getBoundingClientRect();
  return rows().filter((row) => {
    const rect = row.getBoundingClientRect();
    return rect.bottom > bounds.top && rect.top < bounds.bottom;
  }).map((row) => Number(row.dataset.chatIndex)).filter((index) => Number.isInteger(index));
}

export const viewportProbe = {
  async mount(count = 64, dependency: "local" | "history" = "local") {
    if (fixture) await unmount(fixture);
    source?.dispose();
    cancelAnimationFrame(frame);
    restoreScrollBy();
    document.getElementById("viewport-probe")?.remove();
    host = document.createElement("section");
    host.id = "viewport-probe";
    document.body.replaceChildren(host);
    const template = DBState.db.characters[0] as character;
    const messages: Message[] = Array.from({ length: count }, (_, index) => ({
      role: "char", chatId: `synthetic-${index}`,
      data: `row-${index} ` + (dependency === "history" && index !== count - 1 ? "{{lastmessage}} " : "") + "synthetic ".repeat(4 + index % 7 * 8),
    }));
    currentCharacter = { ...template, chaId: "viewport-synthetic", chatPage: 0,
      firstMessage: "", firstMsgIndex: -1, customscript: [{ type: "editdisplay", in: "synthetic", out: "rendered", flag: "g", comment: "" }],
      chats: [{ ...template.chats[0], id: "viewport-conversation", message: messages, isStreaming: count === 64, activeStreamingDisplayOptimizationMode: "balanced" }],
    };
    DBState.db.characters = [currentCharacter];
    currentCharacter = DBState.db.characters[0] as character;
    selectedCharID.set(0);
    DBState.db.streamingDeferDisplayProcessing = false;
    DBState.db.clickToEdit = true;
    setRuntimePerformanceProfile("normal");
    session = new ActiveConversationSession({ characterId: currentCharacter.chaId, conversationId: "viewport-conversation", conversation: currentCharacter.chats[0], storeRevision: 1, measureMessage: () => 1 });
    source = new SynchronousSessionConversationViewportSource({ session, captureCurrent: () => ({ character: currentCharacter, conversation: currentCharacter.chats[0] }) });
    // The fixture owns this complete session, so its complete projection has no storage lease.
    const resolver: LiveChatParserProjectionResolver = { async resolve({ row, totalMessages }) {
      return { kind: "complete", characterId: currentCharacter.chaId, conversationId: "viewport-conversation", revision: 1, totalMessages, chatID: row.absoluteIndex, projectedChatID: row.absoluteIndex, historyOffset: 0, reasons: [], release() {} };
    } };
    Object.assign(globalThis, { __viewportParseMetrics: metrics });
    metrics.calls = [];
    fixture = mount(ViewportFixture, { target: host, props: { currentCharacter, source, resolver } });
    composition = { start: 0, update: 0, end: 0, input: 0, focusin: 0, focusout: 0 };
    for (const [event, key] of [["compositionstart", "start"], ["compositionupdate", "update"], ["compositionend", "end"], ["input", "input"], ["focusin", "focusin"], ["focusout", "focusout"]] as const) {
      host.addEventListener(event, () => composition[key]++);
    }
    await ready();
    return { mountedRows: rows().length, totalMessages: count };
  },
  async publication() {
    await ready();
    metrics.calls = [];
    const count = currentCharacter.chats[0].message.length;
    const started = performance.now();
    const data = `tail-publication-${performance.now()}`;
    session.edit(session.locate(count - 1), { ...currentCharacter.chats[0].message[count - 1], data });
    await until(() => !!host.textContent?.includes(data), "tail-publication-not-visible");
    await ready();
    return { elapsedMs: performance.now() - started, calls: metrics.calls.length,
      settledRowsParsed: new Set(metrics.calls.filter((call) => call.index >= 0 && call.index < count - 1).map((call) => call.index)).size,
      parseWorkMs: metrics.calls.reduce((sum, call) => sum + call.durationMs, 0),
      dependentRowsUpdated: rows().filter((row) => Number(row.dataset.chatIndex) < count - 1 && row.textContent?.includes(data)).length };
  },
  async streamedReply(options: { auto?: boolean; always?: boolean; manualAtChunk?: number; startOlder?: boolean } = {}) {
    await this.mount(64);
    DBState.db.autoScrollToNewMessage = options.auto ?? true;
    DBState.db.alwaysScrollToNewMessage = options.always ?? false;
    const container = scroll();
    container.dispatchEvent(new WheelEvent('wheel', { deltaY: 1 }));
    container.scrollTop = options.startOlder ? -800 : 0;
    container.dispatchEvent(new Event('scroll'));
    await ready();
    session.append({ role: 'user', chatId: 'synthetic-question', data: 'Synthetic question' });
    await ready();
    const index = session.totalMessages;
    session.append({ role: 'char', chatId: 'synthetic-stream', data: 'Synthetic reply' });
    const snapshots: Record<string, number>[] = [];
    const capture = () => {
      const row = rows().find(value => Number(value.dataset.chatIndex) === index);
      if (!row) throw new Error('stream-row-not-mounted');
      const bounds = container.getBoundingClientRect();
      const rect = row.getBoundingClientRect();
      return { scrollTop: container.scrollTop, rowTop: rect.top - bounds.top, rowBottom: rect.bottom - bounds.bottom, rowHeight: rect.height };
    };
    const started = performance.now();
    for (let chunk = 1; chunk <= 40; chunk++) {
      await until(() => performance.now() - started >= chunk * 50, 'stream-chunk-deadline');
      session.edit(session.locate(index), { role: 'char', chatId: 'synthetic-stream', data: ('Synthetic streamed paragraph. '.repeat(5) + '\n\n').repeat(chunk) });
      await domSettled();
      await pause();
      if (chunk === options.manualAtChunk) {
        container.dispatchEvent(new WheelEvent('wheel', { deltaY: -240 }));
        container.scrollTop -= 240;
        container.dispatchEvent(new Event('scroll'));
        await ready();
        snapshots.push({ chunk, manual: 1, ...capture() });
      }
      if (chunk === 10 || chunk === 20 || chunk === 40) snapshots.push({ chunk, ...capture() });
    }
    currentCharacter.chats[0].isStreaming = false;
    await ready();
    return { options, snapshots, settled: capture(), newestVisible: capture().rowBottom <= 1 };
  },
  async seekMiddle() {
    const container = scroll();
    container.dispatchEvent(new WheelEvent("wheel", { deltaY: -1 }));
    const gap = host.querySelector<HTMLElement>("[data-chat-gap]")!;
    const gapRect = gap.getBoundingClientRect();
    const start = Number(gap.dataset.chatGapStart);
    const end = Number(gap.dataset.chatGapEnd);
    const expected = Math.floor((start + end) / 2) - 1;
    container.scrollTop += gapRect.top + gapRect.height / 2 - container.getBoundingClientRect().top;
    await until(() => rows().some((row) => Math.abs(Number(row.dataset.chatIndex) - expected) < 10), "distant-seek-not-mounted");
    await ready();
    const visible = visibleIndices();
    check(visible.some((index) => Math.abs(index - expected) < 10), "distant-seek-not-visible");
    return { expected, visible, mountedRows: rows().length };
  },
  async prepareInput() {
    await fixture!.jumpTo(currentCharacter.chats[0].message.length - 2);
    await ready();
    const button = host.querySelector<HTMLButtonElement>(".button-icon-edit");
    check(button, "editor-button-missing");
    button.click();
    await until(() => !!editor(), "editor-missing");
    editor()!.scrollIntoView({ block: "center" });
    await pause();
    const rect = editor()!.getBoundingClientRect();
    return { x: rect.left + rect.width / 2, y: rect.top + Math.min(rect.height / 2, 30) };
  },
  async growInput(lines = 12) {
    const input = editor();
    check(input, "editor-missing");
    input.value += "\n합성 입력".repeat(lines);
    input.setSelectionRange(input.value.length, input.value.length);
    input.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText" }));
    await pause();
    await pause();
    return this.inputState();
  },
  setInputBlocked(blocked: boolean) {
    fixture!.setInputBlocked(blocked);
  },
  inputState() {
    const input = editor();
    const rect = input?.getBoundingClientRect();
    return { focused: document.activeElement === input, length: input?.value.length ?? 0,
      selectionStart: input?.selectionStart, selectionEnd: input?.selectionEnd,
      top: rect?.top, bottom: rect?.bottom, height: rect?.height,
      viewportTop: visualViewport?.offsetTop, viewportHeight: visualViewport?.height,
      translate: host.querySelector<HTMLElement>("main")?.style.translate, events: { ...composition } };
  },
  async startGesture() {
    await ready();
    samples = [];
    corrections = 0;
    const container = scroll();
    const original = container.scrollBy;
    container.scrollBy = function (first: number | ScrollToOptions, second?: number) {
      corrections++;
      Reflect.apply(original, this, typeof first === "number" ? [first, second ?? 0] : [first]);
    };
    restoreScrollBy = () => { container.scrollBy = original; };
    const sample = (at: number) => { samples.push({ at, top: container.scrollTop }); frame = requestAnimationFrame(sample); };
    frame = requestAnimationFrame(sample);
    const rect = container.getBoundingClientRect();
    return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };
  },
  finishGesture() {
    cancelAnimationFrame(frame);
    restoreScrollBy();
    return { samples, corrections, visible: visibleIndices(), maxFrameGapMs: samples.slice(1).reduce((max, sample, index) => Math.max(max, sample.at - samples[index].at), 0) };
  },
  async displayVariableStream() {
    // Display listener writes reach the session only through the runtime's active session,
    // so this case replaces the synthetic store and mounts the chat screen on it.
    if (DBState.db.characters.length !== 1 || DBState.db.characters[0].chaId !== "viewport-synthetic") throw new Error("isolated-synthetic-fixture-required");
    if (fixture) await unmount(fixture);
    fixture = undefined;
    source?.dispose();
    source = undefined;
    const previous = DBState.db;
    const database = JSON.parse(JSON.stringify({ ...DBState.db, characters: [{ ...DBState.db.characters[0], chats: [{ ...DBState.db.characters[0].chats[0], message: [] }] }] })) as Database;
    const template = database.characters[0] as character;
    database.characters = [{ ...template, chaId: "display-variable-synthetic", customscript: [],
      triggerscript: [{ comment: "", type: "start", conditions: [], effect: [{ type: "triggerlua", code: `
        listenEdit('editDisplay', function(id, value, meta)
          setChatVar(id, 'displayWrites', tostring((tonumber(getChatVar(id, 'displayWrites')) or 0) + 1))
          local chunk = value:match('chunk%-(%d+)')
          if chunk then setChatVar(id, 'shownChunk', chunk) end
          return value .. ' [listener]'
        end)` }] }],
      chats: [{ ...template.chats[0], id: "display-variable-chat", isStreaming: false, scriptstate: {}, message: Array.from({ length: 24 }, (_, index) => ({
        role: index % 2 ? "char" : "user", chatId: `display-variable-${index}`, data: `row-${index} synthetic`,
      })) }] } as character];
    database.streamingDeferDisplayProcessing = false;
    const runtime = getPersistentDataRuntime();
    await runtime.store.open();
    await runtime.store.replaceFromDatabase(database, (await runtime.store.readRoot()).revision);
    DBState.db = database;
    selectedCharID.set(0);
    await runtime.initializeActiveWorkingSet(DBState.db);
    host = document.createElement("section");
    host.style.cssText = "height:100vh;width:100vw;position:relative";
    document.body.replaceChildren(host);
    metrics.calls = [];
    Object.assign(globalThis, { __viewportParseMetrics: metrics });
    const screen = mount(DefaultChatScreen, { target: host });
    // Generation holds the complete conversation while it streams.
    const lease = await runtime.acquireCompleteConversation("generation", runtime.captureSelectedConversationTarget()!);
    try {
      await ready();
      const active = lease.session;
      const chat = DBState.db.characters[0].chats[0];
      const index = active.totalMessages;
      setStreamingConversation(chat.id!, "balanced");
      chat.isStreaming = true;
      chat.activeStreamingDisplayOptimizationMode = "balanced";
      active.append({ role: "char", chatId: "display-variable-stream", data: "" });
      const started = performance.now();
      for (let chunk = 1; chunk <= 20; chunk++) {
        await until(() => performance.now() - started >= chunk * 50, "stream-chunk-deadline");
        active.edit(active.locate(index), { role: "char", chatId: "display-variable-stream", data: `chunk-${chunk} shown={{getvar::shownChunk}} ` + "streamed ".repeat(chunk * 4) });
        await domSettled();
        await pause();
      }
      clearStreamingConversation(chat.id!);
      chat.isStreaming = false;
      await ready();
      const calls = metrics.calls.length;
      for (let frame = 0; frame < 30; frame++) await pause();
      check(metrics.calls.length === calls, "display-write-render-loop");
      // The tail reads the value its own listener writes, so it shows the newest chunk only after a re-parse.
      const tail = rows().find((row) => Number(row.dataset.chatIndex) === index)?.textContent ?? "";
      check(tail.includes("chunk-20 shown=20") && tail.includes("[listener]"), "display-write-not-rendered");
      check(active.readChatVariable("shownChunk") === "20", "display-write-not-committed");
      const callsAt = (at: number) => metrics.calls.filter((call) => call.index === at).length;
      return { chunks: 20, calls, tailCalls: callsAt(index), maxSettledRowCalls: Math.max(...Array.from({ length: index }, (_, at) => callsAt(at))),
        displayWrites: Number(active.readChatVariable("displayWrites")) };
    } finally {
      lease.release();
      await unmount(screen);
      // Later cases and the Android input steps mount the viewport fixture on the earlier synthetic data.
      DBState.db = previous;
    }
  },
  async run() {
    const cases: Record<string, unknown>[] = [];
    for (const dependency of ["local", "history"] as const) {
      await this.mount(64, dependency);
      const result = await this.publication();
      check(result.calls > 0, "parse-instrumentation-missing");
      if (dependency === "history") check(result.dependentRowsUpdated > 0, "history-dependent-render-stale");
      cases.push({ id: `complete-stream-${dependency}`, ...result });
    }
    cases.push({ id: "display-variable-stream", ...await this.displayVariableStream() });
    await this.mount(10000);
    cases.push({ id: "distant-seek", ...await this.seekMiddle() });
    return { passed: true, cases };
  },
};
