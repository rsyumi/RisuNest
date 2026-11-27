import { trackRerollOutput } from '../responseVariants'
import { safeStructuredClone } from '../safeStructuredClone'
import type { ConversationOperationCommit } from './conversationOperationContext'
import type { Chat, Message } from '../storage/database.svelte'
import {
    cloneConversationMetadata,
    type ActiveConversationSession,
    type MessageLocator,
} from '../storage/activeConversationSession'
import type { WindowedConversationMutationController } from '../storage/activeWorkingSet.svelte'

type ConversationMetadataValues = Partial<Omit<Chat, 'message'>>

interface GenerationConversationOperationOptions {
    session: ActiveConversationSession | null
    getCurrentSession(): ActiveConversationSession | null
    chat: Chat
    getCurrentChat(): Chat | null | undefined
    isOwnerCurrent?(): boolean
    append?: Message
    continueLast?: boolean
    messageId?: string
    onFallbackMutation?(): void
    windowedController?: WindowedConversationMutationController | null
}

export interface GenerationConversationOperation {
    readonly absoluteIndex: number
    readonly messageId: string | undefined
    readonly usesFullArrayFallback: boolean
    isOwned(): boolean
    snapshot(): Message | null
    commitData(data: string): boolean
    commitMessage(message: Message): boolean
    refresh(): boolean
    acceptCommit(commit: ConversationOperationCommit): boolean
    /**
     * Sets metadata fields of the operation's conversation through the session or window that
     * records it, so a save covers them, even after the operation lost its output. A target the
     * operation still owns stays owned.
     */
    commitMetadata(values: ConversationMetadataValues): void
    release(): void
}

function changedMetadata(chat: Chat, values: ConversationMetadataValues): [string, unknown][] {
    const record = chat as unknown as Record<string, unknown>
    return Object.entries(values).filter(([key, value]) => !Object.is(record[key], value))
}

function writeMetadata(chat: Chat, changes: readonly [string, unknown][]): void {
    const record = chat as unknown as Record<string, unknown>
    for (const [key, value] of changes) record[key] = value
}

/** Returns whether the current session recorded the change; a chat it does not own is written directly. */
function commitSessionMetadata(
    options: Pick<GenerationConversationOperationOptions, 'chat' | 'getCurrentSession'>,
    values: ConversationMetadataValues,
): boolean {
    const changes = changedMetadata(options.chat, values)
    if (changes.length === 0) return false
    const session = options.getCurrentSession()
    if (!session?.matchesConversation(session.characterId, options.chat)) {
        writeMetadata(options.chat, changes)
        return false
    }
    const expectedMetadata = cloneConversationMetadata(options.chat)
    try {
        session.applyOperation({
            expectedVersion: session.version,
            expectedMetadata,
            metadata: { ...expectedMetadata, ...Object.fromEntries(changes) },
        })
    } catch (error) {
        // The chat shows the new values even when the session cannot record them.
        writeMetadata(options.chat, changes)
        throw error
    }
    return true
}

const windowedControllers = new WeakMap<Chat, WindowedConversationMutationController>()

export function bindWindowedGenerationController(
    chat: Chat,
    controller: WindowedConversationMutationController,
): () => void {
    windowedControllers.set(chat, controller)
    return () => {
        if (windowedControllers.get(chat) === controller) windowedControllers.delete(chat)
    }
}

function hasExactlyOneTarget(options: GenerationConversationOperationOptions): boolean {
    return (
        Number(options.append !== undefined) +
            Number(options.continueLast === true) +
            Number(options.messageId !== undefined) ===
        1
    )
}

function canUseActiveSession(options: GenerationConversationOperationOptions): boolean {
    const session = options.session
    return session !== null
        && session.isActive
        && options.getCurrentSession() === session
        && session.materializeCompatibilityArray() === options.chat.message
}

export function captureGenerationConversationOperation(
    options: GenerationConversationOperationOptions,
): GenerationConversationOperation {
    if (!hasExactlyOneTarget(options)) {
        throw new TypeError('Generation operation requires exactly one target mode')
    }
    if (options.append !== undefined && options.isOwnerCurrent?.() === false) {
        throw new RangeError('Generation operation owner is no longer current')
    }

    const session = options.session
    const usesSession = canUseActiveSession(options)

    const windowedController = options.windowedController ?? windowedControllers.get(options.chat)
    return windowedController
        ? captureWindowedOperation(windowedController, options)
        : usesSession
        ? captureSessionOperation(session!, options)
        : captureFullArrayFallback(options)
}

function uniqueMessageIndex(messages: readonly Message[], id: string | undefined): number {
    if (!id) return -1
    const first = messages.findIndex(message => message.chatId === id)
    return first >= 0 && messages.findLastIndex(message => message.chatId === id) === first ? first : -1
}

function captureWindowedOperation(
    controller: WindowedConversationMutationController,
    options: GenerationConversationOperationOptions,
): GenerationConversationOperation {
    const chat = controller.chat
    if (!controller.isCurrent()) throw new RangeError('Windowed generation operation owner is no longer current')
    let localIndex: number
    if (options.append !== undefined) {
        localIndex = chat.message.length
        trackRerollOutput(chat, options.append, controller.absoluteStartIndex + localIndex)
        if (!controller.applyRange(localIndex, 0, [options.append], 'append')) {
            throw new RangeError('Windowed generation operation owner is no longer current')
        }
    } else if (options.continueLast) {
        localIndex = chat.message.length - 1
        if (!chat.message[localIndex]) throw new RangeError('Cannot continue an empty conversation')
    } else {
        localIndex = uniqueMessageIndex(chat.message, options.messageId)
        if (localIndex < 0) {
            throw new RangeError(`Generation message ${options.messageId} was not found`)
        }
    }
    let released = false
    let currentMessageId = chat.message[localIndex]?.chatId
    let target = chat.message[localIndex]
    let stable = controller.bindTarget?.(localIndex) ?? null
    const operation: GenerationConversationOperation = {
        get absoluteIndex() { return stable ? stable.absoluteIndex ?? -1 : controller.absoluteStartIndex + localIndex },
        get messageId() { return currentMessageId },
        usesFullArrayFallback: false,
        isOwned() {
            if (stable) {
                const index = stable.absoluteIndex
                if (released || options.isOwnerCurrent?.() === false || index === null) return false
                if (controller.isCurrent()) {
                    localIndex = index - controller.absoluteStartIndex
                    target = chat.message[localIndex]
                }
                return true
            }
            return !released
                && controller.isCurrent()
                && options.isOwnerCurrent?.() !== false
                && chat.message[localIndex]?.chatId === currentMessageId
        },
        snapshot() {
            return operation.isOwned() ? safeStructuredClone(stable ? stable.read() : chat.message[localIndex]) : null
        },
        commitData(data) {
            const current = operation.snapshot()
            return current !== null && operation.commitMessage({ ...current, data })
        },
        commitMessage(message) {
            if (!operation.isOwned()) return false
            trackRerollOutput(chat, message, operation.absoluteIndex)
            if (stable) {
                if (!stable.replace(message)) return false
                currentMessageId = message.chatId
                localIndex = stable.absoluteIndex! - controller.absoluteStartIndex
                target = chat.message[localIndex]
                return true
            }
            if (!controller.applyRange(localIndex, 1, [message], 'edit')) return false
            currentMessageId = chat.message[localIndex]?.chatId
            target = chat.message[localIndex]
            return true
        },
        refresh() {
            if (stable) return operation.isOwned()
            if (!controller.isCurrent() || currentMessageId === undefined) return false
            const nextIndex = uniqueMessageIndex(chat.message, currentMessageId)
            if (nextIndex < 0) return false
            localIndex = nextIndex
            target = chat.message[localIndex]
            return true
        },
        acceptCommit(commit) {
            if (released || options.isOwnerCurrent?.() === false) return false
            const nextIndex = commit.remapWindowTarget?.(controller, target)
            if (nextIndex === undefined) {
                if (!stable && currentMessageId && controller.adoptSessionCommit) {
                    const adopted = controller.adoptSessionCommit(commit, localIndex, currentMessageId)
                    if (adopted !== null) {
                        localIndex = adopted
                        target = chat.message[adopted]
                    }
                }
                return operation.isOwned()
            }
            if (nextIndex === null) {
                released = true
                return false
            }
            if (stable && stable.absoluteIndex !== controller.absoluteStartIndex + nextIndex) {
                stable = controller.bindTarget?.(nextIndex) ?? null
            }
            localIndex = nextIndex
            target = chat.message[localIndex]
            return operation.isOwned()
        },
        commitMetadata(values) {
            const changes = changedMetadata(chat, values)
            if (changes.length === 0) return
            writeMetadata(chat, changes)
            // The window copies its chat's metadata to the selected conversation only in a recorded range.
            if (controller.isOwnerCurrent?.() ?? controller.isCurrent()) controller.applyRange(0, 0, [], 'update-metadata')
        },
        release() { released = true },
    }
    return operation
}

export function captureGenerationTailFallbackOperation(
    options: Omit<
        GenerationConversationOperationOptions,
        'append' | 'continueLast' | 'messageId'
    >,
): GenerationConversationOperation {
    try {
        return captureGenerationConversationOperation({ ...options, continueLast: true })
    } catch (error) {
        if (!(error instanceof RangeError)) throw error
        return captureEmptyTailFallbackOperation(options)
    }
}

export function recaptureGenerationConversationOperation(
    options: Omit<
        GenerationConversationOperationOptions,
        'append' | 'continueLast' | 'messageId'
    > & { messageId: string },
): GenerationConversationOperation | null {
    try {
        return captureGenerationConversationOperation(options)
    } catch (error) {
        if (error instanceof RangeError) return null
        throw error
    }
}

function captureSessionOperation(
    session: ActiveConversationSession,
    options: GenerationConversationOperationOptions,
): GenerationConversationOperation {
    const pin = session.acquirePin('transaction')
    let locator: MessageLocator
    try {
        if (options.append !== undefined) {
            trackRerollOutput(options.chat, options.append, session.totalMessages)
            locator = session.append(options.append)
        } else if (options.continueLast) {
            if (session.totalMessages === 0) {
                throw new RangeError('Cannot continue an empty conversation')
            }
            locator = session.locate(session.totalMessages - 1)
        } else {
            const first = session.findMessageTargetsByIds([options.messageId!], 'first')[0]
            const last = session.findMessageTargetsByIds([options.messageId!], 'last')[0]
            const found = first && last && first.absoluteIndex === last.absoluteIndex ? first.locator : null
            if (found === null) {
                throw new RangeError(`Generation message ${options.messageId} was not found`)
            }
            locator = found
        }
    } catch (error) {
        pin.release()
        throw error
    }

    let released = false
    let currentMessageId = session.readMessage(locator).chatId
    const ownerIsCurrent = () =>
        !released &&
        (options.isOwnerCurrent?.() ?? true) &&
        options.getCurrentSession() === session &&
        options.getCurrentChat() === options.chat

    const operation: GenerationConversationOperation = {
        get absoluteIndex() {
            return locator.absoluteIndex
        },
        get messageId() {
            return currentMessageId
        },
        usesFullArrayFallback: false,
        isOwned() {
            if (
                ownerIsCurrent() &&
                session.canContinueGenerationFrom(locator.sessionVersion) &&
                locator.sessionVersion !== session.version
            ) {
                // Metadata publication preserves positions and content but invalidates edit locators.
                locator = session.locate(locator.absoluteIndex)
            }
            return ownerIsCurrent() && session.ownsMessageLocator(locator)
        },
        snapshot() {
            return operation.isOwned() ? session.readMessage(locator) : null
        },
        commitData(data) {
            const current = operation.snapshot()
            return current === null ? false : operation.commitMessage({ ...current, data })
        },
        commitMessage(message) {
            if (!operation.isOwned()) return false
            trackRerollOutput(options.chat, message, operation.absoluteIndex)
            locator = session.edit(locator, message)
            currentMessageId = message.chatId
            return true
        },
        refresh() {
            if (!ownerIsCurrent() || currentMessageId === undefined) return false
            const first = session.findMessageTargetsByIds([currentMessageId], 'first')[0]
            const last = session.findMessageTargetsByIds([currentMessageId], 'last')[0]
            if (!first || !last || first.absoluteIndex !== last.absoluteIndex) return false
            locator = first.locator
            return true
        },
        acceptCommit(commit) {
            if (!commit.follows(session, locator.sessionVersion, options.chat)) return false
            if (commit.mapSessionIndex) {
                const index = commit.mapSessionIndex(locator.absoluteIndex)
                if (index === null || !ownerIsCurrent()) return false
                const next = session.locate(index)
                if (session.readMessage(next).chatId !== currentMessageId) return false
                locator = next
                return true
            }
            return operation.refresh()
        },
        commitMetadata(values) {
            const owned = operation.isOwned()
            // This commit moves no message, so the owned target keeps its position.
            if (commitSessionMetadata(options, values) && owned) {
                locator = session.locate(locator.absoluteIndex)
            }
        },
        release() {
            if (released) return
            released = true
            pin.release()
        },
    }
    return operation
}

function captureFullArrayFallback(
    options: GenerationConversationOperationOptions,
): GenerationConversationOperation {
    const chat = options.chat
    let absoluteIndex: number
    let target: Message
    if (options.append !== undefined) {
        target = safeStructuredClone(options.append)
        absoluteIndex = chat.message.length
        trackRerollOutput(chat, target, absoluteIndex)
        chat.message.push(target)
        target = chat.message[absoluteIndex]
        options.onFallbackMutation?.()
    } else if (options.continueLast) {
        absoluteIndex = chat.message.length - 1
        target = chat.message[absoluteIndex]
        if (!target) throw new RangeError('Cannot continue an empty conversation')
    } else {
        absoluteIndex = uniqueMessageIndex(chat.message, options.messageId)
        target = chat.message[absoluteIndex]
        if (!target) throw new RangeError(`Generation message ${options.messageId} was not found`)
    }

    let released = false
    let currentMessageId = target.chatId
    const ownerIsCurrent = () =>
        !released && (options.isOwnerCurrent?.() ?? true) && options.getCurrentChat() === chat
    const operation: GenerationConversationOperation = {
        get absoluteIndex() {
            return absoluteIndex
        },
        get messageId() {
            return currentMessageId
        },
        usesFullArrayFallback: true,
        acceptCommit: () => false,
        isOwned() {
            return ownerIsCurrent() && chat.message[absoluteIndex] === target
        },
        snapshot() {
            return operation.isOwned() ? safeStructuredClone(target) : null
        },
        commitData(data) {
            const current = operation.snapshot()
            return current === null ? false : operation.commitMessage({ ...current, data })
        },
        commitMessage(message) {
            if (!operation.isOwned()) return false
            trackRerollOutput(chat, message, operation.absoluteIndex)
            chat.message[absoluteIndex] = safeStructuredClone(message)
            target = chat.message[absoluteIndex]
            currentMessageId = target.chatId
            options.onFallbackMutation?.()
            return true
        },
        refresh() {
            if (!ownerIsCurrent() || currentMessageId === undefined) return false
            const refreshedIndex = chat.message.findIndex(
                (message) => message.chatId === currentMessageId,
            )
            if (refreshedIndex === -1) return false
            absoluteIndex = refreshedIndex
            target = chat.message[refreshedIndex]
            return true
        },
        commitMetadata(values) {
            commitSessionMetadata(options, values)
        },
        release() {
            released = true
        },
    }
    return operation
}

function captureEmptyTailFallbackOperation(
    options: Omit<
        GenerationConversationOperationOptions,
        'append' | 'continueLast' | 'messageId'
    >,
): GenerationConversationOperation {
    const usesSession = canUseActiveSession(options)
    const session = usesSession ? options.session! : null
    const pin = session?.acquirePin('transaction')
    let sessionVersion = session?.version
    const messageCount = session?.totalMessages
    let released = false
    const ownerIsCurrent = () => {
        if (
            released
            || options.isOwnerCurrent?.() === false
            || options.getCurrentChat() !== options.chat
        ) return false
        return session === null || (
            options.getCurrentSession() === session
            && session.isContentUnchangedSince(sessionVersion!)
            && session.totalMessages === messageCount
            && session.materializeCompatibilityArray() === options.chat.message
        )
    }

    return {
        absoluteIndex: -1,
        messageId: undefined,
        usesFullArrayFallback: !usesSession,
        isOwned: ownerIsCurrent,
        snapshot: () => null,
        commitData: () => false,
        commitMessage: () => false,
        refresh: ownerIsCurrent,
        acceptCommit: () => false,
        commitMetadata(values) {
            const owned = ownerIsCurrent()
            if (commitSessionMetadata(options, values) && owned && session) {
                sessionVersion = session.version
            }
        },
        release() {
            if (released) return
            released = true
            pin?.release()
        },
    }
}
