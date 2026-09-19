import { trackRerollOutput } from '../responseVariants'
import { safeStructuredClone } from '../polyfill'
import type { ConversationOperationCommit } from './conversationOperationContext'
import type { Chat, Message } from '../storage/database.svelte'
import type {
    ActiveConversationSession,
    MessageLocator,
} from '../storage/activeConversationSession'
import type { WindowedConversationMutationController } from '../storage/activeWorkingSet.svelte'

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
    release(): void
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

function captureWindowedOperation(
    controller: WindowedConversationMutationController,
    options: GenerationConversationOperationOptions,
): GenerationConversationOperation {
    const chat = controller.chat
    let localIndex: number
    if (options.append !== undefined) {
        localIndex = chat.message.length
        trackRerollOutput(chat, options.append)
        if (!controller.applyRange(localIndex, 0, [options.append], 'append')) {
            throw new RangeError('Windowed generation operation owner is no longer current')
        }
    } else if (options.continueLast) {
        localIndex = chat.message.length - 1
        if (!chat.message[localIndex]) throw new RangeError('Cannot continue an empty conversation')
    } else {
        localIndex = chat.message.findIndex((message) => message.chatId === options.messageId)
        if (localIndex < 0) {
            throw new RangeError(`Generation message ${options.messageId} was not found`)
        }
    }
    let released = false
    let currentMessageId = chat.message[localIndex]?.chatId
    const operation: GenerationConversationOperation = {
        get absoluteIndex() { return controller.absoluteStartIndex + localIndex },
        get messageId() { return currentMessageId },
        usesFullArrayFallback: false,
        isOwned() {
            return !released
                && controller.isCurrent()
                && options.isOwnerCurrent?.() !== false
                && chat.message[localIndex]?.chatId === currentMessageId
        },
        snapshot() {
            return operation.isOwned() ? safeStructuredClone(chat.message[localIndex]) : null
        },
        commitData(data) {
            const current = operation.snapshot()
            return current !== null && operation.commitMessage({ ...current, data })
        },
        commitMessage(message) {
            if (!operation.isOwned()) return false
            trackRerollOutput(chat, message)
            if (!controller.applyRange(localIndex, 1, [message], 'edit')) return false
            currentMessageId = chat.message[localIndex]?.chatId
            return true
        },
        refresh() {
            if (!controller.isCurrent() || currentMessageId === undefined) return false
            const nextIndex = chat.message.findIndex((message) => message.chatId === currentMessageId)
            if (nextIndex < 0) return false
            localIndex = nextIndex
            return true
        },
        acceptCommit: () => operation.isOwned(),
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
            trackRerollOutput(options.chat, options.append)
            locator = session.append(options.append)
        } else if (options.continueLast) {
            if (session.totalMessages === 0) {
                throw new RangeError('Cannot continue an empty conversation')
            }
            locator = session.locate(session.totalMessages - 1)
        } else {
            const found = session.findMessageLocatorById(options.messageId!)
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
            trackRerollOutput(options.chat, message)
            locator = session.edit(locator, message)
            currentMessageId = message.chatId
            return true
        },
        refresh() {
            if (!ownerIsCurrent() || currentMessageId === undefined) return false
            const refreshed = session.findMessageLocatorById(currentMessageId)
            if (refreshed === null) return false
            locator = refreshed
            return true
        },
        acceptCommit(commit) {
            return (
                commit.follows(session, locator.sessionVersion, options.chat) && operation.refresh()
            )
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
        trackRerollOutput(chat, target)
        chat.message.push(target)
        target = chat.message[absoluteIndex]
        options.onFallbackMutation?.()
    } else if (options.continueLast) {
        absoluteIndex = chat.message.length - 1
        target = chat.message[absoluteIndex]
        if (!target) throw new RangeError('Cannot continue an empty conversation')
    } else {
        absoluteIndex = chat.message.findIndex((message) => message.chatId === options.messageId)
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
            trackRerollOutput(chat, message)
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
    const sessionVersion = session?.version
    const messageCount = session?.totalMessages
    let released = false
    const ownerIsCurrent = () => {
        if (
            released
            || options.isOwnerCurrent?.() === false
            || options.getCurrentChat() !== options.chat
        ) return false
        return session === null || (
            session.isActive
            && options.getCurrentSession() === session
            && session.version === sessionVersion
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
        release() {
            if (released) return
            released = true
            pin?.release()
        },
    }
}
