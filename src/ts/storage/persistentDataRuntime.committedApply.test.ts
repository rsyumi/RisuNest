import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import type { PersistentDataStore, PersistentRevisionLease } from './persistentDataStore'
import {
    capturePersistentPluginStorage,
    capturePersistentPresets,
    capturePersistentRoot,
    createPersistentDataRuntime,
} from './persistentDataRuntime'
import { PersistentMutationFencedError, type OfficialRevisionPublisher } from './saveCoordinator'
import { deferred, makeDatabase } from './saveCoordinator.testSupport'
import {
    registerCommittedWorkingSetContinuation,
    retryCommittedWorkingSetRefreshWithContinuation,
} from './committedWorkingSetContinuation'

async function createHarness(officialPublisher?: OfficialRevisionPublisher) {
    let database = makeDatabase()
    let durable = structuredClone(database)
    let revision = 1
    let generating = false
    const leases: PersistentRevisionLease[] = []
    const replaceDatabase = vi.fn((replacement: Database) => { database = replacement })
    const onWorkingSetRefreshRequired = vi.fn()
    const onBackgroundError = vi.fn()
    const publishPresetWorkingSet = vi.fn()
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({ revision, value: capturePersistentRoot(durable) })),
        commit: vi.fn(async () => { throw new Error('Unexpected content commit') }),
        replaceFromDatabase: vi.fn(async (replacement: Database, expected: number) => {
            expect(expected).toBe(revision)
            durable = structuredClone(replacement)
            return { revision: ++revision }
        }),
        acquireRevision: vi.fn(async (expected: number) => {
            expect(expected).toBe(revision)
            const snapshot = structuredClone(durable)
            const lease = {
                revision: expected,
                readRoot: vi.fn(async () => ({ revision: expected, value: capturePersistentRoot(snapshot) })),
                queryPresets: vi.fn(async () => ({ revision: expected, items: [] })),
                readPreset: vi.fn(async () => null),
                queryCharacters: vi.fn(async ({ trash }: { trash: boolean }) => ({
                    revision: expected,
                    items: trash ? [] : snapshot.characters.map((character, configuredIndex) => ({
                        id: character.chaId,
                        name: character.name,
                        type: character.type,
                        configuredIndex,
                        recentAt: 0,
                        trashed: false,
                        conversationCount: 0,
                    })),
                })),
                readCharacterSummary: vi.fn(async (id: string) => {
                    const configuredIndex = snapshot.characters.findIndex((character) => character.chaId === id)
                    if (configuredIndex < 0) return null
                    const character = snapshot.characters[configuredIndex]
                    return {
                        id, name: character.name, type: character.type, configuredIndex,
                        recentAt: 0, trashed: false, conversationCount: 0,
                    }
                }),
                readCharacter: vi.fn(async (id: string) => {
                    const value = snapshot.characters.find((character) => character.chaId === id)
                    if (!value) return null
                    const { chats: _chats, ...detail } = value
                    return { revision: expected, value: detail }
                }),
                queryConversations: vi.fn(async () => ({ revision: expected, items: [] })),
                queryPluginStorage: vi.fn(async () => ({ revision: expected, items: [] })),
                release: vi.fn(async () => undefined),
            } as unknown as PersistentRevisionLease
            leases.push(lease)
            return lease
        }),
        materializeDatabase: vi.fn(async () => { throw new Error('Unexpected complete materialization') }),
    } as unknown as PersistentDataStore
    const runtime = createPersistentDataRuntime({
        store,
        state: {
            captureRoot: () => capturePersistentRoot(database),
            capturePluginStorage: () => capturePersistentPluginStorage(database),
            capturePresets: () => capturePersistentPresets(database),
            captureSelectedCharacter: () => database.characters[0] ?? null,
            captureCharacter: (id) => database.characters.find((value) => value.chaId === id) ?? null,
            getSelectedCharacterId: () => database.characters[0]?.chaId,
            getSelectedConversationId: () => null,
            replaceDatabase,
            publishCharacter: vi.fn(),
            publishConversation: vi.fn(),
            publishPresetWorkingSet,
            isConversationOperationActive: () => generating,
        },
        prepareDatabase: async (value) => structuredClone(value),
        officialPublisher,
        onWorkingSetRefreshRequired,
        onBackgroundError,
        clock: { setTimeout: vi.fn(() => 1), clearTimeout: vi.fn() },
    })
    await runtime.initializeActiveWorkingSet(database)
    return {
        runtime, store, replaceDatabase, publishPresetWorkingSet,
        onWorkingSetRefreshRequired, onBackgroundError, leases,
        get database() { return database },
        get durable() { return durable },
        set generating(value: boolean) { generating = value },
        nativeCommit(replacement: Database) {
            durable = structuredClone(replacement)
            return ++revision
        },
    }
}

function replacement(username = 'Committed replacement'): Database {
    return { ...makeDatabase(), username }
}

describe('committed apply outcomes', () => {
    it('preserves the pending official revision while recovering its committed projection', async () => {
        const publication = { publish: vi.fn(async () => undefined), dispose: vi.fn(async () => undefined) }
        const pin = vi.fn(async () => publication)
        const harness = await createHarness({ pin })
        harness.replaceDatabase.mockImplementationOnce(() => { throw new Error('projection failed') })
        await expect(harness.runtime.replacePersistentDatabase(replacement(), 'pending-publication', {
            publishOfficial: true,
        })).resolves.toEqual({ kind: 'committed', revision: 2, projection: 'refresh-required' })
        expect(harness.runtime.hasPendingOfficialPublication()).toBe(true)

        await expect(harness.runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'applied',
        })
        expect(harness.runtime.hasPendingOfficialPublication()).toBe(true)
        expect(pin).not.toHaveBeenCalled()
        await harness.runtime.publishCurrentOfficialRevision()
        expect(pin).toHaveBeenCalledExactlyOnceWith(2)
        expect(publication.publish).toHaveBeenCalledOnce()
        expect(harness.runtime.hasPendingOfficialPublication()).toBe(false)
        expect(harness.store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(harness.store.commit).not.toHaveBeenCalled()
    })

    it('retires an older pending official revision when recovery adopts newer native content', async () => {
        const pin = vi.fn()
        const harness = await createHarness({ pin })
        harness.replaceDatabase.mockImplementationOnce(() => { throw new Error('projection failed') })
        await harness.runtime.replacePersistentDatabase(replacement(), 'pending-publication', {
            publishOfficial: true,
        })
        harness.nativeCommit(replacement('Newer native authority'))

        await expect(harness.runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({
            kind: 'committed', revision: 3, projection: 'applied',
        })
        expect(harness.runtime.hasPendingOfficialPublication()).toBe(false)
        expect(pin).not.toHaveBeenCalled()
        expect(harness.database.username).toBe('Newer native authority')
        expect(harness.store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(harness.store.commit).not.toHaveBeenCalled()
    })

    it('keeps a scoped preset commit successful when its production projection callback fails', async () => {
        const harness = await createHarness()
        const failure = new Error('preset projection failed')
        harness.publishPresetWorkingSet.mockImplementationOnce(() => { throw failure })
        harness.store.queryPresets = vi.fn(async () => ({ revision: 1, items: [] }))
        vi.mocked(harness.store.commit).mockImplementation(async (batch) => ({
            revision: harness.nativeCommit({
                ...harness.durable,
                ...batch.root,
                botPresets: batch.replacePresets ?? [],
            }),
        }))

        await expect(harness.runtime.mutatePersistentPresets('preset-settings', (state) => {
            state.root.username = 'Scoped durable write'
        })).resolves.toBeUndefined()
        expect(harness.runtime.revision).toBe(2)
        expect(harness.runtime.pendingWorkingSetRefreshRevision).toBe(2)
        expect(harness.onBackgroundError).toHaveBeenCalledWith(failure)
        expect(() => harness.runtime.assertPersistentMutationAllowed()).toThrow(PersistentMutationFencedError)
        await expect(harness.runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'applied',
        })
        expect(harness.database.username).toBe('Scoped durable write')
        expect(harness.store.commit).toHaveBeenCalledOnce()
        expect(harness.store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('returns the confirmed revision after one replacement with no compensating content write', async () => {
        const { runtime, store, database } = await createHarness()
        const input = replacement()

        await expect(runtime.replacePersistentDatabase(input, 'local-replacement')).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'applied',
        })

        expect(runtime.revision).toBe(2)
        expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(store.commit).not.toHaveBeenCalled()
        expect(store.acquireRevision).not.toHaveBeenCalled()
        expect(database.username).toBe('Fixture')
    })

    it('committed_projection_failure_is_not_a_failed_save and refreshes through PDS reads only', async () => {
        const harness = await createHarness()
        const { runtime, store, replaceDatabase } = harness
        const failure = new Error('projection failed')
        replaceDatabase.mockImplementationOnce(() => { throw failure })
        const oldEpoch = runtime.getStorageAuthorityEpoch()
        const replacing = runtime.replacePersistentDatabase(replacement(), 'failed-projection')
        await expect(replacing).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'refresh-required',
        })
        const staleWriter = vi.fn(async () => 3)
        expect(() => runtime.runStorageOnlyMutation(staleWriter))
            .toThrow(PersistentMutationFencedError)
        expect(staleWriter).not.toHaveBeenCalled()
        expect(harness.durable.username).toBe('Committed replacement')
        expect(harness.database.username).toBe('Fixture')
        expect(runtime.revision).toBe(2)
        expect(runtime.pendingWorkingSetRefreshRevision).toBe(2)
        expect(() => runtime.markPersistentDataDirty(1)).toThrow(PersistentMutationFencedError)
        expect(() => runtime.assertPersistentMutationAllowed()).toThrow(PersistentMutationFencedError)
        expect(harness.onBackgroundError).toHaveBeenCalledWith(failure)

        await expect(runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'applied',
        })
        expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
        expect(harness.database.username).toBe('Committed replacement')
        expect(() => runtime.assertPersistentMutationAllowed()).not.toThrow()
        expect(() => runtime.assertPersistentMutationAllowed(oldEpoch)).toThrow(PersistentMutationFencedError)
        await runtime.flushPendingDataLocally('after-read-only-refresh')
        expect(store.commit).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(store.materializeDatabase).not.toHaveBeenCalled()
        expect(harness.leases[0].release).toHaveBeenCalledOnce()
        expect(harness.onWorkingSetRefreshRequired.mock.calls).toEqual([[2], [null]])
    })

    it('runs the source continuation after the real recovery install advances authority', async () => {
        const harness = await createHarness()
        harness.replaceDatabase.mockImplementationOnce(() => {
            throw new Error('projection failed')
        })
        await expect(
            harness.runtime.replacePersistentDatabase(replacement(), 'failed-projection'),
        ).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'refresh-required',
        })
        const authorityBeforeRecovery = harness.runtime.getStorageAuthorityEpoch()
        const continuation = vi.fn(async () => {})
        registerCommittedWorkingSetContinuation(
            2,
            harness.runtime,
            authorityBeforeRecovery,
            continuation,
        )

        await expect(
            retryCommittedWorkingSetRefreshWithContinuation(harness.runtime),
        ).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'applied',
        })

        expect(harness.runtime.getStorageAuthorityEpoch()).toBe(
            authorityBeforeRecovery + 1,
        )
        expect(continuation).toHaveBeenCalledOnce()
        expect(harness.store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(harness.store.commit).not.toHaveBeenCalled()
    })

    it('keeps the guard through another failed refresh without repeating the original replacement', async () => {
        const { runtime, store, replaceDatabase } = await createHarness()
        replaceDatabase.mockImplementationOnce(() => { throw new Error('projection failed') })
        await runtime.replacePersistentDatabase(replacement(), 'failed-projection')
        vi.mocked(store.acquireRevision).mockRejectedValueOnce(new Error('read failed'))

        await expect(runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'refresh-required',
        })
        expect(runtime.pendingWorkingSetRefreshRevision).toBe(2)
        expect(() => runtime.assertPersistentMutationAllowed()).toThrow(PersistentMutationFencedError)
        await expect(runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({
            kind: 'committed', revision: 2, projection: 'applied',
        })
        expect(store.commit).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
    })

    it('recovers the latest confirmed revision when another native commit advanced the store', async () => {
        const harness = await createHarness()
        harness.replaceDatabase.mockImplementationOnce(() => { throw new Error('projection failed') })
        await harness.runtime.replacePersistentDatabase(replacement(), 'failed-projection')
        expect(harness.nativeCommit(replacement('Newer native commit'))).toBe(3)

        await expect(harness.runtime.retryCommittedWorkingSetRefresh()).resolves.toEqual({
            kind: 'committed', revision: 3, projection: 'applied',
        })
        expect(harness.database.username).toBe('Newer native commit')
        expect(harness.store.acquireRevision).toHaveBeenCalledWith(3)
        expect(harness.store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(harness.store.commit).not.toHaveBeenCalled()
    })

    it('does not install a delayed projection after navigation has changed', async () => {
        const harness = await createHarness()
        const { runtime, store } = harness
        const token = await runtime.capturePersistentMutationToken('native-prepare', { publishOfficial: false })
        const fence = await runtime.acquireDestructiveReplacementFence(token)
        const revision = harness.nativeCommit(replacement())
        const started = deferred<void>()
        const finish = deferred<void>()
        const acquire = store.acquireRevision.bind(store)
        vi.mocked(store.acquireRevision).mockImplementationOnce(async (requested) => {
            started.resolve()
            await finish.promise
            return acquire(requested)
        })
        const refreshing = fence.refreshCommittedWorkingSet(revision)
        await started.promise
        runtime.invalidateNavigation()
        finish.resolve()

        await expect(refreshing).resolves.toEqual({
            kind: 'committed', revision, projection: 'refresh-required',
        })
        expect(harness.replaceDatabase).not.toHaveBeenCalled()
        fence.release()
        expect(() => runtime.assertPersistentMutationAllowed()).toThrow(PersistentMutationFencedError)
        await expect(runtime.retryCommittedWorkingSetRefresh()).resolves.toMatchObject({ projection: 'applied' })
        expect(store.commit).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
    })

    it('keeps publication failure separate from the completed local replacement', async () => {
        const failure = new Error('official service unavailable')
        const publication = { publish: vi.fn(async () => { throw failure }), dispose: vi.fn() }
        const pin = vi.fn(async () => publication)
        const { runtime, store } = await createHarness({ pin })

        await expect(runtime.replacePersistentDatabase(replacement(), 'local-before-publication', {
            publishOfficial: true,
        })).resolves.toEqual({ kind: 'committed', revision: 2, projection: 'applied' })
        expect(pin).not.toHaveBeenCalled()
        expect(runtime.hasPendingOfficialPublication()).toBe(true)
        await expect(runtime.publishCurrentOfficialRevision()).rejects.toBe(failure)
        expect(pin).toHaveBeenCalledWith(2)
        expect(runtime.hasPendingOfficialPublication()).toBe(true)
        expect(runtime.revision).toBe(2)
        expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
        await runtime.flushPendingDataLocally('local-after-publication-failure')
        expect(store.replaceFromDatabase).toHaveBeenCalledOnce()
        expect(store.commit).not.toHaveBeenCalled()
    })

    it('refuses destructive application during generation without cancelling the generation or retaining a fence', async () => {
        const harness = await createHarness()
        const token = await harness.runtime.capturePersistentMutationToken('prepare', { publishOfficial: false })
        harness.generating = true

        await expect(harness.runtime.acquireDestructiveReplacementFence(token))
            .rejects.toBeInstanceOf(PersistentMutationFencedError)
        expect(() => harness.runtime.assertPersistentMutationAllowed()).not.toThrow()
        expect(harness.store.commit).not.toHaveBeenCalled()
        expect(harness.store.replaceFromDatabase).not.toHaveBeenCalled()
    })
})
