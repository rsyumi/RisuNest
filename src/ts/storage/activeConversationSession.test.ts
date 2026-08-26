import { describe, expect, it, vi } from 'vitest'

import type { Chat, Message } from './database.svelte'
import {
    ActiveConversationSession,
    ConversationNotFoundError,
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
    MessageLocatorNotFoundError,
    type ActiveConversationPinReason,
} from './activeConversationSession'

function message(id: string | undefined, data: string): Message {
    return {
        role: 'user',
        data,
        ...(id === undefined ? {} : { chatId: id }),
    }
}

function chat(messages: Message[] = [
    message('duplicate', 'zero'),
    message(undefined, 'one'),
    message('duplicate', 'two'),
    message('tail', 'three'),
]): Chat {
    return {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: messages,
    }
}

function createSession(conversation = chat(), onMutation = vi.fn()) {
    return {
        conversation,
        onMutation,
        session: new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation,
            storeRevision: 7,
            onMutation,
        }),
    }
}

describe('ActiveConversationSession', () => {
    it('rejects a missing full-array conversation distinctly from a missing locator', () => {
        expect(() => new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'missing',
            conversation: null,
            storeRevision: 7,
        })).toThrow(ConversationNotFoundError)

        const { session } = createSession()
        expect(() => session.locate(99)).toThrow(MessageLocatorNotFoundError)
    })

    it('reads latest, absolute ranges, and backwards entries with stable absolute locators', () => {
        const { session } = createSession()

        expect(session.readRange(1, 2)).toEqual({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            messages: [message(undefined, 'one'), message('duplicate', 'two')],
            locators: [
                {
                    conversationId: 'conversation-a',
                    absoluteIndex: 1,
                    sessionVersion: 0,
                },
                {
                    conversationId: 'conversation-a',
                    absoluteIndex: 2,
                    expectedMessageId: 'duplicate',
                    sessionVersion: 0,
                },
            ],
            startIndex: 1,
            endIndex: 3,
            totalMessages: 4,
            storeRevision: 7,
            sessionVersion: 0,
        })
        expect(session.readLatest(2).messages.map((item) => item.data)).toEqual(['two', 'three'])
        expect(session.readRange(20, 3)).toMatchObject({
            messages: [],
            startIndex: 4,
            endIndex: 4,
            totalMessages: 4,
        })
        const backward = session.scanBackward(3, 2)
        expect(backward).toMatchObject({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            startIndexExclusive: 3,
            totalMessages: 4,
            storeRevision: 7,
            sessionVersion: 0,
        })
        expect(backward.entries.map((entry) => ({
            absoluteIndex: entry.absoluteIndex,
            data: entry.message.data,
        }))).toEqual([
            { absoluteIndex: 2, data: 'two' },
            { absoluteIndex: 1, data: 'one' },
        ])
    })

    it('strictly validates read indices and bounded counts', () => {
        const { session } = createSession()

        for (const [start, limit] of [
            [-1, 1],
            [1.5, 1],
            [Number.POSITIVE_INFINITY, 1],
            [0, 0],
            [0, -1],
            [0, 4_097],
        ]) {
            expect(() => session.readRange(start, limit)).toThrow(RangeError)
        }
        expect(() => session.scanBackward(-1, 1)).toThrow(RangeError)
    })

    it('appends, edits, deletes, and truncates with canonical full-array results', () => {
        const { conversation, session } = createSession()

        const appended = session.append(message('append', 'four'))
        expect(appended).toEqual({
            conversationId: 'conversation-a',
            absoluteIndex: 4,
            expectedMessageId: 'append',
            sessionVersion: 1,
        })
        const edited = session.edit(appended, message('append', 'edited four'))
        expect(edited.sessionVersion).toBe(2)
        session.delete(session.locate(1))
        session.truncate(session.locate(2))

        expect(conversation.message).toEqual([
            message('duplicate', 'zero'),
            message('duplicate', 'two'),
        ])
        expect(session.version).toBe(4)
    })

    it('replaces tails for reroll and exposes an inclusive branch source without cloning the chat shape', () => {
        const { conversation, session } = createSession()

        session.replaceTail(session.positionAt(2), [
            message('replacement-a', 'replacement two'),
            message('replacement-b', 'replacement three'),
        ])
        session.reroll(session.positionAt(3), [message('rerolled', 'rerolled three')])

        expect(conversation.message).toEqual([
            message('duplicate', 'zero'),
            message(undefined, 'one'),
            message('replacement-a', 'replacement two'),
            message('rerolled', 'rerolled three'),
        ])
        expect(session.readBranchSource(session.locate(2))).toEqual({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            messages: conversation.message.slice(0, 3),
            startIndex: 0,
            endIndex: 3,
            totalMessages: 4,
            storeRevision: 7,
            sessionVersion: 2,
        })
    })

    it('fails stale, shifted, mismatched, and replaced locators instead of retargeting', () => {
        const { session } = createSession()
        const shifted = session.locate(2)
        const replaced = session.locate(3)
        const wrongConversation = { ...session.locate(0), conversationId: 'conversation-b' }

        session.delete(session.locate(0))
        expect(() => session.edit(shifted, message('duplicate', 'wrong target'))).toThrow(
            ConversationSessionStaleError,
        )
        expect(() => session.edit(replaced, message('tail', 'wrong replacement'))).toThrow(
            ConversationSessionStaleError,
        )
        expect(() => session.edit(wrongConversation, message('duplicate', 'wrong conversation'))).toThrow(
            MessageLocatorMismatchError,
        )

        const current = session.locate(2)
        session.edit(current, message('tail', 'replacement with same ID'))
        expect(() => session.edit(current, message('tail', 'stale replacement'))).toThrow(
            ConversationSessionStaleError,
        )
    })

    it('publishes a successful transaction once and rolls back the whole draft after any failure', () => {
        const { conversation, onMutation, session } = createSession()
        const original = structuredClone(conversation.message)
        const staleInsideTransaction = session.locate(2)

        expect(() => session.transaction((transaction) => {
            transaction.delete(transaction.locate(0))
            transaction.edit(staleInsideTransaction, message('duplicate', 'must not publish'))
        })).toThrow(ConversationSessionStaleError)
        expect(conversation.message).toEqual(original)
        expect(session.version).toBe(0)
        expect(onMutation).not.toHaveBeenCalled()

        session.transaction((transaction) => {
            transaction.edit(transaction.locate(0), message('duplicate', 'edited zero'))
            transaction.append(message('append', 'four'))
        })
        expect(conversation.message.map((item) => item.data)).toEqual([
            'edited zero',
            'one',
            'two',
            'three',
            'four',
        ])
        expect(session.version).toBe(2)
        expect(onMutation).toHaveBeenCalledOnce()
        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            previousVersion: 0,
            sessionVersion: 2,
            commands: ['edit', 'append'],
        }))
    })

    it('tracks the C1 pin reason contract without enabling eviction', () => {
        const { session } = createSession()
        const reasons: ActiveConversationPinReason[] = [
            'dirty',
            'pending-save',
            'streaming',
            'transaction',
            'compatibility',
        ]
        const pins = reasons.map((reason) => session.acquirePin(reason))
        const secondDirty = session.acquirePin('dirty')

        expect(session.evictionEnabled).toBe(false)
        expect(session.activePinReasons).toEqual(reasons)
        expect(session.pinCount('dirty')).toBe(2)
        pins[0].release()
        pins[0].release()
        expect(session.pinCount('dirty')).toBe(1)
        secondDirty.release()
        expect(session.activePinReasons).toEqual(reasons.slice(1))
    })
})
