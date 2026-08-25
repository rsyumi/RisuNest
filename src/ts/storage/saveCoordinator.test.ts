import { describe, expect, it, vi } from 'vitest'
import {
    SaveCoordinator as ProductionSaveCoordinator,
    type SaveCoordinatorDependencies,
} from './saveCoordinator'
import type { Chat, Database, character, groupChat } from './database.svelte'
import type { PersistentDataStore } from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'

function makeDatabase(): Database {
    return {
        username: 'Fixture',
        botPresets: [],
        characters: [
            {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chats: [],
            },
        ],
    } as unknown as Database
}

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (error: unknown) => void
    const promise = new Promise<T>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, resolve, reject }
}

function makeStore(commit = vi.fn()) {
    return {
        commit,
        replaceFromDatabase: vi.fn(),
    } as unknown as PersistentDataStore
}

function captureRoot(database: Database): Omit<Database, 'characters' | 'botPresets'> {
    const { characters: _characters, botPresets: _botPresets, ...root } = database
    return root
}

type TestCoordinatorDependencies = Omit<SaveCoordinatorDependencies, 'captureCharacter'> &
    Partial<Pick<SaveCoordinatorDependencies, 'captureCharacter'>>

class SaveCoordinator extends ProductionSaveCoordinator {
    constructor(dependencies: TestCoordinatorDependencies) {
        super({
            ...dependencies,
            captureCharacter: dependencies.captureCharacter ?? ((id) => {
                const selected = dependencies.captureSelectedCharacter()
                return selected?.chaId === id ? selected : null
            }),
        })
    }
}

describe('SaveCoordinator', () => {
    function makeAdditionDatabase() {
        const database = makeDatabase()
        const added = structuredClone(database.characters[0])
        added.chaId = 'char-added'
        added.name = 'Added'
        return { database, added }
    }

    it('commits the complete preset array atomically with a changed root', async () => {
        const database = makeDatabase()
        const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        })))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)
        database.username = 'Changed with presets'
        database.botPresets = [{ name: 'Resident preset', mainPrompt: 'complete' }] as Database['botPresets']
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('preset-change')

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 2,
            root: expect.objectContaining({ username: 'Changed with presets' }),
            replacePresets: database.botPresets,
        })
        expect(vi.mocked(store.commit).mock.calls[0][0].root).not.toHaveProperty('botPresets')
    })

    it('never replaces persisted presets from a partial scalable working set', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Active only', mainPrompt: 'resident' }] as Database['botPresets']
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(5)
        database.username = 'Root edit with a partial preset working set'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('partial-preset-working-set')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 5,
            root: expect.objectContaining({ username: 'Root edit with a partial preset working set' }),
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replacePresets')
    })

    it('serializes an explicit preset mutation and publishes only after its atomic commit', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Projected active' }] as Database['botPresets']
        const committed = deferred<{ revision: number }>()
        const store = {
            commit: vi.fn(() => committed.promise),
            readRoot: vi.fn(async () => ({
                revision: 3,
                value: captureRoot(database),
            })),
            queryPresets: vi.fn(async () => ({
                revision: 3,
                items: [
                    { id: '0', configuredIndex: 0, name: 'First' },
                    { id: '1', configuredIndex: 1, name: 'Second' },
                ],
            })),
            readPreset: vi.fn(async (id: string) => ({
                revision: 3,
                value: { name: id === '0' ? 'First' : 'Second', mainPrompt: id },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet,
        })
        coordinator.initialize(3)

        const mutation = coordinator.mutatePersistentPresets('rename', ({ root, presets }) => {
            root.botPresetsId = 1
            presets[1].name = 'Renamed'
        })
        await vi.waitFor(() => expect(store.commit).toHaveBeenCalledTimes(1))

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 3,
            root: expect.objectContaining({ botPresetsId: 1 }),
            replacePresets: [
                { name: 'First', mainPrompt: '0' },
                { name: 'Renamed', mainPrompt: '1' },
            ],
        })
        expect(publishPresetWorkingSet).not.toHaveBeenCalled()

        committed.resolve({ revision: 4 })
        await mutation

        expect(publishPresetWorkingSet).toHaveBeenCalledWith({
            revision: 4,
            root: expect.objectContaining({ botPresetsId: 1 }),
            presets: [
                { name: 'First', mainPrompt: '0' },
                { name: 'Renamed', mainPrompt: '1' },
            ],
        })
        expect(coordinator.revision).toBe(4)
    })

    it('does not publish an explicit preset mutation when the atomic commit fails', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn().mockRejectedValue(new Error('preset commit failed')),
            readRoot: vi.fn(async () => ({ revision: 8, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 8,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 8,
                value: { name: 'First', mainPrompt: 'full' },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet,
        })
        coordinator.initialize(8)

        await expect(coordinator.mutatePersistentPresets('rename', ({ presets }) => {
            presets[0].name = 'Never published'
        })).rejects.toThrow('preset commit failed')

        expect(publishPresetWorkingSet).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(8)
    })

    it('preserves and flushes a maximum preset edit made while explicit commit is pending', async () => {
        const database = makeDatabase()
        database.botPresets = [{ name: 'Initial', mainPrompt: 'initial' }] as Database['botPresets']
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 20, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 20,
                items: [{ id: '0', configuredIndex: 0, name: 'Initial' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 20,
                value: { name: 'Initial', mainPrompt: 'initial' },
            })),
        } as unknown as PersistentDataStore
        const publishPresetWorkingSet = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet,
        })
        coordinator.initialize(20)

        const mutation = coordinator.mutatePersistentPresets('explicit-rename', ({ presets }) => {
            presets[0].name = 'Explicit'
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.botPresets[0].name = 'Concurrent live edit'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 21 })

        await expect(mutation).rejects.toThrow('changed during preset mutation')

        expect(publishPresetWorkingSet).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toEqual({
            expectedRevision: 21,
            replacePresets: [{ name: 'Concurrent live edit', mainPrompt: 'initial' }],
        })
        expect(database.botPresets[0].name).toBe('Concurrent live edit')
        expect(coordinator.revision).toBe(22)
    })

    it('rejects an incomplete live replacement before cloning or capturing state', async () => {
        const database = makeDatabase()
        const candidate = makeDatabase() as Database & { cloneTrap?: unknown }
        Object.defineProperty(candidate, 'cloneTrap', {
            enumerable: true,
            get() {
                throw new Error('candidate was cloned')
            },
        })
        const captureRootValue = vi.fn(() => captureRoot(database))
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: captureRootValue,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            isIncompleteWorkingSet: (value) => value === candidate,
        })
        coordinator.initialize(1)
        captureRootValue.mockClear()

        await expect(coordinator.replacePersistentDatabase(candidate, 'unsafe-live-snapshot'))
            .rejects.toThrow('incomplete persistent working set')

        expect(captureRootValue).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('allows an explicitly authoritative complete replacement and publishes it normally', async () => {
        const database = makeDatabase()
        const candidate = makeDatabase()
        candidate.username = 'Imported complete database'
        const replaceDatabase = vi.fn()
        const store = {
            replaceFromDatabase: vi.fn(async () => ({ revision: 2 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
            isIncompleteWorkingSet: () => true,
        })
        coordinator.initialize(1)

        await coordinator.replacePersistentDatabase(candidate, 'explicit-import', {
            authoritative: true,
        })

        expect(store.replaceFromDatabase).toHaveBeenCalledWith(candidate, 1)
        expect(replaceDatabase).toHaveBeenCalledWith(candidate)
        expect(coordinator.revision).toBe(2)
    })

    it('rejects a replacement whose expected revision is already stale', async () => {
        const database = makeDatabase()
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(6)

        await expect(coordinator.replacePersistentDatabase(database, 'stale-snapshot', {
            authoritative: true,
            expectedRevision: 5,
        })).rejects.toBeInstanceOf(RevisionConflictError)

        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('rejects a replacement whose snapshot mutation generation is stale before cloning', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const candidate = makeDatabase() as Database & { cloneTrap?: unknown }
            Object.defineProperty(candidate, 'cloneTrap', {
                enumerable: true,
                get() {
                    throw new Error('candidate was cloned')
                },
            })
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const store = makeStore(commit)
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)
            database.username = 'Later live edit'
            coordinator.markPersistentDataDirty(1)

            await expect(coordinator.replacePersistentDatabase(candidate, 'stale-snapshot', {
                authoritative: true,
                expectedRevision: 6,
                expectedMutationGeneration: 0,
            })).rejects.toThrow('mutation generation 0')

            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(expect.objectContaining({
                expectedRevision: 6,
                root: { username: 'Later live edit' },
            }))
        } finally {
            vi.useRealTimers()
        }
    })

    it('revalidates the snapshot mutation generation after async replacement preparation', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const prepared = deferred<Database>()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)

            const replacement = coordinator.replacePreparedPersistentDatabase(
                () => prepared.promise,
                'deferred-snapshot',
                {
                    authoritative: true,
                    expectedRevision: 6,
                    expectedMutationGeneration: 0,
                },
            )
            await Promise.resolve()
            database.username = 'Edit during preparation'
            coordinator.markPersistentDataDirty(1)
            prepared.resolve(makeDatabase())

            await expect(replacement).rejects.toThrow('mutation generation 0')
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })

    it('re-arms pending dirty data after a replacement store conflict', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const store = {
                commit,
                replaceFromDatabase: vi.fn().mockRejectedValue(
                    new RevisionConflictError(6, 7),
                ),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(6)
            database.username = 'Retryable edit'
            coordinator.markPersistentDataDirty(1)

            await expect(coordinator.replacePersistentDatabase(
                makeDatabase(),
                'conflicting-replacement',
            )).rejects.toBeInstanceOf(RevisionConflictError)

            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledWith(expect.objectContaining({
                expectedRevision: 6,
                root: { username: 'Retryable edit' },
            }))
        } finally {
            vi.useRealTimers()
        }
    })

    it('commits a stable-ID character detail mutation without replacing its conversations', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a', 'unrelated']
        const committed = deferred<{ revision: number }>()
        const store = {
            commit: vi.fn(() => committed.promise),
            readRoot: vi.fn(async () => ({
                revision: 4,
                value: captureRoot(database),
            })),
            readCharacter: vi.fn(async () => ({
                revision: 4,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(4)

        const mutation = coordinator.mutatePersistentCharacterDetail(
            'char-a',
            'trash-character',
            ({ root, character }) => {
                character.trashTime = 123
                root.characterOrder = root.characterOrder.filter((entry) => entry !== 'char-a')
            },
        )
        await vi.waitFor(() => expect(store.commit).toHaveBeenCalledOnce())

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 4,
            root: expect.objectContaining({ characterOrder: ['unrelated'] }),
            character: expect.objectContaining({
                chaId: 'char-a',
                trashTime: 123,
            }),
        })
        expect(vi.mocked(store.commit).mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(publishCharacterMutation).not.toHaveBeenCalled()

        committed.resolve({ revision: 5 })
        await expect(mutation).resolves.toBe(true)

        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 5,
            characterId: 'char-a',
            kind: 'detail',
            character: expect.objectContaining({ trashTime: 123 }),
        }))
    })

    it('replaces exactly one complete character from authoritative detail and conversations', async () => {
        const database = makeDatabase()
        const storedChat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'preserved' }],
        }
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 7, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 7,
                value: { type: 'character', chaId: 'char-a', name: 'Cold stub' },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 7,
                items: [{
                    id: 'chat-a',
                    characterId: 'char-a',
                    name: 'Stored chat',
                    configuredIndex: 0,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 7, value: storedChat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(7)

        await expect(coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'cold-character-restore',
            (current) => ({ ...current, name: 'Restored' }),
        )).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 7,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                name: 'Restored',
                chats: [storedChat],
            }),
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 8,
            characterId: 'char-a',
            kind: 'replace',
            character: expect.objectContaining({ chats: [storedChat] }),
        }))
    })

    it('does not publish a stable-ID character mutation when its commit fails', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn().mockRejectedValue(new Error('character commit failed')),
            readRoot: vi.fn(async () => ({ revision: 9, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 9,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(9)

        await expect(coordinator.mutatePersistentCharacterDetail(
            'char-a',
            'delete-character',
            () => ({ delete: true }),
        )).rejects.toThrow('character commit failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(9)
    })

    it('keeps selected-group cleanup dirty after deleting another character', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['char-a'],
            characterTalks: [0.5],
            characterActive: [true],
            chats: [],
        } as groupChat
        const target = makeDatabase().characters[0]
        const database = {
            ...makeDatabase(),
            characters: [group, target],
        } as Database
        const commit = vi.fn(async ({ expectedRevision }) => ({
            revision: expectedRevision + 1,
        }))
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 1, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 1,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation: (state) => {
                if (state.kind !== 'delete') return
                group.characters = []
                group.characterTalks = []
                group.characterActive = []
                database.characters.splice(1, 1)
            },
        })
        coordinator.initialize(1)

        await expect(coordinator.mutatePersistentCharacterDetail(
            'char-a',
            'delete-group-member',
            () => ({ delete: true }),
        )).resolves.toBe(true)
        await coordinator.flushPendingData('persist-group-cleanup')

        expect(commit).toHaveBeenNthCalledWith(1, {
            expectedRevision: 1,
            deleteCharacterId: 'char-a',
        })
        expect(commit).toHaveBeenNthCalledWith(2, expect.objectContaining({
            expectedRevision: 2,
            replaceCharacter: expect.objectContaining({
                chaId: 'group-a',
                characters: [],
                characterTalks: [],
                characterActive: [],
            }),
        }))
    })

    it.each(['detail', 'replace', 'upsert'] as const)(
        'rejects an async %s mutation when its resident character changes before commit',
        async (operation) => {
            const database = makeDatabase()
            const mutationStarted = deferred<void>()
            const mutationGate = deferred<void>()
            const store = {
                commit: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            let mutation: Promise<boolean>
            if (operation === 'detail') {
                mutation = coordinator.mutatePersistentCharacterDetail(
                    'char-a',
                    'async-detail',
                    async ({ character }) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        character.name = 'Explicit detail'
                    },
                )
            } else if (operation === 'replace') {
                mutation = coordinator.replacePersistentCompleteCharacter(
                    'char-a',
                    'async-replace',
                    async (character) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        return { ...character, name: 'Explicit replacement' }
                    },
                )
            } else {
                mutation = coordinator.upsertPersistentCompleteCharacter(
                    'char-a',
                    'async-upsert',
                    async (character) => {
                        mutationStarted.resolve()
                        await mutationGate.promise
                        return { ...character!, name: 'Explicit upsert' }
                    },
                )
            }
            await mutationStarted.promise
            ;(database.characters[0] as character).desc = 'Later resident edit'
            coordinator.markPersistentDataDirty(1)
            mutationGate.resolve()

            await expect(mutation).rejects.toThrow('Resident character changed')
            expect(store.commit).not.toHaveBeenCalled()
        },
    )

    it.each(['detail', 'replace', 'upsert'] as const)(
        'rejects a %s mutation when its resident character changes during authoritative reads',
        async (operation) => {
            const database = makeDatabase()
            const characterRead = deferred<{
                revision: number
                value: { type: 'character'; chaId: string; name: string }
            }>()
            const store = {
                commit: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(() => characterRead.promise),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            let mutation: Promise<boolean>
            if (operation === 'detail') {
                mutation = coordinator.mutatePersistentCharacterDetail(
                    'char-a',
                    'read-race-detail',
                    ({ character }) => {
                        character.name = 'Explicit detail'
                    },
                )
            } else if (operation === 'replace') {
                mutation = coordinator.replacePersistentCompleteCharacter(
                    'char-a',
                    'read-race-replace',
                    (character) => ({ ...character, name: 'Explicit replacement' }),
                )
            } else {
                mutation = coordinator.upsertPersistentCompleteCharacter(
                    'char-a',
                    'read-race-upsert',
                    (character) => ({ ...character!, name: 'Explicit upsert' }),
                )
            }
            await vi.waitFor(() => expect(store.readCharacter).toHaveBeenCalledOnce())
            ;(database.characters[0] as character).desc = 'Edit during authoritative read'
            coordinator.markPersistentDataDirty(1)
            characterRead.resolve({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })

            await expect(mutation).rejects.toThrow('Resident character changed')
            expect(store.commit).not.toHaveBeenCalled()
        },
    )

    it('compensates a resident edit made while a complete-character commit is pending', async () => {
        const database = makeDatabase()
        const firstCommit = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockImplementationOnce(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 10,
                value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) =>
                database.characters.find((character) => character.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(10)

        const mutation = coordinator.replacePersistentCompleteCharacter(
            'char-a',
            'pending-replace',
            (character) => ({ ...character, name: 'Explicit replacement' }),
        )
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        ;(database.characters[0] as character).desc = 'Later resident edit'
        coordinator.markPersistentDataDirty(1)
        firstCommit.resolve({ revision: 11 })

        await expect(mutation).rejects.toThrow('Resident character changed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 11,
            replaceCharacter: expect.objectContaining({
                chaId: 'char-a',
                name: 'Alpha',
                desc: 'Later resident edit',
            }),
        })
        expect(coordinator.revision).toBe(12)
    })

    it('bounds resident compensation and leaves continuous edits for a trailing flush', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let coordinator!: SaveCoordinator
            const commit = vi.fn(async ({ expectedRevision }) => {
                const call = commit.mock.calls.length
                if (call > 4) throw new Error('unbounded compensation flush entered')
                ;(database.characters[0] as character).desc = `Concurrent edit ${call}`
                coordinator.markPersistentDataDirty(1)
                return { revision: expectedRevision + 1 }
            })
            const store = {
                commit,
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            await expect(coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'continuous-resident-edits',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )).rejects.toThrow('Resident character changed')

            expect(commit).toHaveBeenCalledTimes(4)
            expect(coordinator.revision).toBe(14)
            expect(coordinator.pendingBytes).toBe(1)
            expect(vi.getTimerCount()).toBeGreaterThan(0)
        } finally {
            vi.useRealTimers()
        }
    })

    it('keeps a continuously edited non-selected resident target dirty after compensation', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const selected = structuredClone(database.characters[0])
            selected.chaId = 'char-b'
            selected.name = 'Beta'
            database.characters.push(selected)
            let coordinator!: SaveCoordinator
            const commit = vi.fn(async ({ expectedRevision }) => {
                const call = commit.mock.calls.length
                if (call <= 4) {
                    ;(database.characters[0] as character).desc = `Concurrent inactive edit ${call}`
                    coordinator.markPersistentDataDirty(1)
                }
                return { revision: expectedRevision + 1 }
            })
            const store = {
                commit,
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[1],
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
            })
            coordinator.initialize(10)

            await expect(coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'continuous-inactive-resident-edits',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )).rejects.toThrow('Resident character changed')

            expect(commit).toHaveBeenCalledTimes(4)
            expect(coordinator.pendingBytes).toBe(1)
            expect(vi.getTimerCount()).toBeGreaterThan(0)

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledTimes(5)
            expect(commit.mock.calls[4][0]).toMatchObject({
                expectedRevision: 14,
                replaceCharacter: expect.objectContaining({
                    chaId: 'char-a',
                    desc: 'Concurrent inactive edit 4',
                }),
            })
            expect(coordinator.revision).toBe(15)
            expect(coordinator.pendingBytes).toBe(0)
        } finally {
            vi.useRealTimers()
        }
    })

    it('publishes a deferred target compensation when a tokened replacement then rejects', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const selected = structuredClone(database.characters[0])
            selected.chaId = 'char-b'
            selected.name = 'Beta'
            database.characters.push(selected)
            let coordinator!: SaveCoordinator
            const commit = vi.fn(async ({ expectedRevision }) => {
                const call = commit.mock.calls.length
                if (call <= 4) {
                    ;(database.characters[0] as character).desc = `Concurrent inactive edit ${call}`
                    coordinator.markPersistentDataDirty(1)
                }
                return { revision: expectedRevision + 1 }
            })
            const store = {
                commit,
                replaceFromDatabase: vi.fn(),
                readRoot: vi.fn(async () => ({ revision: 10, value: captureRoot(database) })),
                readCharacter: vi.fn(async () => ({
                    revision: 10,
                    value: { type: 'character', chaId: 'char-a', name: 'Alpha' },
                })),
                queryConversations: vi.fn(async () => ({ revision: 10, items: [] })),
            } as unknown as PersistentDataStore
            const publishedRevisions: number[] = []
            const pin = vi.fn(async (revision: number) => ({
                publish: async () => {
                    publishedRevisions.push(revision)
                },
                dispose: async () => undefined,
            }))
            coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[1],
                captureCharacter: (id) =>
                    database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
            })
            coordinator.initialize(10)

            await expect(coordinator.replacePersistentCompleteCharacter(
                'char-a',
                'continuous-inactive-resident-edits',
                (current) => ({ ...current, name: 'Explicit replacement' }),
            )).rejects.toThrow('Resident character changed')
            expect(publishedRevisions).toEqual([14])

            await expect(coordinator.replacePersistentDatabase(
                makeDatabase(),
                'stale-tokened-replacement',
                { authoritative: true, expectedRevision: 14 },
            )).rejects.toBeInstanceOf(RevisionConflictError)

            expect(commit).toHaveBeenCalledTimes(5)
            expect(coordinator.revision).toBe(15)
            expect(store.replaceFromDatabase).not.toHaveBeenCalled()
            expect(publishedRevisions).toEqual([14])

            await vi.advanceTimersByTimeAsync(3_000)

            expect(pin).toHaveBeenLastCalledWith(15)
            expect(publishedRevisions).toEqual([14, 15])
        } finally {
            vi.useRealTimers()
        }
    })

    it('materializes a detached authoritative snapshot without publishing it', async () => {
        const database = makeDatabase()
        const snapshot = makeDatabase()
        snapshot.username = 'Authoritative snapshot'
        const store = {
            materializeDatabase: vi.fn(async (revision?: number) => {
                expect(revision).toBe(12)
                return snapshot
            }),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(12)

        const materialized = await coordinator.materializePersistentDatabaseSnapshot(
            'explicit-compatibility-snapshot',
        )

        expect(materialized).toEqual(snapshot)
        expect(materialized).not.toBe(snapshot)
        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(12)
    })

    it('flushes and captures one atomic revision and mutation generation token', async () => {
        const database = makeDatabase()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)
        database.username = 'Flushed before pin'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.capturePersistentMutationToken(
            'sync-conflict-safety-export',
        )).resolves.toEqual({
            revision: 13,
            mutationGeneration: 1,
        })
    })

    it('returns the snapshot revision atomically before a queued replacement advances it', async () => {
        const database = makeDatabase()
        const snapshot = makeDatabase()
        snapshot.username = 'Revision 12 snapshot'
        const store = {
            materializeDatabase: vi.fn(async () => snapshot),
            replaceFromDatabase: vi.fn(async () => ({ revision: 13 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(12)

        const materializing = coordinator.materializePersistentDatabaseSnapshotWithRevision(
            'versioned-snapshot',
        )
        const replacing = coordinator.replacePersistentDatabase(
            makeDatabase(),
            'queued-replacement',
            { authoritative: true },
        )

        await expect(materializing).resolves.toEqual({
            revision: 12,
            mutationGeneration: 0,
            database: snapshot,
        })
        await replacing
        expect(coordinator.revision).toBe(13)
    })

    it('reads one detached complete character without publishing it', async () => {
        const database = makeDatabase()
        const chat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readCharacter: vi.fn(async () => ({
                revision: 13,
                value: { type: 'character', chaId: 'char-a', name: 'Stored detail' },
            })),
            queryConversations: vi.fn(async () => ({
                revision: 13,
                items: [{
                    id: 'chat-a',
                    characterId: 'char-a',
                    name: 'Stored chat',
                    configuredIndex: 0,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 13, value: chat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(13)

        const character = await coordinator.readPersistentCompleteCharacter(
            'char-a',
            'mcp-character-read',
        )

        expect(character).toEqual(expect.objectContaining({
            chaId: 'char-a',
            chats: [chat],
        }))
        expect(publishCharacterMutation).not.toHaveBeenCalled()
    })

    it('reads one detached authoritative conversation without changing selection', async () => {
        const database = makeDatabase()
        const chat = {
            id: 'chat-a',
            name: 'Stored chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readConversation: vi.fn(async () => ({ revision: 17, value: chat })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(17)

        const conversation = await coordinator.readPersistentConversation(
            'char-a',
            'chat-a',
            'mcp-conversation-read',
        )

        expect(conversation).toEqual(chat)
        expect(conversation).not.toBe(chat)
        expect(store.readConversation).toHaveBeenCalledWith('char-a', 'chat-a')
        expect(publishCharacterMutation).not.toHaveBeenCalled()
    })

    it('reads one ordered-position conversation when configured indexes have gaps', async () => {
        const database = makeDatabase()
        const chat = { id: 'chat-c', name: 'Third', note: '', localLore: [], message: [] }
        const store = {
            queryConversations: vi.fn(async () => ({
                revision: 18,
                items: [{
                    id: 'chat-c',
                    characterId: 'char-a',
                    name: 'Third',
                    configuredIndex: 7,
                    recentAt: 0,
                    messageCount: 0,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 18, value: chat })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(18)

        const conversation = await coordinator.readPersistentConversationAt(
            'char-a',
            2,
            'mcp-selected-conversation-read',
        )

        expect(conversation).toEqual(chat)
        expect(store.queryConversations).toHaveBeenCalledWith({
            characterId: 'char-a',
            order: 'configured',
            limit: 1,
            cursor: '2',
        })
        expect(store.readConversation).toHaveBeenCalledOnce()
    })

    it('reads character detail and its selected conversation in one serialized revision', async () => {
        const database = makeDatabase()
        const firstCharacterRead = deferred<{
            revision: number
            value: Database['characters'][number]
        }>()
        const character = {
            ...database.characters[0],
            chatPage: 1,
        }
        const chat = {
            id: 'chat-b',
            name: 'Selected',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: 'authoritative' }],
        }
        const store = {
            readCharacter: vi.fn()
                .mockImplementationOnce(() => firstCharacterRead.promise)
                .mockResolvedValueOnce({ revision: 19, value: character }),
            queryConversations: vi.fn(async () => ({
                revision: 19,
                items: [{
                    id: 'chat-b',
                    characterId: 'char-a',
                    name: 'Selected',
                    configuredIndex: 4,
                    recentAt: 0,
                    messageCount: 1,
                }],
            })),
            readConversation: vi.fn(async () => ({ revision: 19, value: chat })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(19)

        const selectedRead = coordinator.readPersistentSelectedConversation(
            'char-a',
            'mcp-selected-conversation-read',
        )
        await vi.waitFor(() => expect(store.readCharacter).toHaveBeenCalledOnce())
        const laterRead = coordinator.readPersistentCharacterDetail(
            'char-a',
            'queued-character-read',
        )
        firstCharacterRead.resolve({ revision: 19, value: character })

        const selected = await selectedRead
        await laterRead

        expect(selected).toEqual({ character, conversation: chat })
        expect(selected?.character).not.toBe(character)
        expect(selected?.conversation).not.toBe(chat)
        expect(store.queryConversations).toHaveBeenCalledWith({
            characterId: 'char-a',
            order: 'configured',
            limit: 1,
            cursor: '1',
        })
        expect(vi.mocked(store.queryConversations).mock.invocationCallOrder[0])
            .toBeLessThan(vi.mocked(store.readCharacter).mock.invocationCallOrder[1])
    })

    it('distinguishes an existing character without a selected conversation', async () => {
        const database = makeDatabase()
        const character = { ...database.characters[0], chatPage: 0 }
        const store = {
            readCharacter: vi.fn(async () => ({ revision: 20, value: character })),
            queryConversations: vi.fn(async () => ({ revision: 20, items: [] })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(20)

        await expect(coordinator.readPersistentSelectedConversation(
            'char-a',
            'mcp-empty-conversation-read',
        )).resolves.toEqual({
            character,
            conversation: null,
        })
    })

    it('returns null only when the selected character is missing', async () => {
        const database = makeDatabase()
        const store = {
            readCharacter: vi.fn(async () => null),
            queryConversations: vi.fn(),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(21)

        await expect(coordinator.readPersistentSelectedConversation(
            'missing',
            'mcp-missing-character-read',
        )).resolves.toBeNull()
        expect(store.queryConversations).not.toHaveBeenCalled()
    })

    it('returns an existing empty group with a null selected conversation', async () => {
        const database = makeDatabase()
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            chatPage: 0,
            chats: [],
            characters: [],
        } as unknown as Database['characters'][number]
        const store = {
            readCharacter: vi.fn(async () => ({ revision: 22, value: group })),
            queryConversations: vi.fn(async () => ({ revision: 22, items: [] })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(22)

        await expect(coordinator.readPersistentSelectedConversation(
            'group-a',
            'mcp-empty-group-read',
        )).resolves.toEqual({ character: group, conversation: null })
    })

    it('atomically adds an absent complete character before publishing it', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const added = {
            type: 'character',
            chaId: 'temp-char',
            name: 'Temporary',
            chats: [],
        } as Database['characters'][number]
        const store = {
            readRoot: vi.fn(async () => ({ revision: 14, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(14)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'temp-char',
            'multiuser-temp-character',
            (current) => {
                expect(current).toBeNull()
                return added
            },
        )).resolves.toBe(true)

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 14,
            root: expect.objectContaining({ characterOrder: ['char-a', 'temp-char'] }),
            addCharacter: added,
        })
        expect(publishCharacterMutation).toHaveBeenCalledWith(expect.objectContaining({
            revision: 15,
            characterId: 'temp-char',
            kind: 'add',
            character: added,
        }))
    })

    it('uses replacement instead of adding a duplicate complete character ID', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 15, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => ({
                revision: 15,
                value: { type: 'character', chaId: 'char-a', name: 'Existing' },
            })),
            queryConversations: vi.fn(async () => ({ revision: 15, items: [] })),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(15)

        await coordinator.upsertPersistentCompleteCharacter(
            'char-a',
            'multiuser-existing-character',
            (current) => ({ ...current!, name: 'Updated' }),
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 15,
            replaceCharacter: expect.objectContaining({ chaId: 'char-a', name: 'Updated' }),
        })
        expect(vi.mocked(store.commit).mock.calls[0][0]).not.toHaveProperty('addCharacter')
    })

    it('does not publish an absent character when its atomic add fails', async () => {
        const database = makeDatabase()
        const store = {
            readRoot: vi.fn(async () => ({ revision: 16, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn().mockRejectedValue(new Error('add failed')),
        } as unknown as PersistentDataStore
        const publishCharacterMutation = vi.fn()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishCharacterMutation,
        })
        coordinator.initialize(16)

        await expect(coordinator.upsertPersistentCompleteCharacter(
            'temp-char',
            'multiuser-temp-character',
            () => ({ type: 'character', chaId: 'temp-char', name: 'Temporary', chats: [] } as any),
        )).rejects.toThrow('add failed')

        expect(publishCharacterMutation).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(16)
    })

    it('can add an absent sentinel without adding it to character order', async () => {
        const database = makeDatabase()
        database.characterOrder = ['char-a']
        const store = {
            readRoot: vi.fn(async () => ({ revision: 19, value: captureRoot(database) })),
            readCharacter: vi.fn(async () => null),
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(19)

        await coordinator.upsertPersistentCompleteCharacter(
            '§temp',
            'multiuser-sentinel',
            () => ({ type: 'character', chaId: '§temp', name: 'Temporary', chats: [] } as any),
            { includeInCharacterOrder: false },
        )

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 19,
            addCharacter: expect.objectContaining({ chaId: '§temp' }),
        })
    })

    it('rebases concurrent live root edits while reading complete presets', async () => {
        const database = makeDatabase()
        database.botPresetsId = 0
        const presetRead = deferred<{
            revision: number
            value: Database['botPresets'][number]
        }>()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 6, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 6,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(() => presetRead.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(6)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.botPresetsId = 1
        })
        await vi.waitFor(() => expect(store.readPreset).toHaveBeenCalledOnce())
        database.username = 'Concurrent root edit'
        coordinator.markPersistentDataDirty(1)
        presetRead.resolve({ revision: 6, value: { name: 'First' } as Database['botPresets'][number] })
        await mutation

        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 6,
            root: expect.objectContaining({
                botPresetsId: 1,
                username: 'Concurrent root edit',
            }),
            replacePresets: [{ name: 'First' }],
        })
        expect(database.username).toBe('Concurrent root edit')
    })

    it('preserves a same-field live root edit made during preset reads', async () => {
        const database = makeDatabase()
        database.mainPrompt = 'Initial prompt'
        const presetRead = deferred<{
            revision: number
            value: Database['botPresets'][number]
        }>()
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            readRoot: vi.fn(async () => ({ revision: 8, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 8,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(() => presetRead.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(8)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.mainPrompt = 'Preset prompt'
        })
        await vi.waitFor(() => expect(store.readPreset).toHaveBeenCalledOnce())
        database.mainPrompt = 'Later live prompt'
        coordinator.markPersistentDataDirty(1)
        presetRead.resolve({
            revision: 8,
            value: { name: 'First' } as Database['botPresets'][number],
        })
        await mutation

        expect(store.commit).toHaveBeenCalledWith(expect.objectContaining({
            expectedRevision: 8,
            root: expect.objectContaining({ mainPrompt: 'Later live prompt' }),
        }))
        expect(database.mainPrompt).toBe('Later live prompt')
    })

    it('preserves and later flushes live root edits made while a preset commit is pending', async () => {
        const database = makeDatabase()
        database.botPresetsId = 0
        const presetCommit = deferred<{ revision: number }>()
        const commit = vi.fn(({ expectedRevision }: { expectedRevision: number }) => {
            if (commit.mock.calls.length === 1) return presetCommit.promise
            return Promise.resolve({ revision: expectedRevision + 1 })
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 6, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 6,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 6,
                value: { name: 'First' },
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(6)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.botPresetsId = 1
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.username = 'Edit during preset commit'
        coordinator.markPersistentDataDirty(1)
        presetCommit.resolve({ revision: 7 })
        await mutation

        expect(database).toMatchObject({
            botPresetsId: 1,
            username: 'Edit during preset commit',
        })
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 6,
            root: {
                botPresetsId: 1,
                username: 'Fixture',
            },
        })

        await coordinator.flushPendingData('after-preset-commit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 7,
            root: {
                botPresetsId: 1,
                username: 'Edit during preset commit',
            },
        })
        expect(commit.mock.calls[1][0]).not.toHaveProperty('replacePresets')
        expect(coordinator.revision).toBe(8)
    })

    it.each([
        ['scalable', false],
        ['maximum', true],
    ])('preserves a later same-field root edit during a pending %s preset commit', async (
        _profile,
        capturesCompletePresets,
    ) => {
        const database = makeDatabase()
        database.mainPrompt = 'initial'
        database.botPresets = [{ name: 'First' }] as Database['botPresets']
        const presetCommit = deferred<{ revision: number }>()
        const commit = vi.fn(({ expectedRevision }: { expectedRevision: number }) => {
            if (commit.mock.calls.length === 1) return presetCommit.promise
            return Promise.resolve({ revision: expectedRevision + 1 })
        })
        const store = {
            commit,
            readRoot: vi.fn(async () => ({ revision: 30, value: captureRoot(database) })),
            queryPresets: vi.fn(async () => ({
                revision: 30,
                items: [{ id: '0', configuredIndex: 0, name: 'First' }],
            })),
            readPreset: vi.fn(async () => ({
                revision: 30,
                value: { name: 'First' },
            })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => capturesCompletePresets ? database.botPresets : null,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
            publishPresetWorkingSet: ({ root }) => Object.assign(database, root),
        })
        coordinator.initialize(30)

        const mutation = coordinator.mutatePersistentPresets('switch', ({ root }) => {
            root.mainPrompt = 'preset selection'
        })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.mainPrompt = 'later user edit'
        coordinator.markPersistentDataDirty(1)
        presetCommit.resolve({ revision: 31 })
        await mutation

        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 30,
            root: expect.objectContaining({ mainPrompt: 'preset selection' }),
        })
        expect(database.mainPrompt).toBe('later user edit')

        await coordinator.flushPendingData('after-preset-same-field-edit')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 31,
            root: expect.objectContaining({ mainPrompt: 'later user edit' }),
        })
        expect(coordinator.revision).toBe(32)
    })

    it('does not commit when the persistent working copy is clean', async () => {
        const database = makeDatabase()
        const store = { commit: vi.fn() } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)

        await coordinator.flushPendingData('test')

        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(4)
    })

    it('captures root and the selected character without traversing inactive characters', async () => {
        const database = makeDatabase()
        const inactive = makeDatabase().characters[0]
        Object.defineProperty(inactive, 'chats', {
            enumerable: true,
            get: () => {
                throw new Error('inactive character was traversed')
            },
        })
        database.characters.push(inactive)
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })

        coordinator.initialize(1)
        await coordinator.flushPendingData('clean')
    })

    it('initializes baselines from the supplied authoritative database', async () => {
        let database = makeDatabase()
        database.username = 'Stale live state'
        const authoritative = makeDatabase()
        authoritative.username = 'Authoritative state'
        authoritative.characters[0].name = 'Authoritative character'
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
        })

        coordinator.initialize(6, authoritative)
        database = structuredClone(authoritative)
        await coordinator.flushPendingData('clean')

        expect(store.commit).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(6)
    })

    it('returns the exact shared promise for concurrent flushes', async () => {
        const database = makeDatabase()
        const pending = deferred<{ revision: number }>()
        const store = makeStore(vi.fn(() => pending.promise))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(10)

        const first = coordinator.flushPendingData('first')
        const second = coordinator.flushPendingData('second')

        expect(second).toBe(first)
        pending.resolve({ revision: 2 })
        await first
    })

    it('reports the exact shared flush promise and clears it after completion', async () => {
        const database = makeDatabase()
        const pending = deferred<{ revision: number }>()
        const reported: Array<Promise<void> | null> = []
        const coordinator = new SaveCoordinator({
            store: makeStore(vi.fn(() => pending.promise)),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
        })
        coordinator.initialize(1)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(1)

        const first = coordinator.flushPendingData('first')
        const second = coordinator.flushPendingData('second')
        pending.resolve({ revision: 2 })
        await first
        await Promise.resolve()

        expect(second).toBe(first)
        expect(reported).toEqual([first, null])
    })

    it('debounces for 500 ms and flushes immediately at the pending byte limit', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Debounced'

            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(499)
            expect(store.commit).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(1)
            expect(store.commit).toHaveBeenCalledTimes(1)

            database.username = 'Immediate'
            coordinator.markPersistentDataDirty(1_048_576)
            await vi.advanceTimersByTimeAsync(0)
            expect(store.commit).toHaveBeenCalledTimes(2)
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).toHaveBeenCalledTimes(2)
        } finally {
            vi.useRealTimers()
        }
    })

    it('treats byte estimates as absolute pending size instead of summing them', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const store = makeStore(vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })))
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Large snapshots'

            coordinator.markPersistentDataDirty(600_000)
            coordinator.markPersistentDataDirty(600_000)

            expect(coordinator.pendingBytes).toBe(600_000)
            await vi.advanceTimersByTimeAsync(0)
            expect(store.commit).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).toHaveBeenCalledTimes(1)
        } finally {
            vi.useRealTimers()
        }
    })

    it('starts one immediate flush while estimates stay above the byte limit', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const gate = deferred<{ revision: number }>()
            const commit = vi.fn().mockImplementationOnce(() => gate.promise)
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
            })
            coordinator.initialize(1)
            database.username = 'Huge'

            coordinator.markPersistentDataDirty(2_097_152)
            await vi.advanceTimersByTimeAsync(0)
            expect(commit).toHaveBeenCalledTimes(1)
            coordinator.markPersistentDataDirty(2_097_152)
            coordinator.markPersistentDataDirty(2_097_152)
            await vi.advanceTimersByTimeAsync(0)
            expect(commit).toHaveBeenCalledTimes(1)

            gate.resolve({ revision: 2 })
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledTimes(1)
            expect(coordinator.pendingBytes).toBe(0)
        } finally {
            vi.useRealTimers()
        }
    })

    it('clamps invalid estimated byte counts to zero while retaining dirty work', async () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        coordinator.markPersistentDataDirty(-1)
        coordinator.markPersistentDataDirty(Number.NaN)
        coordinator.markPersistentDataDirty(Number.POSITIVE_INFINITY)

        expect(coordinator.pendingBytes).toBe(0)
        await coordinator.flushPendingData('cleanup')
    })

    it('commits only changed root and complete selected-character fields', async () => {
        const database = makeDatabase()
        database.characters[0].chats = [
            { id: 'one', name: 'One', message: [], localLore: [], note: '' },
            { id: 'two', name: 'Two', message: [], localLore: [], note: '' },
        ]
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = makeStore(commit)
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.username = 'Root changed'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('root')
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 2,
            root: { username: 'Root changed' },
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')

        database.characters[0].chats[1].name = 'Inactive renamed'
        database.characters[0].chats.reverse()
        database.characters[0].chats.push({
            id: 'three',
            name: 'Three',
            message: [],
            localLore: [],
            note: '',
        })
        database.characters[0].chats.splice(1, 1)
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('character')
        expect(commit.mock.calls[1][0]).not.toHaveProperty('root')
        expect(commit.mock.calls[1][0].replaceCharacter.chats.map((chat: { id: string }) => chat.id)).toEqual([
            'two',
            'three',
        ])

        database.username = 'Combined root'
        database.characters[0].name = 'Combined character'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('combined')
        expect(commit.mock.calls[2][0]).toMatchObject({
            expectedRevision: 4,
            root: { username: 'Combined root' },
            replaceCharacter: { name: 'Combined character' },
        })
    })

    function makeChattyDatabase() {
        const database = makeDatabase()
        database.characters[0].chats = [
            {
                id: 'one',
                name: 'One',
                message: [{ role: 'user', data: 'hello one' }],
                localLore: [],
                note: '',
            },
            {
                id: 'two',
                name: 'Two',
                message: [
                    { role: 'user', data: 'hello two' },
                    { role: 'char', data: 'reply two' },
                ],
                localLore: [],
                note: '',
            },
        ]
        return database
    }

    it.each([
        {
            label: 'tail edit',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message[1] = { role: 'char', data: 'edited reply' }
            },
            expected: {
                start: 1,
                deleteCount: 1,
                messages: [{ role: 'char', data: 'edited reply' }],
            },
        },
        {
            label: 'middle insert',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message.splice(1, 0, { role: 'user', data: 'inserted' })
            },
            expected: {
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'inserted' }],
            },
        },
        {
            label: 'middle delete',
            prepare: (chat: Chat) => {
                chat.message.push({ role: 'user', data: 'shared suffix' })
            },
            mutate: (chat: Chat) => {
                chat.message.splice(1, 1)
            },
            expected: {
                start: 1,
                deleteCount: 1,
                messages: [],
            },
        },
        {
            label: 'complete replacement',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.message.splice(
                    0,
                    chat.message.length,
                    { role: 'char', data: 'replacement one' },
                    { role: 'user', data: 'replacement two' },
                )
            },
            expected: {
                start: 0,
                deleteCount: 2,
                messages: [
                    { role: 'char', data: 'replacement one' },
                    { role: 'user', data: 'replacement two' },
                ],
            },
        },
        {
            label: 'metadata-only mutation',
            prepare: (_chat: Chat) => undefined,
            mutate: (chat: Chat) => {
                chat.name = 'Renamed conversation'
            },
            expected: {
                start: 2,
                deleteCount: 0,
                messages: [],
            },
        },
    ])('commits the minimal conversation replace range for $label', async ({ prepare, mutate, expected }) => {
        const database = makeChattyDatabase()
        prepare(database.characters[0].chats[1])
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        mutate(database.characters[0].chats[1])
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-range')

        const mutation = commit.mock.calls[0][0].conversations[0]
        expect(mutation).toMatchObject({
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            ...expected,
        })
        expect(mutation.conversation).toEqual({
            id: 'two',
            name: database.characters[0].chats[1].name,
            localLore: [],
            note: '',
        })
    })

    it('preserves a shared suffix around a middle message edit', async () => {
        const database = makeChattyDatabase()
        database.characters[0].chats[1].message.push({ role: 'user', data: 'shared suffix' })
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message[1] = { role: 'char', data: 'middle edit' }
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-range')

        expect(commit.mock.calls[0][0].conversations[0]).toMatchObject({
            type: 'replace-range',
            characterId: 'char-a',
            conversationId: 'two',
            start: 1,
            deleteCount: 1,
            messages: [{ role: 'char', data: 'middle edit' }],
        })
    })

    it('commits only the edited conversation when just chat content changes', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message.push({ role: 'user', data: 'follow-up' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('message-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(commit.mock.calls[0][0]).not.toHaveProperty('root')
        expect(commit.mock.calls[0][0].conversations).toEqual([
            {
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'two',
                start: 2,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'follow-up' }],
                conversation: { id: 'two', name: 'Two', localLore: [], note: '' },
            },
        ])
        expect(coordinator.revision).toBe(3)

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(1)
    })

    it('replaces the whole character when a character field changes alongside chats', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].name = 'Alpha renamed'
        database.characters[0].chats[0].message.push({ role: 'char', data: 'more' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('detail-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({ name: 'Alpha renamed' })
    })

    it.each([
        ['added', (chats: { id?: string }[]) => chats.push({
            id: 'three',
            name: 'Three',
            message: [],
            localLore: [],
            note: '',
        } as never)],
        ['removed', (chats: { id?: string }[]) => chats.splice(0, 1)],
        ['reordered', (chats: { id?: string }[]) => chats.reverse()],
    ] as const)('replaces the whole character when chats are %s', async (_label, mutate) => {
        const database = makeChattyDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        mutate(database.characters[0].chats)
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('structure-edit')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0].replaceCharacter.chats.map((chat: { id: string }) => chat.id))
            .toEqual(database.characters[0].chats.map((chat) => chat.id))

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(1)
    })

    it('replaces the whole character when a chat is missing an id', async () => {
        const database = makeChattyDatabase()
        delete database.characters[0].chats[1].id
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[1].message.push({ role: 'user', data: 'anonymous' })
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('missing-id')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0]).toHaveProperty('replaceCharacter')
    })

    it('keeps conversation edits dirty when the mutation commit fails', async () => {
        const database = makeChattyDatabase()
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(2)

        database.characters[0].chats[0].message.push({ role: 'user', data: 'retry me' })
        coordinator.markPersistentDataDirty(5)
        await expect(coordinator.flushPendingData('fails')).rejects.toThrow('write failed')

        expect(coordinator.revision).toBe(2)
        expect(coordinator.pendingBytes).toBe(5)

        await coordinator.flushPendingData('retry')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].conversations).toMatchObject([
            {
                conversationId: 'one',
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'user', data: 'retry me' }],
            },
        ])
        expect(coordinator.revision).toBe(3)
    })

    it('reports a successful local revision once before official publication', async () => {
        const database = makeDatabase()
        const events: string[] = []
        const store = makeStore(vi.fn(async ({ expectedRevision }) => {
            events.push('commit')
            return { revision: expectedRevision + 1 }
        }))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            onLocalRevision: (revision) => events.push(`local:${revision}`),
            officialPublisher: {
                pin: async () => ({
                    publish: async () => {
                        events.push('publish')
                    },
                    dispose: async () => undefined,
                }),
            },
        })
        coordinator.initialize(4)
        database.username = 'Changed'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('test')
        await coordinator.flushPendingData('clean')

        expect(events).toEqual(['commit', 'local:5', 'publish'])
    })

    it('installs and commits a live character addition with root and previous selected edits', async () => {
        const { database, added } = makeAdditionDatabase()
        const install = vi.fn(() => database.characters.push(added))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(4)
        database.username = 'Root changed'
        database.characters[0].name = 'Selected changed'

        await coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 123,
            install,
        }, 'new-character')

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 4,
            root: { username: 'Root changed' },
            replaceCharacter: { chaId: 'char-a', name: 'Selected changed' },
            addCharacter: { chaId: 'char-added', name: 'Added' },
        })
        expect(coordinator.revision).toBe(5)
    })

    it('reports a standalone addition exact promise and then idle state', async () => {
        const { database, added } = makeAdditionDatabase()
        const pending = deferred<{ revision: number }>()
        const reported: Array<Promise<void> | null> = []
        const coordinator = new SaveCoordinator({
            store: makeStore(vi.fn(() => pending.promise)),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
        })
        coordinator.initialize(1)

        const addition = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')

        expect(reported).toEqual([addition])
        pending.resolve({ revision: 2 })
        await addition
        await Promise.resolve()
        expect(reported).toEqual([addition, null])
    })

    it.each(['store', 'pin', 'publish'] as const)(
        'persists live character edits made while awaiting %s',
        async (stage) => {
            const { database, added } = makeAdditionDatabase()
            const storeGate = deferred<{ revision: number }>()
            const pinGate = deferred<{ publish(): Promise<void>; dispose(): Promise<void> }>()
            const publishGate = deferred<void>()
            const commit = vi.fn()
                .mockImplementationOnce(() => stage === 'store' ? storeGate.promise : Promise.resolve({ revision: 2 }))
                .mockResolvedValueOnce({ revision: 3 })
            const handle = {
                publish: vi.fn(() => stage === 'publish' ? publishGate.promise : Promise.resolve()),
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn(() => stage === 'pin' ? pinGate.promise : Promise.resolve(handle))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            const saving = coordinator.commitCharacterAddition({
                characterId: added.chaId,
                estimatedBytes: 1,
                install: () => database.characters.push(added),
            }, 'new-character')
            await vi.waitFor(() => {
                if (stage === 'store') expect(commit).toHaveBeenCalledOnce()
                if (stage === 'pin') expect(pin).toHaveBeenCalledOnce()
                if (stage === 'publish') expect(handle.publish).toHaveBeenCalledOnce()
            })
            added.name = `Changed during ${stage}`
            coordinator.markPersistentDataDirty(1)
            if (stage === 'store') storeGate.resolve({ revision: 2 })
            if (stage === 'pin') pinGate.resolve(handle)
            if (stage === 'publish') publishGate.resolve()
            await saving

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toMatchObject({
                expectedRevision: 2,
                replaceCharacter: { chaId: 'char-added', name: `Changed during ${stage}` },
            })
        },
    )

    it('serializes previous selected and added-character edits into separate trailing replacements', async () => {
        const { database, added } = makeAdditionDatabase()
        const firstPublish = deferred<void>()
        const publish = vi.fn()
            .mockImplementationOnce(() => firstPublish.promise)
            .mockResolvedValue(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            officialPublisher: {
                pin: async () => ({ publish, dispose: vi.fn(async () => undefined) }),
            },
        })
        coordinator.initialize(1)
        const saving = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        database.characters[0].name = 'Selected during publish'
        added.name = 'Added during publish'
        firstPublish.resolve()
        await saving

        expect(commit).toHaveBeenCalledTimes(3)
        expect(commit.mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Selected during publish',
        })
        expect(commit.mock.calls[2][0].replaceCharacter).toMatchObject({
            chaId: 'char-added',
            name: 'Added during publish',
        })
    })

    it('publishes only the newest revision after addition edits committed while offline', async () => {
        const { database, added } = makeAdditionDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn(async (_revision: number) => handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('offline')
        added.name = 'Edited while offline'
        coordinator.markPersistentDataDirty(1)
        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(pin).toHaveBeenCalledTimes(2)
        expect(pin.mock.calls.map((call) => call[0])).toEqual([2, 3])
        expect(publish).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].replaceCharacter.name).toBe('Edited while offline')
    })

    it('gives an addition requested during an older failing publication its own serialized turn', async () => {
        const { database, added } = makeAdditionDatabase()
        const oldPublish = deferred<void>()
        const reported: Array<Promise<void> | null> = []
        const publish = vi.fn()
            .mockImplementationOnce(() => oldPublish.promise)
            .mockResolvedValue(undefined)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
            onFlushPromise: (promise) => reported.push(promise),
            officialPublisher: {
                pin: async () => ({ publish, dispose: vi.fn(async () => undefined) }),
            },
        })
        coordinator.initialize(1)
        database.username = 'Older change'
        coordinator.markPersistentDataDirty(1)
        const olderFlush = coordinator.flushPendingData('older')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        const install = vi.fn(() => database.characters.push(added))
        const addition = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install,
        }, 'new-character')

        expect(addition).not.toBe(olderFlush)
        expect(reported).toEqual([olderFlush, addition])
        expect(install).not.toHaveBeenCalled()
        oldPublish.reject(new Error('older publication failed'))
        await expect(olderFlush).rejects.toThrow('older publication failed')
        await addition
        await Promise.resolve()

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(reported).toEqual([olderFlush, addition, null])
    })

    it('keeps an installed addition dirty after local failure for one explicit retry', async () => {
        const { database, added } = makeAdditionDatabase()
        const install = vi.fn(() => database.characters.push(added))
        const commit = vi.fn().mockRejectedValueOnce(new Error('write failed')).mockResolvedValueOnce({ revision: 2 })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 7,
            install,
        }, 'new-character')).rejects.toThrow('write failed')
        await coordinator.flushPendingData('retry')

        expect(install).toHaveBeenCalledOnce()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(coordinator.revision).toBe(2)
    })

    it('releases the reservation when install throws so later additions still run', async () => {
        const { database, added } = makeAdditionDatabase()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: 'char-broken',
            estimatedBytes: 1,
            install: () => {
                throw new Error('install failed')
            },
        }, 'broken')).rejects.toThrow('install failed')
        expect(commit).not.toHaveBeenCalled()

        await coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')

        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0].addCharacter).toMatchObject({ chaId: 'char-added' })
    })

    it('surfaces the pending conflict to a later import instead of blocking it', async () => {
        const { database, added } = makeAdditionDatabase()
        const gate = deferred<{ revision: number }>()
        const conflict = new RevisionConflictError(1, 2)
        const commit = vi.fn(() => gate.promise)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        const first = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 9,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        gate.reject(conflict)
        await expect(first).rejects.toBe(conflict)
        expect(commit).toHaveBeenCalledOnce()

        const secondInstall = vi.fn()
        await expect(coordinator.commitCharacterAddition({
            characterId: 'char-other',
            estimatedBytes: 1,
            install: secondInstall,
        }, 'other-character')).rejects.toBe(conflict)

        expect(secondInstall).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledTimes(2)
        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(9)
    })

    it('lets a later import succeed after a transient addition failure', async () => {
        const { database, added } = makeAdditionDatabase()
        const other = structuredClone(added)
        other.chaId = 'char-other'
        other.name = 'Other'
        const commit = vi.fn()
            .mockRejectedValueOnce(new Error('write failed'))
            .mockImplementation(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('write failed')

        await coordinator.commitCharacterAddition({
            characterId: other.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(other),
        }, 'other-character')

        expect(commit.mock.calls[1][0].addCharacter).toMatchObject({ chaId: 'char-added' })
        expect(commit.mock.calls[2][0].addCharacter).toMatchObject({ chaId: 'char-other' })
        expect(coordinator.revision).toBe(3)
    })

    it('defers a second import that starts while the first is still committing', async () => {
        const { database, added } = makeAdditionDatabase()
        const other = structuredClone(added)
        other.chaId = 'char-other'
        other.name = 'Other'
        const gate = deferred<{ revision: number }>()
        let commits = 0
        const commit = vi.fn(async () => {
            commits += 1
            return commits === 1 ? gate.promise : { revision: 3 }
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        const first = coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 9,
            install: () => database.characters.push(added),
        }, 'new-character')
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())

        let installedSecond = false
        const second = coordinator.commitCharacterAddition({
            characterId: other.chaId,
            estimatedBytes: 1,
            install: () => {
                installedSecond = true
                database.characters.push(other)
            },
        }, 'other-character')
        expect(installedSecond).toBe(false)

        gate.resolve({ revision: 2 })
        await first
        await second

        expect(installedSecond).toBe(true)
        expect(database.characters.map((item) => item.chaId)).toContain('char-other')
    })

    it('runs an earlier replacement before installing an addition', async () => {
        let database = makeDatabase()
        const replacement = makeDatabase()
        replacement.username = 'Replacement'
        const replacementGate = deferred<{ revision: number }>()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = {
            commit,
            replaceFromDatabase: vi.fn(() => replacementGate.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => {
                database = structuredClone(candidate)
            },
        })
        coordinator.initialize(1)
        const replacing = coordinator.replacePersistentDatabase(replacement, 'replace')
        const install = vi.fn(() => {
            const added = structuredClone(database.characters[0])
            added.chaId = 'char-added'
            database.characters.push(added)
        })
        const adding = coordinator.commitCharacterAddition({
            characterId: 'char-added',
            estimatedBytes: 1,
            install,
        }, 'new-character')
        expect(install).not.toHaveBeenCalled()
        replacementGate.resolve({ revision: 2 })
        await replacing
        await adding

        expect(install).toHaveBeenCalledOnce()
        expect(database.username).toBe('Replacement')
        expect(commit.mock.calls[0][0]).toMatchObject({
            expectedRevision: 2,
            addCharacter: { chaId: 'char-added' },
        })
    })

    it('successful replacement clears a failed addition and failed replacement preserves it', async () => {
        const { database, added } = makeAdditionDatabase()
        const commit = vi.fn().mockRejectedValue(new Error('addition failed'))
        const replaceFromDatabase = vi.fn()
            .mockRejectedValueOnce(new Error('replacement failed'))
            .mockResolvedValueOnce({ revision: 2 })
        const store = { commit, replaceFromDatabase } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => Object.assign(database, structuredClone(candidate)),
        })
        coordinator.initialize(1)
        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('addition failed')
        await expect(coordinator.replacePersistentDatabase(makeDatabase(), 'failed-replace')).rejects.toThrow('replacement failed')
        await expect(coordinator.flushPendingData('still-pending')).rejects.toThrow('addition failed')
        expect(commit).toHaveBeenCalledTimes(2)

        await coordinator.replacePersistentDatabase(makeDatabase(), 'successful-replace')
        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('authoritative replacement supersedes a locally added character after remote failure', async () => {
        let { database, added } = makeAdditionDatabase()
        const staleHandle = {
            publish: vi.fn().mockRejectedValue(new Error('offline')),
            dispose: vi.fn(async () => undefined),
        }
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = {
            commit,
            replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: (candidate) => {
                database = structuredClone(candidate)
            },
            officialPublisher: { pin: async () => staleHandle },
        })
        coordinator.initialize(1)
        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('offline')

        await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')
        await coordinator.flushPendingData('clean')

        expect(database.characters.map((character) => character.chaId)).toEqual(['char-a'])
        expect(commit).toHaveBeenCalledOnce()
        expect(staleHandle.dispose).toHaveBeenCalledOnce()
    })

    it('does not resurrect a character the replacement removed', async () => {
        vi.useFakeTimers()
        try {
            let db = makeDatabase()
            const replacementGate = deferred<{ revision: number }>()
            const store = {
                commit: vi.fn(),
                replaceFromDatabase: vi.fn(() => replacementGate.promise),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(db),
                captureSelectedCharacter: () => db.characters[0] ?? null,
                replaceDatabase: (replacement) => {
                    db = replacement
                },
            })
            coordinator.initialize(1)

            const candidate = makeDatabase()
            candidate.characters = []
            const replacing = coordinator.replacePersistentDatabase(candidate, 'remove-character')
            db.characters[0].name = 'Edited after enqueue'
            coordinator.markPersistentDataDirty(1)
            replacementGate.resolve({ revision: 2 })
            await replacing

            expect(db.characters).toEqual([])
            await vi.advanceTimersByTimeAsync(500)
            expect(store.commit).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })

    it('reports a successful replacement revision and reports nothing on failure', async () => {
        let database = makeDatabase()
        const revisions: number[] = []
        const store = makeStore()
        vi.mocked(store.replaceFromDatabase)
            .mockResolvedValueOnce({ revision: 8 })
            .mockRejectedValueOnce(new Error('replace failed'))
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
            onLocalRevision: (revision) => revisions.push(revision),
        })
        coordinator.initialize(7)

        await coordinator.replacePersistentDatabase(makeDatabase(), 'first')
        await expect(
            coordinator.replacePersistentDatabase(makeDatabase(), 'second'),
        ).rejects.toThrow('replace failed')

        expect(revisions).toEqual([8])
    })

    it('runs trailing commits for mutations made during every deferred commit', async () => {
        const database = makeDatabase()
        const first = deferred<{ revision: number }>()
        const second = deferred<{ revision: number }>()
        const commit = vi
            .fn()
            .mockImplementationOnce(() => first.promise)
            .mockImplementationOnce(() => second.promise)
            .mockResolvedValueOnce({ revision: 4 })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'one'
        coordinator.markPersistentDataDirty(1)
        const flushing = coordinator.flushPendingData('test')
        await Promise.resolve()
        database.username = 'two'
        coordinator.markPersistentDataDirty(1)
        first.resolve({ revision: 2 })
        await Promise.resolve()
        await Promise.resolve()
        database.username = 'three'
        coordinator.markPersistentDataDirty(1)
        second.resolve({ revision: 3 })

        await flushing

        expect(commit.mock.calls.map((call) => call[0].expectedRevision)).toEqual([1, 2, 3])
        expect(commit.mock.calls.map((call) => call[0].root.username)).toEqual(['one', 'two', 'three'])
    })

    it.each([
        new Error('write failed'),
        new RevisionConflictError(1, 2),
    ])('keeps failed work dirty without retrying for %s', async (error) => {
        const database = makeDatabase()
        const commit = vi.fn().mockRejectedValue(error)
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        database.username = 'Uncommitted'
        coordinator.markPersistentDataDirty(25)

        await expect(coordinator.flushPendingData('test')).rejects.toBe(error)

        expect(commit).toHaveBeenCalledTimes(1)
        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(25)
    })

    it('commits locally even when the official publish fails and retries the pin later', async () => {
        const database = makeDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn().mockResolvedValue(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        database.username = 'Offline edit'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.flushPendingData('offline')).rejects.toThrow('offline')

        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(2)

        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(commit).toHaveBeenCalledOnce()
        expect(pin).toHaveBeenCalledTimes(1)
        expect(publish).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenCalledWith(2)
    })

    it('autonomously retries a failed official publication while preserving its lease', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn()
                .mockRejectedValueOnce(new Error('offline'))
                .mockResolvedValueOnce(undefined)
            const handle = {
                publish,
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn(async () => handle)
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'Autonomous retry'
            coordinator.markPersistentDataDirty(1)

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledOnce()
            expect(pin).toHaveBeenCalledOnce()
            expect(publish).toHaveBeenCalledOnce()
            expect(handle.dispose).not.toHaveBeenCalled()
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await vi.advanceTimersByTimeAsync(2_999)
            expect(publish).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(1)

            expect(pin).toHaveBeenCalledOnce()
            expect(publish).toHaveBeenCalledTimes(2)
            expect(handle.dispose).toHaveBeenCalledOnce()
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('supersedes a failed publication with the newer local revision', async () => {
        const database = makeDatabase()
        let nowValue = 0
        const publish = vi.fn().mockRejectedValueOnce(new Error('remote')).mockResolvedValueOnce(undefined)
        const handle = { publish, dispose: vi.fn(async () => undefined) }
        const pin = vi.fn().mockResolvedValue(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        database.username = 'Local one'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('first')).rejects.toThrow('remote')
        database.username = 'Local two'
        coordinator.markPersistentDataDirty(1)

        nowValue = 4000
        await coordinator.flushPendingData('retry')

        expect(publish).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenCalledTimes(2)
        expect(pin).toHaveBeenNthCalledWith(1, 2)
        expect(pin).toHaveBeenNthCalledWith(2, 3)
        expect(handle.dispose).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].expectedRevision).toBe(2)
    })

    it('retries cleanup of a superseded publication without blocking the newer revision', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let nowValue = 0
            const staleHandle = {
                publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
                dispose: vi.fn()
                    .mockRejectedValueOnce(new Error('cleanup failed'))
                    .mockResolvedValueOnce(undefined),
            }
            const currentHandle = {
                publish: vi.fn(async () => undefined),
                dispose: vi.fn(async () => undefined),
            }
            const pin = vi.fn()
                .mockResolvedValueOnce(staleHandle)
                .mockResolvedValueOnce(currentHandle)
            const commit = vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            }))
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: vi.fn(),
                officialPublisher: { pin },
                now: () => nowValue,
                onBackgroundError,
            })
            coordinator.initialize(1)
            database.username = 'First revision'
            coordinator.markPersistentDataDirty(1)
            await expect(coordinator.flushPendingData('first')).rejects.toThrow('remote failed')

            nowValue = 4_000
            database.username = 'Newer revision'
            coordinator.markPersistentDataDirty(1)
            await coordinator.flushPendingData('newer')

            expect(pin).toHaveBeenNthCalledWith(2, 3)
            expect(currentHandle.publish).toHaveBeenCalledOnce()
            expect(currentHandle.dispose).toHaveBeenCalledOnce()
            expect(staleHandle.dispose).toHaveBeenCalledOnce()
            expect(onBackgroundError).toHaveBeenCalledWith(expect.objectContaining({
                message: 'cleanup failed',
            }))

            await vi.advanceTimersByTimeAsync(3_000)

            expect(staleHandle.dispose).toHaveBeenCalledTimes(2)
            expect(currentHandle.publish).toHaveBeenCalledOnce()
        } finally {
            vi.useRealTimers()
        }
    })

    it('spaces official publishes at least three seconds apart and publishes the newest revision', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(pin).toHaveBeenCalledWith(2)

            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Third edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(commit).toHaveBeenCalledTimes(3)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await vi.advanceTimersByTimeAsync(2000)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(4)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('adopts a validated materialized database without dropping a throttled official publication', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)

            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            expect(coordinator.adoptMaterializedDatabase(
                coordinator.revision,
                coordinator.mutationGeneration,
                structuredClone(database),
            )).toBe(true)

            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            await vi.advanceTimersByTimeAsync(2500)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(3)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('publishes immediately on explicit request despite the publish interval', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await coordinator.publishCurrentOfficialRevision()

            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(3)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
            await vi.advanceTimersByTimeAsync(4000)
            expect(pin).toHaveBeenCalledTimes(2)
        } finally {
            vi.useRealTimers()
        }
    })

    it('drops a deferred publication when an authoritative replacement supersedes it', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const store = {
                commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 9 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(pin).toHaveBeenCalledTimes(1)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')

            expect(coordinator.hasPendingOfficialPublication).toBe(false)
            await vi.advanceTimersByTimeAsync(4000)
            expect(pin).toHaveBeenCalledTimes(1)
        } finally {
            vi.useRealTimers()
        }
    })

    it('retargets a throttled publication to a local replacement revision', async () => {
        vi.useFakeTimers()
        try {
            let database = makeDatabase()
            const publish = vi.fn(async () => undefined)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const store = {
                commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 9 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => {
                    database = replacement
                },
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'First edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            database.username = 'Second edit'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(coordinator.hasPendingOfficialPublication).toBe(true)

            const replacement = makeDatabase()
            replacement.username = 'Local compatibility replacement'
            await coordinator.replacePreparedPersistentDatabase(
                async () => replacement,
                'plugin-profile-change',
                { publishOfficial: true },
            )

            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            await vi.advanceTimersByTimeAsync(2500)
            expect(pin).toHaveBeenCalledTimes(2)
            expect(pin).toHaveBeenLastCalledWith(9)
            expect(coordinator.hasPendingOfficialPublication).toBe(false)
        } finally {
            vi.useRealTimers()
        }
    })

    it('exposes whether an official publication is still pending', async () => {
        const database = makeDatabase()
        let offline = true
        let nowValue = 0
        const publish = vi.fn(async () => {
            if (offline) throw new Error('offline')
        })
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            now: () => nowValue,
        })
        coordinator.initialize(1)
        expect(coordinator.hasPendingOfficialPublication).toBe(false)

        database.username = 'Offline edit'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('offline')).rejects.toThrow('offline')
        expect(coordinator.hasPendingOfficialPublication).toBe(true)

        offline = false
        nowValue = 4000
        await coordinator.flushPendingData('online')
        expect(coordinator.hasPendingOfficialPublication).toBe(false)
    })

    it('reports repeated background publish failures once until a flush succeeds', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            let offline = true
            const publish = vi.fn(async () => {
                if (offline) throw new Error('offline')
            })
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
            const onBackgroundError = vi.fn()
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
                onBackgroundError,
            })
            coordinator.initialize(1)

            database.username = 'Edit one'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(onBackgroundError).toHaveBeenCalledTimes(1)

            database.username = 'Edit two'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            expect(onBackgroundError).toHaveBeenCalledTimes(1)

            offline = false
            database.username = 'Edit three'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            await vi.advanceTimersByTimeAsync(2000)

            offline = true
            database.username = 'Edit four'
            coordinator.markPersistentDataDirty(1)
            await vi.advanceTimersByTimeAsync(500)
            await vi.advanceTimersByTimeAsync(2500)
            expect(onBackgroundError).toHaveBeenCalledTimes(2)
            expect(commit).toHaveBeenCalledTimes(4)
        } finally {
            vi.useRealTimers()
        }
    })

    it('retries pinning the committed revision before creating a newer local revision', async () => {
        const database = makeDatabase()
        const handle = {
            publish: vi.fn(async () => undefined),
            dispose: vi.fn(async () => undefined),
        }
        const pin = vi.fn().mockRejectedValueOnce(new Error('pin failed')).mockResolvedValueOnce(handle)
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Committed locally'
        coordinator.markPersistentDataDirty(1)

        await expect(coordinator.flushPendingData('first')).rejects.toThrow('pin failed')
        await coordinator.flushPendingData('retry')

        expect(pin).toHaveBeenNthCalledWith(1, 2)
        expect(pin).toHaveBeenNthCalledWith(2, 2)
        expect(commit).toHaveBeenCalledTimes(1)
        expect(handle.publish).toHaveBeenCalledTimes(1)
        expect(handle.dispose).toHaveBeenCalledTimes(1)
    })

    it('disposes a failed pre-replacement publication so it can never publish later', async () => {
        const database = makeDatabase()
        const staleHandle = {
            publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
            dispose: vi.fn(async () => undefined),
        }
        const pin = vi.fn(async () => staleHandle)
        const store = {
            commit: vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 })),
            replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
            officialPublisher: { pin },
        })
        coordinator.initialize(1)
        database.username = 'Local revision'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('publish')).rejects.toThrow('remote failed')

        const replacement = makeDatabase()
        replacement.username = 'Authoritative replacement'
        await coordinator.replacePersistentDatabase(replacement, 'replace')
        await coordinator.flushPendingData('clean')

        expect(staleHandle.dispose).toHaveBeenCalledTimes(1)
        expect(staleHandle.publish).toHaveBeenCalledTimes(1)
        expect(pin).toHaveBeenCalledTimes(1)
    })

    it('retries failed stale-publication cleanup after an authoritative replacement', async () => {
        vi.useFakeTimers()
        try {
            const database = makeDatabase()
            const staleHandle = {
                publish: vi.fn().mockRejectedValueOnce(new Error('remote failed')),
                dispose: vi.fn()
                    .mockRejectedValueOnce(new Error('cleanup failed'))
                    .mockResolvedValueOnce(undefined),
            }
            const pin = vi.fn(async () => staleHandle)
            const store = {
                commit: vi.fn(async ({ expectedRevision }) => ({
                    revision: expectedRevision + 1,
                })),
                replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => Object.assign(database, replacement),
                officialPublisher: { pin },
            })
            coordinator.initialize(1)
            database.username = 'Local revision'
            coordinator.markPersistentDataDirty(1)
            await expect(coordinator.flushPendingData('publish')).rejects.toThrow('remote failed')

            await coordinator.replacePersistentDatabase(makeDatabase(), 'authoritative')

            expect(staleHandle.dispose).toHaveBeenCalledOnce()
            await vi.advanceTimersByTimeAsync(3_000)
            expect(staleHandle.dispose).toHaveBeenCalledTimes(2)
            expect(staleHandle.publish).toHaveBeenCalledOnce()
            expect(pin).toHaveBeenCalledOnce()
        } finally {
            vi.useRealTimers()
        }
    })

    it('serializes replacement and leaves post-capture mutations for the next ordinary flush', async () => {
        const database = makeDatabase()
        const firstCommit = deferred<{ revision: number }>()
        const replacementWrite = deferred<{ revision: number }>()
        const commit = vi
            .fn()
            .mockImplementationOnce(() => firstCommit.promise)
            .mockResolvedValueOnce({ revision: 4 })
        const replaceFromDatabase = vi.fn(() => replacementWrite.promise)
        const store = {
            commit,
            replaceFromDatabase,
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, replacement)
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(1)
        database.username = 'Commit before replacement'
        coordinator.markPersistentDataDirty(1)
        const flushing = coordinator.flushPendingData('active')
        const candidate = makeDatabase()
        candidate.username = 'Replacement'
        const replacing = coordinator.replacePersistentDatabase(candidate, 'replace')

        expect(replaceFromDatabase).not.toHaveBeenCalled()
        firstCommit.resolve({ revision: 2 })
        await flushing
        await vi.waitFor(() => expect(replaceFromDatabase).toHaveBeenCalledTimes(1))
        expect(replaceFromDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ username: 'Replacement' }),
            2,
        )
        expect(replaceDatabase).not.toHaveBeenCalled()
        database.characters[0].name = 'Mutation after capture'
        coordinator.markPersistentDataDirty(12)
        replacementWrite.resolve({ revision: 3 })

        await replacing

        expect(commit).toHaveBeenCalledTimes(1)
        expect(replaceDatabase).toHaveBeenCalledTimes(1)
        expect(replaceDatabase.mock.calls[0][0].characters[0].name).toBe(
            'Mutation after capture',
        )
        expect(coordinator.revision).toBe(3)
        expect(coordinator.pendingBytes).toBe(12)

        await coordinator.flushPendingData('post-replacement')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 3,
            replaceCharacter: { name: 'Mutation after capture' },
        })
        expect(coordinator.revision).toBe(4)
    })

    it('three-way rebases only concurrent live deltas onto an authoritative replacement', async () => {
        const database = makeDatabase()
        database.mainPrompt = 'Before prompt'
        database.botPresets = [
            { name: 'Before first', mainPrompt: 'first' },
            { name: 'Before second', mainPrompt: 'second' },
        ] as Database['botPresets']
        ;(database.characters[0] as character).desc = 'Before description'
        database.characters[0].chats = [
            { id: 'chat-a', name: 'First chat', note: '', localLore: [], message: [] },
            { id: 'chat-b', name: 'Second chat', note: '', localLore: [], message: [] },
        ]
        const candidate = structuredClone(database)
        candidate.username = 'Authoritative username'
        candidate.botPresets[1].mainPrompt = 'Authoritative second prompt'
        candidate.characters[0].name = 'Authoritative character name'
        candidate.characters[0].chats = [
            { ...candidate.characters[0].chats[1], name: 'Authoritative second chat' },
            candidate.characters[0].chats[0],
        ]
        const replacementWrite = deferred<{ revision: number }>()
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, structuredClone(replacement))
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(5)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'authoritative', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.mainPrompt = 'Later live prompt'
        database.botPresets[0].name = 'Later live first'
        ;(database.characters[0] as character).desc = 'Later live description'
        database.characters[0].chats[0].note = 'Later live first-chat note'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 6 })
        await replacing

        expect(database).toMatchObject({
            username: 'Authoritative username',
            mainPrompt: 'Later live prompt',
        })
        expect(database.botPresets).toMatchObject([
            { name: 'Later live first', mainPrompt: 'first' },
            { name: 'Before second', mainPrompt: 'Authoritative second prompt' },
        ])
        expect(database.characters[0]).toMatchObject({
            name: 'Authoritative character name',
            desc: 'Later live description',
        })
        expect(database.characters[0].chats).toMatchObject([
            { id: 'chat-b', name: 'Authoritative second chat' },
            { id: 'chat-a', note: 'Later live first-chat note' },
        ])
    })

    it('keeps preset entities intact across an authoritative reorder and concurrent rename', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => Object.assign(database, replacement),
        })
        coordinator.initialize(6)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'preset-reorder', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.botPresets[0].name = 'Preset A2'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 7 })
        await replacing

        expect(database.botPresets).toMatchObject([
            { name: 'Preset B', mainPrompt: 'Candidate B' },
            { name: 'Preset A2', mainPrompt: 'A' },
        ])
    })

    it('rejects an ambiguous preset reorder and rename before replacing the store', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        candidate.botPresets[1].name = 'Preset A2'
        const store = {
            replaceFromDatabase: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'ambiguous-rename', {
            authoritative: true,
        })
        database.botPresets[0].mainPrompt = 'Later A'
        coordinator.markPersistentDataDirty(1)

        await expect(replacing).rejects.toThrow('renamed candidate array entries')
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(database.botPresets).toMatchObject([
            { name: 'Preset A', mainPrompt: 'Later A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ])
    })

    it('rejects an all-renamed preset reorder before live edits can attach to another entity', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Preset A', mainPrompt: 'A' },
            { name: 'Preset B', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets = [
            { ...candidate.botPresets[1], name: 'Preset B2' },
            { ...candidate.botPresets[0], name: 'Preset A2' },
        ]
        const store = {
            replaceFromDatabase: vi.fn(async () => ({ revision: 8 })),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(7)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'all-renamed-reorder', {
            authoritative: true,
        })
        database.botPresets[0].temperature = 1.3
        coordinator.markPersistentDataDirty(1)

        await expect(replacing).rejects.toThrow('renamed candidate array entries')
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(database.botPresets).toMatchObject([
            { name: 'Preset A', mainPrompt: 'A', temperature: 1.3 },
            { name: 'Preset B', mainPrompt: 'B' },
        ])
    })

    it.each(['add', 'delete'] as const)(
        'preserves an authoritative preset %s and a concurrent live edit',
        async (operation) => {
            const database = makeDatabase()
            database.botPresets = [
                { name: 'Preset A', mainPrompt: 'A' },
                { name: 'Preset B', mainPrompt: 'B' },
            ] as Database['botPresets']
            const candidate = structuredClone(database)
            if (operation === 'add') {
                candidate.botPresets.splice(1, 0, {
                    name: 'Imported preset',
                    mainPrompt: 'Imported',
                } as Database['botPresets'][number])
            } else {
                candidate.botPresets.splice(0, 1)
            }
            const replacementWrite = deferred<{ revision: number }>()
            const store = {
                replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: (replacement) => Object.assign(database, replacement),
            })
            coordinator.initialize(7)

            const replacing = coordinator.replacePersistentDatabase(
                candidate,
                `preset-${operation}`,
                { authoritative: true },
            )
            await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
            database.botPresets[1].mainPrompt = 'Later B'
            coordinator.markPersistentDataDirty(1)
            replacementWrite.resolve({ revision: 8 })
            await replacing

            expect(database.botPresets).toMatchObject(operation === 'add'
                ? [
                    { name: 'Preset A', mainPrompt: 'A' },
                    { name: 'Imported preset', mainPrompt: 'Imported' },
                    { name: 'Preset B', mainPrompt: 'Later B' },
                ]
                : [{ name: 'Preset B', mainPrompt: 'Later B' }])
        },
    )

    it('rejects an ambiguous id-less preset reorder instead of publishing a hybrid', async () => {
        const database = makeDatabase()
        database.botPresets = [
            { name: 'Duplicate', mainPrompt: 'A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit: vi.fn(async ({ expectedRevision }) => ({
                revision: expectedRevision + 1,
            })),
        } as unknown as PersistentDataStore
        const replaceDatabase = vi.fn((replacement: Database) => {
            Object.assign(database, structuredClone(replacement))
        })
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(8)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'ambiguous-presets', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.botPresets[0].mainPrompt = 'Later A'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 9 })

        await expect(replacing).rejects.toThrow('Concurrent live changes conflicted')
        expect(replaceDatabase).toHaveBeenCalledOnce()
        expect(database.botPresets).toMatchObject([
            { name: 'Duplicate', mainPrompt: 'Later A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ])
        expect(store.commit).toHaveBeenCalledWith({
            expectedRevision: 9,
            replacePresets: database.botPresets,
        })
        expect(coordinator.revision).toBe(10)

        await coordinator.flushPendingData('after-conflict')
        expect(store.commit).toHaveBeenCalledOnce()
    })

    it('does not baseline a live edit made while replacement compensation is pending', async () => {
        let database = makeDatabase()
        database.botPresets = [
            { name: 'Duplicate', mainPrompt: 'A' },
            { name: 'Duplicate', mainPrompt: 'B' },
        ] as Database['botPresets']
        const candidate = structuredClone(database)
        candidate.botPresets.reverse()
        candidate.botPresets[0].mainPrompt = 'Candidate B'
        const replacementWrite = deferred<{ revision: number }>()
        const compensationWrite = deferred<{ revision: number }>()
        const commit = vi.fn()
            .mockImplementationOnce(() => compensationWrite.promise)
            .mockResolvedValueOnce({ revision: 11 })
        const store = {
            replaceFromDatabase: vi.fn(() => replacementWrite.promise),
            commit,
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: (replacement) => {
                database = replacement
            },
        })
        coordinator.initialize(8)

        const replacing = coordinator.replacePersistentDatabase(candidate, 'pending-compensation', {
            authoritative: true,
        })
        await vi.waitFor(() => expect(store.replaceFromDatabase).toHaveBeenCalledOnce())
        database.botPresets[0].mainPrompt = 'Later A'
        coordinator.markPersistentDataDirty(1)
        replacementWrite.resolve({ revision: 9 })
        await vi.waitFor(() => expect(commit).toHaveBeenCalledOnce())
        database.botPresets[0].mainPrompt = 'Newest A'
        coordinator.markPersistentDataDirty(1)
        compensationWrite.resolve({ revision: 10 })

        await expect(replacing).rejects.toThrow('Concurrent live changes conflicted')
        expect(commit.mock.calls[0][0].replacePresets[0].mainPrompt).toBe('Later A')

        await coordinator.flushPendingData('after-pending-compensation')
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0]).toMatchObject({
            expectedRevision: 10,
            replacePresets: [
                { name: 'Duplicate', mainPrompt: 'Newest A' },
                { name: 'Duplicate', mainPrompt: 'B' },
            ],
        })
    })

    it('re-arms the flush debounce when a replacement leaves dirty state behind', async () => {
        vi.useFakeTimers()
        try {
            let db = makeDatabase()
            const gate = deferred<{ revision: number }>()
            const commit = vi.fn()
                .mockImplementationOnce(() => gate.promise)
                .mockImplementation(async ({ expectedRevision }: { expectedRevision: number }) => ({
                    revision: expectedRevision + 1,
                }))
            const store = {
                commit,
                replaceFromDatabase: vi.fn(async () => ({ revision: 3 })),
            } as unknown as PersistentDataStore
            const coordinator = new SaveCoordinator({
                store,
                captureRoot: () => captureRoot(db),
                captureSelectedCharacter: () => db.characters[0],
                replaceDatabase: (replacement) => {
                    db = replacement
                },
            })
            coordinator.initialize(1)
            db.username = 'Edit one'
            coordinator.markPersistentDataDirty(1)
            const flushing = coordinator.flushPendingData('first')
            await vi.advanceTimersByTimeAsync(0)
            const candidate = makeDatabase()
            candidate.username = 'Replacement'
            const replacing = coordinator.replacePersistentDatabase(candidate, 'replace')
            gate.resolve({ revision: 2 })
            await Promise.resolve()
            db.username = 'Edit two'
            coordinator.markPersistentDataDirty(1)
            await flushing
            await replacing

            expect(commit).toHaveBeenCalledTimes(1)
            expect(db.username).toBe('Edit two')

            await vi.advanceTimersByTimeAsync(500)

            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit.mock.calls[1][0]).toMatchObject({
                expectedRevision: 3,
                root: { username: 'Edit two' },
            })
        } finally {
            vi.useRealTimers()
        }
    })

    it('finishes flushing after the selected character is deselected', async () => {
        const database = makeDatabase()
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        selectedIndex = -1
        database.username = 'Deselected'
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('deselected')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({ root: { username: 'Deselected' } })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect(coordinator.pendingBytes).toBe(0)
    })

    it('rejects hydrated character adoption after the mutation generation changes', () => {
        const database = makeDatabase()
        const coordinator = new SaveCoordinator({
            store: makeStore(),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(4)
        const mutationGeneration = coordinator.mutationGeneration

        coordinator.markPersistentDataDirty(1)

        expect(coordinator.adoptHydratedCharacter(
            4,
            mutationGeneration,
            database.characters[0],
        )).toBe(false)
    })

    it('commits the newly selected character after the selection changes', async () => {
        const database = makeDatabase()
        const second = structuredClone(database.characters[0])
        second.chaId = 'char-b'
        second.name = 'Beta'
        database.characters.push(second)
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        selectedIndex = 1
        database.characters[1].name = 'Beta edited'
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('reselected')

        expect(commit).toHaveBeenCalledTimes(1)
        expect(commit.mock.calls[0][0]).toMatchObject({
            replaceCharacter: { chaId: 'char-b', name: 'Beta edited' },
        })
    })

    it('rescues the previous character edits when the selection switches directly', async () => {
        const database = makeDatabase()
        const second = structuredClone(database.characters[0])
        second.chaId = 'char-b'
        second.name = 'Beta'
        database.characters.push(second)
        let selectedIndex = 0
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[selectedIndex] ?? null,
            captureCharacter: (id) => database.characters.find((item) => item.chaId === id) ?? null,
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(1)
        expect(coordinator.adoptHydratedCharacter(
            1,
            coordinator.mutationGeneration,
            database.characters[0],
        )).toBe(true)

        database.characters[0].name = 'Alpha edited'
        selectedIndex = 1
        coordinator.markPersistentDataDirty(4)

        await coordinator.flushPendingData('switched')

        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[0][0].replaceCharacter).toMatchObject({
            chaId: 'char-a',
            name: 'Alpha edited',
        })
        expect(commit.mock.calls[1][0].replaceCharacter).toMatchObject({
            chaId: 'char-b',
            name: 'Beta',
        })

        await coordinator.flushPendingData('clean')
        expect(commit).toHaveBeenCalledTimes(2)
    })

    it('does not publish or change baselines when replacement fails', async () => {
        const database = makeDatabase()
        const replaceDatabase = vi.fn()
        const commit = vi.fn(async ({ expectedRevision }) => ({ revision: expectedRevision + 1 }))
        const store = {
            commit,
            replaceFromDatabase: vi.fn().mockRejectedValue(new Error('replacement failed')),
        } as unknown as PersistentDataStore
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase,
        })
        coordinator.initialize(7)
        database.username = 'Still dirty'
        coordinator.markPersistentDataDirty(9)

        await expect(
            coordinator.replacePersistentDatabase(makeDatabase(), 'replace'),
        ).rejects.toThrow('replacement failed')

        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(coordinator.revision).toBe(7)
        expect(coordinator.pendingBytes).toBe(9)
        await coordinator.flushPendingData('after-failure')
        expect(commit).toHaveBeenCalledWith(
            expect.objectContaining({ expectedRevision: 7, root: { username: 'Still dirty' } }),
        )
    })
})
