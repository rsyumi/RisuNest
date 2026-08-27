import { describe, expect, it, vi } from 'vitest'
import type { LocalColdStorageRuntime } from './localColdStorageRuntime'

const mocks = vi.hoisted(() => {
    let rootRevision = 4
    let coordinatorRevision = 4
    const adoptedRevisions: number[] = []
    const dispatcher = {
        read: vi.fn(async () => null),
        write: vi.fn(async () => {
            rootRevision++
        }),
        list: vi.fn(async () => ['cold/existing']),
        remove: vi.fn(async () => {
            rootRevision++
        }),
    }
    const runStorageOnlyMutation = vi.fn(async (
        operation: (expectedRevision: number) => Promise<number>,
    ) => {
        coordinatorRevision = await operation(coordinatorRevision)
        adoptedRevisions.push(coordinatorRevision)
    })
    const rawStore = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({ revision: rootRevision, value: {} })),
        readAssetRepositoryAuthority: vi.fn(async () => ({
            revision: rootRevision,
            value: { format: 'v2' },
        })),
        readColdPayloadAuthority: vi.fn(async () => ({
            revision: rootRevision,
            value: {
                format: 'v2',
                migrationId: 'production-routing',
                compatibilityHash: '11'.repeat(32),
            },
        })),
    }
    const gate = {
        runTransition: vi.fn(async <T>(operation: () => Promise<T>) => operation()),
    }
    return {
        adoptedRevisions,
        configureLocalColdStorageRuntime: vi.fn((runtime: LocalColdStorageRuntime) => {
            configuredRuntime = runtime
        }),
        dispatcher,
        gate,
        rawStore,
        runStorageOnlyMutation,
    }
})

let configuredRuntime: LocalColdStorageRuntime | null = null

vi.mock('../platform', () => ({ isNodeServer: false, isTauri: true }))
vi.mock('../process/coldstorage.svelte', () => ({
    configureLocalColdStorageRuntime: mocks.configureLocalColdStorageRuntime,
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => ({
        runStorageOnlyMutation: mocks.runStorageOnlyMutation,
    }),
}))
vi.mock('./persistentDataStoreFactory', () => ({
    getPersistentStorageAuthority: () => ({
        rawStore: mocks.rawStore,
        store: mocks.rawStore,
        gate: mocks.gate,
    }),
}))
vi.mock('./platformBlobStore', () => ({
    configureActiveBlobStore: vi.fn(),
    getLegacyBlobStore: () => ({}),
    getPlatformBlobKeyValueBackend: vi.fn(async () => ({})),
}))
vi.mock('./assetRepositoryMigration', () => ({ migrateLegacyAssetRepository: vi.fn() }))
vi.mock('./coldPayloadMigration', () => ({ migrateLegacyColdPayloads: vi.fn() }))
vi.mock('./coldPayloadRepository', () => ({
    createCompleteColdPayloadStore: vi.fn(() => ({})),
}))
vi.mock('./coldPayloadRuntime', () => ({
    createRuntimeColdPayloadDispatcher: vi.fn(() => mocks.dispatcher),
    selectRuntimeColdPayloadStore: vi.fn(async () => mocks.dispatcher),
}))
vi.mock('./assetRepositoryRuntime', () => ({
    createNativeV2BlobStore: vi.fn(() => ({})),
    createRuntimeAssetRepositoryDispatcher: vi.fn(() => ({})),
    selectRuntimeAssetRepository: vi.fn(async () => ({})),
}))
vi.mock('./nativeAssetRepository', () => ({
    createNativeAssetObjectUrlResolver: vi.fn(() => ({})),
    createNativeImmutablePayloadCas: vi.fn(() => ({})),
    createNativeNewInlayImageEncoder: vi.fn(() => ({})),
}))
vi.mock('./platformColdPayloadStore', () => ({
    createGatedColdPayloadStore: vi.fn((store) => store),
    createLegacyBrowserOpfsColdPayloadStore: vi.fn(() => ({})),
    createLegacyNodeColdPayloadStore: vi.fn(() => ({})),
    createLegacyTauriColdPayloadStore: vi.fn(() => ({})),
}))

import { initializePersistentStorage } from './persistentStorageRuntime'

describe('persistent storage cold runtime routing', () => {
    it('routes configured Tauri writes and removals through the coordinator-owned queue', async () => {
        await initializePersistentStorage()
        const runtime = configuredRuntime
        if (!runtime) throw new Error('Local cold storage runtime was not configured')

        await expect(runtime.list()).resolves.toEqual(['cold/existing'])
        expect(mocks.runStorageOnlyMutation).not.toHaveBeenCalled()

        await expect(runtime.write('cold/new', { message: 'payload' })).resolves.toBe(true)
        await expect(runtime.remove(['cold/new'])).resolves.toBeUndefined()

        expect(mocks.dispatcher.write).toHaveBeenCalledOnce()
        expect(mocks.dispatcher.remove).toHaveBeenCalledWith('cold/new')
        expect(mocks.runStorageOnlyMutation).toHaveBeenCalledTimes(2)
        expect(mocks.gate.runTransition).toHaveBeenCalledTimes(2)
        expect(mocks.adoptedRevisions).toEqual([5, 6])
    })
})
