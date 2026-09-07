import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { RevisionConflictError } from './persistentDataStore'
import {
    capturePersistentPluginStorage,
    capturePersistentPresets,
    capturePersistentRoot,
    createPersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
import { PersistentMutationFencedError } from './saveCoordinator'

vi.mock('./database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No global database in committed working-set refresh tests')
    },
    presetTemplate: {},
}))
vi.mock('../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

function makeDatabase(username: string): Database {
    return {
        username,
        characters: [
            {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 0,
                chats: [{ id: 'chat-a', name: 'First', message: [] }],
            },
        ],
        botPresets: [],
        pluginCustomStorage: {},
    } as unknown as Database
}

function makeState(database: Database): PersistentDataRuntimeStateAdapter & {
    current(): Database
} {
    let current = structuredClone(database)
    return {
        current: () => current,
        captureRoot: () => capturePersistentRoot(current),
        capturePluginStorage: () => capturePersistentPluginStorage(current),
        capturePresets: () => capturePersistentPresets(current),
        captureSelectedCharacter: () => current.characters[0] ?? null,
        captureCharacter: (id) =>
            current.characters.find((character) => character.chaId === id) ?? null,
        getSelectedCharacterId: () => current.characters[0]?.chaId ?? null,
        getSelectedConversationId: () => current.characters[0]?.chats[0]?.id ?? null,
        replaceDatabase: (database) => {
            current = structuredClone(database)
        },
        publishCharacter: (character) => {
            const index = current.characters.findIndex(
                (candidate) => candidate.chaId === character.chaId,
            )
            current.characters[index] = structuredClone(character)
        },
        publishConversation: (characterId, conversation) => {
            const character = current.characters.find(
                (candidate) => candidate.chaId === characterId,
            )!
            character.chats[character.chatPage ?? 0] = structuredClone(conversation)
        },
    }
}

async function makeRuntime(name: string, database = makeDatabase('Initial')) {
    const store = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
    await store.open()
    await store.replaceFromDatabase(database)
    const state = makeState(database)
    const runtime = createPersistentDataRuntime({
        store,
        state,
        prepareDatabase: async (candidate) => structuredClone(candidate),
    })
    await runtime.initializeActiveWorkingSet(database)
    return { runtime, state, store }
}

describe('committed working-set refresh fence', () => {
    it('discards obsolete dirty state after a remote commit and permits later local saves', async () => {
        const { runtime, state, store } = await makeRuntime(
            `committed-refresh-dirty-${crypto.randomUUID()}`,
        )
        state.current().username = 'Obsolete local edit'
        runtime.markPersistentDataDirty(10)
        await store.replaceFromDatabase(makeDatabase('Remote winner'), 1)

        const fence = await runtime.acquireCommittedWorkingSetRefreshFence()
        await fence.refreshCommittedWorkingSet(2)
        fence.release()

        expect(runtime.revision).toBe(2)
        expect(state.current().username).toBe('Remote winner')
        state.current().username = 'Later local edit'
        runtime.markPersistentDataDirty(10)
        await runtime.flushPendingData('later-local-save')
        expect((await store.readRoot()).value.username).toBe('Later local edit')
        expect(runtime.revision).toBe(3)
    })

    it('refreshes the latest committed revision when the notification revision is older', async () => {
        const { runtime, state, store } = await makeRuntime(
            `committed-refresh-latest-${crypto.randomUUID()}`,
        )
        await store.replaceFromDatabase(makeDatabase('Revision two'), 1)
        await store.replaceFromDatabase(makeDatabase('Revision three'), 2)

        const fence = await runtime.acquireCommittedWorkingSetRefreshFence()
        await fence.refreshCommittedWorkingSet(2)
        fence.release()

        expect(runtime.revision).toBe(3)
        expect(state.current().username).toBe('Revision three')
    })

    it('rejects when the authoritative revision has not reached the required minimum', async () => {
        const { runtime, state } = await makeRuntime(
            `committed-refresh-minimum-${crypto.randomUUID()}`,
        )
        const fence = await runtime.acquireCommittedWorkingSetRefreshFence()

        await expect(fence.refreshCommittedWorkingSet(2)).rejects.toEqual(
            new RevisionConflictError(2, 1),
        )
        expect(runtime.revision).toBe(1)
        expect(state.current().username).toBe('Initial')
        fence.release()
    })

    it('rejects a local-only completion flush while the refresh fence is held', async () => {
        const { runtime, state, store } = await makeRuntime(
            `committed-refresh-local-flush-${crypto.randomUUID()}`,
        )
        state.current().username = 'Obsolete local edit'
        runtime.markPersistentDataDirty(10)
        const fence = await runtime.acquireCommittedWorkingSetRefreshFence()

        await expect(runtime.acknowledgeGenerationCompletion()).rejects.toBeInstanceOf(
            PersistentMutationFencedError,
        )
        expect((await store.readRoot()).value.username).toBe('Initial')
        fence.release()
    })

    it('rejects a local-only flush that was waiting when the refresh fence acquired', async () => {
        const { runtime, state, store } = await makeRuntime(
            `committed-refresh-waiting-local-flush-${crypto.randomUUID()}`,
        )
        state.current().username = 'Obsolete local edit'
        runtime.markPersistentDataDirty(10)
        let releaseQueuedOperation!: () => void
        const queuedOperationBlocked = new Promise<void>((resolve) => {
            releaseQueuedOperation = resolve
        })
        let notifyQueuedOperationStarted!: () => void
        const queuedOperationStarted = new Promise<void>((resolve) => {
            notifyQueuedOperationStarted = resolve
        })
        const queuedOperation = runtime.runStorageOnlyMutation(async (revision) => {
            notifyQueuedOperationStarted()
            await queuedOperationBlocked
            return revision
        })
        await queuedOperationStarted
        const localFlush = runtime.acknowledgeGenerationCompletion()
        const fencePromise = runtime.acquireCommittedWorkingSetRefreshFence()
        releaseQueuedOperation()
        await queuedOperation
        const fence = await fencePromise

        await expect(localFlush).rejects.toBeInstanceOf(PersistentMutationFencedError)
        expect((await store.readRoot()).value.username).toBe('Initial')
        fence.release()
    })

    it('rejects mutation during projection and preserves the pending local baseline', async () => {
        const { runtime, state, store } = await makeRuntime(
            `committed-refresh-projection-${crypto.randomUUID()}`,
        )
        state.current().username = 'Pending before refresh'
        runtime.markPersistentDataDirty(10)
        await store.replaceFromDatabase(makeDatabase('Remote winner'), 1)
        const originalAcquireRevision = store.acquireRevision.bind(store)
        let resumeProjection!: () => void
        const projectionPaused = new Promise<void>((resolve) => {
            resumeProjection = resolve
        })
        let notifyProjectionStarted!: () => void
        const projectionStarted = new Promise<void>((resolve) => {
            notifyProjectionStarted = resolve
        })
        store.acquireRevision = async (revision) => {
            notifyProjectionStarted()
            await projectionPaused
            return originalAcquireRevision(revision)
        }

        const fence = await runtime.acquireCommittedWorkingSetRefreshFence()
        const refresh = fence.refreshCommittedWorkingSet(2)
        await projectionStarted
        state.current().username = 'Mutation during projection'
        expect(() => runtime.markPersistentDataDirty(10)).toThrow(PersistentMutationFencedError)
        resumeProjection()
        await expect(refresh).rejects.toBeInstanceOf(PersistentMutationFencedError)
        fence.release()

        await expect(runtime.flushPendingData('preserved-after-failed-refresh')).rejects.toEqual(
            new RevisionConflictError(1, 2),
        )
        expect(state.current().username).toBe('Mutation during projection')
        expect(runtime.revision).toBe(1)
    })

    it('disposes a stale pinned publication without republishing it after refresh', async () => {
        const database = makeDatabase('Initial')
        const store = new IndexedDbPersistentDataStore(
            `committed-refresh-publication-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await store.replaceFromDatabase(database)
        const state = makeState(database)
        const publishError = new Error('publish failed')
        const publish = vi.fn(async () => {
            throw publishError
        })
        const dispose = vi.fn(async () => undefined)
        const runtime = createPersistentDataRuntime({
            store,
            state,
            officialPublisher: {
                pin: vi.fn(async () => ({ publish, dispose })),
            },
            clock: {
                setTimeout: () => Symbol('test-clock'),
                clearTimeout: () => undefined,
            },
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        state.current().username = 'Locally committed'
        runtime.markPersistentDataDirty(10)
        await expect(runtime.flushPendingData('failed-publication')).rejects.toBe(publishError)

        const fence = await runtime.acquireCommittedWorkingSetRefreshFence()
        await fence.refreshCommittedWorkingSet(2)
        fence.release()
        await runtime.flushPendingData('cleanup-stale-publication')

        expect(publish).toHaveBeenCalledTimes(1)
        expect(dispose).toHaveBeenCalledTimes(1)
    })
})
