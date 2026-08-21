import { describe, expect, it, vi } from 'vitest'
import {
    SaveCoordinator as ProductionSaveCoordinator,
    type SaveCoordinatorDependencies,
} from './saveCoordinator'
import type { Database } from './database.svelte'
import type { PersistentDataStore } from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'

function makeDatabase(): Database {
    return {
        username: 'Fixture',
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

function captureRoot(database: Database): Omit<Database, 'characters'> {
    const { characters: _characters, ...root } = database
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

    it('retries exact remote publication before persisting later addition edits', async () => {
        const { database, added } = makeAdditionDatabase()
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
        })
        coordinator.initialize(1)

        await expect(coordinator.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 1,
            install: () => database.characters.push(added),
        }, 'new-character')).rejects.toThrow('offline')
        added.name = 'Edited while offline'
        coordinator.markPersistentDataDirty(1)
        await coordinator.flushPendingData('retry')

        expect(pin).toHaveBeenCalledTimes(2)
        expect(pin.mock.calls.map((call) => call[0])).toEqual([2, 3])
        expect(publish).toHaveBeenCalledTimes(3)
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

    it('does not retry an addition revision conflict and accepts no second pending addition', async () => {
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

        expect(() => coordinator.commitCharacterAddition({
            characterId: 'char-other',
            estimatedBytes: 1,
            install: () => undefined,
        }, 'other-character')).toThrow('already pending')
        gate.reject(conflict)
        await expect(first).rejects.toBe(conflict)

        expect(commit).toHaveBeenCalledOnce()
        expect(coordinator.revision).toBe(1)
        expect(coordinator.pendingBytes).toBe(9)
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

    it('retries the same pinned publication before another local commit', async () => {
        const database = makeDatabase()
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
        })
        coordinator.initialize(1)
        database.username = 'Local one'
        coordinator.markPersistentDataDirty(1)
        await expect(coordinator.flushPendingData('first')).rejects.toThrow('remote')
        database.username = 'Local two'
        coordinator.markPersistentDataDirty(1)

        await coordinator.flushPendingData('retry')

        expect(publish).toHaveBeenCalledTimes(3)
        expect(pin).toHaveBeenCalledTimes(2)
        expect(commit).toHaveBeenCalledTimes(2)
        expect(commit.mock.calls[1][0].expectedRevision).toBe(2)
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
