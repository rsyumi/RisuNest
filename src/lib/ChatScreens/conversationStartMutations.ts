import {
    cloneConversationMetadata,
    type ActiveConversationSession,
} from 'src/ts/storage/activeConversationSession'
import type { Chat } from 'src/ts/storage/database.svelte'

export function moveAlternateGreeting(
    conversation: Chat,
    session: ActiveConversationSession | null,
    alternateGreetingCount: number,
    direction: -1 | 1,
): void {
    const currentIndex = Number.isInteger(conversation.fmIndex)
        ? conversation.fmIndex!
        : -1
    const fmIndex = direction === 1
        ? currentIndex >= alternateGreetingCount - 1
            ? -1
            : currentIndex + 1
        : currentIndex === -1
          ? alternateGreetingCount - 1
          : currentIndex - 1

    if (!session) {
        conversation.fmIndex = fmIndex
        return
    }
    const expectedMetadata = cloneConversationMetadata(conversation)
    session.applyOperation({
        expectedVersion: session.version,
        expectedMetadata,
        metadata: {
            ...expectedMetadata,
            fmIndex,
        },
    })
}
