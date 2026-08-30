import { describe, expect, it, vi } from 'vitest'

import { migrateLegacyAssetRepository } from './assetRepositoryMigration'
import type { AssetRepositoryMigrationInput } from './persistentDataStore'
import { fixtureDatabase } from './tests/persistentDataFixtures'

describe('migrateLegacyAssetRepository', () => {
    it('prepares ordinary assets and restored Inlays byte-exactly before one activation', async () => {
        const database = structuredClone(fixtureDatabase)
        database.modules = [{
            id: 'module',
            name: 'Module',
            description: '',
            assets: [
                ['present', 'assets/present.bin', 'BIN'],
                ['missing', 'assets/missing.dat', 'dat'],
            ],
        }]
        const ordinary = Uint8Array.of(0, 1, 2, 255)
        const restoredInlay = Uint8Array.of(9, 8, 7)
        const metadata = [
            {
                kind: 'asset' as const,
                key: 'assets/present.bin',
                size: ordinary.byteLength,
                mime: 'application/octet-stream',
                name: 'present',
                ext: 'BIN',
            },
            {
                kind: 'inlay' as const,
                key: 'restored-inlay',
                size: restoredInlay.byteLength,
                mime: 'application/octet-stream',
                name: 'restored',
                ext: 'opaque',
                inlayType: 'image' as const,
            },
        ]
        const legacy = {
            list: vi.fn(async () => metadata),
            read: vi.fn(async (key: string) => key === metadata[0].key
                ? ordinary.slice()
                : restoredInlay.slice()),
        }
        const prepared: Uint8Array[] = []
        const sizes = new Map<string, number>()
        const cas = {
            prepare: vi.fn(async (data: Uint8Array) => {
                const bytes = data.slice()
                prepared.push(bytes)
                const contentHash = await crypto.subtle.digest('SHA-256', bytes)
                    .then((digest) => Buffer.from(digest).toString('hex'))
                sizes.set(contentHash, bytes.byteLength)
                return {
                    contentHash,
                    byteSize: bytes.byteLength,
                    physicalKey: `object/${contentHash}`,
                    deduplicated: false,
                }
            }),
            statObject: vi.fn(async (hash: string) => sizes.get(hash) ?? null),
        }
        let finishActivation!: () => void
        const activateAssetRepositoryMigration = vi.fn(
            (_input: AssetRepositoryMigrationInput) => new Promise<{ revision: number }>(
                (resolve) => {
                    finishActivation = () => resolve({ revision: 8 })
                },
            ),
        )
        const release = vi.fn(async () => undefined)
        const seal = vi.fn(async () => undefined)
        const sessionRelease = vi.fn(async () => undefined)
        const sessionPrepare = vi.fn(
            (data: Uint8Array, _role?: 'direct-object' | 'owner-manifest') => cas.prepare(data),
        )
        const session = {
            prepare: sessionPrepare,
            seal,
            release: sessionRelease,
        }
        const writeSessions = { begin: vi.fn(async () => session) }
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({
                revision: 7,
                value: { format: 'legacy' as const },
            })),
            acquireRevision: vi.fn(async () => ({ release })),
            materializeDatabase: vi.fn(async () => database),
            activateAssetRepositoryMigration,
        }

        const pending = migrateLegacyAssetRepository({
            store: store as never,
            legacy: legacy as never,
            cas: cas as never,
            writeSessions,
            migrationId: 'migration-test',
        })
        await vi.waitFor(() => expect(activateAssetRepositoryMigration).toHaveBeenCalledOnce())
        expect(seal).toHaveBeenCalledOnce()
        expect(sessionRelease).not.toHaveBeenCalled()
        finishActivation()
        await expect(pending).resolves.toMatchObject({ revision: 8, aliases: 3 })

        expect(prepared[0]).toEqual(ordinary)
        expect(prepared[1]).toEqual(restoredInlay)
        const activation = activateAssetRepositoryMigration.mock.calls[0]![0]
        expect(activation.assetAliases).toEqual(expect.arrayContaining([
            expect.objectContaining({
                kind: 'asset',
                key: 'assets/present.bin',
                ext: 'BIN',
                objectHash: expect.any(String),
            }),
            expect.objectContaining({
                kind: 'inlay',
                key: 'restored-inlay',
                objectHash: expect.any(String),
            }),
            expect.objectContaining({
                kind: 'asset',
                key: 'assets/missing.dat',
                objectHash: null,
            }),
        ]))
        expect(activation.assetOwnerHeads).toContainEqual(expect.objectContaining({
            owner: { kind: 'root-module-assets', index: 0 },
            present: true,
            entryCount: 2,
        }))
        expect(release).toHaveBeenCalledOnce()
        expect(writeSessions.begin).toHaveBeenCalledOnce()
        expect(sessionRelease).toHaveBeenCalledWith('committed')
        expect(sessionPrepare.mock.calls.some((call) => call[1] === 'owner-manifest')).toBe(true)
    })

    it('keeps legacy authoritative and releases its lease when preparation fails', async () => {
        const error = new Error('disk full')
        const release = vi.fn(async () => undefined)
        const activate = vi.fn()
        const store = {
            readAssetRepositoryAuthority: vi.fn(async () => ({
                revision: 3,
                value: { format: 'legacy' as const },
            })),
            acquireRevision: vi.fn(async () => ({ release })),
            materializeDatabase: vi.fn(async () => structuredClone(fixtureDatabase)),
            activateAssetRepositoryMigration: activate,
        }
        const legacy = {
            list: vi.fn(async () => [{
                kind: 'asset',
                key: 'assets/fail.bin',
                size: 1,
                mime: 'application/octet-stream',
                name: 'fail',
                ext: 'bin',
            }]),
            read: vi.fn(async () => Uint8Array.of(1)),
        }
        const cas = { prepare: vi.fn(async () => { throw error }) }
        const sessionRelease = vi.fn(async () => undefined)

        await expect(migrateLegacyAssetRepository({
            store: store as never,
            legacy: legacy as never,
            cas: cas as never,
            writeSessions: {
                begin: vi.fn(async () => ({
                    prepare: cas.prepare,
                    seal: vi.fn(),
                    release: sessionRelease,
                })),
            },
        })).rejects.toBe(error)
        expect(activate).not.toHaveBeenCalled()
        expect(release).toHaveBeenCalledOnce()
        expect(sessionRelease).toHaveBeenCalledWith('aborted')
    })
})
