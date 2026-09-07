import { describe, expect, it, vi } from 'vitest'

import {
    createCoordinatorOwnedAssetBlobStore,
    createRuntimeAssetRepositoryDispatcher,
    selectRuntimeAssetRepository,
} from './assetRepositoryRuntime'
import { PersistentMutationFencedError } from './saveCoordinator'
import { createGatedBlobStore } from './platformBlobStore'
import { createInRealmStorageLockManager, createStorageMutationGate } from './storageMutationGate'

function facade() {
    return {
        put: vi.fn(),
        putNewInlayImage: vi.fn(),
        read: vi.fn(),
        stat: vi.fn(),
        list: vi.fn(),
        remove: vi.fn(),
        resolveUrl: vi.fn(),
    }
}

describe('selectRuntimeAssetRepository', () => {
    it('selects legacy only for a legacy generation', async () => {
        const legacy = facade()
        const v2 = facade()
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({
                revision: 4,
                value: { format: 'legacy' as const },
            })),
        }
        await expect(selectRuntimeAssetRepository({
            store: store as never,
            legacy,
            v2,
            v2Capability: true,
        })).resolves.toBe(legacy)
    })

    it('selects a complete v2 facade and never falls back when it is unavailable', async () => {
        const legacy = facade()
        const v2 = facade()
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({
                revision: 5,
                value: {
                    format: 'v2' as const,
                    migrationId: 'migration',
                    compatibilityHash: 'ab'.repeat(32),
                },
            })),
        }
        await expect(selectRuntimeAssetRepository({
            store: store as never,
            legacy,
            v2,
            v2Capability: true,
        })).resolves.toBe(v2)
        await expect(selectRuntimeAssetRepository({
            store: store as never,
            legacy,
            v2,
            v2Capability: false,
        })).rejects.toThrow('refusing legacy fallback')
    })

    it('fails closed for a preparing generation', async () => {
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({
                revision: 5,
                value: {
                    format: 'preparing' as const,
                    migrationId: 'migration',
                    sourceRevision: 4,
                },
            })),
        }
        await expect(selectRuntimeAssetRepository({
            store: store as never,
            legacy: facade(),
            v2: facade(),
            v2Capability: true,
        })).rejects.toThrow('cannot be selected')
    })

    it('rechecks generation authority after a replacement instead of retaining stale v2', async () => {
        const legacy = facade()
        const v2 = facade()
        legacy.read.mockResolvedValue(Uint8Array.of(1))
        v2.read.mockResolvedValue(Uint8Array.of(2))
        let value: object = {
            format: 'v2',
            migrationId: 'migration',
            compatibilityHash: 'cd'.repeat(32),
        }
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({ revision: 5, value })),
        }
        const dispatcher = createRuntimeAssetRepositoryDispatcher({
            store: store as never,
            legacy,
            v2,
            v2Capability: true,
        })

        await expect(dispatcher.read('assets/item')).resolves.toEqual(Uint8Array.of(2))
        value = { format: 'legacy' }
        await expect(dispatcher.read('assets/item')).resolves.toEqual(Uint8Array.of(1))
    })

    it('keeps one selected backend authoritative through a fenced write', async () => {
        const locks = createInRealmStorageLockManager()
        const gate = createStorageMutationGate({ locks })
        const legacy = facade()
        const v2 = facade()
        let releaseV2!: () => void
        const v2Blocked = new Promise<void>((resolve) => { releaseV2 = resolve })
        v2.put.mockImplementation(async (key, data, metadata) => {
            await v2Blocked
            return { ...metadata, key, size: data.byteLength }
        })
        legacy.put.mockImplementation(async (key, data, metadata) => ({
            ...metadata,
            key,
            size: data.byteLength,
        }))
        let value: object = {
            format: 'v2',
            migrationId: 'migration',
            compatibilityHash: 'ef'.repeat(32),
        }
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({ revision: 5, value })),
        }
        const dispatcher = createGatedBlobStore(createRuntimeAssetRepositoryDispatcher({
            store: store as never,
            legacy,
            v2,
            v2Capability: true,
        }), gate)
        const metadata = {
            kind: 'asset' as const,
            mime: 'application/octet-stream',
            name: 'item',
            ext: 'bin',
        }

        const write = dispatcher.put('assets/item', Uint8Array.of(1), metadata)
        await vi.waitFor(() => expect(v2.put).toHaveBeenCalledOnce())
        const transition = gate.runTransition(async () => {
            value = { format: 'legacy' }
        })
        await Promise.resolve()
        expect(value).toHaveProperty('format', 'v2')
        releaseV2()
        await write
        await transition

        await dispatcher.put('assets/legacy', Uint8Array.of(2), metadata)
        expect(v2.put).toHaveBeenCalledOnce()
        expect(legacy.put).toHaveBeenCalledOnce()
    })

    it('aborts an unactivated staged v2 write when repository authority changes', async () => {
        const legacy = facade()
        const abort = vi.fn(async () => undefined)
        const activate = vi.fn(async () => ({
            kind: 'asset' as const,
            key: 'assets/item',
            size: 1,
            mime: 'application/octet-stream',
            name: 'item',
            ext: 'bin',
        }))
        const v2 = Object.assign(facade(), {
            prepareOwnedPut: vi.fn(async () => ({ activate, abort })),
            prepareOwnedNewInlayImage: vi.fn(),
        })
        let value: object = {
            format: 'v2',
            migrationId: 'migration',
            compatibilityHash: 'ef'.repeat(32),
        }
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({ revision: 5, value })),
        }
        const dispatcher = createRuntimeAssetRepositoryDispatcher({
            store: store as never,
            legacy,
            v2,
            v2Capability: true,
        })
        const staged = await dispatcher.stagePut(
            'assets/item',
            Uint8Array.of(1),
            {
                kind: 'asset',
                mime: 'application/octet-stream',
                name: 'item',
                ext: 'bin',
            },
        )

        value = { format: 'legacy' }

        await expect(dispatcher.activateStagedWrite(staged)).rejects.toThrow(
            'authority changed',
        )
        expect(abort).toHaveBeenCalledOnce()
        expect(activate).not.toHaveBeenCalled()
        expect(legacy.put).not.toHaveBeenCalled()
    })
})

describe('coordinated staged asset activation', () => {
    const metadata = {
        kind: 'asset' as const,
        mime: 'application/octet-stream',
        name: 'item.bin',
        ext: 'bin',
    }

    function harness() {
        let revision = 1
        let authorityEpoch = 1
        let failure: 'admission' | 'fence' | 'gate' | 'root' | 'authority' | null = null
        let prepareHook: (() => Promise<void>) | null = null
        let resolvePreparationStarted!: () => void
        const preparationStarted = new Promise<void>((resolve) => {
            resolvePreparationStarted = resolve
        })
        let authorityReads = 0
        const handles: Array<{
            abort: ReturnType<typeof vi.fn>
            activate: ReturnType<typeof vi.fn>
        }> = []
        const legacy = facade()
        const v2 = Object.assign(facade(), {
            prepareOwnedPut: vi.fn(async () => {
                resolvePreparationStarted()
                await prepareHook?.()
                const handle = {
                    abort: vi.fn(async () => undefined),
                    activate: vi.fn(async () => {
                        revision++
                        return {
                            ...metadata,
                            key: 'assets/item.bin',
                            size: 1,
                        }
                    }),
                }
                handles.push(handle)
                return handle
            }),
            prepareOwnedNewInlayImage: vi.fn(),
        })
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => {
                authorityReads++
                if (failure === 'authority' && authorityReads === 2) {
                    failure = null
                    throw new Error('authority read failed')
                }
                return {
                    revision,
                    value: {
                        format: 'v2' as const,
                        migrationId: 'same-authority',
                        compatibilityHash: 'ab'.repeat(32),
                    },
                }
            }),
        }
        const dispatcher = createRuntimeAssetRepositoryDispatcher({
            store: store as never,
            legacy,
            v2,
            v2Capability: true,
        })
        const runtime = {
            getStorageAuthorityEpoch: () => authorityEpoch,
            runStorageOnlyMutation(operation: (expectedRevision: number) => Promise<number>) {
                if (failure === 'admission' || failure === 'fence') {
                    const current = failure
                    failure = null
                    throw current === 'fence'
                        ? new PersistentMutationFencedError()
                        : new Error('coordinator admission failed')
                }
                return operation(revision).then((nextRevision) => {
                    revision = nextRevision
                })
            },
        }
        const authority = {
            rawStore: {
                async readRoot() {
                    if (failure === 'root') {
                        failure = null
                        throw new Error('root read failed')
                    }
                    return { revision, value: {} }
                },
            },
            gate: {
                async runKeyedWrite(_key: string, operation: () => Promise<unknown>) {
                    if (failure === 'gate') {
                        failure = null
                        throw new Error('keyed gate failed')
                    }
                    return operation()
                },
            },
        }
        return {
            handles,
            preparationStarted,
            runtime,
            v2Remove: v2.remove,
            setFailure(value: typeof failure) {
                failure = value
            },
            setPrepareHook(value: (() => Promise<void>) | null) {
                prepareHook = value
            },
            bumpAuthorityEpoch() {
                authorityEpoch++
            },
            blob: createCoordinatorOwnedAssetBlobStore(dispatcher, authority as never, runtime),
        }
    }

    it.each([
        ['synchronous coordinator admission', 'admission'],
        ['destructive replacement fence', 'fence'],
        ['keyed gate admission', 'gate'],
        ['raw root precheck', 'root'],
        ['current authority read', 'authority'],
    ] as const)('aborts exactly once after %s failure and settles the key tail', async (_name, point) => {
        const test = harness()
        test.setFailure(point)

        await expect(test.blob.put(
            'assets/item.bin',
            Uint8Array.of(1),
            metadata,
        )).rejects.toThrow()

        expect(test.handles[0].abort).toHaveBeenCalledOnce()
        expect(test.handles[0].activate).not.toHaveBeenCalled()
        await expect(test.blob.put(
            'assets/item.bin',
            Uint8Array.of(2),
            metadata,
        )).resolves.toEqual(expect.objectContaining({ key: 'assets/item.bin' }))
        expect(test.handles[1].activate).toHaveBeenCalledOnce()
    })

    it('aborts without alias resurrection when replacement preserves authority value', async () => {
        const test = harness()
        let releasePrepare!: () => void
        const prepareBlocked = new Promise<void>((resolve) => { releasePrepare = resolve })
        test.setPrepareHook(async () => prepareBlocked)

        const write = test.blob.put(
            'assets/item.bin',
            Uint8Array.of(3),
            metadata,
        )
        await test.preparationStarted
        test.bumpAuthorityEpoch()
        releasePrepare()

        await expect(write).rejects.toThrow('authority changed')

        expect(test.handles[0].abort).toHaveBeenCalledOnce()
        expect(test.handles[0].activate).not.toHaveBeenCalled()
    })

    it('rejects a queued delete after destructive replacement and settles the key tail', async () => {
        const test = harness()
        let releasePrepare!: () => void
        const prepareBlocked = new Promise<void>((resolve) => {
            releasePrepare = resolve
        })
        test.setPrepareHook(async () => prepareBlocked)

        const write = test.blob.put('assets/item.bin', Uint8Array.of(4), metadata)
        await test.preparationStarted
        const remove = test.blob.remove('assets/item.bin')
        const writeRejection = expect(write).rejects.toThrow('authority changed')
        const removeRejection = expect(remove).rejects.toThrow('authority changed')

        test.bumpAuthorityEpoch()
        releasePrepare()

        await writeRejection
        await removeRejection
        expect(test.v2Remove).not.toHaveBeenCalled()

        await expect(test.blob.remove('assets/item.bin')).resolves.toBeUndefined()
        expect(test.v2Remove).toHaveBeenCalledOnce()
    })
})
