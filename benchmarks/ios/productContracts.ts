import { invoke } from "@tauri-apps/api/core";
import { check, initialize, pause } from "./contracts";

const expectedKey = "ios-synthetic-product-edit";
async function until(predicate: () => boolean, message: string) {
  const deadline = performance.now() + 60_000;
  while (!predicate()) {
    check(performance.now() < deadline, message);
    await pause(100);
  }
}

/** Product UI and storage, mounted only by the isolated verification entry. */
export async function productApp(restart: boolean) {
  if (restart) {
    await invoke("pds_open");
    const expected = JSON.parse(localStorage.getItem(expectedKey)!);
    check(expected, "previous synthetic product edit required");
    const saved = await invoke<{ value: { message: { data: string }[] } }>(
      "pds_read_conversation",
      { characterId: "char-a", conversationId: expected.conversationId },
    );
    check(
      saved.value.message.at(-1)?.data === expected.marker,
      "product edit survives process restart",
    );
    return { passed: true, restarted: true };
  }
  await initialize();
  const opened = await invoke<{ revision: number }>("pds_open");
  await invoke("pds_commit", {
    commit: {
      expectedRevision: opened.revision,
      rootMutations: [{ type: "set", key: "didFirstSetup", value: true }],
    },
    assetAliases: [],
  });
  localStorage.setItem("risunest_tos_v1", "true");
  document.getElementById("benchmark")!.remove();
  const app = await import("../../src/main");
  await app.default;
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
  const { tick } = await import("svelte");
  const index = DBState.db.characters.findIndex(
    (character) => character.chaId === "char-a",
  );
  check(
    index >= 0 && (await changeChar(index)),
    "open synthetic character in product UI",
  );
  const marker = "ios-synthetic-ui-edit 🐿️";
  const runtime = getPersistentDataRuntime();
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
      saveCapturedChatMessage(target, context, marker).saved,
      "product message edit accepted",
    );
  } finally {
    lease.release();
  }
  await tick();
  await runtime.flushPendingData("ios-ui-smoke");
  await until(
    () => document.getElementById("app")!.textContent!.includes(marker),
    "edited synthetic message did not render in product chat",
  );
  const persisted = await invoke<{
    revision: number;
    value: { message: { data: string }[] };
  }>("pds_read_conversation", { characterId: "char-a", conversationId });
  check(
    persisted.value.message.at(-1)?.data === marker,
    "product edit persisted through Rust",
  );
  localStorage.setItem(expectedKey, JSON.stringify({ conversationId, marker }));
  return {
    passed: true,
    revision: persisted.revision,
    renderedTextLength: document.getElementById("app")!.textContent!.length,
  };
}

/** Product onboarding, mounted with a reset synthetic working set. */
export async function productOnboarding() {
  await initialize();
  const opened = await invoke<{ revision: number }>("pds_open");
  await invoke("pds_commit", {
    commit: {
      expectedRevision: opened.revision,
      rootMutations: [
        { type: "set", key: "didFirstSetup", value: false },
        { type: "set", key: "language", value: "en" },
      ],
    },
    assetAliases: [],
  });
  localStorage.setItem("risunest_tos_v1", "true");
  document.getElementById("benchmark")!.remove();
  const app = await import("../../src/main");
  await app.default;
  await until(
    () => document.getElementById("app")!.textContent!.includes("Choose how to start."),
    "product onboarding did not initialize",
  );
  return { passed: true };
}

/** RisuNest settings, mounted directly for physical-device visual checks. */
export async function productSettings() {
  await initialize();
  const opened = await invoke<{ revision: number }>("pds_open");
  await invoke("pds_commit", {
    commit: {
      expectedRevision: opened.revision,
      rootMutations: [
        { type: "set", key: "didFirstSetup", value: true },
        { type: "set", key: "language", value: "en" },
      ],
    },
    assetAliases: [],
  });
  localStorage.setItem("risunest_tos_v1", "true");
  document.getElementById("benchmark")!.remove();
  const app = await import("../../src/main");
  await app.default;
  const { SettingsMenuIndex, settingsOpen } = await import(
    "../../src/ts/stores.svelte"
  );
  SettingsMenuIndex.set(17);
  settingsOpen.set(true);
  await until(
    () => document.getElementById("app")!.textContent!.includes("Performance"),
    "RisuNest settings did not initialize",
  );
  return { passed: true };
}
