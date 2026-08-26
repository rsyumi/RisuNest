import { describe, expect, it, vi } from 'vitest'
import type { ColdPayloadAuthorityState } from './persistentDataStore'
import type { ColdPayloadStore } from './coldPayloadStore'
import {
    createRuntimeColdPayloadDispatcher,
    selectRuntimeColdPayloadStore,
} from './coldPayloadRuntime'

function memoryStore(label: number): ColdPayloadStore {
    return {
        read: vi.fn(async () => Uint8Array.of(label)),
        write: vi.fn(),
        list: vi.fn(async () => [String(label)]),
        remove: vi.fn(),
    }
}

describe('cold payload runtime authority', () => {
    it('selects by the current generation marker', async () => {
        const legacy = memoryStore(1)
        const v2 = memoryStore(2)
        const store = {
            readColdPayloadAuthority: async () => ({
                revision: 4,
                value: {
                    format: 'v2' as const,
                    migrationId: 'migration',
                    compatibilityHash: 'ab'.repeat(32),
                },
            }),
        }

        expect(await selectRuntimeColdPayloadStore({
            store,
            legacy,
            v2,
            v2Capability: true,
        })).toBe(v2)
    })

    it('rechecks authority on every operation and never retains stale v2', async () => {
        const legacy = memoryStore(1)
        const v2 = memoryStore(2)
        let value: ColdPayloadAuthorityState = {
            format: 'v2',
            migrationId: 'migration',
            compatibilityHash: 'ab'.repeat(32),
        }
        const dispatcher = createRuntimeColdPayloadDispatcher({
            store: {
                readColdPayloadAuthority: async () => ({ revision: 4, value }),
            },
            legacy,
            v2,
            v2Capability: true,
        })

        expect(await dispatcher.read('cold')).toEqual(Uint8Array.of(2))
        value = { format: 'legacy' }
        expect(await dispatcher.read('cold')).toEqual(Uint8Array.of(1))
        await dispatcher.write('cold', Uint8Array.of(7))
        expect(legacy.write).toHaveBeenCalledWith('cold', Uint8Array.of(7))
    })

    it('refuses a v2 marker when native capability disappears', async () => {
        const dispatcher = createRuntimeColdPayloadDispatcher({
            store: {
                readColdPayloadAuthority: async () => ({
                    revision: 4,
                    value: {
                        format: 'v2' as const,
                        migrationId: 'migration',
                        compatibilityHash: 'ab'.repeat(32),
                    },
                }),
            },
            legacy: memoryStore(1),
            v2Capability: false,
        })

        await expect(dispatcher.list()).rejects.toThrow('refusing legacy fallback')
    })
})
