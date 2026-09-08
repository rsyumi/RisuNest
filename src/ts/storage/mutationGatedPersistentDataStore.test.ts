import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import { createMutationGatedPersistentDataStore } from './mutationGatedPersistentDataStore'
import type { PersistentDataStore, WorkingSetCommit } from './persistentDataStore'
import type { StorageMutationGate } from './storageMutationGate'

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
        readColdPayloadAuthority: vi.fn(),
        readColdAlias: vi.fn(),
        listColdAliases: vi.fn(),
        commitColdAlias: vi.fn(),
        deleteColdAlias: vi.fn(),
        activateColdPayloadMigration: vi.fn(),
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
            pluginStorage: [{ type: 'set', key: 'plugin', value: true }],
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
        await gated.readPluginStorage('plugin')
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

    it('gates cold alias writes and cold authority transitions', async () => {
        const store = makeStore()
        const gate = {
            runWrite: vi.fn(async <T>(operation: () => Promise<T>) => operation()),
            runKeyedWrite: vi.fn(async <T>(_key: string, operation: () => Promise<T>) => operation()),
            runTransition: vi.fn(async <T>(operation: () => Promise<T>) => operation()),
        } as StorageMutationGate
        const alias = {
            key: 'cold/item',
            objectHash: 'ab'.repeat(32),
            size: 1,
            metadata: {},
        }
        vi.mocked(store.commitColdAlias).mockResolvedValue({ revision: 2 })
        vi.mocked(store.deleteColdAlias).mockResolvedValue({ revision: 3 })
        vi.mocked(store.activateColdPayloadMigration).mockResolvedValue({ revision: 4 })
        const gated = createMutationGatedPersistentDataStore(store, gate)

        await gated.commitColdAlias(alias, 1)
        await gated.deleteColdAlias(alias.key, 2)
        await gated.activateColdPayloadMigration({
            sourceRevision: 3,
            migrationId: 'migration',
            compatibilityHash: 'cd'.repeat(32),
            coldAliases: [alias],
        })

        expect(gate.runWrite).toHaveBeenCalledTimes(2)
        expect(gate.runTransition).toHaveBeenCalledOnce()
    })
})
