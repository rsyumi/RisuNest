import { describe, expect, it, vi } from 'vitest'

import type { Chat, Message } from '../storage/database.svelte'
import { ActiveConversationSession, ConversationSessionStaleError } from '../storage/activeConversationSession'
import { beginPinnedConversationHistoryOperation } from '../storage/conversationHistoryOperation'
import {
    adoptTriggeredChat,
    createLivePromptHistoryCompatibilitySnapshot,
    ensurePromptHistoryEntryId,
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

    it('binds each paged entry to the latest live message before processing it', () => {
        const messages = [message('first'), message('before-update')]
        const session = sessionFor(messages)
        const operation = beginPinnedConversationHistoryOperation(session)
        const selection = selectPromptHistory(operation)
        const iterator = iteratePromptHistory(operation, selection)

        const firstEntry = iterator.next().value!
        const first = ensurePromptHistoryEntryId(operation, firstEntry, vi.fn())
        expect(first.data).toBe('first')
        messages[1].data = 'after-update'
        const secondEntry = iterator.next().value!
        const second = ensurePromptHistoryEntryId(operation, secondEntry, vi.fn())

        expect(second.data).toBe('after-update')
        operation.dispose()
    })

    it('preserves existing IDs and assigns IDs only to selected active prompt messages', () => {
        const messages = [
            { role: 'user', data: 'before-reset' },
            { role: 'char', data: 'reset', disabled: 'allBefore' },
            { role: 'user', data: 'disabled', disabled: true },
            { role: 'char', data: 'empty-id', chatId: '' },
            { role: 'user', data: 'existing', chatId: 'existing-id' },
        ] satisfies Message[]
        const session = sessionFor(messages)
        const operation = beginPinnedConversationHistoryOperation(session)
        const selection = selectPromptHistory(operation)
        const generated = ['generated-empty']

        for (const entry of iteratePromptHistory(operation, selection)) {
            ensurePromptHistoryEntryId(operation, entry, () => generated.shift()!)
        }

        expect(messages.map((value) => value.chatId)).toEqual([
            undefined,
            undefined,
            undefined,
            'generated-empty',
            'existing-id',
        ])
        expect(generated).toEqual([])
        operation.dispose()

        const repeatedOperation = beginPinnedConversationHistoryOperation(session)
        const repeatedSelection = selectPromptHistory(repeatedOperation)
        const generateAgain = vi.fn(() => 'different-id')
        for (const entry of iteratePromptHistory(repeatedOperation, repeatedSelection)) {
            ensurePromptHistoryEntryId(repeatedOperation, entry, generateAgain)
        }

        expect(generateAgain).not.toHaveBeenCalled()
        expect(messages[3].chatId).toBe('generated-empty')
        repeatedOperation.dispose()
    })

    it('adopts trigger results without replacing the active Chat identity', () => {
        const target: Chat = {
            id: 'conversation-a',
            name: 'Before',
            note: 'remove-me',
            localLore: [],
            message: [message('before')],
            folderId: 'removed-by-trigger',
        }
        const replacement: Chat = {
            id: 'conversation-a',
            name: 'After',
            note: '',
            localLore: [],
            message: [message('triggered')],
            scriptstate: { triggered: true },
        }
        const session = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'conversation-a',
            conversation: target,
            storeRevision: 12,
        })

        const adopted = adoptTriggeredChat(
            session,
            session.version,
            target.message,
            replacement,
        )

        expect(adopted).toBe(target)
        expect(adopted).toEqual(replacement)
        expect(session.matchesConversation('character-a', adopted)).toBe(true)
        expect(session.version).toBe(1)
        const operation = beginPinnedConversationHistoryOperation(session)
        expect(operation.readLatest(1).messages[0].data).toBe('triggered')
        operation.dispose()
    })

    it('retains and disposes the exact selected message references for compatibility', () => {
        const messages = [message('first'), message('retained'), message('tail')]
        const session = sessionFor(messages)
        const operation = beginPinnedConversationHistoryOperation(session)
        const selection = selectPromptHistory(operation)
        const retained = messages[1]
        const snapshot = createLivePromptHistoryCompatibilitySnapshot(messages, selection)

        messages.splice(1, 1, message('replacement'))
        retained.data = 'mutated-through-retained-reference'

        expect(snapshot.entries.map((entry) => entry.message.data)).toEqual([
            'first',
            'mutated-through-retained-reference',
            'tail',
        ])
        snapshot.dispose()
        expect(snapshot.entries).toEqual([])
        operation.dispose()
    })
})
