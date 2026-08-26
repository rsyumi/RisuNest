import { safeStructuredClone } from '../polyfill'
import type { Chat, Message } from '../storage/database.svelte'
import type {
    ActiveConversationSession,
    MessageLocator,
} from '../storage/activeConversationSession'

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
    release(): void
}

function hasExactlyOneTarget(options: GenerationConversationOperationOptions): boolean {
    return Number(options.append !== undefined)
        + Number(options.continueLast === true)
        + Number(options.messageId !== undefined) === 1
}

export function captureGenerationConversationOperation(
    options: GenerationConversationOperationOptions,
): GenerationConversationOperation {
    if (!hasExactlyOneTarget(options)) {
        throw new TypeError('Generation operation requires exactly one target mode')
    }

    const session = options.session
    const usesSession = session !== null
        && session.isActive
        && options.getCurrentSession() === session
        && session.materializeCompatibilityArray() === options.chat.message

    return usesSession
        ? captureSessionOperation(session, options)
        : captureFullArrayFallback(options)
}

function captureSessionOperation(
    session: ActiveConversationSession,
    options: GenerationConversationOperationOptions,
): GenerationConversationOperation {
    const pin = session.acquirePin('transaction')
    let locator: MessageLocator
    try {
        if (options.append !== undefined) {
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
    const ownerIsCurrent = () => !released
        && (options.isOwnerCurrent?.() ?? true)
        && options.getCurrentSession() === session
        && options.getCurrentChat() === options.chat

    const operation: GenerationConversationOperation = {
        get absoluteIndex() {
            return locator.absoluteIndex
        },
        get messageId() {
            return currentMessageId
        },
        usesFullArrayFallback: false,
        isOwned() {
            return ownerIsCurrent() && session.ownsMessageLocator(locator)
        },
        snapshot() {
            return operation.isOwned() ? session.readMessage(locator) : null
        },
        commitData(data) {
            const current = operation.snapshot()
            return current === null
                ? false
                : operation.commitMessage({ ...current, data })
        },
        commitMessage(message) {
            if (!operation.isOwned()) return false
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
    const ownerIsCurrent = () => !released
        && (options.isOwnerCurrent?.() ?? true)
        && options.getCurrentChat() === chat
    const operation: GenerationConversationOperation = {
        get absoluteIndex() {
            return absoluteIndex
        },
        get messageId() {
            return currentMessageId
        },
        usesFullArrayFallback: true,
        isOwned() {
            return ownerIsCurrent() && chat.message[absoluteIndex] === target
        },
        snapshot() {
            return operation.isOwned() ? safeStructuredClone(target) : null
        },
        commitData(data) {
            const current = operation.snapshot()
            return current === null
                ? false
                : operation.commitMessage({ ...current, data })
        },
        commitMessage(message) {
            if (!operation.isOwned()) return false
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
