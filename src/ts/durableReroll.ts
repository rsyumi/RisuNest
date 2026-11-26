import { writable, get } from 'svelte/store'
export const activeRerollConversations = writable<string[]>([])
import type { ActiveConversationSession } from './storage/activeConversationSession'
import type { WindowedConversationMutationController } from './storage/activeWorkingSet.svelte'
import { cloneConversationMetadata } from './storage/selectedConversationLifecycle'
import type { Chat, Message } from './storage/database.svelte'
import { PersistentMutationFencedError } from './storage/saveCoordinator'
import { safeStructuredClone } from './safeStructuredClone'
import { toAbsoluteIndex } from './process/historyWindowIndex'
import {
    captureResponseVariants,
    projectResponseVariant,
    recoverRerollMessages,
    rerollStart,
    responseRange,
    snapshotResponse,
    type RerollRecovery,
} from './responseVariants'

/**
 * What a response tail is written through: the active session of a complete
 * conversation, the controller of a history window, or nothing for a plain chat.
 */
export type ResponseTailWriter = ActiveConversationSession | WindowedConversationMutationController | null

function isWindowWriter(writer: ResponseTailWriter): writer is WindowedConversationMutationController {
    return writer !== null && 'applyRange' in writer
}

export function replaceResponseTail(
    chat: Chat,
    session: ResponseTailWriter,
    start: number,
    messages: Message[],
    recovery: RerollRecovery | null | undefined = chat.rerollRecovery,
): void {
    if (isWindowWriter(session)) {
        const previous = chat.rerollRecovery
        if (recovery) chat.rerollRecovery = recovery
        else delete chat.rerollRecovery
        const deleteCount = chat.message.length - start
        const written = session.applyRange(
            start,
            deleteCount,
            messages,
            deleteCount === 0 && messages.length === 0 ? 'update-metadata' : 'replace-range',
        )
        if (!written) {
            if (previous) chat.rerollRecovery = previous
            else delete chat.rerollRecovery
            throw new PersistentMutationFencedError()
        }
        return
    }
    const expectedMetadata = cloneConversationMetadata(chat)
    const metadata = { ...expectedMetadata }
    if (recovery) metadata.rerollRecovery = recovery
    else delete metadata.rerollRecovery
    if (session?.isActive) {
        session.applyOperation({
            expectedVersion: session.version,
            expectedMetadata,
            metadata,
            range: {
                position: session.positionAt(start),
                deleteCount: chat.message.length - start,
                messages,
            },
        })
    } else {
        chat.message.splice(start, chat.message.length - start, ...safeStructuredClone(messages))
        if (recovery) chat.rerollRecovery = recovery
        else delete chat.rerollRecovery
    }
}

export function recoverInterruptedReroll(chat: Chat, session: ResponseTailWriter): boolean {
    if (!chat.rerollRecovery) return false
    const messages = recoverRerollMessages(chat)
    // Keep the unchanged prefix outside the mutation and copy budget.
    let start = 0
    while (start < chat.message.length && chat.message[start] === messages[start]) start++
    replaceResponseTail(chat, session, start, messages.slice(start), null)
    return true
}

export function moveResponseCandidate(
    chat: Chat,
    session: ResponseTailWriter,
    direction: -1 | 1,
    createId: () => string,
): boolean {
    if (chat.rerollRecovery) return false
    const range = responseRange(chat.message)
    if (!range) return false
    const variants = captureResponseVariants(chat.message.slice(range.start, range.end), createId)
    const index =
        variants.candidates.findIndex((candidate) => candidate.id === variants.selectedId) + direction
    if (index < 0 || index >= variants.candidates.length) return false
    replaceResponseTail(chat, session, range.start, [
        ...projectResponseVariant(variants, variants.candidates[index].id),
        ...chat.message.slice(range.end),
    ])
    return true
}

interface GenerateResponseCandidateOptions {
    chat: Chat
    currentChat?(): Chat | undefined | Promise<Chat | undefined>
    session(): ResponseTailWriter
    isCurrent(): boolean
    createId(): string
    flush(): Promise<void>
    generate(): Promise<boolean>
    aborted(): boolean
}

export async function generateResponseCandidate(options: GenerateResponseCandidateOptions): Promise<boolean> {
    const id = options.chat.id!
    if (get(activeRerollConversations).includes(id)) return false
    activeRerollConversations.update((values) => [...values, id])
    try {
        return await runResponseCandidate(options)
    } finally {
        activeRerollConversations.update((values) => values.filter((value) => value !== id))
    }
}

async function runResponseCandidate(options: GenerateResponseCandidateOptions): Promise<boolean> {
    let chat = options.chat
    if (chat.rerollRecovery || !options.isCurrent()) return false
    const range = responseRange(chat.message)
    if (!range) return false
    const variants = captureResponseVariants(chat.message.slice(range.start, range.end), options.createId)
    const original = [
        ...projectResponseVariant(variants, variants.selectedId),
        ...safeStructuredClone(chat.message.slice(range.end)),
    ]
    const recovery: RerollRecovery = {
        attemptId: options.createId(),
        phase: 'prepared',
        // Absolute, so recovery on the complete conversation finds the same position.
        startIndex: toAbsoluteIndex(chat, range.start),
        anchorId: chat.message[range.start - 1]?.chatId,
        original,
        responseCount: range.end - range.start,
        outputs: {},
    }
    replaceResponseTail(chat, options.session(), range.start, original, recovery)
    // A failed checkpoint leaves the original projection and candidates intact.
    await options.flush()
    if (!options.isCurrent()) return false
    chat = (await options.currentChat?.()) ?? chat
    const prepared = chat.rerollRecovery!
    const preparedStart = rerollStart(chat, prepared)
    if (JSON.stringify(chat.message.slice(preparedStart)) !== JSON.stringify(prepared.original)) return false
    replaceResponseTail(chat, options.session(), preparedStart, [], { ...prepared, phase: 'generating' })
    let success = false
    try {
        success = await options.generate()
        chat = (await options.currentChat?.()) ?? chat
        if (!success || options.aborted() || !options.isCurrent()) return false
        const active = chat.rerollRecovery
        if (active?.attemptId !== recovery.attemptId) return false
        const start = rerollStart(chat, active)
        if (start < 0) return false
        const followingIds = new Set(active.followingIds)
        const suffix = chat.message.slice(start)
        const generated = suffix.filter(message => !followingIds.has(message.chatId))
        const following = suffix.filter(message => followingIds.has(message.chatId))
        if (
            !generated.some(
                (message) =>
                    message.role === 'char' &&
                    !message.isComment &&
                    !message.disabled &&
                    message.data.length > 0,
            )
        )
            return false
        const generatedRange = responseRange(generated)
        if (!generatedRange) return false
        const generatedVariants = captureResponseVariants(
            generated.slice(generatedRange.start, generatedRange.end),
            options.createId,
        )
        for (const candidate of generatedVariants.candidates) {
            variants.candidates.push({
                id: options.createId(),
                messages: snapshotResponse([
                    ...generated.slice(0, generatedRange.start),
                    ...candidate.messages,
                    ...generated.slice(generatedRange.end),
                ]),
            })
        }
        variants.selectedId =
            variants.candidates[variants.candidates.length - generatedVariants.candidates.length].id
        const trailing = original.slice(recovery.responseCount)
        const projected = projectResponseVariant(variants, variants.selectedId)
        replaceResponseTail(
            chat,
            options.session(),
            start,
            [...projected, ...trailing, ...following],
            null,
        )
        try {
            await options.flush()
        } catch (error) {
            chat = (await options.currentChat?.()) ?? chat
            const carrier = chat.message.findIndex(message => message.responseVariants?.selectedId === variants.selectedId
                && message.responseVariants.groupId === variants.groupId)
            if (carrier >= projected.length - 1) {
                replaceResponseTail(chat, options.session(), carrier - projected.length + 1, [
                    ...original.slice(0, recovery.responseCount), ...chat.message.slice(carrier + 1),
                ], null)
            }
            throw error
        }
        return true
    } finally {
        chat = (await options.currentChat?.()) ?? chat
        if (chat.rerollRecovery?.attemptId === recovery.attemptId) {
            recoverInterruptedReroll(chat, options.session())
            await options.flush()
        }
    }
}

/** A window over the newest messages of the selected conversation. */
export interface ResponseTailWindow {
    readonly chat: Chat
    readonly controller: WindowedConversationMutationController
    release(): void
}

/** Opens the selected conversation from the absolute index `start` returns for its message count. */
export type OpenResponseTail = (
    start: (totalMessages: number) => number,
) => Promise<ResponseTailWindow | null>

const RESPONSE_TAIL_MESSAGES = 8

/**
 * Opens a tail window that holds the last response and the message before it.
 * When the newest shown message is not a response, the first window is returned
 * as it is, since no response exists to widen to.
 */
export async function openResponseTail(open: OpenResponseTail): Promise<ResponseTailWindow | null> {
    let count = RESPONSE_TAIL_MESSAGES
    for (;;) {
        const window = await open((total) => Math.max(total - count, 0))
        if (!window) return null
        const messages = window.chat.message
        const range = responseRange(messages)
        if (
            window.controller.absoluteStartIndex === 0
            || (range && range.start > 0)
            || (!range && messages.some((message) => !message.isComment && !message.disabled))
        ) return window
        count = Math.max(count, messages.length) * 2
        window.release()
    }
}

export interface WindowedResponseCandidateMove {
    moved: boolean
    /** The serialized last message when nothing moved but a response exists to generate a new candidate for. */
    lastMessage: string | null
}

export async function moveWindowedResponseCandidate(
    open: OpenResponseTail,
    direction: -1 | 1,
    createId: () => string,
    flush: () => Promise<void>,
): Promise<WindowedResponseCandidateMove | null> {
    const window = await openResponseTail(open)
    if (!window) return null
    try {
        if (moveResponseCandidate(window.chat, window.controller, direction, createId)) {
            await flush()
            return { moved: true, lastMessage: null }
        }
        return {
            moved: false,
            lastMessage: responseRange(window.chat.message)
                ? JSON.stringify(window.chat.message.at(-1))
                : null,
        }
    } finally {
        window.release()
    }
}

interface GenerateWindowedResponseCandidateOptions
    extends Omit<GenerateResponseCandidateOptions, 'chat' | 'currentChat' | 'session'> {
    open: OpenResponseTail
    /** The serialized last message the candidate replaces; a different one cancels it. */
    expectedLastMessage?: string
}

export interface WindowedResponseCandidateResult {
    completed: boolean
    lastMessage?: string
    /**
     * The generation succeeded but the tail could not be opened again to store
     * the candidate. The stored recovery restores the previous response.
     */
    reopenFailed?: boolean
}

/**
 * Generates a candidate over tail windows. The window is closed while the
 * generation writes the conversation and reopened over the recorded response
 * boundary afterwards. Returns null when no tail window opens.
 */
export async function generateWindowedResponseCandidate(
    options: GenerateWindowedResponseCandidateOptions,
): Promise<WindowedResponseCandidateResult | null> {
    let window = await openResponseTail(options.open)
    if (!window) return null
    const start = window.controller.absoluteStartIndex
    const windowLength = window.chat.message.length
    let generated = false
    let reopenFailed = false
    try {
        if (
            options.expectedLastMessage !== undefined &&
            JSON.stringify(window.chat.message.at(-1)) !== options.expectedLastMessage
        )
            return { completed: false }
        const completed = await generateResponseCandidate({
            ...options,
            chat: window.chat,
            currentChat: async () => {
                if (window) return window.chat
                // A failed reopen is not retried, so the recovery stays stored as it is.
                if (reopenFailed || !options.isCurrent()) return undefined
                window = await options.open(total => Math.max(0, Math.min(start, total - windowLength)))
                const boundary = window?.chat.rerollRecovery?.startIndex
                if (window && boundary !== undefined && window.controller.absoluteStartIndex > boundary) {
                    window.release()
                    window = await options.open(() => Math.max(0, boundary - 1))
                }
                if (!window) reopenFailed = true
                return window?.chat
            },
            session: () => window?.controller ?? null,
            generate: async () => {
                window?.release()
                window = null
                generated = await options.generate()
                return generated
            },
        })
        if (reopenFailed && generated) return { completed: false, reopenFailed: true }
        return { completed, lastMessage: window?.chat.message.at(-1)?.data }
    } finally {
        window?.release()
    }
}
