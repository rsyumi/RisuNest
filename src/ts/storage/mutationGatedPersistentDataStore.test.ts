import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import { createMutationGatedPersistentDataStore } from './mutationGatedPersistentDataStore'
import type {
    PersistentDataStore,
    PreparedPersistentReplacement,
    WorkingSetCommit,
} from './persistentDataStore'
import type { StorageMutationGate } from './storageMutationGate'

function makeStore() {
    return {
        commit: vi.fn(),
        replaceFromDatabase: vi.fn(),
        prepareReplacement: vi.fn(),
        activatePreparedReplacement: vi.fn(),
        discardPreparedReplacement: vi.fn(),
        listPreparedReplacements: vi.fn(),
        readActiveTuple: vi.fn(),
        readActivePayloadGeneration: vi.fn(),
        materializeDatabase: vi.fn(),
        acquireRevision: vi.fn(),
        open: vi.fn(),
        readRoot: vi.fn(),
        queryCharacters: vi.fn(),
        readCharacter: vi.fn(),
        queryConversations: vi.fn(),
        readConversation: vi.fn(),
        readConversationWindow: vi.fn(),
    } as unknown as PersistentDataStore
}

describe('createMutationGatedPersistentDataStore', () => {
    it('gates ordinary commits and full replacements while preserving exact inputs and results', async () => {
        const store = makeStore()
        const calls: string[] = []
        const gate = {
            runWrite: vi.fn(async <T>(operation: () => Promise<T>) => {
                calls.push('gate')
                return operation()
            }),
            runMigration: vi.fn(),
        } as StorageMutationGate
        const commit = { expectedRevision: 3 } as WorkingSetCommit
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

        expect(calls).toEqual(['gate', 'commit', 'gate', 'replace'])
        expect(gate.runWrite).toHaveBeenCalledTimes(2)
    })

    it('delegates migration-owned methods without reacquiring the shared write gate', async () => {
        const store = makeStore()
        const gate = {
            runWrite: vi.fn(),
            runMigration: vi.fn(),
        } as unknown as StorageMutationGate
        const database = { username: 'Fixture', characters: [] } as unknown as Database
        const prepared = {
            id: 'prepared',
            baseRevision: 1,
            dataGeneration: 'data',
            payloadGeneration: 'payload',
            manifestHash: 'hash',
        } satisfies PreparedPersistentReplacement
        vi.mocked(store.prepareReplacement).mockResolvedValue(prepared)
        vi.mocked(store.activatePreparedReplacement).mockResolvedValue({ revision: 2 })
        vi.mocked(store.discardPreparedReplacement).mockResolvedValue(undefined)
        vi.mocked(store.listPreparedReplacements).mockResolvedValue([prepared])
        vi.mocked(store.readActivePayloadGeneration).mockResolvedValue('payload')
        vi.mocked(store.readActiveTuple).mockResolvedValue({
            revision: 2,
            dataGeneration: 'data',
            payloadGeneration: 'payload',
        })
        vi.mocked(store.materializeDatabase).mockResolvedValue(database)
        const lease = { revision: 2 }
        vi.mocked(store.acquireRevision).mockResolvedValue(lease as never)

        await expect(gatedCall(store, gate, (gated) =>
            gated.prepareReplacement(database, 'hash', 'payload'),
        )).resolves.toBe(prepared)
        await gatedCall(store, gate, (gated) =>
            gated.activatePreparedReplacement({ prepared, manifestHash: 'hash' }),
        )
        await gatedCall(store, gate, (gated) => gated.discardPreparedReplacement(prepared))
        await gatedCall(store, gate, (gated) => gated.listPreparedReplacements())
        await gatedCall(store, gate, (gated) => gated.readActivePayloadGeneration())
        await gatedCall(store, gate, (gated) => gated.readActiveTuple())
        await gatedCall(store, gate, (gated) => gated.materializeDatabase(2))
        await gatedCall(store, gate, (gated) => gated.acquireRevision(2))

        expect(gate.runWrite).not.toHaveBeenCalled()
        expect(gate.runMigration).not.toHaveBeenCalled()
    })

    it('preserves ordinary write error identity', async () => {
        const store = makeStore()
        const failure = new Error('commit failed')
        vi.mocked(store.commit).mockRejectedValue(failure)
        const gate = {
            runWrite: <T>(operation: () => Promise<T>) => operation(),
            runMigration: <T>(operation: () => Promise<T>) => operation(),
        }
        const gated = createMutationGatedPersistentDataStore(store, gate)

        await expect(gated.commit({ expectedRevision: 1 })).rejects.toBe(failure)
    })
})

function gatedCall<T>(
    store: PersistentDataStore,
    gate: StorageMutationGate,
    operation: (gated: PersistentDataStore) => Promise<T>,
): Promise<T> {
    return operation(createMutationGatedPersistentDataStore(store, gate))
}
