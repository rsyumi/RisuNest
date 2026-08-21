import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import {
    completeAccountUnmigration,
    installAccountBackup,
    installDriveRestore,
    installInternalBackup,
    installLocalBackup,
    installRisuKeiBackup,
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
            writeLocalMirror: async () => { events.push('local-mirror') },
            relaunch: async () => { events.push('relaunch') },
        })

        expect(events).toEqual(['replace', 'local-mirror', 'relaunch'])
    })

    it('does not write or relaunch when replacement fails', async () => {
        const writeLocalMirror = vi.fn()
        const relaunch = vi.fn()

        await expect(installLocalBackup(database, {
            replaceDatabase: async () => { throw new Error('replacement failed') },
            writeLocalMirror,
            relaunch,
        })).rejects.toThrow('replacement failed')

        expect(writeLocalMirror).not.toHaveBeenCalled()
        expect(relaunch).not.toHaveBeenCalled()
    })
})

describe('Drive restore', () => {
    it('relaunches only after replacement succeeds', async () => {
        const events: string[] = []

        await installDriveRestore(database, {
            replaceDatabase: async () => { events.push('replace') },
            relaunch: async () => { events.push('relaunch') },
        })

        expect(events).toEqual(['replace', 'relaunch'])
    })

    it('does not relaunch when replacement fails', async () => {
        const relaunch = vi.fn()

        await expect(installDriveRestore(database, {
            replaceDatabase: async () => { throw new Error('replacement failed') },
            relaunch,
        })).rejects.toThrow('replacement failed')

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

        expect(events).toEqual(['replace', 'mirror', 'finalize'])
        expect(mirrored).toEqual(accepted)
        expect(mirrored).not.toBe(accepted)
        expect(live).toEqual(original)
    })

    it('retains account state and flags when the local mirror fails', async () => {
        const live = structuredClone(database)
        const original = structuredClone(live)
        const finalize = vi.fn()

        await expect(completeAccountUnmigration(live, {
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
            replaceDatabase: async () => { throw new Error('replacement failed') },
            captureAcceptedDatabase,
            writeLegacyMirror,
            finalize,
        })).rejects.toThrow('replacement failed')

        expect(captureAcceptedDatabase).not.toHaveBeenCalled()
        expect(writeLegacyMirror).not.toHaveBeenCalled()
        expect(finalize).not.toHaveBeenCalled()
    })
})
