import { describe, expect, it, vi } from 'vitest'

import type { Database } from '../database.svelte'
import { capturePersistentRoot, createPersistentDataRuntime } from '../persistentDataRuntime'
import {
    RevisionConflictError,
    type PersistentDataStore,
    type PersistentRevisionLease,
    type WorkingSetCommit,
} from '../persistentDataStore'
import { createDeviceSyncFacade } from './deviceSync'

function makeDatabase(username: string): Database {
    return {
        username,
        botPresetsId: 0,
        botPresets: [],
        pluginCustomStorage: {},
        characters: [],
    } as unknown as Database
}

function makeLease(database: Database, revision: number): PersistentRevisionLease {
    return {
        revision,
        readRoot: vi.fn(async () => ({
            revision,
            value: structuredClone(capturePersistentRoot(database)),
        })),
        queryPresets: vi.fn(async () => ({ revision, items: [] })),
        readPreset: vi.fn(async () => null),
        queryCharacters: vi.fn(async () => ({ revision, items: [] })),
        readCharacter: vi.fn(async () => null),
        queryConversations: vi.fn(async () => ({ revision, items: [] })),
        readConversation: vi.fn(async () => null),
        readConversationWindow: vi.fn(async () => null),
        queryPluginStorage: vi.fn(async () => ({ revision, items: [] })),
        readPluginStorage: vi.fn(async () => null),
        readAssetAlias: vi.fn(async () => null),
        readAssetAliasesByKeys: vi.fn(async () => ({ revision, value: [] })),
        listAssetAliases: vi.fn(async () => ({ revision, items: [] })),
        readAssetRepositoryAuthority: vi.fn(async () => ({
            revision,
            value: { format: 'legacy' as const },
        })),
        readAssetOwnerHead: vi.fn(async () => null),
        readColdPayloadAuthority: vi.fn(async () => ({
            revision,
            value: { format: 'legacy' as const },
        })),
        readColdAlias: vi.fn(async () => null),
        listColdAliases: vi.fn(async () => ({ revision, value: [] })),
        release: vi.fn(async () => undefined),
    }
}

function createRuntimeHarness() {
    let database = makeDatabase('Initial')
    let durable = structuredClone(database)
    let revision = 1
    const acquireRevision = vi.fn(async (requestedRevision: number) => {
        if (requestedRevision !== revision) {
            throw new RevisionConflictError(requestedRevision, revision)
        }
        return makeLease(durable, revision)
    })
    const commit = vi.fn(async (input: WorkingSetCommit) => {
        if (input.expectedRevision !== revision) {
            throw new RevisionConflictError(input.expectedRevision, revision)
        }
        if (input.root) {
            durable = {
                ...durable,
                ...structuredClone(input.root),
            }
        }
        revision += 1
        return { revision }
    })
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({
            revision,
            value: structuredClone(capturePersistentRoot(durable)),
        })),
        acquireRevision,
        commit,
    } as unknown as PersistentDataStore
    const runtime = createPersistentDataRuntime({
        store,
        state: {
            captureRoot: () => capturePersistentRoot(database),
            capturePluginStorage: () => database.pluginCustomStorage,
            capturePresets: () => database.botPresets,
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            getSelectedCharacterId: () => null,
            replaceDatabase: (replacement) => {
                database = replacement
            },
            publishCharacter: vi.fn(),
            publishConversation: vi.fn(),
        },
        prepareDatabase: async (value) => value,
    })
    return {
        runtime,
        store,
        get database() {
            return database
        },
        set username(value: string) {
            database.username = value
        },
        advanceNative(value: string) {
            durable.username = value
            revision += 1
        },
        get durableUsername() {
            return durable.username
        },
        get revision() {
            return revision
        },
    }
}

const operationId = '00000000-0000-4000-8000-0000000000a1'

describe('device sync source runtime integration', () => {
    it('discards a conflicting dirty edit, refreshes the native commit, and saves from the new revision', async () => {
        const harness = createRuntimeHarness()
        await harness.runtime.initializeActiveWorkingSet(harness.database)
        const facade = createDeviceSyncFacade({
            invoke: vi.fn(),
            runtime: harness.runtime,
        })
        harness.username = 'Unsaved stale edit'
        harness.runtime.markPersistentDataDirty(1)
        harness.advanceNative('Remote commit')

        await expect(
            facade.refreshAfterRemoteCommit({
                operationId,
                committedRevision: 2,
            }),
        ).resolves.toEqual({ discardedPendingEdits: true })

        expect(harness.database.username).toBe('Remote commit')
        expect(harness.runtime.revision).toBe(2)
        expect(harness.store.commit).toHaveBeenCalledWith(
            expect.objectContaining({
                expectedRevision: 1,
            }),
        )

        harness.username = 'Saved after refresh'
        harness.runtime.markPersistentDataDirty(1)
        await harness.runtime.flushPendingData('post-remote-edit')

        expect(harness.durableUsername).toBe('Saved after refresh')
        expect(harness.revision).toBe(3)
        expect(harness.store.commit).toHaveBeenLastCalledWith(
            expect.objectContaining({
                expectedRevision: 2,
            }),
        )
    })

    it('projects the current store revision for a delayed older commit notification', async () => {
        const harness = createRuntimeHarness()
        await harness.runtime.initializeActiveWorkingSet(harness.database)
        harness.advanceNative('Remote revision 2')
        harness.advanceNative('Latest remote revision 3')
        const facade = createDeviceSyncFacade({
            invoke: vi.fn(),
            runtime: harness.runtime,
        })

        await facade.refreshAfterRemoteCommit({ operationId, committedRevision: 2 })

        expect(harness.database.username).toBe('Latest remote revision 3')
        expect(harness.runtime.revision).toBe(3)
        expect(harness.store.acquireRevision).toHaveBeenCalledWith(3)
    })
})
