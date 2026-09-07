import type { PluginCompatibilityProfile } from './pluginCompatibility'
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

export type ChatOutputListenerProvenance = 'v2.1-live' | 'v3-legacy'

export interface ChatOutputDispatchInput {
    listeners: Set<ChatOutputListener>
    provenance: WeakMap<ChatOutputListener, ChatOutputListenerProvenance>
    profile: PluginCompatibilityProfile
    char: PluginCompleteCharacter
    chat: PluginCompleteCharacter['chats'][number]
    characterIndex: number
    chatIndex: number
    messageIndex: number
    snapshot<T>(value: T): T
    projectScalable: PluginChatOutputProjector
    onError(error: unknown): void
    signal?: AbortSignal
}

export function registerChatOutputListener(
    listeners: Set<ChatOutputListener>,
    provenance: WeakMap<ChatOutputListener, ChatOutputListenerProvenance>,
    listener: ChatOutputListener,
    source: ChatOutputListenerProvenance,
): void {
    listeners.add(listener)
    provenance.set(listener, source)
}

export function removeChatOutputListener(
    listeners: Set<ChatOutputListener>,
    provenance: WeakMap<ChatOutputListener, ChatOutputListenerProvenance>,
    listener: ChatOutputListener,
): void {
    listeners.delete(listener)
    provenance.delete(listener)
}

export async function dispatchChatOutputListeners(
    input: ChatOutputDispatchInput,
): Promise<void> {
    if (input.signal?.aborted) return
    if (input.listeners.size === 0) return
    const captured = [...input.listeners]
    const needsProjection = input.profile === 'scalable-v3' && captured.some(
        (listener) => input.provenance.get(listener) === 'v3-legacy',
    )
    let event: {
        char: PluginCompleteCharacter
        chat: PluginCompleteCharacter['chats'][number]
    }
    if (needsProjection) {
        try {
            event = await input.projectScalable({
                characterId: input.char.chaId,
                conversationId: input.chat.id!,
                liveCharacter: input.char,
                liveConversation: input.chat,
            })
        } catch (error) {
            if (input.signal?.aborted) return
            // All listeners of one output event share a single consistent
            // event object, so a projection failure skips the whole event,
            // including live-profile listeners that would not have needed
            // the projection themselves.
            input.onError(error)
            return
        }
    } else {
        event = {
            char: input.snapshot(input.char),
            chat: input.snapshot(input.chat),
        }
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
