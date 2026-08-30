import { describe, expect, it, vi } from 'vitest'

import {
    createRuntimeAssetRepositoryDispatcher,
    selectRuntimeAssetRepository,
} from './assetRepositoryRuntime'
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
