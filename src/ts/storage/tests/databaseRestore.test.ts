import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import {
    completeAccountUnmigration,
    replaceDatabaseBefore,
} from '../databaseRestore'

const database = {
    account: { token: 'account-token', useSync: true },
    characters: [],
} as Database

describe('replaceDatabaseBefore', () => {
    it('runs the success action only after persistent replacement succeeds', async () => {
        const events: string[] = []

        await replaceDatabaseBefore(database, 'account-backup', {
            replaceDatabase: async () => {
                events.push('replace')
            },
            afterReplacement: async () => {
                events.push('plugins')
            },
        })

        expect(events).toEqual(['replace', 'plugins'])
    })

    it('does not run the success action when persistent replacement fails', async () => {
        const afterReplacement = vi.fn()

        await expect(replaceDatabaseBefore(database, 'drive-restore', {
            replaceDatabase: async () => {
                throw new Error('replacement failed')
            },
            afterReplacement,
        })).rejects.toThrow('replacement failed')

        expect(afterReplacement).not.toHaveBeenCalled()
    })
})

describe('completeAccountUnmigration', () => {
    it('replaces persistence and writes the same detached local mirror before clearing flags', async () => {
        const live = structuredClone(database)
        const original = structuredClone(live)
        const events: string[] = []
        let replaced: Database | undefined
        let mirrored: Database | undefined

        await completeAccountUnmigration(live, {
            replaceDatabase: async (candidate, reason) => {
                events.push('replace')
                expect(reason).toBe('account-unmigration')
                replaced = candidate
            },
            writeLegacyMirror: async (candidate) => {
                events.push('mirror')
                mirrored = candidate
            },
            finalize: () => {
                events.push('finalize')
            },
        })

        expect(events).toEqual(['replace', 'mirror', 'finalize'])
        expect(replaced.account).toBeNull()
        expect(mirrored).toEqual(replaced)
        expect(mirrored).not.toBe(replaced)
        expect(live).toEqual(original)
    })

    it('retains account state and flags when the local mirror fails', async () => {
        const live = structuredClone(database)
        const original = structuredClone(live)
        const finalize = vi.fn()

        await expect(completeAccountUnmigration(live, {
            replaceDatabase: async () => undefined,
            writeLegacyMirror: async () => {
                throw new Error('mirror failed')
            },
            finalize,
        })).rejects.toThrow('mirror failed')

        expect(finalize).not.toHaveBeenCalled()
        expect(live).toEqual(original)
    })
})
