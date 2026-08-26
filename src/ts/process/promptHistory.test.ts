import { describe, expect, it, vi } from 'vitest'

import type { Chat, Message } from '../storage/database.svelte'
import { ActiveConversationSession, ConversationSessionStaleError } from '../storage/activeConversationSession'
import { beginPinnedConversationHistoryOperation } from '../storage/conversationHistoryOperation'
import {
    ensurePromptHistoryMessageIds,
    iteratePromptHistory,
    selectPromptHistory,
} from './promptHistory'

function message(
    data: string,
    role: Message['role'] = 'user',
    disabled: Message['disabled'] = false,
): Message {
    return { role, data, disabled, chatId: `message-${data}` }
}

function sessionFor(messages: Message[]) {
    const conversation: Chat = {
        id: 'conversation-a',
        name: 'Conversation A',
        note: '',
        localLore: [],
        message: messages,
    }
    return new ActiveConversationSession({
        characterId: 'character-a',
        conversationId: 'conversation-a',
        conversation,
        storeRevision: 23,
    })
}

describe('prompt history paging', () => {
    it('preserves the latest allBefore boundary, disabled filtering, order, duplicates, and roles', () => {
        const session = sessionFor([
            message('old-user'),
            message('reset', 'char', 'allBefore'),
            message('duplicate', 'user'),
            message('hidden', 'char', true),
            message('duplicate', 'char'),
            message('tail', 'user'),
        ])
        const operation = beginPinnedConversationHistoryOperation(session)

        const selection = selectPromptHistory(operation, 2)
        const entries = [...iteratePromptHistory(operation, selection, 2)]

        expect(selection).toEqual({
            startIndex: 2,
            endIndex: 6,
            totalMessages: 6,
            messageCount: 3,
            resetByAllBefore: true,
        })
        expect(entries.map(({ absoluteIndex, relativeIndex, message: value }) => ({
            absoluteIndex,
            relativeIndex,
            data: value.data,
            role: value.role,
        }))).toEqual([
            { absoluteIndex: 2, relativeIndex: 0, data: 'duplicate', role: 'user' },
            { absoluteIndex: 4, relativeIndex: 1, data: 'duplicate', role: 'char' },
            { absoluteIndex: 5, relativeIndex: 2, data: 'tail', role: 'user' },
        ])
        operation.dispose()
    })

    it('uses bounded backward and forward reads for the Roadmap 14 corpus conversation', async () => {
        const { roadmap14Corpus } = await import('../storage/tests/roadmap14/losslessCorpus')
        const fixture = roadmap14Corpus.database.characters[0].chats[0].message
        const messages = Array.from({ length: 11 }, (_, index) => ({
            ...structuredClone(fixture[index % fixture.length]),
            chatId: `fixture-${index}`,
        }))
        const session = sessionFor(messages)
        const backward = vi.spyOn(session, 'scanBackward')
        const range = vi.spyOn(session, 'readRange')
        const operation = beginPinnedConversationHistoryOperation(session)

        const selection = selectPromptHistory(operation, 3)
        const entries = [...iteratePromptHistory(operation, selection, 3)]

        expect(entries.map((entry) => entry.message)).toEqual(messages)
        expect(backward.mock.calls.every(([, limit]) => limit <= 3)).toBe(true)
        expect(range.mock.calls.every(([, limit]) => limit <= 3)).toBe(true)
        expect(backward.mock.calls.length).toBeGreaterThan(1)
        expect(range.mock.calls.length).toBeGreaterThan(1)
        operation.dispose()
    })

    it('fails instead of mixing revisions between forward pages', () => {
        const session = sessionFor([
            message('zero'),
            message('one'),
            message('two'),
        ])
        const operation = beginPinnedConversationHistoryOperation(session)
        const selection = selectPromptHistory(operation, 1)
        const iterator = iteratePromptHistory(operation, selection, 1)

        expect(iterator.next().value?.message.data).toBe('zero')
        session.append(message('new-tail'))

        expect(() => iterator.next()).toThrow(ConversationSessionStaleError)
        operation.dispose()
    })

    it('reads the next message after history-sensitive processing can update it', () => {
        const conversationMessages = [message('first'), message('before-update')]
        const session = sessionFor(conversationMessages)
        const operation = beginPinnedConversationHistoryOperation(session)
        const selection = selectPromptHistory(operation)
        const iterator = iteratePromptHistory(operation, selection)

        expect(iterator.next().value?.message.data).toBe('first')
        conversationMessages[1].data = 'after-update'

        expect(iterator.next().value?.message.data).toBe('after-update')
        operation.dispose()
    })

    it('preserves existing IDs and assigns only missing prompt-history IDs', () => {
        const messages = [
            { role: 'user', data: 'existing', chatId: 'existing-id' },
            { role: 'char', data: 'missing' },
        ] satisfies Message[]

        ensurePromptHistoryMessageIds(messages, () => 'generated-id')

        expect(messages.map((value) => value.chatId)).toEqual(['existing-id', 'generated-id'])
    })
})
