import { safeStructuredClone } from '../safeStructuredClone'
import type { WindowedConversationMutationController } from '../storage/activeWorkingSet.svelte'
import type { Chat, Message } from '../storage/database.svelte'
import type { HistoryWindowController } from './historyWindowController'
import { inheritHistoryWindow } from './historyWindowIndex'
import type { ConversationCommitObserver } from './conversationOperationContext'

export interface HistoryWindowChange {
    start: number
    deleteCount: number
    messages: Message[]
}

function sameMessage(left: Message, right: Message): boolean {
    return left === right || JSON.stringify(left) === JSON.stringify(right)
}

/**
 * The single range that turns `before` into `after`, or null when they match.
 * A range that changes the message count runs to the end of the window,
 * because a windowed conversation changes its count only at its end.
 */
export function diffHistoryWindow(
    before: readonly Message[],
    after: readonly Message[],
): HistoryWindowChange | null {
    const shared = Math.min(before.length, after.length)
    let prefix = 0
    while (prefix < shared && sameMessage(before[prefix], after[prefix])) prefix += 1
    if (prefix === before.length && prefix === after.length) return null
    let suffix = 0
    while (
        before.length === after.length
        && suffix < shared - prefix
        && sameMessage(before[before.length - 1 - suffix], after[after.length - 1 - suffix])
    ) suffix += 1
    return {
        start: prefix,
        deleteCount: before.length - prefix - suffix,
        messages: after.slice(prefix, after.length - suffix),
    }
}

function changeCommand(change: HistoryWindowChange, length: number) {
    return change.deleteCount === 0 && change.start === length
        ? 'append'
        : change.deleteCount === 1 && change.messages.length === 1
        ? 'edit'
        : 'replace-range'
}

/** Writes a script's edited window back through the controller as one range. */
export function writeHistoryWindow(
    controller: WindowedConversationMutationController,
    after: readonly Message[],
): boolean {
    const before = controller.chat.message
    const change = diffHistoryWindow(before, after)
    if (!change) return true
    return controller.applyRange(
        change.start,
        change.deleteCount,
        change.messages,
        changeCommand(change, before.length),
    )
}

/**
 * Writes back a copy of the window chat that a script edited: its metadata
 * replaces the window's, and its messages are written as one range.
 */
export function writeHistoryWindowChat(controller: HistoryWindowController, result: Chat): boolean {
    const window = controller.chat
    if (result !== window) {
        const target = window as unknown as Record<string, unknown>
        const source = result as unknown as Record<string, unknown>
        for (const key of Object.keys(target)) {
            if (key !== 'message' && !Object.hasOwn(source, key)) delete target[key]
        }
        for (const key of Object.keys(source)) {
            if (key !== 'message') target[key] = source[key]
        }
    }
    const change = diffHistoryWindow(window.message, result.message)
    if (!change) return controller.reconcileMetadata()
    return controller.applyRange(
        change.start,
        change.deleteCount,
        change.messages,
        changeCommand(change, window.message.length),
    )
}

export interface HistoryWindowCopy {
    readonly chat: Chat
    /** Writes the copy's changes back through the controller. */
    commit(): boolean
}

/** A script may publish its positional copy only into the window it read. */
export function captureHistoryWindowSource(controller: HistoryWindowController) {
    const current = controller.isCurrent()
    const version = controller.mutationVersion
    const start = controller.absoluteStartIndex
    const messages = [...controller.chat.message]
    return () => current && controller.isCurrent()
        && version === controller.mutationVersion && start === controller.absoluteStartIndex
        && messages.length === controller.chat.message.length
        && messages.every((message, index) => controller.chat.message[index] === message)
}

/** A copy of the window chat for a script that edits the chat it is handed. */
export function openHistoryWindowCopy(
    controller: HistoryWindowController,
    onCommit?: ConversationCommitObserver,
): HistoryWindowCopy {
    const isSourceCurrent = captureHistoryWindowSource(controller)
    const source = [...controller.chat.message]
    const chat = safeStructuredClone(controller.chat)
    const drafts = [...chat.message]
    inheritHistoryWindow(controller.chat, chat)
    let committed = false
    return {
        chat,
        commit() {
            if (committed || !isSourceCurrent()) return false
            if (!writeHistoryWindowChat(controller, chat)) return false
            committed = true
            const writtenVersion = controller.mutationVersion
            const written = [...controller.chat.message]
            onCommit?.({
                follows: () => false,
                remapWindowTarget(owner, message) {
                    if (owner !== controller || !controller.isCurrent()
                        || writtenVersion !== controller.mutationVersion
                        || written.length !== controller.chat.message.length
                        || written.some((entry, index) => controller.chat.message[index] !== entry)) return null
                    const oldIndex = source.indexOf(message)
                    if (oldIndex < 0 || !message.chatId
                        || source.filter((entry) => entry.chatId === message.chatId).length !== 1) return null
                    const nextIndex = chat.message.indexOf(drafts[oldIndex])
                    if (nextIndex < 0 || chat.message[nextIndex].chatId !== message.chatId
                        || chat.message.filter((entry) => entry.chatId === message.chatId).length !== 1) return null
                    return nextIndex
                },
            })
            return true
        },
    }
}

/**
 * Writes one message edit by absolute index. A message before the window is
 * left alone, as a write to an out-of-range index is.
 */
export function writeHistoryWindowMessage(
    controller: WindowedConversationMutationController,
    absoluteIndex: number,
    edit: (message: Message) => Message,
): boolean {
    const index = absoluteIndex - controller.absoluteStartIndex
    const message = controller.chat.message[index]
    if (index < 0 || !message) return true
    return controller.applyRange(index, 1, [edit(message)], 'edit')
}
