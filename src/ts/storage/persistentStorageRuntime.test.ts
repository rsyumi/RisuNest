import { describe, expect, it, vi } from 'vitest'
import type { BlobStore } from './blobStore'
import type { LocalColdStorageRuntime } from './localColdStorageRuntime'

const mocks = vi.hoisted(() => {
    let rootRevision = 4
    let coordinatorRevision = 4
    const adoptedRevisions: number[] = []
    const lockOrder: string[] = []
    const assetDispatcher: BlobStore = {
        put: vi.fn(async (key, data, metadata) => {
            rootRevision++
            return { ...metadata, key, size: data.byteLength }
        }),
        putNewInlayImage: vi.fn(async (key, data, input) => {
            rootRevision++
            return {
                key,
                kind: 'inlay' as const,
                size: data.byteLength,
                mime: 'image/webp',
                name: input.name,
                ext: 'webp',
                inlayType: 'image' as const,
            }
        }),
        read: vi.fn(async () => null),
        stat: vi.fn(async () => null),
        list: vi.fn(async () => []),
        remove: vi.fn(async () => {
            rootRevision++
        }),
        resolveUrl: vi.fn(async () => null),
    }
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
        lockOrder.push('coordinator')
        coordinatorRevision = await operation(coordinatorRevision)
        adoptedRevisions.push(coordinatorRevision)
    })
    const flushPendingData = vi.fn(async () => {
        if (coordinatorRevision !== rootRevision) {
            throw new Error(`stale revision ${coordinatorRevision}, current ${rootRevision}`)
        }
    })
    const rawStore = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => {
            lockOrder.push('root:read')
            return { revision: rootRevision, value: {} }
        }),
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
        runKeyedWrite: vi.fn(async <T>(key: string, operation: () => Promise<T>) => {
            lockOrder.push(`gate:${key}`)
            return operation()
        }),
        runTransition: vi.fn(async <T>(operation: () => Promise<T>) => operation()),
    }
    return {
        adoptedRevisions,
        assetDispatcher,
        configureLocalColdStorageRuntime: vi.fn((runtime: LocalColdStorageRuntime) => {
            configuredRuntime = runtime
        }),
        dispatcher,
        flushPendingData,
        gate,
        lockOrder,
        rawStore,
        runStorageOnlyMutation,
    }
})

let configuredRuntime: LocalColdStorageRuntime | null = null
let configuredAssetStore: BlobStore | null = null

vi.mock('../platform', () => ({ isNodeServer: false, isTauri: true }))
vi.mock('../process/coldstorage.svelte', () => ({
    configureLocalColdStorageRuntime: mocks.configureLocalColdStorageRuntime,
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => ({
        flushPendingData: mocks.flushPendingData,
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
    configureActiveBlobStore: vi.fn((_gate, store: BlobStore) => {
        configuredAssetStore = store
    }),
    createGatedBlobStore: vi.fn((store: BlobStore, gate) => ({
        ...store,
        put: (key, data, metadata) =>
            gate.runKeyedWrite(key, () => store.put(key, data, metadata)),
        putNewInlayImage: (key, data, input) =>
            gate.runKeyedWrite(key, () => store.putNewInlayImage!(key, data, input)),
        remove: (key) => gate.runKeyedWrite(key, () => store.remove(key)),
    })),
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
    createNativeV2BlobStore: vi.fn(() => mocks.assetDispatcher),
    createRuntimeAssetRepositoryDispatcher: vi.fn((selection) => selection.v2),
    selectRuntimeAssetRepository: vi.fn(async (selection) => selection.v2),
}))
vi.mock('./nativeAssetRepository', () => ({
    createNativeAssetObjectUrlResolver: vi.fn(() => ({})),
    createNativeDurableAssetWriteSessionFactory: vi.fn(() => ({})),
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

describe('persistent storage runtime routing', () => {
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

    it('adopts configured native asset revisions before a following ordinary flush', async () => {
        await initializePersistentStorage()
        const store = configuredAssetStore
        if (!store) throw new Error('Active BlobStore was not configured')
        if (!store.putNewInlayImage) throw new Error('Native Inlay image writer was not configured')
        const mutationCallOffset = mocks.runStorageOnlyMutation.mock.calls.length
        const gateCallOffset = mocks.gate.runKeyedWrite.mock.calls.length
        const revisionOffset = mocks.adoptedRevisions.length
        mocks.lockOrder.length = 0

        await store.put('assets/avatar.png', new Uint8Array([1, 2]), {
            kind: 'asset',
            mime: 'image/png',
            name: 'avatar.png',
            ext: 'png',
        })
        await store.putNewInlayImage('inlay-image', new Uint8Array([3]), {
            name: 'inlay.png',
        })
        await store.remove('assets/avatar.png')

        expect(mocks.runStorageOnlyMutation.mock.calls.length - mutationCallOffset).toBe(3)
        expect(mocks.gate.runKeyedWrite.mock.calls.length - gateCallOffset).toBe(3)
        expect(mocks.adoptedRevisions.slice(revisionOffset)).toEqual([7, 8, 9])
        expect(mocks.lockOrder).toEqual([
            'coordinator',
            'gate:assets/avatar.png',
            'root:read',
            'root:read',
            'coordinator',
            'gate:inlay-image',
            'root:read',
            'root:read',
            'coordinator',
            'gate:assets/avatar.png',
            'root:read',
            'root:read',
        ])
        await expect(mocks.flushPendingData()).resolves.toBeUndefined()
    })
})
