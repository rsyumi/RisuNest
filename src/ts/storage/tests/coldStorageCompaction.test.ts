import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { compactColdStorageDatabase } from '../coldStorageCompaction'

function fixtureDatabase(): Database {
    return {
        coldstorage: true,
        characters: [
            {
                type: 'character',
                chaId: 'character-1',
                name: 'Archived character',
                image: 'assets/character.png',
                lastInteraction: 1,
                chatPage: 0,
                firstMsgIndex: 0,
                chats: [
                    {
                        id: 'chat-1',
                        name: 'Chat',
                        note: '',
                        localLore: [],
                        message: [
                            { role: 'user', data: 'hello', time: 1 },
                            { role: 'char', data: 'one', time: 2 },
                            { role: 'user', data: 'two', time: 3 },
                            { role: 'char', data: 'three', time: 4 },
                        ],
                    },
                ],
            },
        ],
    } as Database
}

describe('compactColdStorageDatabase', () => {
    it('activates verified character stubs through replacement without mutating the live database', async () => {
        const live = fixtureDatabase()
        const original = structuredClone(live)
        const events: string[] = []
        const payloads = new Map<string, unknown>()
        const replaceDatabase = vi.fn(async (candidate: Database, reason: string) => {
            events.push('replace')
            expect(reason).toBe('cold-storage-compaction')
            expect(candidate.characters[0].coldstorage).toBe('cold-character')
        })

        const changed = await compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-character',
            write: async (key, value) => {
                events.push('write')
                payloads.set(key, structuredClone(value))
                return true
            },
            read: async (key) => {
                events.push('verify')
                return structuredClone(payloads.get(key))
            },
            replaceDatabase,
        })

        expect(changed).toBe(true)
        expect(events).toEqual(['write', 'verify', 'replace'])
        expect(replaceDatabase).toHaveBeenCalledTimes(1)
        expect(live).toEqual(original)
    })

    it('leaves the live database intact when replacement fails', async () => {
        const live = fixtureDatabase()
        const original = structuredClone(live)
        const payloads = new Map<string, unknown>()

        await expect(compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-character',
            write: async (key, value) => {
                payloads.set(key, structuredClone(value))
                return true
            },
            read: async (key) => structuredClone(payloads.get(key)),
            replaceDatabase: async () => {
                throw new Error('replacement failed')
            },
        })).rejects.toThrow('replacement failed')

        expect(live).toEqual(original)
    })
})
