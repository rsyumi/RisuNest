import { tick } from "svelte";
import { get } from "svelte/store";
import { failureRecord, type FailureEntry, type FailureRecord } from "../failureRecord";

/** Drives the product Sync settings like a user. Records structure only, never codes, tokens or bodies. */
export type SyncLabel = "sync-env" | "sync-result";
export type SyncReport = (stage: string, result: Record<string, unknown>) => Promise<unknown>;

export class SyncStepError extends Error {
  readonly detail: Record<string, unknown>;
  constructor(label: SyncLabel, step: string, message: string, detail: Record<string, unknown> = {}, options?: ErrorOptions) {
    super(`${label} ${step}: ${message}`, options);
    this.detail = { label, step, ...detail };
  }
}

export const pause = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

function fail(step: string, message: string, detail?: Record<string, unknown>): never {
  throw new SyncStepError("sync-result", step, message, detail);
}

type Detail = () => Record<string, unknown> | Promise<Record<string, unknown>>;

async function until<T>(read: () => T | Promise<T>, timeoutMs: number, step: string, message: string, detail?: Detail): Promise<NonNullable<T>> {
  const deadline = performance.now() + timeoutMs;
  for (;;) {
    const value = await read();
    if (value) return value as NonNullable<T>;
    if (performance.now() > deadline) fail(step, message, await detail?.());
    await pause(100);
  }
}

// Long identifiers and encoded values never leave the device in a report.
export const redact = (text: string) => text.replace(/[A-Za-z0-9_+/=-]{32,}/g, "<redacted>").slice(0, 300);
// Bundle frames name the chunk and position, which a rebuild of the same snapshot maps back to source.
const frames = (stack: string | undefined) => (stack ?? "").split(/\r?\n/).slice(0, 4)
  .map((frame) => frame.trim().replace(/[A-Za-z0-9_-]{32,}/g, "<redacted>").slice(0, 200)).filter(Boolean);

/** The failure record of `error`, with the message and stack of it and of each cause redacted. */
export function syncFailureRecord(error: unknown): FailureRecord {
  const redacted = (entry: FailureEntry): FailureEntry => ({
    ...entry,
    message: redact(entry.message),
    ...(entry.stack ? { stack: frames(entry.stack).join("\n") } : {}),
  });
  const record = failureRecord(error);
  return { ...record, ...redacted(record), ...(record.causes ? { causes: record.causes.map(redacted) } : {}) };
}

// The shape of a field, never its value.
const shapeOf = (owner: unknown, field: string) => {
  if (!owner || typeof owner !== "object" || !Object.hasOwn(owner, field)) return "missing";
  const value = (owner as Record<string, unknown>)[field];
  return Array.isArray(value) ? `array:${value.length}` : typeof value;
};

export class SyncDriver {
  private readonly errors: string[] = [];
  private readonly dialogOrigins = new WeakMap<object, string[]>();
  private current = "start";
  private readonly orderEvents: Record<string, unknown>[] = [];
  private syncController: Awaited<ReturnType<typeof import("../../src/ts/storage/sync/serverSyncProduction").getServerSyncController>> | undefined;

  constructor(readonly phase: string, private readonly sink: SyncReport, private readonly show: (step: string) => void = () => {}) {
    // Errors the page logs are attached, redacted, to a failed step so it can be attributed.
    const record = (values: unknown[]) => {
      if (this.errors.length >= 50) return;
      this.errors.push(redact(values.map((value) => value instanceof Error ? `${value.name}: ${value.message}` : String(value)).join(" ")));
      const stack = values.find((value): value is Error => value instanceof Error)?.stack;
      if (stack) this.errors.push(`stack: ${frames(stack).join(" | ")}`);
    };
    const original = console.error.bind(console);
    console.error = (...values: unknown[]) => { record(values); original(...values); };
    addEventListener("error", (event) => record([event.error ?? event.message]));
    addEventListener("unhandledrejection", (event) => record(["unhandled rejection", event.reason]));
  }

  async step<T extends Record<string, unknown>>(label: SyncLabel, name: string, run: () => Promise<T>): Promise<T> {
    this.show(name);
    this.current = name;
    try {
      const result = await run();
      await this.sink(`${label}:${name}`, { label, phase: this.phase, step: name, passed: true, ...result, characterOrder: await this.orderShapes() });
      return result;
    } catch (error) {
      const failure = error instanceof SyncStepError ? error
        : new SyncStepError(label, name, error instanceof Error ? redact(error.message) : "failed",
          { errorName: error instanceof Error ? error.name : typeof error }, { cause: error });
      failure.detail.consoleErrors = this.errors.slice(-8);
      failure.detail.characterOrder = { ...(await this.orderShapes()), events: this.orderEvents };
      throw failure;
    }
  }

  /** The binding the native store holds before the product mounts. */
  async bindingTarget() {
    const { invoke } = await import("@tauri-apps/api/core");
    await invoke("pds_open");
    const { createNativeSyncBindingBridge } = await import("../../src/ts/storage/sync/bindingNative");
    return (await createNativeSyncBindingBridge().state()).target.kind;
  }

  /** Seeds an unbound synthetic profile, completes local onboarding and accepts the terms. */
  async prepareProfile(seed?: () => Promise<unknown>) {
    const { invoke } = await import("@tauri-apps/api/core");
    if (seed) await seed();
    const opened = await invoke<{ revision: number }>("pds_open");
    await invoke("pds_commit", {
      commit: { expectedRevision: opened.revision, rootMutations: [{ type: "set", key: "didFirstSetup", value: true }] },
      assetAliases: [],
    });
    const { createNativeDeviceSettings } = await import("../../src/ts/storage/nativeDeviceSettings");
    // A queued terms dialog would hold every later dialog, including the replacement confirmation.
    await createNativeDeviceSettings().set("risunest_tos_v1", "true");
    localStorage.setItem("tos4", "true");
  }

  async mountProduct() {
    document.getElementById("benchmark")?.remove();
    const { alertStore } = await import("../../src/ts/stores.svelte");
    alertStore.subscribe((value) => {
      if (value.type !== "none" && alertStore.dialogVisible() && !this.dialogOrigins.has(value)) {
        this.dialogOrigins.set(value, (new Error().stack ?? "").split(/\r?\n/).slice(1, 12)
          .map((frame) => frame.trim().replace(/[A-Za-z0-9_-]{32,}/g, "<redacted>").slice(0, 200)));
      }
    });
    const app = await import("../../src/main");
    await app.default;
    const { getPersistentDataRuntime } = await import("../../src/ts/storage/persistentDataRuntime.svelte");
    await until(() => {
      try {
        return Boolean(getPersistentDataRuntime().store) && performance.getEntriesByName("boot:interactive").length > 0
          && document.getElementById("app")!.textContent!.length > 100;
      } catch {
        return false;
      }
    }, 90_000, "mount", "product app did not initialize");
    await this.watchOrder();
    return { rendered: true };
  }

  /** Records each change in the shape of the character order the sidebar iterates, with the step and sync stages. */
  private async watchOrder() {
    const { DBState } = await import("../../src/ts/stores.svelte");
    let last = shapeOf(DBState.db, "characterOrder");
    this.orderEvents.push({ step: this.current, shape: last });
    setInterval(() => {
      const shape = shapeOf(DBState.db, "characterOrder");
      if (shape === last || this.orderEvents.length >= 16) return;
      last = shape;
      const view = this.syncController?.snapshot();
      this.orderEvents.push({ step: this.current, shape, running: view?.running ?? null, stages: view?.progress?.stages ?? null });
    }, 50);
  }

  /** The character order's shape in the working set and in the stored root. */
  private async orderShapes() {
    try {
      const { DBState } = await import("../../src/ts/stores.svelte");
      const { getPersistentDataRuntime } = await import("../../src/ts/storage/persistentDataRuntime.svelte");
      const live = shapeOf(DBState.db, "characterOrder");
      const store = getPersistentDataRuntime().store;
      return { live, stored: store ? shapeOf((await store.readRoot()).value, "characterOrder") : "unavailable" };
    } catch {
      return { live: "unavailable", stored: "unavailable" };
    }
  }

  /** Searches every conversation in the native store for a marker. */
  async findMarker(marker: string) {
    const { getPersistentDataRuntime } = await import("../../src/ts/storage/persistentDataRuntime.svelte");
    const store = getPersistentDataRuntime().store;
    let characters = 0, conversations = 0;
    let location: { characterId: string; conversationId: string; last: boolean } | undefined;
    let cursor: string | undefined;
    do {
      const page = await store.queryCharacters({ order: "configured", trash: false, limit: 100, cursor });
      for (const character of page.items) {
        characters++;
        let next: string | undefined;
        do {
          const list = await store.queryConversations({ characterId: character.id, order: "configured", limit: 100, cursor: next });
          for (const conversation of list.items) {
            if (++conversations > 5000) fail("marker-search", "library too large for a synthetic search");
            const read = await store.readConversation(character.id, conversation.id);
            const messages = read?.value.message ?? [];
            const index = messages.findIndex((message) => typeof message.data === "string" && message.data.includes(marker));
            if (index >= 0 && !location)
              location = { characterId: character.id, conversationId: conversation.id, last: index === messages.length - 1 };
          }
          next = list.nextCursor;
        } while (next);
      }
      cursor = page.nextCursor;
    } while (cursor);
    return { found: !!location, markerInLastMessage: location?.last ?? false, characters, conversations, location };
  }

  /** Opens Settings on the RisuNest page the way the product's own sync link does, then the sync tab. */
  async openSyncSettings(step = "settings-open") {
    const { openRisuNestSettingsTab } = await import("../../src/ts/setting/risuNestSettingsTabs");
    const { SettingsMenuIndex, settingsOpen } = await import("../../src/ts/stores.svelte");
    await this.waitForNoDialog(step);
    openRisuNestSettingsTab("settings");
    SettingsMenuIndex.set(17);
    settingsOpen.set(true);
    const tab = await until(() => document.querySelector<HTMLButtonElement>('[data-risunest-tab="sync"]'),
      30_000, step, "RisuNest settings tabs did not render");
    tab.click();
    await tick();
    await until(() => document.querySelector('[data-risunest-panel="sync"]'), 15_000, step, "sync tab did not open");
    await until(() => this.section(), 30_000, step, "Sync server settings did not render");
    return { tab: "sync" };
  }

  private section() {
    return document.querySelector<HTMLElement>('[data-risunest-panel="sync"] #risunest-server-sync');
  }

  private buttons(root: ParentNode, label: string) {
    return [...root.querySelectorAll<HTMLButtonElement>("button")].filter((button) => {
      const copy = button.cloneNode(true) as HTMLElement;
      copy.querySelectorAll(".sr-only").forEach((node) => node.remove());
      return copy.textContent?.trim() === label;
    });
  }

  private busy(root: ParentNode) {
    return !!root.querySelector('button[aria-busy="true"]');
  }

  private tone() {
    return this.section()?.querySelector<HTMLElement>('[aria-live="polite"][data-tone]')?.dataset.tone ?? null;
  }

  private async uiAlert() {
    const { language } = await import("../../src/lang");
    const text = this.section()?.querySelector('[role="alert"]')?.textContent?.trim();
    if (!text) return null;
    // The product's message is named by its string key, never copied.
    for (const [group, strings] of [["serverSync", language.risuNest.serverSync], ["lwwSync", language.lwwSync]] as const) {
      const key = Object.entries(strings as unknown as Record<string, unknown>).find(([, value]) => value === text)?.[0];
      if (key) return `${group}.${key}`;
    }
    return "unrecognized";
  }

  private async controller() {
    const { getServerSyncController } = await import("../../src/ts/storage/sync/serverSyncProduction");
    return this.syncController = getServerSyncController();
  }

  private async state() {
    const view = (await this.controller()).snapshot();
    return {
      configured: !!view.status.configured, bound: !!view.status.bound, running: view.running,
      progress: !!view.progress, error: view.error || null, bindingIncomplete: view.bindingIncomplete,
      paused: view.paused, tone: this.tone(),
    };
  }

  private async alertState() {
    const { alertStore } = await import("../../src/ts/stores.svelte");
    const value = get(alertStore);
    return { visible: alertStore.dialogVisible(), value };
  }

  /** Names a dialog by its type and the product string key of its message, never by its text. */
  async describeDialog(value: { type: string; msg?: string; stackTrace?: string; checkboxConfirm?: { title: string } }) {
    const { language } = await import("../../src/lang");
    const text = value.checkboxConfirm?.title ?? value.msg ?? "";
    const find = (node: unknown, path: string, depth: number): string | undefined => {
      if (typeof node === "string") return node === text ? path : undefined;
      if (!node || typeof node !== "object" || depth > 6) return undefined;
      for (const [key, child] of Object.entries(node)) {
        const found = find(child, path ? `${path}.${key}` : key, depth + 1);
        if (found) return found;
      }
      return undefined;
    };
    const messageKey = text ? find(language, "", 0) ?? "unrecognized" : "empty";
    // A message the string table does not hold is an error or a filled template; long values are redacted.
    return {
      alertType: value.type, messageKey,
      messageLength: text.length, origin: this.dialogOrigins.get(value) ?? [],
      ...(messageKey === "unrecognized" ? { message: redact(text) } : {}),
      ...(value.stackTrace ? { stack: frames(value.stackTrace) } : {}),
    };
  }

  private async waitForNoDialog(step: string) {
    await until(async () => !(await this.alertState()).visible, 30_000, step, "a product dialog stayed open",
      async () => this.describeDialog((await this.alertState()).value));
  }

  /** Pastes the registration and reads it into the review the product shows before connecting. */
  async readRegistration(registration: string) {
    const { language } = await import("../../src/lang");
    const copy = language.risuNest.serverSync;
    const section = this.section() ?? fail("registration", "Sync server settings are not open");
    const area = await until(() => section.querySelector<HTMLTextAreaElement>("#server-registration"),
      15_000, "registration", "registration input is not shown");
    area.value = registration;
    area.dispatchEvent(new Event("input", { bubbles: true }));
    await tick();
    const read = this.buttons(section, copy.readRegistration);
    if (read.length !== 1 || read[0].disabled) fail("registration", "read button unavailable", { buttons: read.length });
    read[0].click();
    await tick();
    await until(async () => !section.querySelector("#server-registration") && this.buttons(section, copy.connect).length === 1,
      15_000, "registration", "registration review did not open", async () => ({ uiAlert: await this.uiAlert() }));
    return { reviewed: true };
  }

  /**
   * Connects from the review and answers the dialogs the product asks on the way. A replacement is
   * accepted only when `replacement` is "accept"; otherwise its appearance fails the step.
   */
  async connect(replacement: "accept" | "refuse", timeoutMs = 180_000) {
    const { language } = await import("../../src/lang");
    const copy = language.risuNest.serverSync;
    const section = this.section() ?? fail("connect", "Sync server settings are not open");
    const connect = this.buttons(section, copy.connect).filter((button) => !button.disabled);
    if (connect.length !== 1) fail("connect", "connect button unavailable", { buttons: connect.length });
    const outcome = { replacementShown: false, replacementGatedByCheckbox: null as boolean | null, previousFilesShown: false };
    connect[0].click();
    const deadline = performance.now() + timeoutMs;
    let settledSince: number | undefined;
    for (;;) {
      const dialog = await this.alertState();
      if (dialog.visible) {
        settledSince = undefined;
        await this.answerDialog(dialog.value, replacement, outcome);
      } else {
        const state = await this.state();
        if (state.configured && state.bound && !state.running && !state.progress && !state.error
          && !state.bindingIncomplete && !this.busy(section) && state.tone === "connected") {
          return { ...outcome, configured: true, bound: true, tone: state.tone };
        }
        // The product leaves an unbound review or an error once the attempt ends.
        const stopped = !this.busy(section) && !state.running && !state.progress
          && (!!section.querySelector('[role="alert"]') || !!section.querySelector("#server-registration") || state.bindingIncomplete);
        if (stopped) {
          settledSince ??= performance.now();
          if (performance.now() - settledSince > 3000)
            fail("connect", "connection attempt ended without a connection", { ...outcome, ...state, uiAlert: await this.uiAlert() });
        } else settledSince = undefined;
      }
      if (performance.now() > deadline)
        fail("connect", "connection did not settle", { ...outcome, ...(await this.state()), uiAlert: await this.uiAlert() });
      await pause(100);
    }
  }

  /** Answers a sync binding confirmation the way a person would: replace only when asked to accept, connect without downloading. */
  async answerDialog(value: { type: string; checkboxConfirm?: { title: string } ; msg?: string }, replacement: "accept" | "refuse",
    outcome: { replacementShown: boolean; replacementGatedByCheckbox: boolean | null; previousFilesShown: boolean }) {
    const { language } = await import("../../src/lang");
    if (value.type !== "checkboxConfirm" || !value.checkboxConfirm)
      fail("connect", "unexpected product dialog", await this.describeDialog(value));
    const dialog = await until(() => document.querySelector<HTMLElement>('[role="dialog"][aria-labelledby="checkbox-confirm-title"]'),
      5000, "connect", "confirmation dialog did not render");
    const title = value.checkboxConfirm.title;
    if (title === language.lwwSync.replaceTitle) {
      outcome.replacementShown = true;
      if (replacement !== "accept") fail("connect", "the product asked to replace local data", outcome);
      const action = this.buttons(dialog, language.lwwSync.replaceAction);
      const box = dialog.querySelector<HTMLInputElement>('input[type="checkbox"]');
      if (action.length !== 1 || !box) fail("connect", "replacement dialog controls missing", { actions: action.length, checkbox: !!box });
      outcome.replacementGatedByCheckbox = action[0].disabled && !box.checked;
      box.click();
      await tick();
      if (!box.checked || action[0].disabled) fail("connect", "replacement action stayed disabled after acknowledging", outcome);
      action[0].click();
    } else if (title === language.lwwSync.previousFilesTitle) {
      outcome.previousFilesShown = true;
      const action = this.buttons(dialog, language.risuNest.serverSync.connect);
      if (action.length !== 1 || action[0].disabled) fail("connect", "previous files dialog action unavailable");
      action[0].click();
    } else {
      fail("connect", "unexpected confirmation dialog", await this.describeDialog(value));
    }
    await until(async () => (await this.alertState()).value !== value, 10_000, "connect", "confirmation dialog did not close");
  }

  /** Requires the connected state without entering a registration. */
  async connectedWithoutRegistration(timeoutMs = 120_000) {
    const section = this.section() ?? fail("reconnect", "Sync server settings are not open");
    let last = await this.state();
    await until(async () => {
      last = await this.state();
      return last.configured && last.bound && !last.running && !last.progress && !last.error && last.tone === "connected" && !this.busy(section);
    }, timeoutMs, "reconnect", "stored registration did not reconnect", () => ({ ...last }));
    return { configured: true, bound: true, registrationShown: !!section.querySelector("#server-registration") };
  }

  async pendingChanges() {
    return (await this.controller()).pendingChanges();
  }

  /** Presses the product's sync button and waits for one complete attempt with nothing left to send. */
  async syncNow(step: string, timeoutMs = 180_000) {
    const { language } = await import("../../src/lang");
    const section = this.section() ?? fail(step, "Sync server settings are not open");
    const controller = await this.controller();
    const stages = new Set<string>();
    let started = Number.POSITIVE_INFINITY;
    // Stages of an attempt that began before the button was pressed are not this attempt's.
    const collect = (view: ReturnType<typeof controller.snapshot>) => {
      for (const attempt of [view.progress, view.finished])
        if (attempt && attempt.startedAt >= started) for (const stage of attempt.stages) stages.add(stage);
    };
    const stop = controller.subscribe(collect);
    try {
      const button = await until(() => this.buttons(section, language.risuNest.serverSync.syncNow).find((candidate) => !candidate.disabled),
        30_000, step, "sync button unavailable");
      started = Date.now();
      button.click();
      let pending: number | undefined;
      let erroredSince: number | undefined;
      await until(async () => {
        const view = controller.snapshot();
        if (view.error && !view.running && !view.progress) {
          erroredSince ??= performance.now();
          if (performance.now() - erroredSince > 3000) fail(step, "sync attempt reported an error", { ...(await this.state()), stages: [...stages] });
          return false;
        }
        erroredSince = undefined;
        if (view.running || view.progress || this.busy(section) || view.lastSuccessAt === undefined || view.lastSuccessAt < started) return false;
        collect(view);
        pending = await controller.pendingChanges();
        return pending === 0;
      }, timeoutMs, step, "sync attempt did not complete", () => ({ pending: pending ?? null, stages: [...stages] }));
      return { pending: 0, stages: [...stages].sort() };
    } finally {
      stop();
    }
  }

  /** Opens the character through the product and edits the last message of its selected conversation. */
  async editLastMessage(characterId: string, value: string, reason: string) {
    const { DBState } = await import("../../src/ts/stores.svelte");
    const { changeChar } = await import("../../src/ts/characters");
    const { getPersistentDataRuntime } = await import("../../src/ts/storage/persistentDataRuntime.svelte");
    const index = DBState.db.characters.findIndex((character) => character.chaId === characterId);
    if (index < 0 || !(await changeChar(index)))
      fail("edit", "character is not open in the product", { workingSetCharacters: DBState.db.characters.length, found: index >= 0 });
    const runtime = getPersistentDataRuntime();
    const lease = await runtime.acquireCompleteConversation("edit-message");
    let conversationId: string;
    try {
      const { captureChatMessageTarget, saveCapturedChatMessage } = await import("../../src/ts/chatMessageUi");
      const context = {
        captureCurrent: () => {
          const character = DBState.db.characters[index];
          return { character, conversation: character.chats[character.chatPage] };
        },
        getCurrentSession: () => runtime.getActiveConversationSession(),
      };
      conversationId = lease.session.conversationId;
      const target = captureChatMessageTarget({ ...context, absoluteIndex: lease.session.totalMessages - 1 });
      if (!target) fail("edit", "no message to edit in the selected conversation");
      if (!saveCapturedChatMessage(target, context, value).saved) fail("edit", "product did not accept the edit");
    } finally {
      lease.release();
    }
    await tick();
    await runtime.flushPendingData(reason);
    const saved = await runtime.store.readConversation(characterId, conversationId);
    if (saved?.value.message.at(-1)?.data !== value) fail("edit", "edit did not persist", { messages: saved?.value.message.length ?? null });
    return { persisted: true, conversationId };
  }

  /**
   * Leaves Settings, opens the character holding a marker and waits until the chat shows it, when it is
   * the selected conversation.
   */
  async renderMarker(location: { characterId: string; conversationId: string }, marker: string) {
    const { DBState, settingsOpen } = await import("../../src/ts/stores.svelte");
    await this.waitForNoDialog("render");
    settingsOpen.set(false);
    await tick();
    const { changeChar } = await import("../../src/ts/characters");
    const { getPersistentDataRuntime } = await import("../../src/ts/storage/persistentDataRuntime.svelte");
    const index = DBState.db.characters.findIndex((character) => character.chaId === location.characterId);
    const workingSetCharacters = DBState.db.characters.length;
    if (index < 0) fail("render", "character holding the marker is missing from the product", { workingSetCharacters });
    if (!(await changeChar(index))) fail("render", "character did not open", { workingSetCharacters });
    const selected = getPersistentDataRuntime().captureSelectedConversationTarget();
    if (selected?.conversationId !== location.conversationId) return { workingSetCharacters, rendered: false, selectedConversation: false };
    await until(() => document.getElementById("app")!.textContent!.includes(marker), 30_000, "render", "marker did not render in the chat",
      () => ({ workingSetCharacters }));
    return { workingSetCharacters, rendered: true, selectedConversation: true };
  }

  /** Quits through `quit` and fails if the product's exit stops at a blocked or failed state. */
  async quitWatchingExit(quit: () => Promise<unknown>) {
    const { syncExitDialogState } = await import("../../src/ts/storage/syncExitProduction");
    const phases: string[] = [];
    let stopped: string | undefined;
    const unsubscribe = syncExitDialogState.subscribe((state) => {
      phases.push(state.phase);
      if (["remote-blocked", "local-failed", "edit-blocked"].includes(state.phase)) stopped = state.phase;
    });
    try {
      void quit().catch(() => {});
      await until(() => stopped, 60_000, "quit", "the application did not exit", () => ({ phases }));
      fail("quit", "the product exit stopped", { exitPhase: stopped, phases });
    } finally {
      unsubscribe();
    }
  }
}
