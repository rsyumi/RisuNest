import { describe, expect, it } from 'vitest'

import type { Chat, Message } from './database.svelte'
import { ActiveConversationSession, ConversationSessionStaleError } from './activeConversationSession'
import {
    ConversationHistoryOperationDisposedError,
    beginPinnedConversationHistoryOperation,
    createCompatibilityConversationHistorySnapshot,
} from './conversationHistoryOperation'

function message(data: string, role: Message['role'] = 'user'): Message {
    return { role, data, chatId: `message-${data}` }
}

function chat(messages: Message[]): Chat {
    return {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: messages,
    }
}

describe('ConversationHistoryOperation', () => {
    it('pins one active-session version for bounded reads and releases it once', () => {
        const conversation = chat([
            message('zero'),
            message('one', 'char'),
            message('two'),
        ])
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 17,
        })

        const operation = beginPinnedConversationHistoryOperation(session)

        expect(operation).toMatchObject({
            source: 'active-session',
            characterId: 'character-a',
            conversationId: 'conversation-a',
            storeRevision: 17,
            sessionVersion: 0,
            totalMessages: 3,
        })
        expect(session.pinCount('prompt')).toBe(1)
        expect(operation.readLatest(2).messages).toEqual([
            message('one', 'char'),
            message('two'),
        ])
        expect(operation.readRange(0, 1).messages).toEqual([message('zero')])
        expect(operation.scanBackward(3, 2).entries.map((entry) => entry.message.data))
            .toEqual(['two', 'one'])

        operation.dispose()
        operation.dispose()

        expect(session.pinCount('prompt')).toBe(0)
        expect(() => operation.readLatest(1)).toThrow(ConversationHistoryOperationDisposedError)
    })

    it('rejects every later read when the pinned session version changes', () => {
        const conversation = chat([message('before')])
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 2,
        })
        const operation = beginPinnedConversationHistoryOperation(session)

        session.append(message('after'))

        expect(() => operation.assertCurrent()).toThrow(ConversationSessionStaleError)
        expect(() => operation.readRange(0, 1)).toThrow(ConversationSessionStaleError)
        operation.dispose()
        expect(session.pinCount('prompt')).toBe(0)
    })

    it('makes an explicit detached compatibility snapshot and disposes its data', () => {
        const messages = [message('original')]
        const operation = createCompatibilityConversationHistorySnapshot({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            messages,
            storeRevision: 9,
        })

        messages[0].data = 'mutated-live-value'

        expect(operation.source).toBe('compatibility-snapshot')
        expect(operation.readLatest(1).messages).toEqual([message('original')])
        operation.dispose()
        expect(() => operation.scanBackward(1, 1)).toThrow(
            ConversationHistoryOperationDisposedError,
        )
    })
})
