import { describe, expect, it, vi } from 'vitest'
import type { ColdPayloadAuthorityState } from './persistentDataStore'
import { createLegacyNodeColdPayloadStore, createLegacyOpfsColdPayloadStore, createLegacyTauriColdPayloadStore } from './platformColdPayloadStore'
import { migrateLegacyColdPayloads } from './coldPayloadMigration'

function memoryBackend(initial: Record<string, Uint8Array>) {
    const values = new Map(Object.entries(initial).map(([key, value]) => [key, value.slice()]))
    return {
        values,
        read: vi.fn(async (key: string) => values.get(key)?.slice() ?? null),
        write: vi.fn(async (key: string, value: Uint8Array) => void values.set(key, value.slice())),
        keys: vi.fn(async () => [...values.keys()]),
        remove: vi.fn(async (key: string) => void values.delete(key)),
    }
}

function migrationHarness(platform: 'tauri' | 'node' | 'opfs') {
    const physical = platform === 'tauri'
        ? { 'coldstorage/z.json': Uint8Array.of(9), 'coldstorage/a.json': Uint8Array.of(1, 2) }
        : platform === 'node'
            ? { 'coldstorage/z': Uint8Array.of(9), 'coldstorage/a': Uint8Array.of(1, 2) }
            : { 'coldstorage_z.json': Uint8Array.of(9), 'coldstorage_a.json': Uint8Array.of(1, 2) }
    const backend = memoryBackend(physical)
    const legacy = platform === 'tauri'
        ? createLegacyTauriColdPayloadStore(backend)
        : platform === 'node'
            ? createLegacyNodeColdPayloadStore(backend)
            : createLegacyOpfsColdPayloadStore(backend)
    const release = vi.fn(async () => undefined)
    let pinnedAuthority: ColdPayloadAuthorityState = { format: 'legacy' }
    const catalog = {
        readColdPayloadAuthority: vi.fn(async () => ({
            revision: 3,
            value: { format: 'legacy' as const },
        })),
        acquireRevision: vi.fn(async () => ({
            revision: 3,
            readColdPayloadAuthority: async () => ({
                revision: 3,
                value: pinnedAuthority,
            }),
            release,
        })),
        activateColdPayloadMigration: vi.fn(async () => ({ revision: 4 })),
    }
    const prepared: Uint8Array[] = []
    const cas = {
        prepare: vi.fn(async (data: Uint8Array) => {
            prepared.push(data.slice())
            const contentHash = data[0] === 9 ? '99'.repeat(32) : '12'.repeat(32)
            return {
                contentHash,
                byteSize: data.byteLength,
                physicalKey: `assets-v2/objects/${contentHash.slice(0, 2)}/${contentHash.slice(2)}`,
                deduplicated: false,
            }
        }),
        readObject: vi.fn(),
        readObjectRange: vi.fn(),
        statObject: vi.fn(),
    }
    const session = {
        prepare: cas.prepare,
        seal: vi.fn(async () => undefined),
        release: vi.fn(async () => undefined),
    }
    const writeSessions = { begin: vi.fn(async () => session) }
    return {
        backend,
        legacy,
        catalog,
        cas,
        prepared,
        release,
        session,
        writeSessions,
        setPinnedAuthority(value: ColdPayloadAuthorityState) {
            pinnedAuthority = value
        },
    }
}

describe.each(['tauri', 'node', 'opfs'] as const)('legacy %s cold migration', (platform) => {
    it('prepares exact bytes and atomically publishes a sorted complete alias inventory', async () => {
        const fixture = migrationHarness(platform)

        let finishActivation!: () => void
        fixture.catalog.activateColdPayloadMigration.mockImplementationOnce(
            () => new Promise<{ revision: number }>((resolve) => {
                finishActivation = () => resolve({ revision: 4 })
            }),
        )
        const pending = migrateLegacyColdPayloads({
            store: fixture.catalog,
            legacy: fixture.legacy,
            cas: fixture.cas,
            writeSessions: fixture.writeSessions,
            migrationId: 'cold-migration',
        })
        await vi.waitFor(() => {
            expect(fixture.catalog.activateColdPayloadMigration).toHaveBeenCalledOnce()
        })
        expect(fixture.session.seal).toHaveBeenCalledOnce()
        expect(fixture.session.release).not.toHaveBeenCalled()
        finishActivation()
        const result = await pending

        expect(fixture.prepared).toEqual([Uint8Array.of(1, 2), Uint8Array.of(9)])
        expect(fixture.catalog.activateColdPayloadMigration).toHaveBeenCalledWith({
            sourceRevision: 3,
            migrationId: 'cold-migration',
            compatibilityHash: result.compatibilityHash,
            coldAliases: [
                {
                    key: 'a',
                    objectHash: '12'.repeat(32),
                    size: 2,
                    metadata: {},
                },
                {
                    key: 'z',
                    objectHash: '99'.repeat(32),
                    size: 1,
                    metadata: {},
                },
            ],
        })
        expect(result).toMatchObject({
            sourceRevision: 3,
            revision: 4,
            migrationId: 'cold-migration',
            aliases: 2,
        })
        expect(result.compatibilityHash).toMatch(/^[0-9a-f]{64}$/)
        expect(fixture.backend.remove).not.toHaveBeenCalled()
        expect(fixture.release).toHaveBeenCalledOnce()
        expect(fixture.session.release).toHaveBeenCalledWith('committed')
    })
})

describe('cold payload migration failure boundary', () => {
    it('does not activate when a listed legacy payload disappears', async () => {
        const fixture = migrationHarness('tauri')
        fixture.backend.read.mockResolvedValueOnce(null)

        await expect(migrateLegacyColdPayloads({
            store: fixture.catalog,
            legacy: fixture.legacy,
            cas: fixture.cas,
            writeSessions: fixture.writeSessions,
        })).rejects.toThrow('disappeared during migration')
        expect(fixture.catalog.activateColdPayloadMigration).not.toHaveBeenCalled()
        expect(fixture.release).toHaveBeenCalledOnce()
        expect(fixture.session.release).toHaveBeenCalledWith('aborted')
    })

    it('requires the pinned generation to remain legacy before doing any payload work', async () => {
        const fixture = migrationHarness('tauri')
        fixture.setPinnedAuthority({
            format: 'v2',
            migrationId: 'other',
            compatibilityHash: 'ab'.repeat(32),
        })

        await expect(migrateLegacyColdPayloads({
            store: fixture.catalog,
            legacy: fixture.legacy,
            cas: fixture.cas,
            writeSessions: fixture.writeSessions,
        })).rejects.toThrow('pinned legacy authority')
        expect(fixture.backend.keys).not.toHaveBeenCalled()
        expect(fixture.cas.prepare).not.toHaveBeenCalled()
        expect(fixture.catalog.activateColdPayloadMigration).not.toHaveBeenCalled()
        expect(fixture.release).toHaveBeenCalledOnce()
        expect(fixture.writeSessions.begin).not.toHaveBeenCalled()
    })
})
