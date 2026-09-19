import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import {
    completeAccountUnmigration,
    installAccountBackup,
    installLocalBackup,
    installRisuKeiBackup,
    materializeAccountUnmigrationResources,
} from '../databaseRestore'

const database = {
    account: { token: 'account-token', useSync: true },
    characters: [],
} as Database
const committed = { kind: 'committed', revision: 8, projection: 'applied' } as const
const refreshRequired = { ...committed, projection: 'refresh-required' } as const

describe.each([
    ['account backup', installAccountBackup, 'account-backup'],
    ['Risu-Kei backup', installRisuKeiBackup, 'risu-kei-backup'],
] as const)('%s restore', (_name, install, expectedReason) => {
    it('loads plugins only after persistent replacement succeeds', async () => {
        const events: string[] = []

        await install(database, {
            replaceDatabase: async (_candidate, reason) => {
                events.push(`replace:${reason}`)
                return committed
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
            replaceDatabase: async () => { events.push('replace'); return committed },
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

        const onPostCommitError = vi.fn()
        await expect(installLocalBackup(database, {
            replaceDatabase: async () => committed,
            publishAcceptedRevision: async () => { throw new Error('official offline') },
            relaunch,
            onPostCommitError,
        })).resolves.toEqual(committed)

        expect(onPostCommitError).toHaveBeenCalledWith(new Error('official offline'))
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
                return committed
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

describe.each([
    ['account', installAccountBackup],
    ['Risu-Kei', installRisuKeiBackup],
] as const)('%s committed restore follow-ups', (_name, install) => {
    it('does not load plugins from a stale projection or repeat the replacement', async () => {
        const replaceDatabase = vi.fn(async () => refreshRequired)
        const loadPlugins = vi.fn()
        await expect(install(database, { replaceDatabase, loadPlugins })).resolves.toEqual(refreshRequired)
        expect(replaceDatabase).toHaveBeenCalledOnce()
        expect(loadPlugins).not.toHaveBeenCalled()
    })

    it('reports plugin failure without rejecting a completed local replacement', async () => {
        const failure = new Error('plugin startup unavailable')
        const replaceDatabase = vi.fn(async () => committed)
        const onPostCommitError = vi.fn()
        await expect(install(database, {
            replaceDatabase,
            loadPlugins: async () => { throw failure },
            onPostCommitError,
        })).resolves.toEqual(committed)
        expect(replaceDatabase).toHaveBeenCalledOnce()
        expect(onPostCommitError).toHaveBeenCalledExactlyOnceWith(failure)
    })
})

describe.each([
    ['local', installLocalBackup, 'local-backup'],
] as const)('%s committed restore follow-ups', (_name, install, reason) => {
    it('queues publication without immediately publishing or restarting a stale projection', async () => {
        const replaceDatabase = vi.fn(async () => refreshRequired)
        const publishAcceptedRevision = vi.fn()
        const relaunch = vi.fn()
        await expect(install(database, {
            replaceDatabase, publishAcceptedRevision, relaunch,
        })).resolves.toEqual(refreshRequired)
        expect(replaceDatabase).toHaveBeenCalledExactlyOnceWith(database, reason, { publishOfficial: true })
        expect(publishAcceptedRevision).not.toHaveBeenCalled()
        expect(relaunch).not.toHaveBeenCalled()
    })
})

it('awaits account marker finalization even when committed projection needs refresh', async () => {
    let finished = false
    const finalize = vi.fn(async () => { await Promise.resolve(); finished = true })
    await expect(completeAccountUnmigration(database, {
        prepareResources: async () => undefined,
        replaceDatabase: async () => refreshRequired,
        finalize,
    })).resolves.toEqual(refreshRequired)
    expect(finished).toBe(true)
    expect(finalize).toHaveBeenCalledOnce()
})

describe('account unmigration resource materialization', () => {
    it('returns the account cold payloads and copies verified remote-only assets', async () => {
        const localAssets = new Map([['assets/local.png', new Uint8Array([1])]])
        const assetWrites: string[] = []
        const onProgress = vi.fn()

        const selectedCold = await materializeAccountUnmigrationResources({
            onProgress,
            coldKeys: ['cold-a', 'cold-b'],
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
            readRemoteCold: async (key) => ({ message: [key] }),
        })

        expect(assetWrites).toEqual(['assets/remote.png'])
        expect(localAssets.get('assets/remote.png')).toEqual(new Uint8Array([9, 8]))
        expect([...selectedCold]).toEqual([
            ['cold-a', { message: ['cold-a'] }],
            ['cold-b', { message: ['cold-b'] }],
        ])
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
            readRemoteCold: async () => null,
        })).rejects.toThrow('Failed to verify local asset: assets/remote.png')
    })

    it('fails before transition when the account has no payload for a reference', async () => {
        await expect(materializeAccountUnmigrationResources({
            coldKeys: ['cold-missing'],
            collectAssetKeys: () => [],
            isValidCold: () => true,
            readLocalAsset: async () => null,
            readRemoteAsset: async () => null,
            writeLocalAsset: async () => undefined,
            readRemoteCold: async () => null,
        })).rejects.toThrow('Missing account cold payload: cold-missing')
    })

    it('enumerates assets from the account cold character', async () => {
        const remoteCold = {
            character: { chaId: 'cold-character', additionalAssets: [['remote', 'assets/remote-only.png']] },
        }
        const copiedAssets: string[] = []
        const localAssets = new Map<string, Uint8Array>()

        await materializeAccountUnmigrationResources({
            coldKeys: ['cold-character-key'],
            collectAssetKeys: (selectedCold) => {
                const selected = selectedCold.get('cold-character-key') as typeof remoteCold
                return selected.character.additionalAssets.map((asset) => asset[1])
            },
            isValidCold: () => true,
            readLocalAsset: async (key) => localAssets.get(key) ?? null,
            readRemoteAsset: async (key) => key === 'assets/remote-only.png'
                ? new Uint8Array([7])
                : null,
            writeLocalAsset: async (key, bytes) => {
                copiedAssets.push(key)
                localAssets.set(key, bytes.slice())
            },
            readRemoteCold: async () => remoteCold,
        })

        expect(copiedAssets).toEqual(['assets/remote-only.png'])
    })
})
