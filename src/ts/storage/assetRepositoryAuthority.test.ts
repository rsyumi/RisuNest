import { describe, expect, it, vi } from 'vitest'
import type { BlobStore } from './blobStore'
import {
    type CompleteAssetRepositoryBlobStore,
    parseAssetRepositoryAuthorityState,
    selectAssetRepositoryAuthority,
} from './assetRepositoryAuthority'

function completeStore(): CompleteAssetRepositoryBlobStore {
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

describe('asset repository authority marker', () => {
    it.each([
        [{ format: 'legacy' }, { format: 'legacy' }],
        [
            { format: 'preparing', migrationId: 'migration_1', sourceRevision: 7 },
            { format: 'preparing', migrationId: 'migration_1', sourceRevision: 7 },
        ],
        [
            {
                format: 'v2',
                migrationId: 'migration-2',
                compatibilityHash: 'ab'.repeat(32),
            },
            {
                format: 'v2',
                migrationId: 'migration-2',
                compatibilityHash: 'ab'.repeat(32),
            },
        ],
    ])('accepts one exact generation-scoped state %#', (input, expected) => {
        expect(parseAssetRepositoryAuthorityState(input)).toEqual(expected)
    })

    it.each([
        null,
        {},
        { format: 'legacy', migrationId: 'unexpected' },
        { format: 'preparing', migrationId: '', sourceRevision: 0 },
        { format: 'preparing', migrationId: 'bad/id', sourceRevision: 0 },
        { format: 'preparing', migrationId: 'valid', sourceRevision: -1 },
        { format: 'preparing', migrationId: 'valid', sourceRevision: 1.5 },
        { format: 'v2', migrationId: 'valid', compatibilityHash: 'AB'.repeat(32) },
        {
            format: 'v2',
            migrationId: 'valid',
            compatibilityHash: 'ab'.repeat(32),
            sourceRevision: 1,
        },
        { format: 'v3' },
    ])('rejects malformed or ambiguous state %#', (input) => {
        expect(() => parseAssetRepositoryAuthorityState(input)).toThrow(TypeError)
    })
})

describe('asset repository authority selection', () => {
    it('keeps legacy authoritative only for an exact legacy marker', () => {
        const legacy = completeStore()

        expect(selectAssetRepositoryAuthority(
            { format: 'legacy' },
            { legacy },
        )).toBe(legacy)
    })

    it('never exposes an incomplete preparing generation', () => {
        expect(() => selectAssetRepositoryAuthority(
            { format: 'preparing', migrationId: 'migration', sourceRevision: 4 },
            { legacy: completeStore() },
        )).toThrow('preparing generation cannot be selected')
    })

    it('refuses every v2 marker until a kind-aware v2 authority exists', () => {
        expect(() => selectAssetRepositoryAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            { legacy: completeStore() },
        )).toThrow('refusing legacy fallback')
    })

    it('selects a complete v2 facade when the v2 capability is available', () => {
        const v2 = completeStore()

        expect(selectAssetRepositoryAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            { legacy: completeStore(), v2, v2Capability: true },
        )).toBe(v2)
    })

    it('rejects an available capability without a v2 facade', () => {
        expect(() => selectAssetRepositoryAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            { legacy: completeStore(), v2Capability: true },
        )).toThrow('requires a complete BlobStore facade')
    })

    it('rejects a complete v2 facade when the capability is unavailable', () => {
        expect(() => selectAssetRepositoryAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            { legacy: completeStore(), v2: completeStore(), v2Capability: false },
        )).toThrow('refusing legacy fallback')
    })

    it.each([
        'put',
        'putNewInlayImage',
        'read',
        'stat',
        'list',
        'remove',
        'resolveUrl',
    ] as const)('rejects a v2 facade missing %s', (operation) => {
        const incomplete = { ...completeStore() }
        delete incomplete[operation]

        expect(() => selectAssetRepositoryAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            {
                legacy: completeStore(),
                v2: incomplete as BlobStore,
                v2Capability: true,
            },
        )).toThrow('requires a complete BlobStore facade')
    })

    it('does not let a caller relabel the legacy store as v2 authority', () => {
        const legacy = completeStore()

        expect(() => selectAssetRepositoryAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'ef'.repeat(32),
            },
            { legacy, v2: legacy, v2Capability: true } as never,
        )).toThrow('refusing legacy fallback')
    })

    it('revalidates persisted marker fields before selecting a store', () => {
        expect(() => selectAssetRepositoryAuthority(
            { format: 'legacy', migrationId: 'unexpected' } as never,
            { legacy: completeStore() },
        )).toThrow(TypeError)
    })
})
