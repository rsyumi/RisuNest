const PLUGIN_ACCESS_OWNER = 'test-plugin'
import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { RevisionConflictError } from '../persistentDataStore'
import {
    capturePersistentRoot,
    capturePersistentPresets,
    captureSelectedPersistentCharacter,
    createPersistentDataRuntime,
    publishPersistentCharacterMutationToWorkingSet,
    publishPersistentConversationReplacementToWorkingSet,
    type PersistentDataRuntimeStateAdapter,
} from '../persistentDataRuntime'
import { createCatalogPresetWorkingSet } from '../workingSetCatalog'
import { createPluginDatabaseAccess } from '../../plugins/pluginDatabaseAccess'
import { WorkingSetResidencyRegistry } from '../workingSetResidency'
import { decodeRisuSave } from '../risuSave'
import { streamRisuSaveFromStore } from '../risuSaveStoreAdapter'
import {
    createPersistentSaveObserverInstallation,
    installPersistentSaveNotifications,
} from '../persistentSaveNotifications'

vi.mock('../database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No live database in runtime integration tests')
    },
    presetTemplate: {},
}))
vi.mock('../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

function makeDatabase(): Database {
    return {
        username: 'Fixture',
        characters: [
            {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 0,
                chats: [
                    {
                        id: 'chat-a',
                        name: 'First',
                        message: [],
                    },
                ],
            },
        ],
    } as unknown as Database
}

function makeAdapter(database: Database): PersistentDataRuntimeStateAdapter & {
    current(): Database
} {
    let workingCopy = structuredClone(database)
    const residency = new WorkingSetResidencyRegistry()
    residency.setEvictionAllowed(false)
    return {
        current: () => workingCopy,
        captureRoot: () => {
            const { characters: _characters, botPresets: _botPresets, ...root } = structuredClone(workingCopy)
            return root
        },
        capturePresets: () => capturePersistentPresets(workingCopy),
        captureSelectedCharacter: () => structuredClone(workingCopy.characters[0] ?? null),
        captureCharacter: (id) => {
            const character = workingCopy.characters.find((item) => item.chaId === id)
            return character ?? null
        },
        getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
        replaceDatabase: (replacement) => {
            workingCopy = structuredClone(replacement)
        },
        publishPresetWorkingSet: ({ revision, root, presets }) => {
            Object.assign(workingCopy, root)
            const catalog = {
                revision,
                items: presets.map((preset, configuredIndex) => ({
                    id: String(configuredIndex),
                    configuredIndex,
                    name: preset.name ?? '',
                    image: preset.image,
                })),
            }
            const active = catalog.items[root.botPresetsId]
            workingCopy.botPresets = createCatalogPresetWorkingSet(catalog, active ? {
                summary: active,
                value: presets[active.configuredIndex],
            } : null)
        },
        publishCharacter: (character) => {
            const index = workingCopy.characters.findIndex((item) => item.chaId === character.chaId)
            workingCopy.characters[index] = structuredClone(character)
        },
        publishCharacterMutation: (result) => {
            publishPersistentCharacterMutationToWorkingSet(
                workingCopy,
                result,
                residency,
                0,
                vi.fn(),
            )
        },
        publishConversation: (characterId, conversation) => {
            const character = workingCopy.characters.find((item) => item.chaId === characterId)!
            const index = character.chats.findIndex((chat) => chat.id === conversation.id)
            character.chats[index] = structuredClone(conversation)
            character.chatPage = index
        },
        publishConversationReplacement: (result) => {
            publishPersistentConversationReplacementToWorkingSet(workingCopy, result)
        },
    }
}

function makeStore(name: string) {
    return new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
}

const rendererOwnedDebounceClock = {
    setTimeout: () => Symbol('renderer-owned-debounce'),
    clearTimeout: () => undefined,
}

async function concatenate(chunks: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
    const values: Uint8Array[] = []
    let length = 0
    for await (const chunk of chunks) {
        values.push(chunk)
        length += chunk.length
    }
    const result = new Uint8Array(length)
    let offset = 0
    for (const value of values) {
        result.set(value, offset)
        offset += value.length
    }
    return result
}

describe('persistent production runtime', () => {
    it('refreshes an owned whole-character publication even when official publish rejects', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-plugin-official-failure-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const officialError = new Error('official publish rejected')
        const publish = vi.fn(async () => {
            throw officialError
        })
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            officialPublisher: {
                pin: vi.fn(async () => ({
                    publish,
                    dispose: vi.fn(async () => undefined),
                })),
            },
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const oldSession = runtime.getActiveConversationSession()!
        const access = createPluginDatabaseAccess({
        owner: PLUGIN_ACCESS_OWNER,
            store,
            flushPendingData: (reason) => runtime.flushPendingData(reason),
            getCompatibilityDatabase: () => adapter.current(),
            getSelectedCharacterId: () => adapter.current().characters[0]?.chaId ?? null,
            captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
            acquireCompleteConversation: (reason, target) =>
                runtime.acquireCompleteConversation(reason, target),
            refreshSelectedConversationAfterReplacement: (target, expectedSession) =>
                runtime.refreshSelectedConversationAfterReplacement(target, expectedSession),
            replacePersistentCompleteCharacter: (characterId, reason, mutate, options) =>
                runtime.replacePersistentCompleteCharacter(characterId, reason, mutate, options),
            replacePersistentConversation: (
                characterId,
                conversationId,
                reason,
                replacement,
                options,
            ) => runtime.replacePersistentConversation(
                characterId,
                conversationId,
                reason,
                replacement,
                options,
            ),
            reportIdentityReplacementRejected: vi.fn(),
            getNavigationGeneration: () => runtime.getNavigationGeneration(),
            applyCompatibilityDatabaseLite: vi.fn(),
            readPluginStorageSnapshot: vi.fn(async () => ({})),
            mutatePluginStorage: vi.fn(),
            invalidatePluginStorage: vi.fn(),
            getStorageAuthorityEpoch: () => runtime.getStorageAuthorityEpoch(),
            assertPersistentMutationAllowed: (epoch) => runtime.assertPersistentMutationAllowed(epoch),
            materializeDatabaseSnapshot: vi.fn(),
            replacePersistentDatabase: vi.fn(),
            snapshot: structuredClone,
        })
        const replacement = structuredClone(adapter.current().characters[0])
        replacement.name = 'Locally published replacement'
        // Metadata-only publication deliberately retains the live session.
        // Replace message content to exercise the whole-conversation refresh contract.
        replacement.chats[0].message.push({
            role: 'user',
            data: 'Locally published synthetic message',
            chatId: 'published-message',
        })

        await expect(access.setCurrentCharacter(replacement, {
            pluginName: 'official-failure-plugin',
            signal: new AbortController().signal,
        })).resolves.toBeUndefined()
        expect(publish).not.toHaveBeenCalled()
        expect(runtime.hasPendingOfficialPublication()).toBe(true)

        const resident = adapter.current().characters[0]
        expect(resident.name).toBe('Locally published replacement')
        expect(resident.chats[0].message).toEqual(replacement.chats[0].message)
        expect(runtime.revision).toBe(2)
        const refreshedSession = runtime.getActiveConversationSession()
        expect(refreshedSession).not.toBeNull()
        expect(refreshedSession).not.toBe(oldSession)
        expect(refreshedSession!.matchesConversation(
            resident.chaId,
            resident.chats[resident.chatPage ?? 0],
        )).toBe(true)
        expect(oldSession.isActive).toBe(false)
        expect(runtime.captureSelectedConversationTarget()).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
            storeRevision: 2,
        })
        await expect(runtime.publishCurrentOfficialRevision()).rejects.toBe(officialError)
        expect(publish).toHaveBeenCalledOnce()
        expect(runtime.revision).toBe(2)
        expect(runtime.hasPendingOfficialPublication()).toBe(true)
    })

    it('persists selected, inactive character, and inactive chat replacements across restart', async () => {
        const databaseName = `runtime-scoped-replacement-${crypto.randomUUID()}`
        const database = makeDatabase()
        database.characters[0].chats.push({
            id: 'chat-a-2', name: 'Selected sibling', message: [],
        } as any)
        database.characters.push({
            type: 'character',
            chaId: 'char-b',
            name: 'Beta',
            chatPage: 0,
            chats: [
                { id: 'chat-b', name: 'Inactive target', note: '', localLore: [], message: [] },
                { id: 'chat-b-2', name: 'Inactive sibling', note: '', localLore: [], message: [] },
            ],
        } as any)
        const untouchedSelectedSibling = structuredClone(database.characters[0].chats[1])
        const untouchedInactiveSibling = structuredClone(database.characters[1].chats[1])
        const store = makeStore(databaseName)
        await store.open()
        await store.replaceFromDatabase(database)
        const materializeDatabase = vi.spyOn(store, 'materializeDatabase')
        const replaceFromDatabase = vi.spyOn(store, 'replaceFromDatabase')
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const materializeDatabaseSnapshot = vi.fn()
        const replacePersistentDatabase = vi.fn()
        const access = createPluginDatabaseAccess({
        owner: PLUGIN_ACCESS_OWNER,
            store,
            flushPendingData: (reason) => runtime.flushPendingData(reason),
            getCompatibilityDatabase: () => adapter.current(),
            getSelectedCharacterId: () => adapter.current().characters[0]?.chaId ?? null,
            captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
            acquireCompleteConversation: (reason, target) =>
                runtime.acquireCompleteConversation(reason, target),
            refreshSelectedConversationAfterReplacement: (target, expectedSession) =>
                runtime.refreshSelectedConversationAfterReplacement(target, expectedSession),
            replacePersistentCompleteCharacter: (characterId, reason, mutate, options) =>
                runtime.replacePersistentCompleteCharacter(characterId, reason, mutate, options),
            replacePersistentConversation: (
                characterId,
                conversationId,
                reason,
                replacement,
                options,
            ) => runtime.replacePersistentConversation(
                characterId,
                conversationId,
                reason,
                replacement,
                options,
            ),
            reportIdentityReplacementRejected: vi.fn(),
            getNavigationGeneration: () => runtime.getNavigationGeneration(),
            applyCompatibilityDatabaseLite: vi.fn(),
            readPluginStorageSnapshot: vi.fn(async () => ({})),
            mutatePluginStorage: vi.fn(),
            invalidatePluginStorage: vi.fn(),
            getStorageAuthorityEpoch: () => runtime.getStorageAuthorityEpoch(),
            assertPersistentMutationAllowed: (epoch) => runtime.assertPersistentMutationAllowed(epoch),
            materializeDatabaseSnapshot,
            replacePersistentDatabase,
            snapshot: structuredClone,
        })
        const context = {
            pluginName: 'restart-plugin',
            signal: new AbortController().signal,
        }
        const selectedReplacement = structuredClone(adapter.current().characters[0])
        selectedReplacement.name = 'Selected replaced'
        await access.setCurrentCharacter(selectedReplacement, context)
        const inactiveReplacement = structuredClone(adapter.current().characters[1])
        inactiveReplacement.name = 'Inactive replaced'
        await access.setCharacterToIndex(1, inactiveReplacement, context)
        const inactiveChat = structuredClone(adapter.current().characters[1].chats[0])
        inactiveChat.note = 'inactive chat replaced'
        await access.setChatToIndex(1, 0, inactiveChat, context)

        expect(materializeDatabaseSnapshot).not.toHaveBeenCalled()
        expect(replacePersistentDatabase).not.toHaveBeenCalled()
        expect(materializeDatabase).not.toHaveBeenCalled()
        expect(replaceFromDatabase).not.toHaveBeenCalled()

        const reopened = makeStore(databaseName)
        await reopened.open()
        const persisted = await reopened.materializeDatabase(runtime.revision)
        expect(persisted.characters.map((character) => character.chaId)).toEqual([
            'char-a',
            'char-b',
        ])
        expect(persisted.characters[0].name).toBe('Selected replaced')
        expect(persisted.characters[1].name).toBe('Inactive replaced')
        expect(persisted.characters[1].chats[0].note).toBe('inactive chat replaced')
        expect(persisted.characters[0].chats[1]).toEqual(untouchedSelectedSibling)
        expect(persisted.characters[1].chats[1]).toEqual(untouchedInactiveSibling)
    })

    it('preserves V2 plugin insertion order through nested edits, restart, and export', async () => {
        const databaseName = `runtime-plugin-order-${crypto.randomUUID()}`
        const database = makeDatabase()
        database.pluginCustomStorage = {
            zeta: { nested: { count: 1 } },
            alpha: { nested: { count: 1 } },
        }
        const store = makeStore(databaseName)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)

        adapter.current().pluginCustomStorage.alpha.nested.count = 2
        adapter.current().pluginCustomStorage.beta = { nested: true }
        delete adapter.current().pluginCustomStorage.zeta
        adapter.current().pluginCustomStorage.zeta = { nested: { count: 1 } }
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('v2-plugin-order')

        const expectedKeys = ['alpha', 'beta', 'zeta']
        const reopened = makeStore(databaseName)
        await reopened.open()
        const persisted = await reopened.materializeDatabase(runtime.revision)
        expect(Object.keys(persisted.pluginCustomStorage)).toEqual(expectedKeys)
        expect(persisted.pluginCustomStorage.alpha).toEqual({ nested: { count: 2 } })
        const exported = await decodeRisuSave(await concatenate(
            streamRisuSaveFromStore(reopened, runtime.revision),
        ))
        expect(Object.keys(exported.pluginCustomStorage)).toEqual(expectedKeys)
    })

    it('preserves replacement plugin order through publication, restart, snapshot, and export', async () => {
        const databaseName = `runtime-plugin-replacement-order-${crypto.randomUUID()}`
        const database = makeDatabase()
        const store = makeStore(databaseName)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)

        const replacement = structuredClone(database)
        const storage: Record<string, unknown> = {}
        storage.zeta = { value: 'first string' }
        storage.alpha = { value: 'second string' }
        storage.renewed = { value: 'before reinsert' }
        storage['10'] = 'ten'
        storage['2'] = 0
        storage['01'] = 'non-index'
        storage['4294967294'] = true
        storage['4294967295'] = false
        delete storage.renewed
        storage.renewed = { value: 'after reinsert' }
        replacement.pluginCustomStorage = storage
        const expectedKeys = Object.keys(storage)

        await runtime.replacePersistentDatabase(replacement, 'plugin-replacement-order')

        expect(Object.keys(adapter.current().pluginCustomStorage)).toEqual(expectedKeys)
        const snapshot = await runtime.materializePersistentDatabaseSnapshot(
            'plugin-replacement-order-snapshot',
        )
        expect(Object.keys(snapshot.pluginCustomStorage)).toEqual(expectedKeys)
        const reopened = makeStore(databaseName)
        await reopened.open()
        const persisted = await reopened.materializeDatabase(runtime.revision)
        expect(Object.keys(persisted.pluginCustomStorage)).toEqual(expectedKeys)
        expect(persisted.pluginCustomStorage).toEqual(storage)
        const exported = await decodeRisuSave(await concatenate(
            streamRisuSaveFromStore(reopened, runtime.revision),
        ))
        expect(Object.keys(exported.pluginCustomStorage)).toEqual(expectedKeys)
        expect(exported.pluginCustomStorage).toEqual(storage)
    })

    it('captures root and the selected character without traversing inactive characters', () => {
        const database = makeDatabase()
        const inactive = structuredClone(database.characters[0])
        Object.defineProperty(inactive, 'chats', {
            enumerable: true,
            get: () => {
                throw new Error('inactive character was traversed')
            },
        })
        database.characters.push(inactive)

        expect(capturePersistentRoot(database).username).toBe('Fixture')
        expect(captureSelectedPersistentCharacter(database, 0)?.chaId).toBe('char-a')
    })

    it('commits ordinary root and selected-character edits without a legacy writer', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const revisions: number[] = []
        const legacyWriter = vi.fn()
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            onLocalRevision: (revision) => revisions.push(revision),
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)

        adapter.current().username = 'Changed root'
        adapter.current().characters[0].name = 'Changed character'
        runtime.markPersistentDataDirty(64)
        await runtime.flushPendingData('test')

        const reopened = await store.materializeDatabase(runtime.revision)
        expect(reopened.username).toBe('Changed root')
        expect(reopened.characters[0].name).toBe('Changed character')
        expect(revisions).toEqual([2])
        expect(legacyWriter).not.toHaveBeenCalled()
    })

    it.each(['generation', 'exit'])(
        'makes %s durable across reload before waiting for remote publication',
        async (reason) => {
            const databaseName = `runtime-generation-reload-${crypto.randomUUID()}`
            const database = makeDatabase()
            const store = makeStore(databaseName)
            await store.open()
            await store.replaceFromDatabase(database)
            const adapter = makeAdapter(database)
            const pin = vi.fn(() => new Promise<never>(() => undefined))
            const runtime = createPersistentDataRuntime({
                store,
                state: adapter,
                clock: rendererOwnedDebounceClock,
                officialPublisher: { pin },
                prepareDatabase: async (candidate) =>
                    structuredClone(candidate),
            })
            await runtime.initializeActiveWorkingSet(database)

            const completedGeneration = {
                role: 'char' as const,
                data: 'completed provider response',
                chatId: 'generation-complete',
                generationInfo: {
                    generationId: 'generation-complete',
                    model: 'synthetic-model',
                },
            }
            adapter
                .current()
                .characters[0].chats[0].message.push(completedGeneration)
            runtime.markPersistentDataDirty(64)
            if (reason === 'exit') await runtime.flushPendingDataLocally('exit')
            else await runtime.acknowledgeGenerationCompletion()

            expect(pin).not.toHaveBeenCalled()
            expect(runtime.hasPendingOfficialPublication()).toBe(true)

            const reopened = makeStore(databaseName)
            await reopened.open()
            const recovered = await reopened.materializeDatabase()

            expect(recovered.characters[0].chats[0].message).toContainEqual(
                completedGeneration,
            )
        },
    )

    it('keeps a failed completion dirty so a retry makes it durable', async () => {
        const databaseName = `runtime-generation-retry-${crypto.randomUUID()}`
        const database = makeDatabase()
        const store = makeStore(databaseName)
        await store.open()
        await store.replaceFromDatabase(database)
        const commit = store.commit.bind(store)
        let failNextCommit = true
        store.commit = async (input) => {
            if (failNextCommit) {
                failNextCommit = false
                throw new Error('synthetic generation commit failure')
            }
            return commit(input)
        }
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            clock: rendererOwnedDebounceClock,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const completedGeneration = {
            role: 'char' as const,
            data: 'retryable completed response',
            chatId: 'generation-retry',
        }
        adapter.current().characters[0].chats[0].message.push(completedGeneration)
        runtime.markPersistentDataDirty(64)

        await expect(runtime.acknowledgeGenerationCompletion()).rejects.toThrow(
            'synthetic generation commit failure',
        )
        const beforeRetry = makeStore(databaseName)
        await beforeRetry.open()
        expect((await beforeRetry.materializeDatabase()).characters[0].chats[0].message).toEqual([])

        await runtime.acknowledgeGenerationCompletion()

        const reopened = makeStore(databaseName)
        await reopened.open()
        expect((await reopened.materializeDatabase()).characters[0].chats[0].message).toContainEqual(
            completedGeneration,
        )
    })

    it('rejects completion acknowledgement on revision conflict without exposing the generation', async () => {
        const databaseName = `runtime-generation-conflict-${crypto.randomUUID()}`
        const database = makeDatabase()
        const store = makeStore(databaseName)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            clock: rendererOwnedDebounceClock,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)

        const competingStore = makeStore(databaseName)
        await competingStore.open()
        const competingRoot = await competingStore.readRoot()
        await competingStore.commit({
            expectedRevision: competingRoot.revision,
            root: {
                ...competingRoot.value,
                username: 'Competing renderer',
            },
        })
        adapter.current().characters[0].chats[0].message.push({
            role: 'char',
            data: 'conflicted completed response',
            chatId: 'generation-conflict',
        })
        runtime.markPersistentDataDirty(64)

        await expect(runtime.acknowledgeGenerationCompletion()).rejects.toBeInstanceOf(
            RevisionConflictError,
        )
        await expect(runtime.acknowledgeGenerationCompletion()).rejects.toBeInstanceOf(
            RevisionConflictError,
        )

        const reopened = makeStore(databaseName)
        await reopened.open()
        const persisted = await reopened.materializeDatabase()
        expect(persisted.username).toBe('Competing renderer')
        expect(persisted.characters[0].chats[0].message).toEqual([])
    })

    it('preserves inactive preset rows across scalable root flushes and active switches', async () => {
        const complete = makeDatabase()
        complete.botPresetsId = 0
        complete.botPresets = [
            { name: 'First', mainPrompt: 'complete first' },
            { name: 'Second', mainPrompt: 'complete second' },
        ] as Database['botPresets']
        const store = makeStore(`runtime-scalable-presets-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(complete)
        const live = structuredClone(complete)
        live.botPresets = createCatalogPresetWorkingSet({
            revision: 1,
            items: complete.botPresets.map((preset, configuredIndex) => ({
                id: String(configuredIndex),
                configuredIndex,
                name: preset.name,
            })),
        }, {
            summary: { id: '0', configuredIndex: 0, name: 'First' },
            value: structuredClone(complete.botPresets[0]),
        })
        const adapter = makeAdapter(live)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(live)

        adapter.current().username = 'Scalable root edit'
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('scalable-root')
        expect((await store.readPreset('1'))?.value.mainPrompt).toBe('complete second')

        await runtime.mutatePersistentPresets('switch-preset', ({ root }) => {
            root.botPresetsId = 1
        })

        const persisted = await store.materializeDatabase(runtime.revision)
        expect(persisted.botPresets.map((preset) => preset.mainPrompt)).toEqual([
            'complete first',
            'complete second',
        ])
        expect(adapter.current().botPresets[0]).toEqual({ name: 'First' })
        expect(adapter.current().botPresets[1].mainPrompt).toBe('complete second')
    })

    it('commits a character addition through the production request API', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-addition-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const added = structuredClone(adapter.current().characters[0])
        added.chaId = 'char-added'
        added.name = 'Added character'
        added.chats[0].id = 'chat-added'
        const install = vi.fn(() => adapter.current().characters.push(added))
        adapter.current().username = 'Root with addition'
        adapter.current().characters[0].name = 'Previous selected edit'

        await runtime.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 256,
            install,
        }, 'new-character')

        expect(install).toHaveBeenCalledOnce()
        const persisted = await store.materializeDatabase(runtime.revision)
        expect(persisted.username).toBe('Root with addition')
        expect(persisted.characters[0].name).toBe('Previous selected edit')
        expect(persisted.characters[1]).toEqual(added)
    })

    it('reopens an inactive character converted from a module with complete chats', async () => {
        const database = makeDatabase()
        const databaseName = `runtime-module-conversion-${crypto.randomUUID()}`
        const store = makeStore(databaseName)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const converted = structuredClone(adapter.current().characters[0])
        converted.chaId = 'module-character'
        converted.name = 'Converted module'
        converted.chats = [{
            id: 'module-chat',
            name: 'Module chat',
            note: '',
            localLore: [],
            message: [{ role: 'char', data: 'Converted message' }],
        }]

        await runtime.commitCharacterAddition({
            characterId: converted.chaId,
            estimatedBytes: 256,
            install: () => adapter.current().characters.push(converted),
        }, 'convert-module-to-character')

        const reopened = makeStore(databaseName)
        await reopened.open()
        const persisted = await reopened.materializeDatabase(runtime.revision)
        expect(persisted.characters.find((character) => character.chaId === converted.chaId)).toEqual(
            converted,
        )
    })

    it('retries one exact dynamically selected publication and disposes it after success', async () => {
        const database = makeDatabase()
        const store = makeStore(`publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const publish = vi.fn(async () => {
            if (publish.mock.calls.length === 1) throw new Error('offline')
        })
        const dispose = vi.fn(async () => undefined)
        const pin = vi.fn(async () => ({ publish, dispose }))
        let officialPublisher = { pin }
        let nowValue = 0
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            getOfficialPublisher: () => officialPublisher,
            now: () => nowValue,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        adapter.current().username = 'Published'
        runtime.markPersistentDataDirty(64)

        await expect(runtime.flushPendingData('first')).rejects.toThrow('offline')
        officialPublisher = { pin: vi.fn() }
        nowValue = 4000
        await runtime.flushPendingData('retry')

        expect(pin).toHaveBeenCalledOnce()
        expect(publish).toHaveBeenCalledTimes(2)
        expect(dispose).toHaveBeenCalledOnce()
        expect(runtime.revision).toBe(2)
    })

    it('uses the official publisher selected after local initialization', async () => {
        const database = makeDatabase()
        const store = makeStore(`late-publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const publish = vi.fn(async () => undefined)
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        let officialPublisher: { pin: typeof pin } | null = null
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            getOfficialPublisher: () => officialPublisher,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        }
        )
        await runtime.initializeActiveWorkingSet(database)
        officialPublisher = { pin }
        adapter.current().username = 'Account enabled'
        runtime.markPersistentDataDirty(64)

        await runtime.flushPendingData('account-enabled')

        expect(pin).toHaveBeenCalledWith(2)
        expect(publish).toHaveBeenCalledOnce()
    })

    it('uses an idempotent no-op publication when account mode remains disabled', async () => {
        const database = makeDatabase()
        const store = makeStore(`disabled-publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const getOfficialPublisher = vi.fn(() => null)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            getOfficialPublisher,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        adapter.current().username = 'Local only'
        runtime.markPersistentDataDirty(64)

        await runtime.flushPendingData('local-only')
        await runtime.publishCurrentOfficialRevision()

        expect(getOfficialPublisher).toHaveBeenCalled()
        expect(runtime.revision).toBe(2)
    })

    it('publishes an accepted replacement revision and retains its exact handle for retry', async () => {
        const database = makeDatabase()
        const store = makeStore(`replacement-publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const publish = vi.fn(async () => {
            if (publish.mock.calls.length === 1) throw new Error('official offline')
        })
        const dispose = vi.fn(async () => undefined)
        const pin = vi.fn(async () => ({ publish, dispose }))
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            officialPublisher: { pin },
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const replacement = structuredClone(database)
        replacement.username = 'Restored account backup'
        await runtime.replacePersistentDatabase(replacement, 'account-restore')

        await expect(runtime.publishCurrentOfficialRevision()).rejects.toThrow('official offline')
        await runtime.publishCurrentOfficialRevision()

        expect(pin).toHaveBeenCalledOnce()
        expect(pin).toHaveBeenCalledWith(2)
        expect(publish).toHaveBeenCalledTimes(2)
        expect(dispose).toHaveBeenCalledOnce()
    })

    it('prepares a character activation replacement before storing it', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-activation-preparation-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const prepareDatabase = vi.fn(async (candidate: Database) => ({
            ...structuredClone(candidate),
            username: 'Prepared cold replacement',
        }))
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase,
        })
        await runtime.initializeActiveWorkingSet(database)

        expect(await runtime.activateCharacter('char-a', {
            prepare: async () => ({
                database: structuredClone(database),
                reason: 'character-detail-replace',
            }),
        })).toBe(true)

        expect(prepareDatabase).toHaveBeenCalledOnce()
        expect((await store.materializeDatabase(runtime.revision)).username).toBe(
            'Prepared cold replacement',
        )
    })

    it('broadcasts successful revisions and warns only once for foreign sessions', async () => {
        const posted: string[] = []
        let onmessage: ((event: MessageEvent) => void) | null = null
        let callbacks: {
            onLocalRevision?: (revision: number) => void
            onFlushPromise?: (promise: Promise<void> | null) => void
        } = {}
        const warning = vi.fn()
        const savingStates: boolean[] = []
        const dispose = installPersistentSaveNotifications({
            sessionId: 'local-session',
            channel: {
                postMessage: (value) => posted.push(value as string),
                close: vi.fn(),
                get onmessage() {
                    return onmessage
                },
                set onmessage(value) {
                    onmessage = value
                },
            },
            configureRuntime: (next) => {
                callbacks = next
            },
            showForeignRevisionWarning: warning,
            setSaving: (value) => savingStates.push(value),
        })

        callbacks.onLocalRevision?.(2)
        onmessage?.({ data: 'local-session' } as MessageEvent)
        onmessage?.({ data: 'foreign-a' } as MessageEvent)
        onmessage?.({ data: 'foreign-b' } as MessageEvent)
        await Promise.resolve()
        callbacks.onFlushPromise?.(Promise.resolve())
        callbacks.onFlushPromise?.(null)
        dispose()

        expect(posted).toEqual(['local-session'])
        expect(warning).toHaveBeenCalledOnce()
        expect(savingStates).toEqual([true, false])
    })

    it('stops and reinstalls the production observer without stale callbacks', () => {
        const installation = createPersistentSaveObserverInstallation()
        const warning = vi.fn()
        const savingStates: boolean[] = []
        let activeCallbacks: {
            onLocalRevision?: (revision: number) => void
            onFlushPromise?: (promise: Promise<void> | null) => void
        } = {}
        const installSession = (sessionId: string) => {
            let onmessage: ((event: MessageEvent) => void) | null = null
            const close = vi.fn()
            const disposeEffects = vi.fn()
            const channel = {
                postMessage: vi.fn(),
                close,
                get onmessage() {
                    return onmessage
                },
                set onmessage(value) {
                    onmessage = value
                },
            }
            installation.install(() => {
                const disposeNotifications = installPersistentSaveNotifications({
                    sessionId,
                    channel,
                    configureRuntime: (callbacks) => {
                        activeCallbacks = callbacks
                    },
                    showForeignRevisionWarning: warning,
                    setSaving: (value) => savingStates.push(value),
                })
                return () => {
                    disposeEffects()
                    disposeNotifications()
                }
            })
            return { channel, close, disposeEffects }
        }

        const first = installSession('first')
        const second = installSession('second')
        first.channel.onmessage?.({ data: 'foreign-old' } as MessageEvent)
        second.channel.onmessage?.({ data: 'foreign-new' } as MessageEvent)
        activeCallbacks.onFlushPromise?.(Promise.resolve())
        activeCallbacks.onFlushPromise?.(null)
        installation.stop()
        const third = installSession('third')

        expect(first.disposeEffects).toHaveBeenCalledOnce()
        expect(first.close).toHaveBeenCalledOnce()
        expect(second.disposeEffects).toHaveBeenCalledOnce()
        expect(second.close).toHaveBeenCalledOnce()
        expect(warning).toHaveBeenCalledOnce()
        expect(savingStates).toEqual([true, false])
        expect(third.close).not.toHaveBeenCalled()
        installation.stop()
        expect(third.disposeEffects).toHaveBeenCalledOnce()
        expect(third.close).toHaveBeenCalledOnce()
    })
})
