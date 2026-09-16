import { describe, expect, it, vi } from 'vitest'
import type { Chat, Database } from './database.svelte'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
} from './persistentDataRuntime'
import type {
    CharacterDetail,
    CharacterPage,
    CharacterQuery,
    ConversationPage,
    ConversationQuery,
    PersistentRevisionLease,
    PersistentRevisionReader,
    PersistentDataStore,
    PersistentRoot,
    PluginStorageCatalog,
    PresetCatalog,
} from './persistentDataStore'
import {
    getCatalogCharacterMetadata,
    isCatalogCharacterStub,
    isCatalogPresetWorkingSet,
    materializePinnedCompatibilityDatabase,
    projectPinnedScalableWorkingSet,
    projectScalableWorkingSetAtRevision,
} from './workingSetCatalog'

function clone<T>(value: T): T {
    return JSON.parse(JSON.stringify(value)) as T
}

function fixtureDatabase(): Database {
    return {
        username: 'Pinned profile',
        botPresetsId: 1,
        botPresets: [
            { name: 'Inactive', mainPrompt: 'inactive preset body' },
            { name: 'Active', mainPrompt: 'active preset body' },
        ],
        pluginCustomStorage: JSON.parse(
            '{"0":0,"zeta":{"memory":"last string key inserted first"},' +
            '"__proto__":{"safe":true},"alpha":false}',
        ),
        characters: [
            {
                type: 'character',
                chaId: 'member-a',
                name: 'Alpha',
                personality: 'alpha detail',
                chats: [{
                    id: 'member-chat',
                    name: 'Member chat',
                    message: [{ role: 'assistant', data: 'member history' }],
                }],
            },
            {
                type: 'character',
                chaId: 'member-b',
                name: 'Beta',
                personality: 'beta detail',
                chats: [],
            },
            {
                type: 'group',
                chaId: 'group-a',
                name: 'Group',
                characters: ['member-a', 'member-b'],
                characterTalks: [1, 1],
                characterActive: [true, true],
                chatPage: 1,
                chats: [
                    {
                        id: 'group-old',
                        name: 'Old group chat',
                        message: [{ role: 'assistant', data: 'old group history' }],
                    },
                    {
                        id: 'group-selected',
                        name: 'Selected group chat',
                        message: [{ role: 'assistant', data: 'selected group history' }],
                    },
                ],
            },
            {
                type: 'character',
                chaId: 'inactive',
                name: 'Inactive',
                personality: 'must not remain resident',
                chats: [{
                    id: 'inactive-chat',
                    name: 'Inactive chat',
                    message: [{ role: 'user', data: 'large inactive history' }],
                }],
            },
        ],
    } as unknown as Database
}

function createReader(
    database: Database,
    revision = 7,
    onRelease: () => Promise<void> = async () => undefined,
): PersistentRevisionLease {
    const {
        characters,
        botPresets,
        pluginCustomStorage,
        ...root
    } = database
    const characterSummaries = characters.map((character, configuredIndex) => ({
        id: character.chaId,
        name: character.name,
        image: character.image,
        configuredIndex,
        recentAt: character.lastInteraction ?? 0,
        trashed: character.trashTime !== undefined,
        conversationCount: character.chats.length,
        type: character.type,
        creatorNotes: character.creatorNotes,
        trashTime: character.trashTime,
    }))
    const presetCatalog: PresetCatalog = {
        revision,
        items: botPresets.map((preset, configuredIndex) => ({
            id: String(configuredIndex),
            name: preset.name ?? '',
            image: preset.image,
            configuredIndex,
        })),
    }
    const pluginCatalog: PluginStorageCatalog = {
        revision,
        items: Object.keys(pluginCustomStorage).map((key) => ({
            owner: 'test-plugin',
            key,
            byteSize: JSON.stringify(pluginCustomStorage[key]).length,
        })),
    }
    const page = <T>(values: T[], cursor?: string): { items: T[]; nextCursor?: string } => {
        const offset = Number(cursor ?? 0)
        return {
            items: values.slice(offset, offset + 1),
            ...(offset + 1 < values.length ? { nextCursor: String(offset + 1) } : {}),
        }
    }
    const reader: PersistentRevisionLease = {
        revision,
        readRoot: async () => ({ revision, value: clone(root) as PersistentRoot }),
        queryPresets: async () => clone(presetCatalog),
        readPreset: vi.fn(async (id) => {
            const preset = botPresets[Number(id)]
            return preset ? { revision, value: clone(preset) } : null
        }),
        queryCharacters: async (query: CharacterQuery): Promise<CharacterPage> => {
            const values = characterSummaries.filter((summary) => summary.trashed === query.trash)
            return { revision, ...page(values, query.cursor) }
        },
        readCharacterSummary: vi.fn(async (id: string) =>
            characterSummaries.find((summary) => summary.id === id) ?? null),
        readCharacter: async (id) => {
            const character = characters.find((candidate) => candidate.chaId === id)
            if (!character) return null
            const { chats: _chats, ...detail } = character
            return { revision, value: clone(detail) as CharacterDetail }
        },
        queryConversations: vi.fn(async (
            query: ConversationQuery,
        ): Promise<ConversationPage> => {
            const character = characters.find(
                (candidate) => candidate.chaId === query.characterId,
            )
            const summaries = (character?.chats ?? []).map((conversation, configuredIndex) => ({
                id: conversation.id!,
                characterId: query.characterId,
                name: conversation.name ?? '',
                folderId: conversation.folderId,
                bindedPersona: conversation.bindedPersona,
                configuredIndex,
                recentAt: conversation.lastDate ?? 0,
                messageCount: conversation.message.length,
            }))
            return { revision, ...page(summaries, query.cursor) }
        }),
        readConversation: vi.fn(async (characterId, conversationId) => {
            const conversation = characters
                .find((candidate) => candidate.chaId === characterId)
                ?.chats.find((candidate) => candidate.id === conversationId)
            return conversation
                ? { revision, value: clone(conversation) as Chat }
                : null
        }),
        readConversationMetadata: vi.fn(async () => null),
        readConversationWindow: async () => null,
        queryPluginStorage: vi.fn(async () => clone(pluginCatalog)),
        readPluginStorage: async (_owner, key) => Object.hasOwn(pluginCustomStorage, key)
            ? { revision, value: clone(pluginCustomStorage[key]) }
            : null,
        readAssetAlias: async () => null,
        readAssetAliasesByKeys: async () => ({ revision, value: [] }),
        listAssetAliases: async () => ({ revision, items: [] }),
        readAssetRepositoryAuthority: async () => ({
            revision,
            value: { format: 'legacy' },
        }),
        readAssetOwnerHead: async () => null,
        release: onRelease,
    }
    return reader
}

describe('paged profile projections', () => {
    it('constructs the single complete compatibility database without a full clone', async () => {
        const fixture = fixtureDatabase()
        const reader = createReader(fixture)
        const fullClone = vi.spyOn(globalThis, 'structuredClone').mockImplementation(() => {
            throw new Error('full structured clone is forbidden')
        })

        const result = await materializePinnedCompatibilityDatabase(reader)

        expect(result).toEqual(fixture)
        expect(result).not.toBe(fixture)
        expect(result.characters[2].chats[1].message[0].data).toBe('selected group history')
        expect(Object.keys(result.pluginCustomStorage)).toEqual([
            '0',
            'zeta',
            '__proto__',
            'alpha',
        ])
        expect(result.pluginCustomStorage['0']).toBe(0)
        expect(Object.hasOwn(result.pluginCustomStorage, '__proto__')).toBe(true)
        expect(result.pluginCustomStorage.__proto__).toEqual({ safe: true })
        expect(fullClone).not.toHaveBeenCalled()
    })

    it('projects only the selected conversation and group member detail pins from pages', async () => {
        const reader = createReader(fixtureDatabase())
        const fullClone = vi.spyOn(globalThis, 'structuredClone').mockImplementation(() => {
            throw new Error('full structured clone is forbidden')
        })

        const result = await projectPinnedScalableWorkingSet(reader, {
            selectedCharacterId: 'group-a',
            selectedConversationId: 'group-selected',
            activeCharacterIds: new Set(['group-a', 'member-a', 'member-b']),
        })

        expect(result.username).toBe('Pinned profile')
        expect(result.pluginCustomStorage).toEqual({})
        expect(isCatalogPresetWorkingSet(result.botPresets)).toBe(true)
        expect(result.botPresets[0]).toEqual({ name: 'Inactive' })
        expect(result.botPresets[1].mainPrompt).toBe('active preset body')
        expect(result.characters[0].personality).toBe('alpha detail')
        expect(result.characters[0].chats).toEqual([])
        expect(getCatalogCharacterMetadata(result.characters[0])).toMatchObject({
            configuredIndex: 0,
            conversationCount: 1,
            residency: 'detail',
        })
        expect(result.characters[1].personality).toBe('beta detail')
        expect(result.characters[1].chats).toEqual([])
        expect(result.characters[2].type).toBe('group')
        expect(result.characters[2].chats[0].message).toEqual([])
        expect(result.characters[2].chats[1].message[0].data).toBe('selected group history')
        expect(result.characters[2].chatPage).toBe(1)
        expect(isCatalogCharacterStub(result.characters[3])).toBe(true)
        expect(result.characters[3]).not.toHaveProperty('personality')
        expect(reader.readPreset).toHaveBeenCalledTimes(1)
        expect(reader.readPreset).toHaveBeenCalledWith('1')
        expect(reader.queryPluginStorage).not.toHaveBeenCalled()
        expect(vi.mocked(reader.queryConversations).mock.calls.filter(
            ([query]) => query.characterId === 'member-a' || query.characterId === 'member-b',
        )).toEqual([])
        expect(reader.readConversation).toHaveBeenCalledOnce()
        expect(reader.readConversation).toHaveBeenCalledWith('group-a', 'group-selected')
        expect(fullClone).not.toHaveBeenCalled()
    })

    it('releases scalable revision leases on success and read failure', async () => {
        const successRelease = vi.fn()
            .mockRejectedValueOnce(new Error('transient lease cleanup failure'))
            .mockResolvedValueOnce(undefined)
        const successStore = {
            acquireRevision: vi.fn(async () => createReader(
                fixtureDatabase(),
                7,
                successRelease,
            )),
            materializeDatabase: vi.fn(async () => {
                throw new Error('legacy materializer must not be called')
            }),
        }

        await expect(projectScalableWorkingSetAtRevision(successStore, 7, {
            selectedCharacterId: 'group-a',
            selectedConversationId: 'group-selected',
        })).resolves.toMatchObject({ pluginCustomStorage: {} })

        const primary = new Error('paged projection failed')
        const failedRelease = vi.fn(async () => {
            throw new Error('lease cleanup failed')
        })
        const failedLease = createReader(fixtureDatabase(), 7, failedRelease)
        failedLease.readRoot = vi.fn(async () => {
            throw primary
        })
        const failedStore = {
            acquireRevision: vi.fn(async () => failedLease),
            materializeDatabase: vi.fn(async () => {
                throw new Error('legacy materializer must not be called')
            }),
        }

        await expect(projectScalableWorkingSetAtRevision(failedStore, 7, {
            selectedCharacterId: 'group-a',
        })).rejects.toBe(primary)
        expect(successStore.materializeDatabase).not.toHaveBeenCalled()
        expect(failedStore.materializeDatabase).not.toHaveBeenCalled()
        expect(successRelease).toHaveBeenCalledTimes(2)
        expect(failedRelease).toHaveBeenCalledTimes(2)
    })
})

function createRuntimeStore(
    authoritative: Database,
    lease: PersistentRevisionLease,
    revision = 7,
): PersistentDataStore {
    return {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({
            revision,
            value: capturePersistentRoot(authoritative),
        })),
        acquireRevision: vi.fn(async () => lease),
        commit: vi.fn(async () => ({ revision: revision + 1 })),
        materializeDatabase: vi.fn(async () => {
            throw new Error('legacy materializer must not be called')
        }),
    } as unknown as PersistentDataStore
}

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

describe('scalable profile return', () => {
    it('publishes the authoritative bounded pins and restores stable selection', async () => {
        const authoritative = fixtureDatabase()
        let database = clone(authoritative)
        const release = vi.fn()
            .mockRejectedValueOnce(new Error('transient lease cleanup failure'))
            .mockResolvedValueOnce(undefined)
        const store = createRuntimeStore(authoritative, createReader(authoritative, 7, release))
        const restoreSelection = vi.fn()
        const replaceDatabase = vi.fn((replacement: Database) => {
            database = replacement
        })
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePluginStorage: () => database.pluginCustomStorage,
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters.find(
                    (character) => character.chaId === 'group-a',
                ) ?? null,
                captureCharacter: (id) => database.characters.find(
                    (character) => character.chaId === id,
                ) ?? null,
                getSelectedCharacterId: () => 'group-a',
                getSelectedConversationId: () => 'group-selected',
                replaceDatabase,
                restoreSelection,
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            clock: {
                setTimeout: vi.fn(() => 1),
                clearTimeout: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)

        await expect(runtime.releaseInactiveWorkingSet()).resolves.toBe(true)

        expect(store.materializeDatabase).not.toHaveBeenCalled()
        expect(store.acquireRevision).toHaveBeenCalledWith(7)
        expect(release).toHaveBeenCalledTimes(2)
        expect(database.pluginCustomStorage).toEqual({})
        expect(database.characters[0].personality).toBe('alpha detail')
        expect(database.characters[1].personality).toBe('beta detail')
        expect(database.characters[2].chats[0].message).toEqual([])
        expect(database.characters[2].chats[1].message[0].data).toBe(
            'selected group history',
        )
        expect(isCatalogCharacterStub(database.characters[3])).toBe(true)
        expect(restoreSelection).toHaveBeenCalledWith('group-a', 'group-selected')
    })

    it('flushes a pending save before pinning the committed revision', async () => {
        const initial = fixtureDatabase()
        const committed = clone(initial)
        committed.username = 'Pending edit'
        let database = clone(initial)
        const store = createRuntimeStore(initial, createReader(committed, 8))
        store.acquireRevision = vi.fn(async (revision) => createReader(committed, revision))
        const replaceDatabase = vi.fn((replacement: Database) => {
            database = replacement
        })
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePluginStorage: () => database.pluginCustomStorage,
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[2],
                captureCharacter: (id) => database.characters.find(
                    (character) => character.chaId === id,
                ) ?? null,
                getSelectedCharacterId: () => 'group-a',
                getSelectedConversationId: () => 'group-selected',
                replaceDatabase,
                publishCharacter: vi.fn(),
                publishConversation: vi.fn(),
            },
            clock: {
                setTimeout: vi.fn(() => 1),
                clearTimeout: vi.fn(),
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        database = { ...database, username: 'Pending edit' }
        runtime.markPersistentDataDirty(1)

        await expect(runtime.releaseInactiveWorkingSet()).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledOnce()
        expect(store.acquireRevision).toHaveBeenCalledWith(8)
        expect(database.username).toBe('Pending edit')
    })

    it.each(['navigation', 'mutation', 'selection'] as const)(
        'does not publish when %s changes during a paged load',
        async (change) => {
            const authoritative = fixtureDatabase()
            let database = clone(authoritative)
            let selectedCharacterId = 'group-a'
            const page = deferred<CharacterPage>()
            const leaseRelease = vi.fn(async () => undefined)
            const lease = createReader(authoritative, 7, leaseRelease)
            const originalQuery = lease.queryCharacters.bind(lease)
            let queryCount = 0
            lease.queryCharacters = vi.fn(async (query) => {
                queryCount++
                if (queryCount === 1) return page.promise
                return originalQuery(query)
            })
            const store = createRuntimeStore(authoritative, lease)
            const replaceDatabase = vi.fn()
            const runtime = createPersistentDataRuntime({
                store,
                state: {
                    captureRoot: () => capturePersistentRoot(database),
                    capturePluginStorage: () => database.pluginCustomStorage,
                    capturePresets: () => database.botPresets,
                    captureSelectedCharacter: () => database.characters[2],
                    captureCharacter: (id) => database.characters.find(
                        (character) => character.chaId === id,
                    ) ?? null,
                    getSelectedCharacterId: () => selectedCharacterId,
                    getSelectedConversationId: () => 'group-selected',
                    replaceDatabase,
                    publishCharacter: vi.fn(),
                    publishConversation: vi.fn(),
                },
                clock: {
                    setTimeout: vi.fn(() => 1),
                    clearTimeout: vi.fn(),
                },
                prepareDatabase: async (value) => value,
            })
            await runtime.initializeActiveWorkingSet(database)

            const result = runtime.releaseInactiveWorkingSet()
            await vi.waitFor(() => expect(lease.queryCharacters).toHaveBeenCalledOnce())
            if (change === 'navigation') runtime.invalidateNavigation()
            else if (change === 'mutation') {
                database = { ...database, username: 'edited while loading' }
                runtime.markPersistentDataDirty(1)
                await runtime.flushPendingData('edit-during-profile-load')
            } else selectedCharacterId = 'inactive'
            page.resolve(await originalQuery({
                order: 'configured',
                trash: false,
                limit: 128,
            }))

            await expect(result).resolves.toBe(false)
            expect(replaceDatabase).not.toHaveBeenCalled()
            expect(leaseRelease).toHaveBeenCalledOnce()
        },
    )
})