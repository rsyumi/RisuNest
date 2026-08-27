import {
    cloneConversationMetadata,
    type ActiveConversationSession,
} from './storage/activeConversationSession'
import type { Chat, Database } from './storage/database.svelte'

type ConversationOwner = Database['characters'][number]

export interface AutoSuggestionRequestIdentity {
    readonly characterIndex: number
    readonly chatPage: number
    readonly navigationGeneration: number
}

export function isAutoSuggestionRequestIdentityCurrent(
    request: AutoSuggestionRequestIdentity,
    current: AutoSuggestionRequestIdentity,
): boolean {
    return request.characterIndex === current.characterIndex
        && request.chatPage === current.chatPage
        && request.navigationGeneration === current.navigationGeneration
}

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
