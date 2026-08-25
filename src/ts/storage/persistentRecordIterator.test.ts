import { describe, expect, it, vi } from 'vitest'
import type {
    CharacterDetail,
    CharacterPage,
    CharacterQuery,
    ConversationPage,
    ConversationQuery,
    PersistentRevisionReader,
} from './persistentDataStore'
import {
    iteratePinnedCharacters,
    iteratePinnedConversations,
} from './persistentRecordIterator'

function characterPage(
    revision: number,
    items: CharacterPage['items'],
    nextCursor?: string,
): CharacterPage {
    return { revision, items, nextCursor }
}

function conversationPage(
    revision: number,
    items: ConversationPage['items'],
    nextCursor?: string,
): ConversationPage {
    return { revision, items, nextCursor }
}

describe('persistent record iterators', () => {
    it('merges active and trash pages in configured order without collecting either catalog', async () => {
        const activePages = [
            characterPage(7, [{
                id: 'active-0', name: 'A0', configuredIndex: 0, recentAt: 0,
                trashed: false, conversationCount: 0, type: 'character',
            }], '1'),
            characterPage(7, [{
                id: 'active-4', name: 'A4', configuredIndex: 4, recentAt: 0,
                trashed: false, conversationCount: 0, type: 'character',
            }]),
        ]
        const trashPages = [
            characterPage(7, [{
                id: 'trash-1', name: 'T1', configuredIndex: 1, recentAt: 0,
                trashed: true, conversationCount: 0, type: 'character',
            }], '1'),
            characterPage(7, [{
                id: 'trash-3', name: 'T3', configuredIndex: 3, recentAt: 0,
                trashed: true, conversationCount: 0, type: 'character',
            }]),
        ]
        const queryCharacters = vi.fn(async (query: CharacterQuery) => {
            expect(query.limit).toBeLessThanOrEqual(128)
            const pages = query.trash ? trashPages : activePages
            return pages[Number(query.cursor ?? '0')]
        })
        const readCharacter = vi.fn(async (id: string) => ({
            revision: 7,
            value: { chaId: id, name: id, type: 'character' } as CharacterDetail,
        }))
        const reader = {
            revision: 7,
            queryCharacters,
            readCharacter,
        } as unknown as PersistentRevisionReader

        const ids: string[] = []
        for await (const record of iteratePinnedCharacters(reader)) ids.push(record.summary.id)

        expect(ids).toEqual(['active-0', 'trash-1', 'trash-3', 'active-4'])
        expect(readCharacter.mock.calls.map(([id]) => id)).toEqual(ids)
    })

    it('reads conversations across late pages in configured order', async () => {
        const queryConversations = vi.fn(async (query: ConversationQuery) => {
            const pageIndex = Number(query.cursor ?? '0')
            return conversationPage(11, [{
                id: `chat-${pageIndex}`,
                characterId: 'char-1',
                name: `Chat ${pageIndex}`,
                configuredIndex: pageIndex,
                recentAt: 0,
                messageCount: pageIndex + 1,
            }], pageIndex < 2 ? String(pageIndex + 1) : undefined)
        })
        const readConversation = vi.fn(async (_characterId: string, conversationId: string) => ({
            revision: 11,
            value: { id: conversationId, name: conversationId, message: [] },
        }))
        const reader = {
            revision: 11,
            queryConversations,
            readConversation,
        } as unknown as PersistentRevisionReader

        const ids: string[] = []
        for await (const record of iteratePinnedConversations(reader, 'char-1')) {
            ids.push(record.summary.id)
        }

        expect(ids).toEqual(['chat-0', 'chat-1', 'chat-2'])
        expect(queryConversations).toHaveBeenCalledTimes(3)
    })

    it('fails before publishing a page from another revision', async () => {
        const readCharacter = vi.fn()
        const reader = {
            revision: 5,
            queryCharacters: vi.fn(async () => characterPage(6, [{
                id: 'wrong', name: 'Wrong', configuredIndex: 0, recentAt: 0,
                trashed: false, conversationCount: 0, type: 'character',
            }])),
            readCharacter,
        } as unknown as PersistentRevisionReader

        const iterator = iteratePinnedCharacters(reader)[Symbol.asyncIterator]()

        await expect(iterator.next()).rejects.toThrow('revision 6')
        expect(readCharacter).not.toHaveBeenCalled()
    })

    it('fails immediately on a missing conversation without scheduling later reads', async () => {
        const readConversation = vi.fn(async (_characterId: string, conversationId: string) =>
            conversationId === 'missing'
                ? null
                : { revision: 3, value: { id: conversationId, message: [] } },
        )
        const reader = {
            revision: 3,
            queryConversations: vi.fn(async () => conversationPage(3, [
                {
                    id: 'missing', characterId: 'char-1', name: 'Missing',
                    configuredIndex: 0, recentAt: 0, messageCount: 0,
                },
                {
                    id: 'later', characterId: 'char-1', name: 'Later',
                    configuredIndex: 1, recentAt: 0, messageCount: 0,
                },
            ])),
            readConversation,
        } as unknown as PersistentRevisionReader

        const iterator = iteratePinnedConversations(reader, 'char-1')[Symbol.asyncIterator]()

        await expect(iterator.next()).rejects.toThrow('Missing conversation missing')
        expect(readConversation.mock.calls.map(([, id]) => id)).toEqual(['missing'])
    })
})
