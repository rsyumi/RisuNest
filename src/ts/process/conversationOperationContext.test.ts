import { expect, test, vi } from 'vitest'
import {
    ActiveConversationCompatibilitySnapshot,
    ActiveConversationSession,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
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

test.each([
    'scriptstate',
    'GLGlobalVariables',
    'message',
    'name',
    'mixed',
    'external',
] as const)(
    'marks only display variable writes as non-invalidating (%s)',
    (kind) => {
        const conversation = chat([message('before', 'message')])
        const session = createSession(conversation)
        const onMutation = vi.fn()
        session.subscribe(onMutation)
        const operation = createConversationOperationContext(
            session,
            conversation,
        )
        if (kind === 'message') operation.chat.message[0].data = 'after'
        else if (kind === 'name') operation.chat.name = 'renamed'
        else if (kind === 'GLGlobalVariables')
            operation.chat.GLGlobalVariables = { global: 'changed' }
        else operation.chat.scriptstate = { $scratch: 'changed' }
        if (kind === 'mixed') operation.chat.name = 'also renamed'
        operation.commit(
            session,
            kind === 'external' ? {} : { origin: 'display' },
        )
        expect(onMutation).toHaveBeenCalledOnce()
        const event = onMutation.mock.calls[0][0]
        expect(event.displayVariableUpdate === true).toBe(
            kind === 'scriptstate' || kind === 'GLGlobalVariables',
        )
        expect(event.conversation).toEqual(
            expect.objectContaining(
                kind === 'message'
                    ? {}
                    : kind === 'name'
                      ? { name: 'renamed' }
                      : kind === 'GLGlobalVariables'
                        ? { GLGlobalVariables: { global: 'changed' } }
                        : { scriptstate: { $scratch: 'changed' } },
            ),
        )
        expect(session.version).toBe(1)
        expect(session.activePinReasons).toEqual([])
    },
)

test('does not reuse a commit receipt after an external mutation', () => {
    const conversation = chat([message('before', 'message')])
    const session = createSession(conversation)
    let receipt: import('./conversationOperationContext').ConversationOperationCommit | undefined
    const operation = createConversationOperationContext(session, conversation, (committed) => {
        receipt = committed
    })
    operation.chat.scriptstate = { $own: 'change' }
    operation.commit(session)
    expect(receipt!.follows(session, 0, conversation)).toBe(true)
    session.append(message('external mutation', 'external'))
    expect(receipt!.follows(session, 0, conversation)).toBe(false)
    expect(conversation.message.at(-1)?.data).toBe('external mutation')
    expect(session.activePinReasons).toEqual([])
})

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
    const snapshotSpy = vi.spyOn(session, 'materializeCompatibilitySnapshot')
    const takeMessagesSpy = vi.spyOn(
        ActiveConversationCompatibilitySnapshot.prototype,
        'takeMessages',
    )

    const operation = createConversationOperationContext(session, conversation)
    const snapshot = snapshotSpy.mock.results[0]?.value

    expect(operation.mode).toBe('compatibility')
    expect(operation.chat.message).toHaveLength(4097)
    expect(snapshotSpy).toHaveBeenCalledOnce()
    expect(takeMessagesSpy).toHaveBeenCalledOnce()
    expect(snapshot.residentMessageCount).toBe(0)
    expect(session.pinCount('compatibility')).toBe(1)

    operation.release()
    expect(session.pinCount('compatibility')).toBe(0)
    expect(session.residentBytes).toBe(0)
})

test('uses compatibility mode when a small message count exceeds the prefetch byte cap', () => {
    const conversation = chat([
        message('x'.repeat(3 * 1024 * 1024), 'large-message'),
    ])
    const session = createSession(conversation)

    const operation = createConversationOperationContext(session, conversation)

    expect(operation.mode).toBe('compatibility')
    expect(session.pinCount('compatibility')).toBe(1)
    expect(session.pinCount('transaction')).toBe(0)

    operation.release()
})

test('does not deep-clone source messages again while cloning chat metadata', () => {
    const conversation = chat([message('zero', 'message-0')])
    conversation.note = 'metadata'
    const session = createSession(conversation)
    const structuredCloneSpy = vi.spyOn(globalThis, 'structuredClone')

    try {
        const operation = createConversationOperationContext(session, conversation)

        expect(structuredCloneSpy.mock.calls.some(([value]) => value === conversation)).toBe(false)
        operation.release()
    } finally {
        structuredCloneSpy.mockRestore()
    }
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

test('CAS-applies chat variables and metadata with the same original owner', () => {
    const conversation = chat([message('zero', 'message-0')])
    conversation.note = 'before'
    conversation.scriptstate = { '$before': 'value' }
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)

    operation.chat.note = 'after'
    operation.chat.scriptstate = {
        '$before': 'value',
        '$new': '',
    }
    operation.commit(session)

    expect(conversation.note).toBe('after')
    expect(conversation.scriptstate).toEqual({
        '$before': 'value',
        '$new': '',
    })
    expect(session.version).toBe(1)
    expect(session.activePinReasons).toEqual([])
})

test('rejects concurrent metadata changes without overwriting them', () => {
    const conversation = chat([message('zero', 'message-0')])
    conversation.note = 'before'
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)
    operation.chat.note = 'operation note'

    conversation.note = 'concurrent note'

    expect(() => operation.commit(session)).toThrow(/metadata baseline changed/i)
    expect(conversation.note).toBe('concurrent note')
    expect(session.version).toBe(0)
    expect(session.activePinReasons).toEqual([])
})

test('rolls back message and metadata together when mutation publication fails', () => {
    const conversation = chat([message('zero', 'message-0')])
    conversation.note = 'before'
    const session = new ActiveConversationSession({
        characterId: 'character-1',
        conversationId: 'conversation-1',
        conversation,
        storeRevision: 7,
        onMutation: () => {
            throw new Error('publication failed')
        },
    })
    const operation = createConversationOperationContext(session, conversation)
    operation.chat.message[0].data = 'after'
    operation.chat.note = 'after'

    expect(() => operation.commit(session)).toThrow('publication failed')
    expect(conversation.message[0].data).toBe('zero')
    expect(conversation.note).toBe('before')
    expect(session.version).toBe(0)
    expect(session.activePinReasons).toEqual([])
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

test.each(['array', 'message'] as const)(
    'a batch refuses a deep-equal live %s identity replacement',
    (replacement) => {
        const conversation = chat([
            message('zero', 'message-0'),
            message('one', 'message-1'),
        ])
        const session = createSession(conversation)
        const operation = createConversationOperationContext(session, conversation)
        operation.chat.message[0].data = 'operation edit'

        if (replacement === 'array') {
            conversation.message = conversation.message.slice()
        } else {
            conversation.message[1] = { ...conversation.message[1] }
        }

        expect(() => operation.commit(session)).toThrow(MessageLocatorMismatchError)
        expect(conversation.message.map((entry) => entry.data)).toEqual(['zero', 'one'])
        expect(session.version).toBe(0)
        expect(session.activePinReasons).toEqual([])
    },
)

test('adopts only the generated ID of a future prompt message into its pinned baseline', () => {
    const conversation = chat([
        message('zero', 'message-0'),
        message('one', ''),
    ])
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)
    const locator = session.locate(1)

    session.ensureMessageId(locator, () => 'generated-message-1')
    operation.adoptMessageId(locator, 'generated-message-1')
    operation.chat.message[0].data = 'operation edit'
    operation.commit(session)

    expect(conversation.message).toEqual([
        message('operation edit', 'message-0'),
        message('one', 'generated-message-1'),
    ])
    expect(session.version).toBe(1)
    expect(session.activePinReasons).toEqual([])
})

test('refuses ID adoption when another unversioned message field changed', () => {
    const conversation = chat([
        message('zero', 'message-0'),
        message('one', ''),
    ])
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)
    const locator = session.locate(1)

    session.ensureMessageId(locator, () => 'generated-message-1')
    conversation.message[1].data = 'unversioned edit'

    expect(() => operation.adoptMessageId(locator, 'generated-message-1')).toThrow(
        MessageLocatorMismatchError,
    )
    operation.release()
    expect(session.activePinReasons).toEqual([])
})

test('refuses ID adoption after the operation structurally changes the projection', () => {
    const conversation = chat([
        message('zero', 'message-0'),
        message('one', ''),
    ])
    const session = createSession(conversation)
    const operation = createConversationOperationContext(session, conversation)
    const locator = session.locate(1)

    operation.chat.message.reverse()
    session.ensureMessageId(locator, () => 'generated-message-1')

    expect(() => operation.adoptMessageId(locator, 'generated-message-1')).toThrow(
        MessageLocatorMismatchError,
    )
    operation.release()
    expect(session.activePinReasons).toEqual([])
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
