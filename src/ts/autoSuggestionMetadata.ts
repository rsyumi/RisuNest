import {
    cloneConversationMetadata,
    type ActiveConversationSession,
} from './storage/activeConversationSession'
import type { Chat, Database } from './storage/database.svelte'

type ConversationOwner = Database['characters'][number]

export function readConversationSuggestions(
    owner: ConversationOwner | undefined,
): string[] | undefined {
    return owner?.chats[owner.chatPage]?.suggestMessages
}

export function writeConversationSuggestions(
    conversation: Chat,
    session: ActiveConversationSession | null,
    suggestions: readonly string[],
): void {
    if (!session) {
        conversation.suggestMessages = [...suggestions]
        return
    }
    const expectedMetadata = cloneConversationMetadata(conversation)
    session.applyOperation({
        expectedVersion: session.version,
        expectedMetadata,
        metadata: {
            ...expectedMetadata,
            suggestMessages: [...suggestions],
        },
    })
}
