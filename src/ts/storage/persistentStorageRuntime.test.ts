import { describe, expect, it, vi } from 'vitest'
import type { BlobStore } from './blobStore'

const mocks = vi.hoisted(() => {
    let rootRevision = 4
    let coordinatorRevision = 4
    let nextAssetPutFailure: unknown
    let nextAssetPostReadFailure: unknown
    let pendingRootReadFailure: unknown
    let assetPrepareHook: ((key: string, data: Uint8Array) => Promise<void>) | null = null
    const adoptedRevisions: number[] = []
    const lockOrder: string[] = []
    const stagedPayloads: Uint8Array[] = []
    const assetDispatcher = {
        put: vi.fn(async (key, data, metadata) => {
            rootRevision++
            if (nextAssetPutFailure !== undefined) {
                const failure = nextAssetPutFailure
                nextAssetPutFailure = undefined
                pendingRootReadFailure = nextAssetPostReadFailure
                nextAssetPostReadFailure = undefined
                throw failure
            }
            return { ...metadata, key, size: data.byteLength }
        }),
        stagePut: vi.fn(async (key, data, metadata) => {
            lockOrder.push(`prepare:start:${key}:${data[0] ?? 'empty'}`)
            await assetPrepareHook?.(key, data)
            const staged = { key, data: data.slice(), metadata: { ...metadata } }
            stagedPayloads.push(staged.data)
            lockOrder.push(`prepare:end:${key}:${data[0] ?? 'empty'}`)
            return staged
        }),
        stageNewInlayImage: vi.fn(async (key, data, input) => {
            await assetPrepareHook?.(key, data)
            return { key, data: data.slice(), input: { ...input }, inlay: true as const }
        }),
        activateStagedWrite: vi.fn(async (staged: {
            key: string
            data: Uint8Array
            metadata?: Record<string, unknown>
            input?: { name: string }
            inlay?: true
        }) => {
            lockOrder.push(`activate:${staged.key}:${staged.data[0] ?? 'empty'}`)
            rootRevision++
            if (nextAssetPutFailure !== undefined) {
                const failure = nextAssetPutFailure
                nextAssetPutFailure = undefined
                pendingRootReadFailure = nextAssetPostReadFailure
                nextAssetPostReadFailure = undefined
                throw failure
            }
            if (staged.inlay) {
                return {
                    key: staged.key,
                    kind: 'inlay' as const,
                    size: staged.data.byteLength,
                    mime: 'image/webp',
                    name: staged.input!.name,
                    ext: 'webp',
                    inlayType: 'image' as const,
                }
            }
            return { ...staged.metadata, key: staged.key, size: staged.data.byteLength }
        }),
        abortStagedWrite: vi.fn(async () => undefined),
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
            lockOrder.push('remove')
            rootRevision++
        }),
        resolveUrl: vi.fn(async () => null),
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
            if (pendingRootReadFailure !== undefined) {
                const failure = pendingRootReadFailure
                pendingRootReadFailure = undefined
                throw failure
            }
            return { revision: rootRevision, value: {} }
        }),
        readAssetRepositoryAuthority: vi.fn(async () => ({
            revision: rootRevision,
            value: { format: 'v2' },
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
        flushPendingData,
        gate,
        lockOrder,
        rawStore,
        rejectNextAssetPut(error: unknown, postReadError?: unknown) {
            nextAssetPutFailure = error
            nextAssetPostReadFailure = postReadError
        },
        runStorageOnlyMutation,
        setAssetPrepareHook(hook: ((key: string, data: Uint8Array) => Promise<void>) | null) {
            assetPrepareHook = hook
        },
        stagedPayloads,
        alignCoordinatorRevision() {
            coordinatorRevision = rootRevision
        },
    }
})

let configuredAssetStore: BlobStore | null = null

vi.mock('../platform', () => ({ isNodeServer: false, isTauri: true }))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => ({
        flushPendingData: mocks.flushPendingData,
        getStorageAuthorityEpoch: () => 1,
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
vi.mock('./assetRepositoryRuntime', async (importOriginal) => ({
    ...await importOriginal<typeof import('./assetRepositoryRuntime')>(),
    createNativeV2BlobStore: vi.fn(() => mocks.assetDispatcher),
    createRuntimeAssetRepositoryDispatcher: vi.fn((selection) => selection.v2),
    selectRuntimeAssetRepository: vi.fn(async (selection) => selection.v2),
}))
vi.mock('./nativeAssetRepository', () => ({
    createNativeAssetObjectUrlResolver: vi.fn(() => ({})),
    createNativeRemoteAssetReader: vi.fn(() => ({})),
    createNativeDurableAssetWriteSessionFactory: vi.fn(() => ({})),
    createNativeDurableCasJobSessionFactory: vi.fn(() => ({})),
    createNativeImmutablePayloadCas: vi.fn(() => ({})),
    createNativeNewInlayImageEncoder: vi.fn(() => ({})),
}))

import { initializePersistentStorage } from './persistentStorageRuntime'

describe('persistent storage runtime routing', () => {
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
        expect(mocks.adoptedRevisions.slice(revisionOffset)).toEqual([5, 6, 7])
        expect(mocks.lockOrder).toEqual([
            'prepare:start:assets/avatar.png:1',
            'prepare:end:assets/avatar.png:1',
            'coordinator',
            'gate:assets/avatar.png',
            'root:read',
            'activate:assets/avatar.png:1',
            'root:read',
            'coordinator',
            'gate:inlay-image',
            'root:read',
            'activate:inlay-image:3',
            'root:read',
            'coordinator',
            'gate:assets/avatar.png',
            'root:read',
            'remove',
            'root:read',
        ])
        await expect(mocks.flushPendingData()).resolves.toBeUndefined()
    })

    it('adopts a committed native asset revision before rethrowing a later write failure', async () => {
        await initializePersistentStorage()
        const store = configuredAssetStore
        if (!store) throw new Error('Active BlobStore was not configured')
        const failure = new Error('durable session release failed after alias commit')
        const previousRevision = mocks.adoptedRevisions.at(-1) ?? 4
        const revisionOffset = mocks.adoptedRevisions.length
        mocks.rejectNextAssetPut(failure)

        await expect(store.put('assets/committed.png', new Uint8Array([4]), {
            kind: 'asset',
            mime: 'image/png',
            name: 'committed.png',
            ext: 'png',
        })).rejects.toBe(failure)

        expect(mocks.adoptedRevisions.slice(revisionOffset)).toEqual([previousRevision + 1])
        await expect(mocks.flushPendingData()).resolves.toBeUndefined()
    })

    it('preserves both failures when a committed write and its revision read fail', async () => {
        await initializePersistentStorage()
        const store = configuredAssetStore
        if (!store) throw new Error('Active BlobStore was not configured')
        const writeFailure = new Error('durable session release failed after alias commit')
        const readFailure = new Error('root revision read failed')
        mocks.rejectNextAssetPut(writeFailure, readFailure)

        const failure = await store.put('assets/uncertain.png', new Uint8Array([5]), {
            kind: 'asset',
            mime: 'image/png',
            name: 'uncertain.png',
            ext: 'png',
        }).catch((error: unknown) => error)

        expect(failure).toBeInstanceOf(AggregateError)
        expect((failure as AggregateError).errors).toEqual([writeFailure, readFailure])
        await expect(mocks.flushPendingData()).rejects.toThrow('stale revision')
    })

    it('prepares immutable native payloads before entering SaveCoordinator and snapshots caller bytes once', async () => {
        await initializePersistentStorage()
        mocks.alignCoordinatorRevision()
        const store = configuredAssetStore
        if (!store) throw new Error('Active BlobStore was not configured')
        let releasePrepare!: () => void
        const prepareBlocked = new Promise<void>((resolve) => { releasePrepare = resolve })
        mocks.setAssetPrepareHook(async () => prepareBlocked)
        const coordinatorOffset = mocks.runStorageOnlyMutation.mock.calls.length
        const stagedOffset = mocks.stagedPayloads.length
        const input = Uint8Array.of(7, 8, 9)

        const write = store.put('assets/outside.bin', input, {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'outside.bin',
            ext: 'bin',
        })
        input.fill(0)

        await vi.waitFor(() => expect(mocks.assetDispatcher.stagePut).toHaveBeenCalled())
        expect(mocks.runStorageOnlyMutation.mock.calls.length).toBe(coordinatorOffset)
        releasePrepare()
        await write

        expect(mocks.stagedPayloads[stagedOffset]).toEqual(Uint8Array.of(7, 8, 9))
        mocks.setAssetPrepareHook(null)
    })

    it('keeps same-key activation in invocation order when preparation completes out of order', async () => {
        await initializePersistentStorage()
        mocks.alignCoordinatorRevision()
        const store = configuredAssetStore
        if (!store) throw new Error('Active BlobStore was not configured')
        let releaseFirst!: () => void
        const firstBlocked = new Promise<void>((resolve) => { releaseFirst = resolve })
        mocks.setAssetPrepareHook(async (_key, data) => {
            if (data[0] === 1) await firstBlocked
        })
        mocks.lockOrder.length = 0

        const first = store.put('assets/ordered.bin', Uint8Array.of(1), {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'first.bin',
            ext: 'bin',
        })
        const second = store.put('assets/ordered.bin', Uint8Array.of(2), {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'second.bin',
            ext: 'bin',
        })

        await vi.waitFor(() => expect(mocks.lockOrder).toContain(
            'prepare:end:assets/ordered.bin:2',
        ))
        expect(mocks.lockOrder).not.toContain('activate:assets/ordered.bin:2')
        releaseFirst()
        await Promise.all([first, second])

        expect(mocks.lockOrder.filter((event) => event.startsWith('activate:'))).toEqual([
            'activate:assets/ordered.bin:1',
            'activate:assets/ordered.bin:2',
        ])
        mocks.setAssetPrepareHook(null)
    })

    it('retains the same-key invocation tail when a later preparation fails early', async () => {
        await initializePersistentStorage()
        mocks.alignCoordinatorRevision()
        const store = configuredAssetStore
        if (!store) throw new Error('Active BlobStore was not configured')
        let releaseFirst!: () => void
        const firstBlocked = new Promise<void>((resolve) => { releaseFirst = resolve })
        const preparationFailure = new Error('second preparation failed')
        mocks.setAssetPrepareHook(async (_key, data) => {
            if (data[0] === 1) await firstBlocked
            if (data[0] === 2) throw preparationFailure
        })
        mocks.lockOrder.length = 0
        const metadata = {
            kind: 'asset' as const,
            mime: 'application/octet-stream',
            name: 'ordered.bin',
            ext: 'bin',
        }

        const first = store.put('assets/failed-order.bin', Uint8Array.of(1), metadata)
        await expect(store.put(
            'assets/failed-order.bin',
            Uint8Array.of(2),
            metadata,
        )).rejects.toBe(preparationFailure)
        const third = store.put('assets/failed-order.bin', Uint8Array.of(3), metadata)
        await vi.waitFor(() => expect(mocks.lockOrder).toContain(
            'prepare:end:assets/failed-order.bin:3',
        ))
        expect(mocks.lockOrder).not.toContain('activate:assets/failed-order.bin:3')

        releaseFirst()
        await Promise.all([first, third])
        expect(mocks.lockOrder.filter((event) => event.startsWith('activate:'))).toEqual([
            'activate:assets/failed-order.bin:1',
            'activate:assets/failed-order.bin:3',
        ])
        mocks.setAssetPrepareHook(null)
    })

    it('keeps a same-key removal behind an earlier staged write', async () => {
        await initializePersistentStorage()
        mocks.alignCoordinatorRevision()
        const store = configuredAssetStore
        if (!store) throw new Error('Active BlobStore was not configured')
        let releasePrepare!: () => void
        const prepareBlocked = new Promise<void>((resolve) => { releasePrepare = resolve })
        mocks.setAssetPrepareHook(async () => prepareBlocked)
        mocks.lockOrder.length = 0

        const write = store.put('assets/write-remove.bin', Uint8Array.of(4), {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'write-remove.bin',
            ext: 'bin',
        })
        const remove = store.remove('assets/write-remove.bin')
        await vi.waitFor(() => expect(mocks.lockOrder).toContain(
            'prepare:start:assets/write-remove.bin:4',
        ))
        expect(mocks.lockOrder).not.toContain('remove')

        releasePrepare()
        await Promise.all([write, remove])
        expect(mocks.lockOrder.indexOf('activate:assets/write-remove.bin:4'))
            .toBeLessThan(mocks.lockOrder.indexOf('remove'))
        mocks.setAssetPrepareHook(null)
    })
})
