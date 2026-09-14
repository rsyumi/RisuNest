import { describe, expect, it, vi } from 'vitest'
import type { ActiveConversationSession } from 'src/ts/storage/activeConversationSession'
import type { Chat } from 'src/ts/storage/database.svelte'
import { moveAlternateGreeting } from './conversationStartMutations'

describe('conversation start mutations', () => {
    it('treats a missing first-message index as the original greeting', () => {
        const conversation = { id: 'chat-a', message: [] } as unknown as Chat

        moveAlternateGreeting(conversation, null, 2, 1)
        expect(conversation.fmIndex).toBe(0)
        moveAlternateGreeting(conversation, null, 2, -1)
        expect(conversation.fmIndex).toBe(-1)
    })

    it('records first-message selection through the active session', () => {
        const conversation = {
            id: 'chat-a',
            fmIndex: 0,
            message: [],
        } as unknown as Chat
        const applyOperation = vi.fn()
        const session = {
            version: 7,
            applyOperation,
        } as unknown as ActiveConversationSession

        moveAlternateGreeting(conversation, session, 2, 1)

        expect(applyOperation).toHaveBeenCalledWith({
            expectedVersion: 7,
            expectedMetadata: { id: 'chat-a', fmIndex: 0 },
            metadata: { id: 'chat-a', fmIndex: 1 },
        })
    })
})
