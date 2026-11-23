import { mount, settled as domSettled, unmount } from "svelte";
import DefaultChatScreen from "../../src/lib/ChatScreens/DefaultChatScreen.svelte";
import { DBState, selectedCharID } from "../../src/ts/stores.svelte";
import type { Database, character } from "../../src/ts/storage/database.svelte";
import { getPersistentDataRuntime } from "../../src/ts/storage/persistentDataRuntime.svelte";

const pause = () => new Promise<void>(resolve => requestAnimationFrame(() => resolve()));

/** Requires the disposable synthetic streaming fixture, never an installed profile. */
export async function runParserScreenProbe(assets: number, baseline: boolean, stopFixture: () => Promise<void>) {
  if (!document.querySelector('[data-streaming-smoke="synthetic-v1"]')
      || DBState.db.characters.length !== 1 || DBState.db.characters[0].chaId !== 'streaming-synthetic') {
    throw new Error('isolated-synthetic-fixture-required');
  }
  const database = JSON.parse(JSON.stringify(DBState.db)) as Database;
  const makeCharacter = (id: string, assetCount: number): character => ({
    ...database.characters[0] as character, chaId: id, name: id, customscript: [],
    additionalAssets: Array.from({ length: assetCount }, (_, index) => [`asset-${index}`, `synthetic-${index}.png`, 'png']),
    chats: [{ ...database.characters[0].chats[0], id: `${id}-chat`, message: Array.from({ length: 80 }, (_, index) => ({
      role: index % 2 ? 'char' : 'user', chatId: `${id}-${index}`, data: `Synthetic row ${index}.`,
    })) }],
  });
  database.characters = [makeCharacter('synthetic-before', 0), makeCharacter('synthetic-after', assets)];
  await stopFixture();
  const runtime = getPersistentDataRuntime();
  await runtime.store.open();
  await runtime.store.replaceFromDatabase(database, (await runtime.store.readRoot()).revision);
  DBState.db = database;
  selectedCharID.set(0);
  await runtime.initializeActiveWorkingSet(DBState.db);
  const metrics: { name: string; durationMs: number }[] = [];
  const parse = { active: 0, calls: [] as { index: number; durationMs: number }[] };
  Object.assign(globalThis, { __parserSeedBaseline: baseline, __parserSeedMetrics: metrics, __viewportParseMetrics: parse });
  const host = document.createElement('section');
  host.style.cssText = 'height:100vh;width:100vw;position:relative';
  document.body.replaceChildren(host);
  const component = mount(DefaultChatScreen, { target: host });
  const ready = async (characterId: string) => {
    await domSettled();
    const deadline = performance.now() + 60000;
    let previous = '';
    let stableFrames = 0;
    while (true) {
      if (performance.now() > deadline) throw new Error('parser-screen-not-ready');
      await pause();
      await domSettled();
      const target = runtime.captureSelectedConversationTarget();
      const selectedConversation = target && { characterId: target.characterId, conversationId: target.conversationId };
      const rowIdentities = [...host.querySelectorAll<HTMLElement>('[data-chat-render-key]')].map(row => {
        const message = row.querySelector<HTMLElement>('.risu-chat[data-chat-index][data-chat-id]');
        return {
          key: row.dataset.chatRenderKey!,
          viewportIndex: Number(row.dataset.chatViewportIndex),
          index: row.dataset.chatIndex === undefined ? null : Number(row.dataset.chatIndex),
          chatId: message?.dataset.chatId ?? null,
          renderedIndex: message ? Number(message.dataset.chatIndex) : null,
        };
      });
      const ordinaryRows = rowIdentities.filter(row => row.index !== null);
      const complete = selectedConversation?.characterId === characterId
        && selectedConversation.conversationId === `${characterId}-chat`
        && ordinaryRows.length > 0 && !host.querySelector('[data-chat-mount-pending]') && parse.active === 0
        && ordinaryRows.every(row => Number.isInteger(row.index) && row.index === row.renderedIndex
          && row.chatId === `${characterId}-${row.index}` && parse.calls.some(call => call.index === row.index));
      // ResizeObserver can schedule another reconciliation frame after rows first appear ready.
      const snapshot = JSON.stringify({ selectedConversation, rowIdentities, parseCalls: parse.calls.length, metrics: metrics.length });
      stableFrames = complete && snapshot === previous ? stableFrames + 1 : 0;
      previous = complete ? snapshot : '';
      if (stableFrames >= 2) return { selectedConversation, rowIdentities };
    }
  };
  try {
    await ready('synthetic-before');
    metrics.length = 0; parse.calls.length = 0;
    const started = performance.now();
    if (!await runtime.activateCharacter('synthetic-after')) throw new Error('synthetic-switch-failed');
    const settled = await ready('synthetic-after');
    const elapsedMs = performance.now() - started;
    if (!baseline && metrics.filter(entry => entry.name === 'createChatParserContextSeed').length !== 1) {
      throw new Error('parser-seed-rebuilt-within-same-revision');
    }
    return { assets, baseline, elapsedMs, rows: settled.rowIdentities.length, ...settled,
      selected: true, settledFrames: 3,
      metrics: Object.fromEntries(['createChatParserContextSeed', 'captureChatParserContextSeed', 'forkChatParserContext'].map(name => {
        const entries = metrics.filter(entry => entry.name === name);
        return [name, { count: entries.length, durationMs: entries.reduce((sum, entry) => sum + entry.durationMs, 0) }];
      })), parseCalls: parse.calls.length,
      parseCallsByIndex: [...new Set(parse.calls.map(call => call.index))].sort((a, b) => a - b)
        .map(index => ({ index, count: parse.calls.filter(call => call.index === index).length })) };
  } finally { await unmount(component); }
}
