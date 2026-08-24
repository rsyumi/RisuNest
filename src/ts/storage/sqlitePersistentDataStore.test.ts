import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))

import { SqlitePersistentDataStore } from './sqlitePersistentDataStore'
import { RevisionConflictError, SnapshotReleasedError } from './persistentDataStore'
import { fixtureDatabase } from './tests/persistentDataFixtures'

describe('SqlitePersistentDataStore', () => {
    beforeEach(() => {
        mocks.invoke.mockReset()
    })

    it('maps ordinary store operations to their native commands with camelCase payloads', async () => {
        mocks.invoke.mockResolvedValue({ revision: 9 })
        const store = new SqlitePersistentDataStore()
        const characterQuery = {
            search: 'alpha',
            order: 'recent' as const,
            trash: false,
            limit: 3,
            cursor: 'character-cursor',
        }
        const conversationQuery = {
            characterId: 'char-a',
            order: 'configured' as const,
            limit: 2,
            cursor: 'conversation-cursor',
        }
        const windowQuery = {
            characterId: 'char-a',
            conversationId: 'conv-long',
            anchorMessageId: 'msg-050',
            before: 4,
            after: 5,
        }
        const commit = { expectedRevision: 8, deleteCharacterId: 'char-c' }

        await store.open()
        await store.readRoot()
        await store.queryCharacters(characterQuery)
        await store.readCharacter('char-a')
        await store.queryConversations(conversationQuery)
        await store.readConversation('char-a', 'conv-long')
        await store.readConversationWindow(windowQuery)
        await store.commit(commit)
        await store.materializeDatabase(9)

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_open'],
            ['pds_read_root', {}],
            ['pds_query_characters', { query: characterQuery }],
            ['pds_read_character', { id: 'char-a' }],
            ['pds_query_conversations', { query: conversationQuery }],
            [
                'pds_read_conversation',
                { characterId: 'char-a', conversationId: 'conv-long' },
            ],
            ['pds_read_conversation_window', { query: windowQuery }],
            ['pds_commit', { commit }],
            ['pds_materialize', { revision: 9 }],
        ])
    })

    it('restores native revision-conflict errors', async () => {
        mocks.invoke.mockRejectedValue({ code: 'revision-conflict', expected: 12, actual: 13 })
        const store = new SqlitePersistentDataStore()

        await expect(store.commit({ expectedRevision: 12 })).rejects.toEqual(
            new RevisionConflictError(12, 13),
        )
    })

    it('restores native snapshot-released errors', async () => {
        mocks.invoke.mockRejectedValue(JSON.stringify({ code: 'snapshot-released' }))
        const store = new SqlitePersistentDataStore()

        await expect(store.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
    })

    it('restores native validation errors as ordinary errors', async () => {
        mocks.invoke.mockRejectedValue({ code: 'validation', message: 'invalid character' })
        const store = new SqlitePersistentDataStore()

        await expect(store.readCharacter('char-a')).rejects.toEqual(
            new Error('invalid character'),
        )
    })

    it('restores native store errors as ordinary errors', async () => {
        mocks.invoke.mockRejectedValue({ code: 'store-error', message: 'disk I/O error' })
        const store = new SqlitePersistentDataStore()

        await expect(store.readRoot()).rejects.toEqual(new Error('disk I/O error'))
    })

    it('replaces a database in staged 16-character batches before committing', async () => {
        mocks.invoke
            .mockResolvedValueOnce({ stagingId: 'staging-1' })
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce({ revision: 4 })
        const database = structuredClone(fixtureDatabase)
        database.characters = Array.from({ length: 17 }, (_, index) => ({
            ...structuredClone(fixtureDatabase.characters[0]),
            chaId: `character-${index}`,
            name: `Character ${index}`,
        }))
        const { characters, ...root } = database
        const store = new SqlitePersistentDataStore()

        await expect(store.replaceFromDatabase(database, 3)).resolves.toEqual({ revision: 4 })

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_replace_begin'],
            ['pds_replace_put_root', { stagingId: 'staging-1', root }],
            [
                'pds_replace_add_characters',
                { stagingId: 'staging-1', characters: characters.slice(0, 16) },
            ],
            [
                'pds_replace_add_characters',
                { stagingId: 'staging-1', characters: characters.slice(16) },
            ],
            ['pds_replace_commit', { stagingId: 'staging-1', expectedRevision: 3 }],
        ])
    })

    it('aborts a failed staged replacement without replacing its primary error', async () => {
        const primaryError = new Error('character batch failed')
        mocks.invoke
            .mockResolvedValueOnce({ stagingId: 'staging-2' })
            .mockResolvedValueOnce(undefined)
            .mockRejectedValueOnce(primaryError)
            .mockRejectedValueOnce(new Error('abort failed'))
        const store = new SqlitePersistentDataStore()
        const { characters, ...root } = fixtureDatabase

        await expect(store.replaceFromDatabase(fixtureDatabase)).rejects.toBe(primaryError)
        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_replace_begin'],
            ['pds_replace_put_root', { stagingId: 'staging-2', root }],
            [
                'pds_replace_add_characters',
                { stagingId: 'staging-2', characters },
            ],
            ['pds_replace_abort', { stagingId: 'staging-2' }],
        ])
    })

    it('splits staged character batches at approximately four MiB', async () => {
        mocks.invoke
            .mockResolvedValueOnce({ stagingId: 'staging-large' })
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce({ revision: 5 })
        const database = structuredClone(fixtureDatabase)
        database.characters = ['a', 'b'].map((suffix) => ({
            ...structuredClone(fixtureDatabase.characters[0]),
            chaId: `large-${suffix}`,
            name: suffix.repeat(2 * 1024 * 1024),
        }))
        const store = new SqlitePersistentDataStore()

        await store.replaceFromDatabase(database)

        const characterCalls = mocks.invoke.mock.calls.filter(
            ([command]) => command === 'pds_replace_add_characters',
        )
        expect(characterCalls).toHaveLength(2)
        expect(characterCalls.map(([, args]) => args.characters)).toEqual([
            database.characters.slice(0, 1),
            database.characters.slice(1),
        ])
    })

    it('forwards a lease to reads, releases it once, and rejects later reads locally', async () => {
        mocks.invoke.mockResolvedValueOnce({ lease: 'lease-7' }).mockResolvedValue(undefined)
        const store = new SqlitePersistentDataStore()
        const lease = await store.acquireRevision(7)

        await lease.readRoot()
        await lease.queryCharacters({ order: 'configured', trash: false, limit: 10 })
        await lease.readCharacter('char-a')
        await lease.queryConversations({ characterId: 'char-a', order: 'recent', limit: 10 })
        await lease.readConversation('char-a', 'conv-long')
        await lease.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            limit: 10,
        })
        await lease.release()
        await lease.release()

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_acquire_revision', { revision: 7 }],
            ['pds_read_root', { lease: 'lease-7' }],
            [
                'pds_query_characters',
                { query: { order: 'configured', trash: false, limit: 10 }, lease: 'lease-7' },
            ],
            ['pds_read_character', { id: 'char-a', lease: 'lease-7' }],
            [
                'pds_query_conversations',
                {
                    query: { characterId: 'char-a', order: 'recent', limit: 10 },
                    lease: 'lease-7',
                },
            ],
            [
                'pds_read_conversation',
                { characterId: 'char-a', conversationId: 'conv-long', lease: 'lease-7' },
            ],
            [
                'pds_read_conversation_window',
                {
                    query: { characterId: 'char-a', conversationId: 'conv-long', limit: 10 },
                    lease: 'lease-7',
                },
            ],
            ['pds_release_revision', { lease: 'lease-7' }],
        ])
        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        expect(mocks.invoke).toHaveBeenCalledTimes(8)
    })
})
