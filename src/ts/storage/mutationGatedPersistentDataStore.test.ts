import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import { createMutationGatedPersistentDataStore } from './mutationGatedPersistentDataStore'
import type { PersistentDataStore, WorkingSetCommit } from './persistentDataStore'
import {
    createInRealmStorageLockManager,
    createStorageMutationGate,
    type StorageMutationGate,
} from './storageMutationGate'

function deferred<T = void>() {
    let resolve!: (value: T | PromiseLike<T>) => void
    const promise = new Promise<T>((complete) => { resolve = complete })
    return { promise, resolve }
}

function makeStore() {
    return {
        commit: vi.fn(),
        replaceFromDatabase: vi.fn(),
        materializeDatabase: vi.fn(),
        acquireRevision: vi.fn(),
        open: vi.fn(),
        readRoot: vi.fn(),
        queryPresets: vi.fn(),
        readPreset: vi.fn(),
        queryCharacters: vi.fn(),
        readCharacter: vi.fn(),
        queryConversations: vi.fn(),
        readConversation: vi.fn(),
        readConversationMetadata: vi.fn(),
        readConversationWindow: vi.fn(),
        queryPluginStorage: vi.fn(),
        readPluginStorage: vi.fn(),
        readAssetAliasesByKeys: vi.fn(),
    } as unknown as PersistentDataStore
}

describe('createMutationGatedPersistentDataStore', () => {
    it('gates ordinary commits and full replacements while preserving exact inputs and results', async () => {
        const store = makeStore()
        const calls: string[] = []
        const gate = {
            runWrite: vi.fn(async <T>(operation: () => Promise<T>) => {
                calls.push('write-gate')
                return operation()
            }),
            runKeyedWrite: vi.fn(async <T>(_key: string, operation: () => Promise<T>) => operation()),
            runTransition: vi.fn(async <T>(operation: () => Promise<T>) => {
                calls.push('transition-gate')
                return operation()
            }),
        } as StorageMutationGate
        const commit = {
            expectedRevision: 3,
            characterDetails: [{ type: 'group', chaId: 'group-a', name: 'Group' }],
            pluginStorage: [{ type: 'set', owner: 'test-plugin', key: 'plugin', value: true }],
        } as WorkingSetCommit
        const database = { username: 'Fixture', characters: [] } as unknown as Database
        const commitResult = { revision: 4 }
        const replacementResult = { revision: 5 }
        vi.mocked(store.commit).mockImplementation(async (input) => {
            calls.push('commit')
            expect(input).toBe(commit)
            return commitResult
        })
        vi.mocked(store.replaceFromDatabase).mockImplementation(async (input, revision) => {
            calls.push('replace')
            expect(input).toBe(database)
            expect(revision).toBe(4)
            return replacementResult
        })
        const gated = createMutationGatedPersistentDataStore(store, gate)

        await expect(gated.commit(commit)).resolves.toBe(commitResult)
        await expect(gated.replaceFromDatabase(database, 4)).resolves.toBe(replacementResult)

        expect(calls).toEqual(['write-gate', 'commit', 'transition-gate', 'replace'])
        expect(gate.runWrite).toHaveBeenCalledOnce()
        expect(gate.runTransition).toHaveBeenCalledOnce()
    })

    it('transition_does_not_reenter_shared_gate', async () => {
        const store = makeStore()
        const gate = createStorageMutationGate({ locks: createInRealmStorageLockManager() })
        const shared = vi.spyOn(gate, 'runWrite')
        const exclusive = vi.spyOn(gate, 'runTransition')
        const started = deferred()
        const finishFirstWrite = deferred()
        const calls: string[] = []
        vi.mocked(store.commit).mockImplementation(async ({ expectedRevision }) => {
            calls.push(`write:${expectedRevision}`)
            if (expectedRevision === 0) {
                started.resolve()
                await finishFirstWrite.promise
            }
            calls.push(`written:${expectedRevision + 1}`)
            return { revision: expectedRevision + 1 }
        })
        vi.mocked(store.replaceFromDatabase).mockImplementation(async (_database, revision) => {
            calls.push('transition')
            // The replacement owns the exclusive permit and uses its internal store.
            const result = await store.commit({ expectedRevision: revision! })
            calls.push('replaced')
            return result
        })
        const gated = createMutationGatedPersistentDataStore(store, gate)
        const first = gated.commit({ expectedRevision: 0 })
        await started.promise
        const replacement = gated.replaceFromDatabase({ characters: [] } as unknown as Database, 1)
        const last = gated.commit({ expectedRevision: 2 })

        expect(calls).toEqual(['write:0'])
        finishFirstWrite.resolve()
        await expect(Promise.all([first, replacement, last])).resolves.toEqual([
            { revision: 1 }, { revision: 2 }, { revision: 3 },
        ])
        expect(calls).toEqual([
            'write:0', 'written:1', 'transition', 'write:1', 'written:2',
            'replaced', 'write:2', 'written:3',
        ])
        expect(shared).toHaveBeenCalledTimes(2)
        expect(exclusive).toHaveBeenCalledOnce()
    })

    it('releases a failed exclusive transition before the next shared writer', async () => {
        const store = makeStore()
        const gate = createStorageMutationGate({ locks: createInRealmStorageLockManager() })
        const started = deferred()
        const finishReplacement = deferred()
        const failure = new Error('replacement failed')
        vi.mocked(store.replaceFromDatabase).mockImplementation(async () => {
            started.resolve()
            await finishReplacement.promise
            throw failure
        })
        vi.mocked(store.commit).mockResolvedValue({ revision: 2 })
        const gated = createMutationGatedPersistentDataStore(store, gate)
        const replacement = gated.replaceFromDatabase({ characters: [] } as unknown as Database, 1)
        const rejected = expect(replacement).rejects.toBe(failure)
        await started.promise
        const write = gated.commit({ expectedRevision: 1 })
        expect(store.commit).not.toHaveBeenCalled()
        finishReplacement.resolve()
        await rejected
        await expect(write).resolves.toEqual({ revision: 2 })
        expect(store.commit).toHaveBeenCalledOnce()
    })

    it('reads and exports without acquiring the write gate', async () => {
        const store = makeStore()
        const gate = { runWrite: vi.fn() } as unknown as StorageMutationGate
        const database = { username: 'Fixture', characters: [] } as unknown as Database
        vi.mocked(store.materializeDatabase).mockResolvedValue(database)
        vi.mocked(store.acquireRevision).mockResolvedValue({ revision: 2 } as never)
        const gated = createMutationGatedPersistentDataStore(store, gate)

        await expect(gated.materializeDatabase(2)).resolves.toBe(database)
        await gated.acquireRevision(2)
        await gated.readRoot()
        await gated.queryPresets()
        await gated.readPreset('0')
        await gated.queryPluginStorage()
        await gated.readPluginStorage('test-plugin', 'plugin')
        await gated.readConversationMetadata('char-a', 'conv-a')
        await gated.readAssetAliasesByKeys('asset', ['assets/batch.bin'])

        expect(gate.runWrite).not.toHaveBeenCalled()
        expect(store.readAssetAliasesByKeys).toHaveBeenCalledWith('asset', ['assets/batch.bin'])
        expect(store.readConversationMetadata).toHaveBeenCalledWith('char-a', 'conv-a')
    })

    it('preserves ordinary write error identity', async () => {
        const store = makeStore()
        const failure = new Error('commit failed')
        vi.mocked(store.commit).mockRejectedValue(failure)
        const gate = {
            runWrite: <T>(operation: () => Promise<T>) => operation(),
            runKeyedWrite: <T>(_key: string, operation: () => Promise<T>) => operation(),
            runTransition: <T>(operation: () => Promise<T>) => operation(),
        }
        const gated = createMutationGatedPersistentDataStore(store, gate)

        await expect(gated.commit({ expectedRevision: 1 })).rejects.toBe(failure)
    })
})
