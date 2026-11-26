import type { PinnedConversationPosition } from './pinnedConversationPosition'
import type {
    PluginChatOutputHistory,
    PluginChatOutputProjection,
    PluginChatOutputProjector,
    PluginCompleteCharacter,
} from './pluginDatabaseAccess'

export type ChatOutputListenerArg = {
    char: any
    chat: any
    characterIndex: number
    chatIndex: number
    messageIndex: number
}

export type ChatOutputListener = (
    arg: ChatOutputListenerArg,
) => void | Promise<void>

export interface ChatOutputListenerOptions {
    history?: unknown
}

/** Counts only; the event contents never reach diagnostics. */
export interface ChatOutputPayloadDiagnostic {
    listeners: number
    fullHistoryListeners: number
    conversations?: number
    messages?: number
    messageCharacters?: number
}

export interface ChatOutputDispatchInput {
    listeners: Set<ChatOutputListener>
    char: PluginCompleteCharacter
    chat: PluginCompleteCharacter['chats'][number]
    resolvePosition(characterId: string, conversationId: string): Promise<PinnedConversationPosition>
    /** Absolute index of the output message in the whole conversation. */
    messageIndex: number
    /** Absolute index of `chat.message[0]`; earlier messages are read from the store. */
    windowStart?: number
    /** Whether listeners may receive the character's other conversations. */
    fullHistory?: boolean
    projectScalable: PluginChatOutputProjector
    onPayload?(payload: ChatOutputPayloadDiagnostic): void
    onError(error: unknown): void
    signal?: AbortSignal
}

// Listeners that asked for the current conversation only. A listener cannot
// ask for more than the device setting allows.
const currentOnlyListeners = new WeakSet<ChatOutputListener>()

export function registerChatOutputListener(
    listeners: Set<ChatOutputListener>,
    listener: ChatOutputListener,
    options?: ChatOutputListenerOptions,
): void {
    listeners.add(listener)
    if (options?.history === 'current') currentOnlyListeners.add(listener)
    else currentOnlyListeners.delete(listener)
}

export function removeChatOutputListener(
    listeners: Set<ChatOutputListener>,
    listener: ChatOutputListener,
): void {
    listeners.delete(listener)
}

/** The same event without other conversations' messages. */
export function reduceChatOutputProjection(
    projection: PluginChatOutputProjection,
): PluginChatOutputProjection {
    const { chats, ...fields } = projection.char
    return {
        char: {
            ...fields,
            chats: chats.map(({ message: _message, ...metadata }) => metadata),
        } as PluginCompleteCharacter,
        chat: projection.chat,
    }
}

function payloadCounts(projection: PluginChatOutputProjection) {
    let messages = 0
    let messageCharacters = 0
    const count = (chat: { message?: { data?: unknown }[] }) => {
        if (!Array.isArray(chat.message)) return
        messages += chat.message.length
        for (const message of chat.message) {
            if (typeof message?.data === 'string') messageCharacters += message.data.length
        }
    }
    for (const chat of projection.char.chats) count(chat)
    count(projection.chat)
    return { conversations: projection.char.chats.length, messages, messageCharacters }
}

export async function dispatchChatOutputListeners(
    input: ChatOutputDispatchInput,
): Promise<void> {
    if (input.signal?.aborted) return
    if (input.listeners.size === 0) return
    const fullHistory = input.fullHistory ?? true
    const captured = [...input.listeners].map((listener) => ({
        listener,
        history: (fullHistory && !currentOnlyListeners.has(listener) ? 'full' : 'current') as PluginChatOutputHistory,
    }))
    const fullHistoryListeners = captured.filter(({ history }) => history === 'full').length
    const characterId = input.char.chaId
    const conversationId = input.chat.id!
    const messageIndex = input.messageIndex
    let position: PinnedConversationPosition
    let projected: PluginChatOutputProjection
    try {
        input.onPayload?.({ listeners: captured.length, fullHistoryListeners })
        // Resolving the position saves pending data first, so the output is
        // stored before the event is built.
        position = await input.resolvePosition(characterId, conversationId)
        if (input.signal?.aborted) return
        projected = await input.projectScalable({
            characterId,
            conversationId,
            liveCharacter: input.char,
            liveConversation: input.chat,
            windowStart: input.windowStart ?? 0,
            history: fullHistoryListeners > 0 ? 'full' : 'current',
        })
        if (input.signal?.aborted) return
        input.onPayload?.({ listeners: captured.length, fullHistoryListeners, ...payloadCounts(projected) })
    } catch (error) {
        if (input.signal?.aborted) return
        // All listeners of one output event share a single consistent event
        // object, so a projection failure skips the whole event.
        input.onError(error)
        return
    }
    const events: Record<PluginChatOutputHistory, PluginChatOutputProjection | undefined> = {
        full: fullHistoryListeners > 0 ? projected : undefined,
        current: fullHistoryListeners === captured.length
            ? undefined
            : fullHistoryListeners > 0 ? reduceChatOutputProjection(projected) : projected,
    }

    for (const { listener, history } of captured) {
        if (input.signal?.aborted) return
        if (!input.listeners.has(listener)) continue
        const event = events[history]!
        try {
            await listener({
                char: event.char,
                chat: event.chat,
                ...position,
                messageIndex: projected.messageIndex ?? messageIndex,
            })
        } catch (error) {
            input.onError(error)
        }
    }
}
