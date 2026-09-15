import type {
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

export interface ChatOutputDispatchInput {
    listeners: Set<ChatOutputListener>
    char: PluginCompleteCharacter
    chat: PluginCompleteCharacter['chats'][number]
    characterIndex: number
    chatIndex: number
    messageIndex: number
    projectScalable: PluginChatOutputProjector
    onError(error: unknown): void
    signal?: AbortSignal
}

export function registerChatOutputListener(
    listeners: Set<ChatOutputListener>,
    listener: ChatOutputListener,
): void {
    listeners.add(listener)
}

export function removeChatOutputListener(
    listeners: Set<ChatOutputListener>,
    listener: ChatOutputListener,
): void {
    listeners.delete(listener)
}

export async function dispatchChatOutputListeners(
    input: ChatOutputDispatchInput,
): Promise<void> {
    if (input.signal?.aborted) return
    if (input.listeners.size === 0) return
    const captured = [...input.listeners]
    let event: {
        char: PluginCompleteCharacter
        chat: PluginCompleteCharacter['chats'][number]
    }
    try {
        event = await input.projectScalable({
            characterId: input.char.chaId,
            conversationId: input.chat.id!,
            liveCharacter: input.char,
            liveConversation: input.chat,
        })
    } catch (error) {
        if (input.signal?.aborted) return
        // All listeners of one output event share a single consistent event
        // object, so a projection failure skips the whole event.
        input.onError(error)
        return
    }

    for (const listener of captured) {
        if (input.signal?.aborted) return
        if (!input.listeners.has(listener)) continue
        try {
            await listener({
                char: event.char,
                chat: event.chat,
                characterIndex: input.characterIndex,
                chatIndex: input.chatIndex,
                messageIndex: input.messageIndex,
            })
        } catch (error) {
            input.onError(error)
        }
    }
}
