import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../storage/database.svelte'
import type {
    CharacterPage,
    ConversationPage,
    ConversationWindow,
    PersistentDataStore,
} from '../storage/persistentDataStore'
import { createPluginDatabaseAccess } from './pluginDatabaseAccess'

const characterPage: CharacterPage = {
    revision: 4,
    items: [
        {
            id: 'char-a',
            name: 'Alpha',
            configuredIndex: 0,
            recentAt: 10,
            trashed: false,
            conversationCount: 1,
        },
    ],
}

const conversationPage: ConversationPage = {
    revision: 4,
    items: [
        {
            id: 'conv-a',
            characterId: 'char-a',
            name: 'First chat',
            configuredIndex: 0,
            recentAt: 10,
            messageCount: 1,
        },
    ],
}

const conversationWindow: ConversationWindow = {
    characterId: 'char-a',
    conversationId: 'conv-a',
    messages: [{ role: 'user', data: 'hello' }],
    startIndex: 0,
    endIndex: 1,
    totalMessages: 1,
    hasMoreBefore: false,
    hasMoreAfter: false,
}

function createHarness() {
    const compatibilityDatabase = {
        username: 'Live user',
        maxContext: 8192,
    } as unknown as Database
    const materializedDatabases: Database[] = []
    const store = {
        open: vi.fn(async () => undefined),
        queryCharacters: vi.fn(async () => characterPage),
        queryConversations: vi.fn(async () => conversationPage),
        readConversationWindow: vi.fn(async () => ({ revision: 4, value: conversationWindow })),
        materializeDatabase: vi.fn(async () => materializedDatabases.shift()!),
        readRoot: vi.fn(),
        readCharacter: vi.fn(),
        readConversation: vi.fn(),
        commit: vi.fn(),
        replaceFromDatabase: vi.fn(),
        acquireRevision: vi.fn(),
    } as unknown as PersistentDataStore
    const flushPendingData = vi.fn(async () => undefined)
    const snapshot = vi.fn((value: unknown) => structuredClone(value))
    const access = createPluginDatabaseAccess({
        store,
        flushPendingData,
        getCompatibilityDatabase: () => compatibilityDatabase,
        snapshot: <T>(value: T) => snapshot(value) as T,
    })
    return {
        access,
        compatibilityDatabase,
        flushPendingData,
        materializedDatabases,
        snapshot,
        store,
    }
}

describe('plugin database access', () => {
    it('runs scalable queries without touching the compatibility character array', async () => {
        const harness = createHarness()
        Object.defineProperty(harness.compatibilityDatabase, 'characters', {
            get() {
                throw new Error('scalable query touched DBState.db.characters')
            },
        })

        expect(await harness.access.queryCharacters({ limit: 10 })).toEqual(characterPage)
        expect(
            await harness.access.queryConversations({ characterId: 'char-a', limit: 10 }),
        ).toEqual(conversationPage)
        expect(
            await harness.access.queryConversationMessages({
                characterId: 'char-a',
                conversationId: 'conv-a',
                limit: 10,
            }),
        ).toEqual(conversationWindow)
        expect(harness.snapshot).not.toHaveBeenCalled()
        expect(harness.store.open).toHaveBeenCalledTimes(1)
        expect(harness.store.readRoot).not.toHaveBeenCalled()
        expect(harness.store.readCharacter).not.toHaveBeenCalled()
        expect(harness.store.readConversation).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('finishes each flush before the matching persistent query begins', async () => {
        const harness = createHarness()
        const events: string[] = []
        vi.mocked(harness.flushPendingData).mockImplementation(async () => {
            events.push('flush')
        })
        vi.mocked(harness.store.queryCharacters).mockImplementation(async () => {
            events.push('characters')
            return characterPage
        })
        vi.mocked(harness.store.queryConversations).mockImplementation(async () => {
            events.push('conversations')
            return conversationPage
        })
        vi.mocked(harness.store.readConversationWindow).mockImplementation(async () => {
            events.push('messages')
            return { revision: 4, value: conversationWindow }
        })

        await harness.access.queryCharacters()
        await harness.access.queryConversations({ characterId: 'char-a' })
        await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
        })

        expect(events).toEqual([
            'flush',
            'characters',
            'flush',
            'conversations',
            'flush',
            'messages',
        ])
        expect(harness.flushPendingData).toHaveBeenNthCalledWith(1, 'plugin-database-query')
    })

    it('applies documented defaults and clamps maximum query sizes', async () => {
        const harness = createHarness()

        await harness.access.queryCharacters()
        await harness.access.queryCharacters({ limit: 500 })
        await harness.access.queryConversations({ characterId: ' char-a ', limit: 500 })
        await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
        })
        await harness.access.queryConversationMessages({
            characterId: 'char-a',
            conversationId: 'conv-a',
            limit: 500,
        })

        expect(harness.store.queryCharacters).toHaveBeenNthCalledWith(1, {
            order: 'configured',
            trash: false,
            limit: 50,
        })
        expect(harness.store.queryCharacters).toHaveBeenNthCalledWith(2, {
            order: 'configured',
            trash: false,
            limit: 100,
        })
        expect(harness.store.queryConversations).toHaveBeenCalledWith({
            characterId: ' char-a ',
            order: 'configured',
            limit: 100,
        })
        expect(harness.store.readConversationWindow).toHaveBeenNthCalledWith(1, {
            characterId: 'char-a',
            conversationId: 'conv-a',
            limit: 128,
        })
        expect(harness.store.readConversationWindow).toHaveBeenNthCalledWith(2, {
            characterId: 'char-a',
            conversationId: 'conv-a',
            limit: 128,
        })
    })

    it.each([
        () => ({ kind: 'conversations', input: { characterId: '' } }),
        () => ({ kind: 'conversations', input: { characterId: '   ' } }),
        () => ({ kind: 'characters', input: { limit: 1.5 } }),
        () => ({ kind: 'characters', input: { limit: 0 } }),
        () => ({ kind: 'messages', input: { characterId: '', conversationId: 'conv-a' } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: '' } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', before: 1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: '', before: 1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: 'm', limit: 1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: 'm', before: -1 } }),
        () => ({ kind: 'messages', input: { characterId: 'char-a', conversationId: 'conv-a', anchorMessageId: 'm', before: 64, after: 64 } }),
    ])('rejects invalid query input before persistence access', async (makeCase) => {
        const harness = createHarness()
        const testCase = makeCase()
        const operation =
            testCase.kind === 'characters'
                ? harness.access.queryCharacters(testCase.input as never)
                : testCase.kind === 'conversations'
                  ? harness.access.queryConversations(testCase.input as never)
                  : harness.access.queryConversationMessages(testCase.input as never)

        await expect(operation).rejects.toBeInstanceOf(RangeError)
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.store.queryCharacters).not.toHaveBeenCalled()
        expect(harness.store.queryConversations).not.toHaveBeenCalled()
        expect(harness.store.readConversationWindow).not.toHaveBeenCalled()
    })

    it('unwraps versioned message windows and preserves missing windows', async () => {
        const harness = createHarness()
        expect(
            await harness.access.queryConversationMessages({
                characterId: 'char-a',
                conversationId: 'conv-a',
                anchorMessageId: 'message-a',
                before: 2,
                after: 3,
            }),
        ).toEqual(conversationWindow)
        vi.mocked(harness.store.readConversationWindow).mockResolvedValueOnce(null)
        expect(
            await harness.access.queryConversationMessages({
                characterId: 'char-a',
                conversationId: 'missing',
            }),
        ).toBeNull()
    })

    it('snapshots root-only compatibility keys without opening persistence', async () => {
        const harness = createHarness()

        await expect(
            harness.access.getDatabaseSnapshot(['username', 'maxContext'], [
                'characters',
                'username',
                'maxContext',
            ]),
        ).resolves.toEqual({ username: 'Live user', maxContext: 8192 })
        expect(harness.store.open).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.flushPendingData).not.toHaveBeenCalled()
    })

    it('materializes characters only for the explicit full compatibility snapshot', async () => {
        const harness = createHarness()
        const materialized = {
            characters: [{ chaId: 'persisted-character' }],
        } as unknown as Database
        harness.materializedDatabases.push(materialized)
        Object.defineProperty(harness.compatibilityDatabase, 'characters', {
            get() {
                throw new Error('full snapshot read compatibility characters')
            },
        })

        await expect(
            harness.access.getDatabaseSnapshot('all', ['characters', 'username', 'unknown']),
        ).resolves.toEqual({
            characters: materialized.characters,
            username: 'Live user',
            unknown: undefined,
        })
        expect(harness.flushPendingData).toHaveBeenCalledWith('plugin-full-database-snapshot')
        expect(harness.store.open).toHaveBeenCalledTimes(1)
        expect(harness.store.materializeDatabase).toHaveBeenCalledTimes(1)
    })

    it('does not retain full compatibility snapshots between calls', async () => {
        const harness = createHarness()
        const first = { characters: [{ chaId: 'first' }] } as unknown as Database
        const second = { characters: [{ chaId: 'second' }] } as unknown as Database
        harness.materializedDatabases.push(first, second)

        const firstResult = await harness.access.getDatabaseSnapshot(['characters'], ['characters'])
        const secondResult = await harness.access.getDatabaseSnapshot(['characters'], ['characters'])

        expect(firstResult.characters).toEqual(first.characters)
        expect(secondResult.characters).toEqual(second.characters)
        expect(harness.store.materializeDatabase).toHaveBeenCalledTimes(2)
    })

    it('omits unapproved selected keys and snapshots live non-character values', async () => {
        const harness = createHarness()

        await expect(
            harness.access.getDatabaseSnapshot(['username', 'secret'], ['username']),
        ).resolves.toEqual({ username: 'Live user' })
        expect(harness.snapshot).toHaveBeenCalledWith('Live user')
    })
})
