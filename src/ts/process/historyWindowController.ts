import isEqual from 'lodash/isEqual'

import { attachHistoryWindow } from './historyWindowIndex'
import { replaceArrayRange } from '../arrayRange'
import { safeStructuredClone } from '../safeStructuredClone'
import {
    cloneConversationMetadata,
    type ActiveConversationSession,
} from '../storage/activeConversationSession'
import type { WindowedConversationMutationController } from '../storage/activeWorkingSet.svelte'
import type { Chat } from '../storage/database.svelte'
import { cloneConversationMetadata as cloneMetadataFields } from '../storage/selectedConversationLifecycle'

type ConversationFields = Omit<Chat, 'message'>

export interface HistoryWindowSessionSource {
    session: ActiveConversationSession
    /** The conversation object whose message array the session owns. */
    conversation: Chat
}

export interface HistoryWindowControllerEnvironment {
    /** A windowed controller over `chat`, when the generation's conversation is windowed. */
    captureWindowed(chat: Chat, absoluteStartIndex: number): WindowedConversationMutationController | null
    /** The active session, when the generation's conversation is complete. */
    captureSession(): HistoryWindowSessionSource | null
    getCurrentSession(): ActiveConversationSession | null
    /**
     * The conversation's live metadata by reference, which other writers may
     * change during the generation. The controller copies only what it takes.
     */
    readLiveMetadata(): ConversationFields | null
    /** Called once when no backend holds the window any more. */
    onLost?(): void
}

export interface HistoryWindowController extends WindowedConversationMutationController {
    /** Changes whenever this controller accepts a write or adopts metadata. */
    readonly mutationVersion?: number
    /**
     * Takes metadata changes made on the live conversation into the window and
     * publishes the window's own metadata changes.
     */
    reconcileMetadata(): boolean
}

/** The fields of a conversation other than its messages, by reference. */
export function conversationFieldsOf(conversation: Chat | ConversationFields): ConversationFields {
    const fields: Record<string, unknown> = {}
    for (const key of Object.keys(conversation)) {
        if (key !== 'message') fields[key] = (conversation as unknown as Record<string, unknown>)[key]
    }
    return fields as ConversationFields
}

function sessionHoldsWindow(
    source: HistoryWindowSessionSource,
    chat: Chat,
    absoluteStartIndex: number,
): boolean {
    const { session, conversation } = source
    if (!session.isActive || session.materializeCompatibilityArray() !== conversation.message) return false
    if (session.totalMessages !== absoluteStartIndex + chat.message.length) return false
    const messages = conversation.message
    for (let index = 0; index < chat.message.length; index += 1) {
        if (messages[absoluteStartIndex + index]?.chatId !== chat.message[index].chatId) return false
    }
    return true
}

/**
 * Writes a window of a complete conversation through its active session, by
 * absolute index. Capture fails unless the session ends with the window's
 * messages, identified by message id.
 */
export function captureSessionHistoryWindowController(
    source: HistoryWindowSessionSource,
    getCurrentSession: () => ActiveConversationSession | null,
    chat: Chat,
    absoluteStartIndex: number,
): WindowedConversationMutationController | null {
    const { session, conversation } = source
    if (
        !Number.isSafeInteger(absoluteStartIndex)
        || absoluteStartIndex < 0
        || getCurrentSession() !== session
        || !sessionHoldsWindow(source, chat, absoluteStartIndex)
    ) return null
    const pin = session.acquirePin('transaction')
    let released = false
    let expectedVersion = session.version
    const isCurrentOwner = () => !released && session.isActive && getCurrentSession() === session
    const isCurrent = () => {
        if (!isCurrentOwner() || session.materializeCompatibilityArray() !== conversation.message ||
            absoluteStartIndex + chat.message.length > session.totalMessages) return false
        if (session.version === expectedVersion) return true
        if (!session.canContinueGenerationFrom(expectedVersion)) return false
        if (session.contentVersion > expectedVersion) {
            // An admitted metadata publication can replace full message objects with the same content.
            const rows = conversation.message.slice(absoluteStartIndex, absoluteStartIndex + chat.message.length)
            replaceArrayRange(chat.message, 0, chat.message.length, safeStructuredClone(rows))
        }
        expectedVersion = session.version
        return true
    }
    return {
        chat,
        get absoluteStartIndex() { return absoluteStartIndex },
        get mutationVersion() { return session.contentVersion },
        isOwnerCurrent: isCurrentOwner,
        isCurrent,
        adoptSessionCommit(commit, localIndex, id) {
            if (!isCurrentOwner() || !commit.follows(session, expectedVersion, conversation)) return null
            let index = commit.mapSessionIndex?.(absoluteStartIndex + localIndex)
            if (index === undefined) {
                const first = session.findMessageTargetsByIds([id], 'first')[0]
                const last = session.findMessageTargetsByIds([id], 'last')[0]
                if (!first || !last || first.absoluteIndex !== last.absoluteIndex) return null
                index = first.absoluteIndex
            }
            if (index === null || conversation.message[index]?.chatId !== id) return null
            const length = Math.min(session.totalMessages, chat.message.length)
            absoluteStartIndex = Math.max(0, Math.min(index - localIndex, session.totalMessages - length))
            replaceArrayRange(chat.message, 0, chat.message.length, safeStructuredClone(conversation.message.slice(absoluteStartIndex, absoluteStartIndex + length)))
            attachHistoryWindow(chat, absoluteStartIndex)
            expectedVersion = session.version
            return index - absoluteStartIndex
        },
        applyRange(localStart, deleteCount, messages) {
            if (
                !isCurrent()
                || !Number.isSafeInteger(localStart)
                || localStart < 0
                || !Number.isSafeInteger(deleteCount)
                || deleteCount < 0
                || localStart + deleteCount > chat.message.length
            ) return false
            const detached = safeStructuredClone([...messages])
            const expectedMetadata = conversationFieldsOf(conversation)
            session.applyOperation({
                expectedVersion,
                expectedMetadata,
                // Unchanged metadata is passed as the baseline itself, so it is not copied.
                metadata: isEqual(conversationFieldsOf(chat), expectedMetadata)
                    ? expectedMetadata
                    : cloneConversationMetadata(chat),
                ...(deleteCount === 0 && detached.length === 0
                    ? {}
                    : {
                        range: {
                            position: session.positionAt(absoluteStartIndex + localStart),
                            deleteCount,
                            messages: detached,
                        },
                    }),
            })
            replaceArrayRange(chat.message, localStart, deleteCount, safeStructuredClone(detached))
            expectedVersion = session.version
            return true
        },
        release() {
            if (released) return
            released = true
            pin.release()
        },
    }
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && Object.getPrototypeOf(value) === Object.prototype
}

// Three-way merge against the metadata last published: a side that left a
// value as it was takes the other side's value, and the window wins when both
// changed it. Plain objects such as chat variables merge one level deeper.
function mergeField(window: unknown, baseline: unknown, live: unknown, nested: boolean): unknown {
    if (isEqual(window, baseline)) return live
    if (isEqual(live, baseline)) return window
    if (!nested && isPlainRecord(window) && isPlainRecord(live)) {
        const base = isPlainRecord(baseline) ? baseline : {}
        const merged: Record<string, unknown> = {}
        for (const key of new Set([...Object.keys(window), ...Object.keys(base), ...Object.keys(live)])) {
            const value = mergeField(window[key], base[key], live[key], true)
            if (value !== undefined) merged[key] = value
        }
        return merged
    }
    return window
}

/** Merges live metadata into the window and returns the fields that differ from the baseline. */
function mergeLiveMetadata(chat: Chat, baseline: ConversationFields, live: ConversationFields): string[] {
    const fields = chat as unknown as Record<string, unknown>
    const base = baseline as Record<string, unknown>
    const current = live as Record<string, unknown>
    const changed: string[] = []
    for (const key of new Set([...Object.keys(fields), ...Object.keys(base), ...Object.keys(current)])) {
        if (key === 'message') continue
        const windowChanged = !isEqual(fields[key], base[key])
        if (!windowChanged && isEqual(current[key], base[key])) continue
        changed.push(key)
        const value = windowChanged ? mergeField(fields[key], base[key], current[key], false) : current[key]
        // Live values are references, so whatever the window takes is copied.
        if (value === undefined) delete fields[key]
        else if (value !== fields[key]) fields[key] = safeStructuredClone(value)
    }
    return changed
}

function updateBaseline(baseline: ConversationFields, chat: Chat, keys: readonly string[]): void {
    const base = baseline as Record<string, unknown>
    const fields = chat as unknown as Record<string, unknown>
    for (const key of keys) {
        if (Object.hasOwn(fields, key)) base[key] = safeStructuredClone(fields[key])
        else delete base[key]
    }
}

/**
 * Keeps a generation's window writable across a promotion: when the current
 * backend goes stale, it captures whichever backend the conversation now
 * offers over the same window.
 */
export function createHistoryWindowController(
    environment: HistoryWindowControllerEnvironment,
    chat: Chat,
    absoluteStartIndex: number,
    initial: WindowedConversationMutationController | null = null,
): HistoryWindowController {
    let backend = initial
    let released = false
    let lost = false
    let mutationVersion = 0
    const baseline = cloneMetadataFields(chat)
    const resolve = (): WindowedConversationMutationController | null => {
        if (released) return null
        if (backend?.isCurrent()) return backend
        // A displaced window still owns its output handle. Positional consumers must reload it.
        if (backend?.isOwnerCurrent?.()) return null
        const source = environment.captureSession()
        if (backend?.isOwnerCurrent && !source) return null
        backend?.release()
        if (source?.session.canAdoptPersistedMetadata && sessionHoldsWindow(source, chat, absoluteStartIndex)) {
            const persisted = source.conversation.message.slice(absoluteStartIndex)
            const contentKeys = ['role', 'data', 'saying', 'chatId', 'name', 'disabled', 'isComment'] as const
            if (persisted.every((message, index) =>
                contentKeys.every((key) => Object.is(message[key], chat.message[index][key])))) {
                replaceArrayRange(chat.message, 0, persisted.length, safeStructuredClone(persisted))
            }
        }
        backend = source
            ? captureSessionHistoryWindowController(
                source,
                environment.getCurrentSession,
                chat,
                absoluteStartIndex,
            )
            : environment.captureWindowed(chat, absoluteStartIndex)
        if (!backend && !lost) {
            lost = true
            environment.onLost?.()
        }
        return backend
    }
    const applyRange: WindowedConversationMutationController['applyRange'] = (
        localStart,
        deleteCount,
        messages,
        command,
    ) => {
        const current = command === 'update-metadata' && backend?.isOwnerCurrent?.() ? backend : resolve()
        const live = current ? environment.readLiveMetadata() : null
        if (!current || !live) return false
        const changed = mergeLiveMetadata(chat, baseline, live)
        if (!current.applyRange(localStart, deleteCount, messages, command)) return false
        mutationVersion += 1
        updateBaseline(baseline, chat, changed)
        return true
    }
    return {
        chat,
        get absoluteStartIndex() { return backend?.absoluteStartIndex ?? absoluteStartIndex },
        get mutationVersion() { return mutationVersion + (backend?.mutationVersion ?? 0) },
        isOwnerCurrent: () => !released && (backend?.isOwnerCurrent?.() ?? (resolve() !== null)),
        adoptSessionCommit: (commit, index, id) => backend?.adoptSessionCommit?.(commit, index, id) ?? null,
        bindTarget: index => backend?.bindTarget?.(index) ?? null,
        prepareTarget: index => backend?.prepareTarget?.(index) ?? Promise.resolve(resolve() !== null),
        async refreshWindow(anchorIndex) {
            if (released) return false
            if (!backend?.refreshWindow) return resolve() !== null
            const current = await backend.refreshWindow(anchorIndex)
            if (current) attachHistoryWindow(chat, backend.absoluteStartIndex)
            return current
        },
        isCurrent: () => resolve() !== null,
        applyRange,
        reconcileMetadata() {
            const live = resolve() ? environment.readLiveMetadata() : null
            if (!live) return false
            const changed = mergeLiveMetadata(chat, baseline, live)
            const fields = chat as unknown as Record<string, unknown>
            const current = live as Record<string, unknown>
            if (changed.every((key) => isEqual(fields[key], current[key]))) {
                updateBaseline(baseline, chat, changed)
                if (changed.length) mutationVersion += 1
                return true
            }
            return applyRange(0, 0, [], 'update-metadata')
        },
        release() {
            if (released) return
            released = true
            backend?.release()
            backend = null
        },
    }
}
