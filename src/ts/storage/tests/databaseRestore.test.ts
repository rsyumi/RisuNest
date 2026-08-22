import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import {
    completeAccountUnmigration,
    installAccountBackup,
    installDriveRestore,
    installInternalBackup,
    installLocalBackup,
    installRisuKeiBackup,
    materializeAccountUnmigrationResources,
} from '../databaseRestore'

const database = {
    account: { token: 'account-token', useSync: true },
    characters: [],
} as Database

describe.each([
    ['internal backup', installInternalBackup, 'internal-backup'],
    ['account backup', installAccountBackup, 'account-backup'],
    ['Risu-Kei backup', installRisuKeiBackup, 'risu-kei-backup'],
] as const)('%s restore', (_name, install, expectedReason) => {
    it('loads plugins only after persistent replacement succeeds', async () => {
        const events: string[] = []

        await install(database, {
            replaceDatabase: async (_candidate, reason) => {
                events.push(`replace:${reason}`)
            },
            loadPlugins: async () => {
                events.push('plugins')
            },
        })

        expect(events).toEqual([`replace:${expectedReason}`, 'plugins'])
    })

    it('does not load plugins when persistent replacement fails', async () => {
        const loadPlugins = vi.fn()

        await expect(install(database, {
            replaceDatabase: async () => {
                throw new Error('replacement failed')
            },
            loadPlugins,
        })).rejects.toThrow('replacement failed')

        expect(loadPlugins).not.toHaveBeenCalled()
    })
})

describe('local backup restore', () => {
    it('writes the explicit local mirror and relaunches after replacement', async () => {
        const events: string[] = []

        await installLocalBackup(database, {
            replaceDatabase: async () => { events.push('replace') },
            publishAcceptedRevision: async () => { events.push('publish') },
            writeLocalMirror: async () => { events.push('local-mirror') },
            relaunch: async () => { events.push('relaunch') },
        })

        expect(events).toEqual(['replace', 'publish', 'local-mirror', 'relaunch'])
    })

    it('does not write or relaunch when replacement fails', async () => {
        const writeLocalMirror = vi.fn()
        const relaunch = vi.fn()

        await expect(installLocalBackup(database, {
            replaceDatabase: async () => { throw new Error('replacement failed') },
            publishAcceptedRevision: vi.fn(),
            writeLocalMirror,
            relaunch,
        })).rejects.toThrow('replacement failed')

        expect(writeLocalMirror).not.toHaveBeenCalled()
        expect(relaunch).not.toHaveBeenCalled()
    })

    it('retains publication retry ownership and does not mirror or relaunch on publish failure', async () => {
        const writeLocalMirror = vi.fn()
        const relaunch = vi.fn()

        await expect(installLocalBackup(database, {
            replaceDatabase: async () => undefined,
            publishAcceptedRevision: async () => { throw new Error('official offline') },
            writeLocalMirror,
            relaunch,
        })).rejects.toThrow('official offline')

        expect(writeLocalMirror).not.toHaveBeenCalled()
        expect(relaunch).not.toHaveBeenCalled()
    })
})

describe('Drive restore', () => {
    it('relaunches only after replacement succeeds', async () => {
        const events: string[] = []

        await installDriveRestore(database, {
            replaceDatabase: async () => { events.push('replace') },
            publishAcceptedRevision: async () => { events.push('publish') },
            relaunch: async () => { events.push('relaunch') },
        })

        expect(events).toEqual(['replace', 'publish', 'relaunch'])
    })

    it('does not relaunch when replacement fails', async () => {
        const relaunch = vi.fn()

        await expect(installDriveRestore(database, {
            replaceDatabase: async () => { throw new Error('replacement failed') },
            publishAcceptedRevision: vi.fn(),
            relaunch,
        })).rejects.toThrow('replacement failed')

        expect(relaunch).not.toHaveBeenCalled()
    })

    it('does not relaunch when accepted-revision publication fails', async () => {
        const relaunch = vi.fn()

        await expect(installDriveRestore(database, {
            replaceDatabase: async () => undefined,
            publishAcceptedRevision: async () => { throw new Error('official offline') },
            relaunch,
        })).rejects.toThrow('official offline')

        expect(relaunch).not.toHaveBeenCalled()
    })
})

describe('completeAccountUnmigration', () => {
    it('replaces persistence and writes the same detached local mirror before clearing flags', async () => {
        const live = structuredClone(database)
        const original = structuredClone(live)
        const events: string[] = []
        let accepted = structuredClone(database)
        accepted.account = null
        accepted.characters = [{ chaId: 'accepted-after-time-boundary', chats: [] }] as any
        let mirrored: Database | undefined

        await completeAccountUnmigration(live, {
            prepareResources: async () => { events.push('resources') },
            replaceDatabase: async (candidate, reason) => {
                events.push('replace')
                expect(reason).toBe('account-unmigration')
                expect(candidate.account).toBeNull()
            },
            captureAcceptedDatabase: () => accepted,
            writeLegacyMirror: async (candidate) => {
                events.push('mirror')
                mirrored = candidate
            },
            finalize: () => {
                events.push('finalize')
            },
        })

        expect(events).toEqual(['resources', 'replace', 'mirror', 'finalize'])
        expect(mirrored).toEqual(accepted)
        expect(mirrored).not.toBe(accepted)
        expect(live).toEqual(original)
    })

    it('retains account state and flags when the local mirror fails', async () => {
        const live = structuredClone(database)
        const original = structuredClone(live)
        const finalize = vi.fn()

        await expect(completeAccountUnmigration(live, {
            prepareResources: async () => undefined,
            replaceDatabase: async () => undefined,
            captureAcceptedDatabase: () => live,
            writeLegacyMirror: async () => {
                throw new Error('mirror failed')
            },
            finalize,
        })).rejects.toThrow('mirror failed')

        expect(finalize).not.toHaveBeenCalled()
        expect(live).toEqual(original)
    })

    it('does not capture, mirror, clear flags, or reload when replacement fails', async () => {
        const captureAcceptedDatabase = vi.fn()
        const writeLegacyMirror = vi.fn()
        const finalize = vi.fn()

        await expect(completeAccountUnmigration(structuredClone(database), {
            prepareResources: async () => undefined,
            replaceDatabase: async () => { throw new Error('replacement failed') },
            captureAcceptedDatabase,
            writeLegacyMirror,
            finalize,
        })).rejects.toThrow('replacement failed')

        expect(captureAcceptedDatabase).not.toHaveBeenCalled()
        expect(writeLegacyMirror).not.toHaveBeenCalled()
        expect(finalize).not.toHaveBeenCalled()
    })

    it('does not replace, mirror, or clear flags when remote-only materialization fails', async () => {
        const replaceDatabase = vi.fn()
        const writeLegacyMirror = vi.fn()
        const finalize = vi.fn()

        await expect(completeAccountUnmigration(structuredClone(database), {
            prepareResources: async () => { throw new Error('remote asset missing') },
            replaceDatabase,
            captureAcceptedDatabase: () => database,
            writeLegacyMirror,
            finalize,
        })).rejects.toThrow('remote asset missing')

        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(writeLegacyMirror).not.toHaveBeenCalled()
        expect(finalize).not.toHaveBeenCalled()
    })
})

describe('account unmigration resource materialization', () => {
    it('retains local payloads and copies verified remote-only assets and cold data', async () => {
        const localAssets = new Map([['assets/local.png', new Uint8Array([1])]])
        const localCold = new Map<string, unknown>([['cold-local', { message: ['local'] }]])
        const assetWrites: string[] = []
        const coldWrites: string[] = []

        await materializeAccountUnmigrationResources({
            assetKeys: ['assets/local.png', 'assets/remote.png'],
            coldKeys: ['cold-local', 'cold-remote'],
            readLocalAsset: async (key) => localAssets.get(key) ?? null,
            readRemoteAsset: async (key) => key === 'assets/remote.png'
                ? new Uint8Array([9, 8])
                : null,
            writeLocalAsset: async (key, bytes) => {
                assetWrites.push(key)
                localAssets.set(key, bytes.slice())
            },
            readLocalCold: async (key) => localCold.get(key) ?? null,
            readRemoteCold: async (key) => key === 'cold-remote'
                ? { message: ['remote'] }
                : null,
            writeLocalCold: async (key, value) => {
                coldWrites.push(key)
                localCold.set(key, structuredClone(value))
            },
        })

        expect(assetWrites).toEqual(['assets/remote.png'])
        expect(coldWrites).toEqual(['cold-remote'])
        expect(localAssets.get('assets/remote.png')).toEqual(new Uint8Array([9, 8]))
        expect(localCold.get('cold-remote')).toEqual({ message: ['remote'] })
    })

    it('fails before transition when a copied payload cannot be verified', async () => {
        await expect(materializeAccountUnmigrationResources({
            assetKeys: ['assets/remote.png'],
            coldKeys: [],
            readLocalAsset: async () => null,
            readRemoteAsset: async () => new Uint8Array([9]),
            writeLocalAsset: async () => undefined,
            readLocalCold: async () => null,
            readRemoteCold: async () => null,
            writeLocalCold: async () => undefined,
        })).rejects.toThrow('Failed to verify local asset: assets/remote.png')
    })
})
