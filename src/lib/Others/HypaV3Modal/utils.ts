import { get } from "svelte/store";
import { processScriptFull, risuChatParser } from "src/ts/process/scripts";
import { type Message } from "src/ts/storage/database.svelte";
import { DBState, selectedCharID } from "src/ts/stores.svelte";
import { language } from "src/lang";
import {
  queryChatMessageTargetAt,
  queryChatMessageTargetById,
  queryChatMessageTargetsByIds,
  type CapturedChatMessageTarget,
} from "src/ts/chatMessageUi";
import {
  acquireCompleteConversation,
  captureSelectedConversationTarget,
  getPersistentDataRuntime,
  peekActiveConversationSession,
} from "src/ts/storage/persistentDataRuntime.svelte";

export type DualActionParams = {
  onMainAction?: () => void;
  onAlternativeAction?: () => void;
};

export function handleDualAction(
  element: HTMLElement,
  params: DualActionParams = {}
) {
  const DOUBLE_TAP_DELAY = 300;
  // A finger that travels farther than this is scrolling, not tapping.
  const TAP_SLOP_PX = 8;
  // Touch screens follow a tap with a click of their own.
  const TOUCH_CLICK_WINDOW = 700;

  const state = {
    lastTap: 0,
    tapTimeout: null,
    touch: null as { identifier: number; x: number; y: number } | null,
    lastTouchEnd: 0,
  };

  const trackedTouch = (event: TouchEvent) => {
    const touch = state.touch;
    return touch
      ? Array.from(event.changedTouches).find((t) => t.identifier === touch.identifier)
      : undefined;
  };

  const moved = (touch: Touch) =>
    Math.hypot(touch.clientX - state.touch!.x, touch.clientY - state.touch!.y) > TAP_SLOP_PX;

  const handleTouchStart = (event: TouchEvent) => {
    const touch = event.changedTouches[0];
    state.touch = event.touches.length === 1 && touch
      ? { identifier: touch.identifier, x: touch.clientX, y: touch.clientY }
      : null;
  };

  const handleTouchMove = (event: TouchEvent) => {
    const touch = trackedTouch(event);
    if (event.touches.length !== 1 || (touch && moved(touch))) state.touch = null;
  };

  const handleTouchCancel = () => {
    state.touch = null;
  };

  const handleTouch = (event: TouchEvent) => {
    state.lastTouchEnd = new Date().getTime();
    const touch = trackedTouch(event);
    const tapped = touch && !moved(touch) && !(element as HTMLButtonElement).disabled;
    state.touch = null;
    if (!tapped) return;

    const currentTime = state.lastTouchEnd;
    const tapLength = currentTime - state.lastTap;

    if (tapLength < DOUBLE_TAP_DELAY && tapLength > 0) {
      // Double tap detected
      event.preventDefault();
      window.clearTimeout(state.tapTimeout); // Cancel the first tap timeout
      params.onAlternativeAction?.();
      state.lastTap = 0; // Reset state
    } else {
      state.lastTap = currentTime; // First tap
      // Delayed single tap execution
      state.tapTimeout = window.setTimeout(() => {
        if (state.lastTap === currentTime) {
          // If no double tap occurred
          params.onMainAction?.();
        }
      }, DOUBLE_TAP_DELAY);
    }
  };

  const handleClick = (event: MouseEvent) => {
    // The touch handlers have already decided this press.
    if (new Date().getTime() - state.lastTouchEnd < TOUCH_CLICK_WINDOW) return;
    if (event.shiftKey) {
      params.onAlternativeAction?.();
    } else {
      params.onMainAction?.();
    }
  };

  // Touch-capable computers also have a mouse, so both paths stay registered.
  element.addEventListener("touchstart", handleTouchStart, { passive: true });
  element.addEventListener("touchmove", handleTouchMove, { passive: true });
  element.addEventListener("touchcancel", handleTouchCancel);
  element.addEventListener("touchend", handleTouch);
  element.addEventListener("click", handleClick);

  return {
    destroy() {
      element.removeEventListener("touchstart", handleTouchStart);
      element.removeEventListener("touchmove", handleTouchMove);
      element.removeEventListener("touchcancel", handleTouchCancel);
      element.removeEventListener("touchend", handleTouch);
      element.removeEventListener("click", handleClick);

      window.clearTimeout(state.tapTimeout); // Cleanup timeout
    },
    update(newParams: DualActionParams) {
      params = newParams;
    },
  };
}

export function getFirstMessage(): string | null {
  const char = DBState.db.characters[get(selectedCharID)];
  const chat = char.chats[DBState.db.characters[get(selectedCharID)].chatPage];

  return chat.fmIndex === -1
    ? char.firstMessage
    : char.alternateGreetings?.[chat.fmIndex]
    ? char.alternateGreetings[chat.fmIndex]
    : null;
}

const currentHypaMessageContext = {
  captureCurrent: () => {
    const character = DBState.db.characters[get(selectedCharID)];
    const conversation = character?.chats[character.chatPage];
    return character && conversation ? { character, conversation } : null;
  },
  getCurrentSession: peekActiveConversationSession,
  captureSelectedConversationTarget,
  acquirePersistentRevision: (revision: number) =>
    getPersistentDataRuntime().store.acquireRevision(revision),
  acquireCompleteConversation,
};

export function captureCurrentHypaMessageAt(
  absoluteIndex: number
): Promise<CapturedChatMessageTarget | null> {
  return queryChatMessageTargetAt(currentHypaMessageContext, absoluteIndex);
}

export function captureCurrentHypaMessageById(
  messageId: string
): Promise<CapturedChatMessageTarget | null> {
  return queryChatMessageTargetById(
    currentHypaMessageContext,
    messageId,
    "first"
  );
}

export function captureCurrentHypaMessagesByIds(
  messageIds: readonly string[]
): Promise<CapturedChatMessageTarget[]> {
  return queryChatMessageTargetsByIds(
    currentHypaMessageContext,
    messageIds,
    "first"
  );
}

export async function processRegexScript(
  msg: Message,
  msgIndex: number = -1
): Promise<Message> {
  const char = DBState.db.characters[get(selectedCharID)];
  const newData: string = (
    await processScriptFull(
      char,
      risuChatParser(msg.data, { chara: char, role: msg.role }),
      "editprocess",
      msgIndex,
      {
        chatRole: msg.role,
      }
    )
  ).data;

  return {
    ...msg,
    data: newData,
  };
}

export function getCategoryName(categoryId: string | undefined, categories: any[]): string {
  const category = categories.find(c => c.id === (categoryId || ""));
  return category?.name || language.hypaV3Modal.unclassified;
}

export function createCategoryId(): string {
  return `cat_${Date.now()}`;
}

export function shouldShowSummary(
  summary: any, 
  index: number, 
  showImportantOnly: boolean, 
  selectedCategoryFilter: string
): boolean {
  if (showImportantOnly && !summary.isImportant) {
    return false;
  }

  if (selectedCategoryFilter !== "all") {
    const summaryCategory = summary.categoryId || "";
    if (summaryCategory !== selectedCategoryFilter) {
      return false;
    }
  }

  return true;
}

export function isGuidLike(str: string): boolean {
  const strTrimed = str.trim();
  if (strTrimed.length < 4) return false;
  return /^[0-9a-f]{4,12}(-[0-9a-f]{4,12}){0,4}-?$/i.test(strTrimed);
}

export function parseSelectionInput(input: string, totalCount: number): Set<number> {
  const newSelection = new Set<number>();
  const parts = input.split(',').map(s => s.trim()).filter(s => s);

  for (const part of parts) {
    if (part.includes('-')) {
      const [startStr, endStr] = part.split('-').map(s => s.trim());
      const start = parseInt(startStr);
      const end = parseInt(endStr);

      if (!isNaN(start) && !isNaN(end) && start <= end) {
        for (let i = start; i <= end; i++) {
          const index = i - 1;
          if (index >= 0 && index < totalCount) {
            newSelection.add(index);
          }
        }
      }
    } else {
      const num = parseInt(part);
      if (!isNaN(num)) {
        const index = num - 1;
        if (index >= 0 && index < totalCount) {
          newSelection.add(index);
        }
      }
    }
  }

  return newSelection;
}
