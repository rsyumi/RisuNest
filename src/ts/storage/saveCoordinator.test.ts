import { describe, expect, it, vi } from 'vitest'
import { SaveCoordinator } from './saveCoordinator'
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

describe('SaveCoordinator', () => {
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
