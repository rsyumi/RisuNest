import { expect, test } from 'vitest'
import {
    ActiveConversationSession,
    ConversationSessionStaleError,
} from '../storage/activeConversationSession'
import type { Chat, Message } from '../storage/database.svelte'
import {
    createConversationOperationContext,
} from './conversationOperationContext'

const message = (data: string, chatId: string): Message => ({
    role: 'user',
    data,
    chatId,
})

const chat = (messages: Message[]): Chat => ({
    id: 'conversation-1',
    message: messages,
} as Chat)

function createSession(conversation: Chat) {
    return new ActiveConversationSession({
        characterId: 'character-1',
        conversationId: 'conversation-1',
        conversation,
        storeRevision: 7,
    })
}

test('prefetches one complete bounded session version into a detached operation chat', () => {
    const conversation = chat([
        message('zero', 'message-0'),
        message('one', 'message-1'),
    ])
    const session = createSession(conversation)

    const operation = createConversationOperationContext(session, conversation)

    expect(operation.mode).toBe('prefetched')
    expect(operation.baseVersion).toBe(0)
    expect(operation.chat.message).toEqual(conversation.message)
    expect(operation.chat.message).not.toBe(conversation.message)
    expect(session.pinCount('transaction')).toBe(1)

    operation.chat.message[0].data = 'detached'
    expect(conversation.message[0].data).toBe('zero')

    operation.release()
    expect(session.pinCount('transaction')).toBe(0)
})

test('marks an oversized full-history consumer as an explicit compatibility snapshot', () => {
    const messages = Array.from({ length: 4097 }, (_, index) =>
        message(`message-${index}`, `id-${index}`),
    )
    const conversation = chat(messages)
    const session = createSession(conversation)

    const operation = createConversationOperationContext(session, conversation)

    expect(operation.mode).toBe('compatibility')
    expect(operation.chat.message).toHaveLength(4097)
    expect(session.pinCount('compatibility')).toBe(1)

    operation.release()
    expect(session.pinCount('compatibility')).toBe(0)
})

test('CAS-applies the final ordered mutation result through a stable replace-range position', () => {
    const conversation = chat([
        message('zero', 'message-0'),
        message('one', 'message-1'),
        message('two', 'message-2'),
    ])
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)

    operation.chat.message[0].data = 'edited'
    operation.chat.message.splice(1, 1)
    operation.chat.message.push(message('tail', 'message-tail'))

    const batch = operation.collectMutationBatch()
    expect(batch).toEqual([
        expect.objectContaining({
            type: 'replace-range',
            startIndex: 0,
            deleteCount: 3,
            messages: [
                message('edited', 'message-0'),
                message('two', 'message-2'),
                message('tail', 'message-tail'),
            ],
        }),
    ])

    operation.commit(session)

    expect(conversation.message).toEqual([
        message('edited', 'message-0'),
        message('two', 'message-2'),
        message('tail', 'message-tail'),
    ])
    expect(session.version).toBe(1)
    expect(session.pinCount('transaction')).toBe(0)
})

test('a stale operation cannot touch a concurrently replaced conversation', () => {
    const conversation = chat([
        message('zero', 'message-0'),
        message('one', 'message-1'),
    ])
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)
    operation.chat.message[0].data = 'stale edit'

    session.edit(session.locate(0), message('concurrent edit', 'message-0'))

    expect(() => operation.commit(session)).toThrow(ConversationSessionStaleError)
    expect(conversation.message).toEqual([
        message('concurrent edit', 'message-0'),
        message('one', 'message-1'),
    ])
    expect(session.pinCount('transaction')).toBe(0)
})

test('a batch refuses an unversioned direct mutation of its pinned baseline', () => {
    const conversation = chat([
        message('zero', 'message-0'),
        message('one', 'message-1'),
    ])
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)
    operation.chat.message[1].data = 'operation edit'

    conversation.message[0].data = 'direct concurrent edit'

    expect(() => operation.commit(session)).toThrow(/baseline changed/i)
    expect(conversation.message).toEqual([
        message('direct concurrent edit', 'message-0'),
        message('one', 'message-1'),
    ])
    expect(session.pinCount('transaction')).toBe(0)
})

test('a batch refuses a different active session even when IDs and contents match', () => {
    const original = chat([message('zero', 'message-0')])
    const replacement = chat([message('zero', 'message-0')])
    const originalSession = createSession(original)
    const replacementSession = createSession(replacement)
    const operation = createConversationOperationContext(originalSession, original)
    operation.chat.message[0].data = 'stale edit'

    expect(() => operation.commit(replacementSession)).toThrow(/inactive/i)
    expect(original.message[0].data).toBe('zero')
    expect(replacement.message[0].data).toBe('zero')
    expect(originalSession.pinCount('transaction')).toBe(0)
})
