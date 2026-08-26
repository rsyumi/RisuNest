import { describe, expect, it, vi } from 'vitest'
import type { ColdPayloadStore } from './coldPayloadStore'
import {
    parseColdPayloadAuthorityState,
    selectColdPayloadAuthority,
} from './coldPayloadAuthority'

function completeStore(): ColdPayloadStore {
    return {
        read: vi.fn(),
        write: vi.fn(),
        list: vi.fn(),
        remove: vi.fn(),
    }
}

describe('cold payload authority marker', () => {
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
        expect(parseColdPayloadAuthorityState(input)).toEqual(expected)
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
        expect(() => parseColdPayloadAuthorityState(input)).toThrow(TypeError)
    })
})

describe('cold payload authority selection', () => {
    it('keeps the legacy store authoritative only for an exact legacy marker', () => {
        const legacy = completeStore()

        expect(selectColdPayloadAuthority({ format: 'legacy' }, { legacy })).toBe(legacy)
    })

    it('never exposes a preparing generation', () => {
        expect(() => selectColdPayloadAuthority(
            { format: 'preparing', migrationId: 'migration', sourceRevision: 4 },
            { legacy: completeStore() },
        )).toThrow('preparing generation cannot be selected')
    })

    it('fails closed when a v2 marker has no v2 capability', () => {
        expect(() => selectColdPayloadAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            { legacy: completeStore() },
        )).toThrow('refusing legacy fallback')
    })

    it('selects a distinct complete v2 store only when the capability is available', () => {
        const legacy = completeStore()
        const v2 = completeStore()

        expect(selectColdPayloadAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            { legacy, v2, v2Capability: true },
        )).toBe(v2)
        expect(() => selectColdPayloadAuthority(
            {
                format: 'v2',
                migrationId: 'migration',
                compatibilityHash: 'cd'.repeat(32),
            },
            { legacy, v2: legacy, v2Capability: true },
        )).toThrow('refusing legacy fallback')
    })

    it('revalidates persisted marker fields before selecting a store', () => {
        expect(() => selectColdPayloadAuthority(
            { format: 'legacy', migrationId: 'unexpected' } as never,
            { legacy: completeStore() },
        )).toThrow(TypeError)
    })
})
