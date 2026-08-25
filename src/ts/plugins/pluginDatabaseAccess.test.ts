import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../storage/database.svelte'
import type {
    CharacterPage,
    ConversationPage,
    ConversationWindow,
    PersistentDataStore,
} from '../storage/persistentDataStore'
import { getPersistentDataStore } from '../storage/persistentDataStoreFactory'
import { createCatalogCharacterStub } from '../storage/workingSetCatalog'
import {
    createPluginDatabaseAccess,
    createProductionPluginDatabaseAccess,
} from './pluginDatabaseAccess'

vi.mock('../storage/persistentDataStoreFactory', () => ({
    getPersistentDataStore: vi.fn(),
}))

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (reason?: unknown) => void
    const promise = new Promise<T>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, reject, resolve }
}

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
            type: 'character',
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
    let compatibilityProfile: 'scalable-v3' | 'maximum-compatibility' = 'scalable-v3'
    let navigationGeneration = 0
    const materializedDatabases: Database[] = []
    const authoritativeSnapshots: Array<{
        database: Database
        revision: number
        mutationGeneration?: number
    }> = []
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
    const applyCompatibilityDatabaseLite = vi.fn((_database: Record<string, unknown>) => undefined)
    const applyCompatibilityDatabase = vi.fn(async (_database: Record<string, unknown>) => undefined)
    const materializeDatabaseSnapshot = vi.fn(async () => {
        const snapshot = authoritativeSnapshots.shift()!
        return {
            ...snapshot,
            mutationGeneration: snapshot.mutationGeneration ?? 0,
        }
    })
    const prepareAuthoritativeDatabaseUpdate = vi.fn(async (
        database: Record<string, unknown>,
    ) => database)
    const replacePersistentDatabase = vi.fn(async (
        _database: Database,
        _reason: string,
        _options: {
            authoritative?: boolean
            publishOfficial?: boolean
            expectedRevision?: number
            expectedMutationGeneration?: number
        },
    ) => undefined)
    const access = createPluginDatabaseAccess({
        store,
        flushPendingData,
        getCompatibilityDatabase: () => compatibilityDatabase,
        getCompatibilityProfile: () => compatibilityProfile,
        getNavigationGeneration: () => navigationGeneration,
        applyCompatibilityDatabaseLite,
        applyCompatibilityDatabase,
        materializeDatabaseSnapshot,
        replacePersistentDatabase,
        prepareAuthoritativeDatabaseUpdate,
        snapshot: <T>(value: T) => snapshot(value) as T,
    })
    return {
        access,
        applyCompatibilityDatabase,
        applyCompatibilityDatabaseLite,
        authoritativeSnapshots,
        compatibilityDatabase,
        flushPendingData,
        materializedDatabases,
        materializeDatabaseSnapshot,
        prepareAuthoritativeDatabaseUpdate,
        replacePersistentDatabase,
        setCompatibilityProfile(profile: 'scalable-v3' | 'maximum-compatibility') {
            compatibilityProfile = profile
        },
        setNavigationGeneration(generation: number) {
            navigationGeneration = generation
        },
        snapshot,
        store,
    }
}

describe('plugin database access', () => {
    it('composes scalable API v3 queries with the shared production store', async () => {
        const harness = createHarness()
        vi.mocked(getPersistentDataStore).mockReturnValue(harness.store)
        Object.defineProperty(harness.compatibilityDatabase, 'characters', {
            get() {
                throw new Error('scalable query touched DBState.db.characters')
            },
        })
        const access = createProductionPluginDatabaseAccess({
            flushPendingData: harness.flushPendingData,
            getCompatibilityDatabase: () => harness.compatibilityDatabase,
            getCompatibilityProfile: () => 'scalable-v3',
            getNavigationGeneration: () => 0,
            applyCompatibilityDatabaseLite: harness.applyCompatibilityDatabaseLite,
            applyCompatibilityDatabase: harness.applyCompatibilityDatabase,
            materializeDatabaseSnapshot: harness.materializeDatabaseSnapshot,
            replacePersistentDatabase: harness.replacePersistentDatabase,
            prepareAuthoritativeDatabaseUpdate: harness.prepareAuthoritativeDatabaseUpdate,
            snapshot: <T>(value: T) => harness.snapshot(value) as T,
        })

        await expect(access.queryCharacters({ limit: 500 })).resolves.toEqual(characterPage)

        expect(harness.store.queryCharacters).toHaveBeenCalledWith({
            search: undefined,
            order: 'configured',
            trash: false,
            limit: 100,
            cursor: undefined,
        })
        expect(harness.snapshot).not.toHaveBeenCalled()
    })

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
            username: 'Persisted user from the same revision',
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
            username: 'Persisted user from the same revision',
            unknown: undefined,
        })
        expect(harness.flushPendingData).toHaveBeenCalledWith('plugin-full-database-snapshot')
        expect(harness.store.open).toHaveBeenCalledTimes(1)
        expect(harness.store.materializeDatabase).toHaveBeenCalledTimes(1)
    })

    it('snapshots maximum compatibility root and v2.1 character edits from one live value', async () => {
        const harness = createHarness()
        harness.setCompatibilityProfile('maximum-compatibility')
        harness.compatibilityDatabase.username = 'Live maximum user'
        harness.compatibilityDatabase.characters = [
            { chaId: 'inactive', name: 'Edited by v2.1', chats: [] },
        ] as Database['characters']

        await expect(
            harness.access.getDatabaseSnapshot('all', ['characters', 'username']),
        ).resolves.toEqual({
            characters: harness.compatibilityDatabase.characters,
            username: 'Live maximum user',
        })
        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.store.open).not.toHaveBeenCalled()
        expect(harness.store.materializeDatabase).not.toHaveBeenCalled()
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

    it('replaces scalable character updates from a detached authoritative snapshot', async () => {
        const harness = createHarness()
        const authoritative = {
            username: 'Before',
            botPresets: [{ name: 'Preserved preset', prompt: 'preset body' }],
            characters: [
                {
                    chaId: 'active',
                    name: 'Active',
                    chats: [{ id: 'active-chat', message: [{ role: 'user', data: 'keep active' }] }],
                },
                {
                    chaId: 'inactive',
                    name: 'Inactive',
                    chats: [{ id: 'inactive-chat', message: [{ role: 'char', data: 'keep inactive' }] }],
                },
            ],
        } as unknown as Database
        harness.authoritativeSnapshots.push({ database: authoritative, revision: 7 })
        const pluginCharacters = structuredClone(authoritative.characters)
        pluginCharacters[1].name = 'Edited while inactive'

        await harness.access.setDatabase(
            { characters: pluginCharacters, username: 'After', privateValue: 42 },
            ['characters', 'username'],
        )

        expect(harness.materializeDatabaseSnapshot).toHaveBeenCalledWith(
            'plugin-database-set',
        )
        expect(harness.replacePersistentDatabase).toHaveBeenCalledTimes(1)
        const [candidate, reason, options] = harness.replacePersistentDatabase.mock.calls[0]
        expect(reason).toBe('plugin-database-set')
        expect(options).toEqual({
            authoritative: true,
            publishOfficial: true,
            expectedRevision: 7,
            expectedMutationGeneration: 0,
        })
        expect(candidate.username).toBe('After')
        expect(candidate.characters[1].name).toBe('Edited while inactive')
        expect(candidate.characters[0].chats[0].message[0].data).toBe('keep active')
        expect(candidate.characters[1].chats[0].message[0].data).toBe('keep inactive')
        expect(candidate.botPresets).toEqual(authoritative.botPresets)
        expect(candidate.pluginCustomStorage.privateValue).toBe(42)
        expect(candidate).not.toHaveProperty('privateValue')
        expect(candidate).not.toBe(authoritative)
        expect(candidate.characters).not.toBe(pluginCharacters)
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
    })

    it('waits for authoritative replacement so scalable live reprojection is observable', async () => {
        const harness = createHarness()
        harness.authoritativeSnapshots.push({
            database: {
                characters: [{ chaId: 'inactive', name: 'Before', chats: [] }],
                botPresets: [],
            } as unknown as Database,
            revision: 8,
        })
        vi.mocked(harness.replacePersistentDatabase).mockImplementation(async () => {
            harness.compatibilityDatabase.characters = [
                { chaId: 'inactive', name: 'Projected', chats: [] },
            ] as never
        })

        await harness.access.setDatabase(
            { characters: [{ chaId: 'inactive', name: 'After', chats: [] }] },
            ['characters'],
        )

        expect(harness.compatibilityDatabase.characters[0].name).toBe('Projected')
    })

    it('rejects synchronous scalable character updates before live mutation', () => {
        const harness = createHarness()

        expect(() => harness.access.setDatabaseLite(
            { characters: [{ chaId: 'inactive', chats: [] }] },
            ['characters'],
        )).toThrow(/async setDatabase.*maximum-compatibility/i)
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('rejects incomplete scalable character values before materialization', async () => {
        const harness = createHarness()

        await expect(harness.access.setDatabase(
            { characters: [{ chaId: 'catalog-stub', chats: [{ id: 'summary-only' }] }] },
            ['characters'],
        )).rejects.toThrow(/not fully hydrated/i)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
    })

    it('rejects live catalog stubs instead of merging them into the full snapshot', async () => {
        const harness = createHarness()
        const stub = createCatalogCharacterStub({
            id: 'catalog-stub',
            name: 'Catalog stub',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 3,
            type: 'character',
        })

        await expect(harness.access.setDatabase(
            { characters: [stub] },
            ['characters'],
        )).rejects.toThrow(/catalog working-set stubs/i)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('keeps synchronous root-only setters on the observer path and serializes async updates', async () => {
        const harness = createHarness()
        const liteUpdate = { username: 'Lite' }
        const asyncUpdate = { username: 'Async' }
        harness.authoritativeSnapshots.push({
            database: {
                username: 'Before',
                characters: [],
                botPresets: [],
            } as unknown as Database,
            revision: 9,
        })

        harness.access.setDatabaseLite(liteUpdate, ['characters', 'username'])
        await harness.access.setDatabase(asyncUpdate, ['characters', 'username'])

        expect(harness.applyCompatibilityDatabaseLite).toHaveBeenCalledWith(liteUpdate)
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).toHaveBeenCalledWith('plugin-database-set')
        expect(harness.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ username: 'Async' }),
            'plugin-database-set',
            expect.objectContaining({ expectedRevision: 9 }),
        )
    })

    it('keeps maximum-compatibility character setters on the live full database paths', async () => {
        const harness = createHarness()
        harness.setCompatibilityProfile('maximum-compatibility')
        const liteUpdate = { characters: [{ chaId: 'lite', chats: [] }] }
        const asyncUpdate = { characters: [{ chaId: 'async', chats: [] }] }

        harness.access.setDatabaseLite(liteUpdate, ['characters'])
        await harness.access.setDatabase(asyncUpdate, ['characters'])

        expect(harness.applyCompatibilityDatabaseLite).toHaveBeenCalledWith(liteUpdate)
        expect(harness.applyCompatibilityDatabase).toHaveBeenCalledWith(asyncUpdate)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('leaves live and authoritative snapshots unchanged when scalable replacement fails', async () => {
        const harness = createHarness()
        const liveBefore = structuredClone(harness.compatibilityDatabase)
        const authoritative = {
            username: 'Before',
            botPresets: [{ name: 'Preset' }],
            characters: [{ chaId: 'inactive', name: 'Before', chats: [] }],
        } as unknown as Database
        const authoritativeBefore = structuredClone(authoritative)
        harness.authoritativeSnapshots.push({ database: authoritative, revision: 10 })
        harness.replacePersistentDatabase.mockRejectedValueOnce(new Error('replacement failed'))

        await expect(harness.access.setDatabase(
            { characters: [{ chaId: 'inactive', name: 'After', chats: [] }] },
            ['characters'],
        )).rejects.toThrow('replacement failed')

        expect(harness.compatibilityDatabase).toEqual(liveBefore)
        expect(authoritative).toEqual(authoritativeBefore)
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
    })

    it('allows only one of two setters materialized from the same revision to commit', async () => {
        const harness = createHarness()
        const base = {
            username: 'Before',
            characters: [],
            botPresets: [{ name: 'Preset' }],
        } as unknown as Database
        harness.authoritativeSnapshots.push(
            { database: structuredClone(base), revision: 20 },
            { database: structuredClone(base), revision: 20 },
        )
        let currentRevision = 20
        harness.replacePersistentDatabase.mockImplementation(async (
            _database,
            _reason,
            options,
        ) => {
            if (options.expectedRevision !== currentRevision) {
                throw new Error('revision-conflict')
            }
            currentRevision++
        })

        const outcomes = await Promise.allSettled([
            harness.access.setDatabase({ username: 'First' }, ['username']),
            harness.access.setDatabase({ username: 'Second' }, ['username']),
        ])

        expect(outcomes.filter((outcome) => outcome.status === 'fulfilled')).toHaveLength(1)
        expect(outcomes.filter((outcome) => outcome.status === 'rejected')).toHaveLength(1)
        expect(harness.replacePersistentDatabase).toHaveBeenCalledTimes(2)
    })

    it('does not overwrite a disjoint edit committed after materialization', async () => {
        const harness = createHarness()
        const storeDatabase = {
            username: 'Before',
            characters: [{ chaId: 'char', name: 'Before', chats: [] }],
            botPresets: [],
        } as unknown as Database
        harness.authoritativeSnapshots.push({
            database: structuredClone(storeDatabase),
            revision: 30,
        })
        storeDatabase.characters[0].name = 'Concurrent character edit'
        harness.replacePersistentDatabase.mockRejectedValueOnce(new Error('revision-conflict'))

        await expect(harness.access.setDatabase(
            { username: 'Plugin root edit' },
            ['username'],
        )).rejects.toThrow('revision-conflict')

        expect(harness.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.anything(),
            'plugin-database-set',
            expect.objectContaining({ expectedRevision: 30 }),
        )
        expect(storeDatabase.characters[0].name).toBe('Concurrent character edit')
        expect(storeDatabase.username).toBe('Before')
    })

    it('uses the revision paired with the materialized database snapshot', async () => {
        const harness = createHarness()
        let currentRevision = 50
        harness.authoritativeSnapshots.push({
            database: {
                username: 'Before',
                characters: [],
                botPresets: [],
            } as unknown as Database,
            revision: currentRevision,
        })
        harness.materializeDatabaseSnapshot.mockImplementationOnce(async () => {
            const snapshot = harness.authoritativeSnapshots.shift()!
            currentRevision++
            return {
                ...snapshot,
                mutationGeneration: snapshot.mutationGeneration ?? 0,
            }
        })

        await harness.access.setDatabase({ username: 'After' }, ['username'])

        expect(currentRevision).toBe(51)
        expect(harness.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ username: 'After' }),
            'plugin-database-set',
            expect.objectContaining({ expectedRevision: 50 }),
        )
    })

    it('rejects replacement after an unflushed live mutation changes generation', async () => {
        const harness = createHarness()
        const liveBefore = structuredClone(harness.compatibilityDatabase)
        let currentMutationGeneration = 70
        harness.authoritativeSnapshots.push({
            database: {
                username: 'Before',
                characters: [],
                botPresets: [],
            } as unknown as Database,
            revision: 60,
            mutationGeneration: currentMutationGeneration,
        })
        harness.materializeDatabaseSnapshot.mockImplementationOnce(async () => {
            const snapshot = harness.authoritativeSnapshots.shift()!
            currentMutationGeneration++
            return {
                ...snapshot,
                mutationGeneration: snapshot.mutationGeneration!,
            }
        })
        harness.replacePersistentDatabase.mockImplementationOnce(async (
            _database,
            _reason,
            options,
        ) => {
            if (options.expectedMutationGeneration !== currentMutationGeneration) {
                throw new Error('mutation-generation-conflict')
            }
        })

        await expect(harness.access.setDatabase(
            { username: 'Plugin edit' },
            ['username'],
        )).rejects.toThrow('mutation-generation-conflict')

        expect(harness.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.anything(),
            'plugin-database-set',
            expect.objectContaining({
                expectedRevision: 60,
                expectedMutationGeneration: 70,
            }),
        )
        expect(harness.compatibilityDatabase).toEqual(liveBefore)
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
    })

    it('rejects a scalable update when confirmation outlives profile or navigation state', async () => {
        const harness = createHarness()
        const confirmation = deferred<Record<string, unknown>>()
        harness.prepareAuthoritativeDatabaseUpdate.mockReturnValueOnce(confirmation.promise)
        const pending = harness.access.setDatabase(
            { plugins: [], username: 'After' },
            ['plugins', 'username'],
        )

        harness.setCompatibilityProfile('maximum-compatibility')
        harness.setNavigationGeneration(1)
        confirmation.resolve({ plugins: [], username: 'After' })

        await expect(pending).rejects.toThrow(/became stale/i)
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
    })

    it('does not publish a maximum-compatibility capture after switching profiles', async () => {
        const harness = createHarness()
        harness.setCompatibilityProfile('maximum-compatibility')
        const confirmation = deferred<Record<string, unknown>>()
        harness.prepareAuthoritativeDatabaseUpdate.mockReturnValueOnce(confirmation.promise)
        const pending = harness.access.setDatabase(
            { plugins: [], username: 'After' },
            ['plugins', 'username'],
        )

        harness.setCompatibilityProfile('scalable-v3')
        confirmation.resolve({ plugins: [], username: 'After' })

        await expect(pending).rejects.toThrow(/became stale/i)
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
        expect(harness.materializeDatabaseSnapshot).not.toHaveBeenCalled()
    })

    it('merges explicit and extra custom storage independently of input key order', async () => {
        const first = createHarness()
        const second = createHarness()
        const base = {
            characters: [],
            botPresets: [],
            pluginCustomStorage: { existing: 'kept only without explicit replacement' },
        } as unknown as Database
        first.authoritativeSnapshots.push({ database: structuredClone(base), revision: 40 })
        second.authoritativeSnapshots.push({ database: structuredClone(base), revision: 40 })
        const explicit = { shared: 'explicit', explicitOnly: 'value' }
        const firstUpdate = {
            pluginCustomStorage: explicit,
            shared: 'extra',
            extraOnly: 2,
        }
        const secondUpdate = {
            extraOnly: 2,
            shared: 'extra',
            pluginCustomStorage: explicit,
        }

        await first.access.setDatabase(firstUpdate, ['pluginCustomStorage'])
        await second.access.setDatabase(secondUpdate, ['pluginCustomStorage'])

        const firstCandidate = first.replacePersistentDatabase.mock.calls[0][0]
        const secondCandidate = second.replacePersistentDatabase.mock.calls[0][0]
        expect(firstCandidate.pluginCustomStorage).toEqual({
            explicitOnly: 'value',
            extraOnly: 2,
            shared: 'extra',
        })
        expect(secondCandidate.pluginCustomStorage).toEqual(firstCandidate.pluginCustomStorage)
    })

    it.each([
        null,
        [],
        new Date(),
        Object.create({ inherited: true }),
    ])('rejects non-plain database updates', async (update) => {
        const harness = createHarness()

        await expect(harness.access.setDatabase(
            update as unknown as Record<string, unknown>,
            ['username'],
        )).rejects.toThrow(/plain record/i)
        expect(() => harness.access.setDatabaseLite(
            update as unknown as Record<string, unknown>,
            ['username'],
        )).toThrow(/plain record/i)
        expect(harness.applyCompatibilityDatabase).not.toHaveBeenCalled()
        expect(harness.applyCompatibilityDatabaseLite).not.toHaveBeenCalled()
    })

    it.each(['__proto__', 'prototype', 'constructor'])(
        'rejects dangerous custom key %s',
        async (key) => {
            const harness = createHarness()
            const update = JSON.parse(`{"${key}":"blocked"}`) as Record<string, unknown>

            await expect(harness.access.setDatabase(update, ['username'])).rejects.toThrow(
                /unsafe plugin database key/i,
            )
            expect(() => harness.access.setDatabaseLite(update, ['username'])).toThrow(
                /unsafe plugin database key/i,
            )
        },
    )

    it('rejects invalid explicit plugin custom storage', async () => {
        const harness = createHarness()
        const update = { pluginCustomStorage: null }

        await expect(harness.access.setDatabase(update, ['pluginCustomStorage'])).rejects.toThrow(
            /pluginCustomStorage must be a plain record/i,
        )
        expect(() => harness.access.setDatabaseLite(update, ['pluginCustomStorage'])).toThrow(
            /pluginCustomStorage must be a plain record/i,
        )
    })
})
