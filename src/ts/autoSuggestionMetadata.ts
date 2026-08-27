import type { Database } from './storage/database.svelte'

type ConversationOwner = Database['characters'][number]

export function readConversationSuggestions(
    owner: ConversationOwner | undefined,
): string[] | undefined {
    return owner?.chats[owner.chatPage]?.suggestMessages
}

export function writeConversationSuggestions(
    owner: ConversationOwner | undefined,
    chatPage: number,
    suggestions: string[],
): void {
    const conversation = owner?.chats[chatPage]
    if (conversation) conversation.suggestMessages = suggestions
}
