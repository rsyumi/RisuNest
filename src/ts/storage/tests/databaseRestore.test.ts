import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import {
    completeAccountUnmigration,
    installAccountBackup,
    installDriveRestore,
    installLocalBackup,
    installRisuKeiBackup,
    materializeAccountUnmigrationResources,
} from '../databaseRestore'

const database = {
    account: { token: 'account-token', useSync: true },
    characters: [],
} as Database

describe.each([
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
    it('publishes the accepted revision before relaunching', async () => {
        const events: string[] = []

        await installLocalBackup(database, {
            replaceDatabase: async () => { events.push('replace') },
            publishAcceptedRevision: async () => { events.push('publish') },
            relaunch: async () => { events.push('relaunch') },
        })

        expect(events).toEqual(['replace', 'publish', 'relaunch'])
    })

    it('does not relaunch when replacement fails', async () => {
        const relaunch = vi.fn()

        await expect(installLocalBackup(database, {
            replaceDatabase: async () => { throw new Error('replacement failed') },
            publishAcceptedRevision: vi.fn(),
            relaunch,
        })).rejects.toThrow('replacement failed')

        expect(relaunch).not.toHaveBeenCalled()
    })

    it('retains publication retry ownership and does not relaunch on publish failure', async () => {
        const relaunch = vi.fn()

        await expect(installLocalBackup(database, {
            replaceDatabase: async () => undefined,
            publishAcceptedRevision: async () => { throw new Error('official offline') },
            relaunch,
        })).rejects.toThrow('official offline')

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
    it('replaces persistence with a detached account-free candidate before clearing flags', async () => {
        const live = structuredClone(database)
        const original = structuredClone(live)
        const events: string[] = []

        await completeAccountUnmigration(live, {
            prepareResources: async () => { events.push('resources') },
            replaceDatabase: async (candidate, reason) => {
                events.push('replace')
                expect(reason).toBe('account-unmigration')
                expect(candidate.account).toBeNull()
                expect(candidate).not.toBe(live)
            },
            finalize: () => { events.push('finalize') },
        })

        expect(events).toEqual(['resources', 'replace', 'finalize'])
        expect(live).toEqual(original)
    })

    it('does not clear flags when replacement fails', async () => {
        const finalize = vi.fn()

        await expect(completeAccountUnmigration(structuredClone(database), {
            prepareResources: async () => undefined,
            replaceDatabase: async () => { throw new Error('replacement failed') },
            finalize,
        })).rejects.toThrow('replacement failed')

        expect(finalize).not.toHaveBeenCalled()
    })

    it('does not replace or clear flags when remote-only materialization fails', async () => {
        const replaceDatabase = vi.fn()
        const finalize = vi.fn()

        await expect(completeAccountUnmigration(structuredClone(database), {
            prepareResources: async () => { throw new Error('remote asset missing') },
            replaceDatabase,
            finalize,
        })).rejects.toThrow('remote asset missing')

        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(finalize).not.toHaveBeenCalled()
    })
})

describe('account unmigration resource materialization', () => {
    it('retains local payloads and copies verified remote-only assets and cold data', async () => {
        const localAssets = new Map([['assets/local.png', new Uint8Array([1])]])
        const localCold = new Map<string, unknown>([['cold-local', { message: ['local'] }]])
        const assetWrites: string[] = []
        const coldWrites: string[] = []
        const onProgress = vi.fn()

        await materializeAccountUnmigrationResources({
            onProgress,
            coldKeys: ['cold-local', 'cold-remote'],
            collectAssetKeys: () => ['assets/local.png', 'assets/remote.png'],
            isValidCold: (value) => typeof value === 'object' && value !== null,
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
        expect(onProgress.mock.calls).toEqual([
            ['cold', 0, 2],
            ['cold', 1, 2],
            ['cold', 2, 2],
            ['assets', 0, 2],
            ['assets', 1, 2],
            ['assets', 2, 2],
        ])
    })

    it('fails before transition when a copied payload cannot be verified', async () => {
        await expect(materializeAccountUnmigrationResources({
            coldKeys: [],
            collectAssetKeys: () => ['assets/remote.png'],
            isValidCold: () => true,
            readLocalAsset: async () => null,
            readRemoteAsset: async () => new Uint8Array([9]),
            writeLocalAsset: async () => undefined,
            readLocalCold: async () => null,
            readRemoteCold: async () => null,
            writeLocalCold: async () => undefined,
        })).rejects.toThrow('Failed to verify local asset: assets/remote.png')
    })

    it('enumerates assets from the retained local cold character instead of official cold', async () => {
        const localCold = {
            character: { chaId: 'cold-character', additionalAssets: [['local', 'assets/local-only.png']] },
        }
        const remoteCold = {
            character: { chaId: 'cold-character', additionalAssets: [['remote', 'assets/remote-only.png']] },
        }
        const copiedAssets: string[] = []
        const localAssets = new Map<string, Uint8Array>()
        const readRemoteCold = vi.fn(async () => remoteCold)

        await materializeAccountUnmigrationResources({
            coldKeys: ['cold-character-key'],
            collectAssetKeys: (selectedCold) => {
                const selected = selectedCold.get('cold-character-key') as typeof localCold
                return selected.character.additionalAssets.map((asset) => asset[1])
            },
            isValidCold: () => true,
            readLocalAsset: async (key) => localAssets.get(key) ?? null,
            readRemoteAsset: async (key) => key === 'assets/local-only.png'
                ? new Uint8Array([7])
                : null,
            writeLocalAsset: async (key, bytes) => {
                copiedAssets.push(key)
                localAssets.set(key, bytes.slice())
            },
            readLocalCold: async () => localCold,
            readRemoteCold,
            writeLocalCold: async () => undefined,
        })

        expect(readRemoteCold).not.toHaveBeenCalled()
        expect(copiedAssets).toEqual(['assets/local-only.png'])
    })
})
