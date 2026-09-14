import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { compactColdStorageDatabase } from '../coldStorageCompaction'
import { createCatalogCharacterStub } from '../workingSetCatalog'

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
    it.each(['character', 'chat'] as const)('rejects a changed %s payload after a successful write', async (target) => {
        const live = fixtureDatabase()
        const now = 20 * 24 * 60 * 60 * 1000
        if (target === 'chat') live.characters[0].lastInteraction = now
        const original = structuredClone(live)
        const replaceDatabase = vi.fn()
        const failures: unknown[] = []
        const changed = await compactColdStorageDatabase(live, {
            now,
            createId: () => 'corrupt-payload',
            write: async () => true,
            read: async () => target === 'character' ? { character: {} } : { message: [] },
            remove: vi.fn(async () => undefined),
            replaceDatabase,
            onFailure: (failure) => failures.push(failure),
        })
        expect(changed).toBe(false)
        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(failures).toContainEqual(expect.objectContaining({ kind: 'verify', target }))
        expect(live).toEqual(original)
    })

    it('keeps edits made while cold payloads are written', async () => {
        const live = fixtureDatabase()
        const payloads = new Map<string, unknown>()
        const replaceDatabase = vi.fn()
        const remove = vi.fn(async () => undefined)
        await expect(compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-character',
            write: async (key, payload) => {
                payloads.set(key, structuredClone(payload))
                live.characters[0].chats[0].message[0].data = 'edited during compaction'
                return true
            },
            read: async (key) => payloads.get(key),
            remove,
            replaceDatabase,
        })).rejects.toThrow('changed during cold storage compaction')
        expect(replaceDatabase).not.toHaveBeenCalled()
        expect(remove).toHaveBeenCalledWith(['cold-character'])
        expect(live.characters[0].chats[0].message[0].data).toBe('edited during compaction')
    })

    it('accepts payloads normalized by the JSON cold storage codec', async () => {
        const live = fixtureDatabase()
        live.characters[0].lastInteraction = 20 * 24 * 60 * 60 * 1000
        live.characters[0].chats[0].scriptstate = {
            codecDate: new Date('2026-09-07T00:00:00.000Z'),
            omitted: undefined,
        } as any
        const payloads = new Map<string, unknown>()
        const replaceDatabase = vi.fn()
        expect(await compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-chat',
            write: async (key, value) => {
                payloads.set(key, JSON.parse(JSON.stringify(value)))
                return true
            },
            read: async (key) => payloads.get(key),
            remove: vi.fn(async () => undefined),
            replaceDatabase,
        })).toBe(true)
        expect(replaceDatabase).toHaveBeenCalledTimes(1)
    })

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
            remove: vi.fn(async () => undefined),
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
        const remove = vi.fn(async () => undefined)

        await expect(compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-character',
            write: async (key, value) => {
                payloads.set(key, structuredClone(value))
                return true
            },
            read: async (key) => structuredClone(payloads.get(key)),
            remove,
            replaceDatabase: async () => {
                throw new Error('replacement failed')
            },
        })).rejects.toThrow('replacement failed')

        expect(live).toEqual(original)
        expect(remove).toHaveBeenCalledWith(['cold-character'])
    })

    it('reports activation and cleanup failures together', async () => {
        const live = fixtureDatabase()
        const payloads = new Map<string, unknown>()
        const activationError = new Error('replacement failed')
        const cleanupError = new Error('cleanup failed')

        const failure = await compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-character',
            write: async (key, value) => {
                payloads.set(key, structuredClone(value))
                return true
            },
            read: async (key) => structuredClone(payloads.get(key)),
            remove: async () => { throw cleanupError },
            replaceDatabase: async () => { throw activationError },
        }).catch((error) => error)

        expect(failure).toBeInstanceOf(AggregateError)
        expect((failure as AggregateError).errors).toEqual([activationError, cleanupError])
    })

    it('reports bounded progress and diagnoses a skipped payload without publishing a candidate', async () => {
        const live = fixtureDatabase()
        const progress: Array<[string, number]> = []
        const failures: unknown[] = []
        const replaceDatabase = vi.fn()

        const changed = await compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-character',
            write: async () => false,
            read: vi.fn(),
            remove: vi.fn(async () => undefined),
            replaceDatabase,
            onProgress: (phase, remaining) => progress.push([phase, remaining]),
            onFailure: (failure) => failures.push(failure),
        })

        expect(changed).toBe(false)
        expect(progress).toEqual([
            ['character', 0],
            ['chat', 0],
        ])
        expect(failures).toEqual([{
            kind: 'write',
            target: 'character',
            characterIndex: 0,
        }, {
            kind: 'write',
            target: 'chat',
            characterIndex: 0,
            chatIndex: 0,
        }])
        expect(replaceDatabase).not.toHaveBeenCalled()
    })

    it('skips the whole-database clone when nothing is eligible', async () => {
        const now = 20 * 24 * 60 * 60 * 1000
        const live = fixtureDatabase()
        live.characters[0].lastInteraction = now
        live.characters[0].chats[0].lastDate = now
        ;(live as any).cloneTrap = new Proxy({}, {
            ownKeys() { throw new Error('database must not be cloned') },
        })
        const write = vi.fn(async () => true)
        const read = vi.fn(async () => null)
        const replaceDatabase = vi.fn(async () => undefined)

        const changed = await compactColdStorageDatabase(live, {
            now,
            createId: () => 'unused',
            write,
            read,
            remove: vi.fn(async () => undefined),
            replaceDatabase,
        })

        expect(changed).toBe(false)
        expect(write).not.toHaveBeenCalled()
        expect(read).not.toHaveBeenCalled()
        expect(replaceDatabase).not.toHaveBeenCalled()
    })

    it('skips maintenance before cloning an incomplete catalog working set', async () => {
        const live = fixtureDatabase()
        live.botPresets = []
        live.characters = [createCatalogCharacterStub({
            id: 'character-1',
            type: 'character',
            name: 'Archived character',
            configuredIndex: 0,
            recentAt: 1,
            trashed: false,
            conversationCount: 1,
        })]
        const write = vi.fn(async () => true)
        const read = vi.fn(async () => null)
        const replaceDatabase = vi.fn(async () => undefined)

        const changed = await compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'unused',
            write,
            read,
            remove: vi.fn(async () => undefined),
            replaceDatabase,
        })

        expect(changed).toBe(false)
        expect(write).not.toHaveBeenCalled()
        expect(read).not.toHaveBeenCalled()
        expect(replaceDatabase).not.toHaveBeenCalled()
    })

    it('diagnoses a failed verification read and leaves the candidate unpublished', async () => {
        const live = fixtureDatabase()
        const failures: unknown[] = []
        const replaceDatabase = vi.fn()

        const changed = await compactColdStorageDatabase(live, {
            now: 20 * 24 * 60 * 60 * 1000,
            createId: () => 'cold-character',
            write: async () => true,
            read: async () => { throw new Error('read failed') },
            remove: vi.fn(async () => undefined),
            replaceDatabase,
            onFailure: (failure) => failures.push(failure),
        })

        expect(changed).toBe(false)
        expect(failures).toEqual([
            { kind: 'read', target: 'character', characterIndex: 0 },
            { kind: 'read', target: 'chat', characterIndex: 0, chatIndex: 0 },
        ])
        expect(replaceDatabase).not.toHaveBeenCalled()
    })
})
