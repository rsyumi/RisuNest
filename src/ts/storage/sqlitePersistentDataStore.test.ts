import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))

import { SqlitePersistentDataStore } from './sqlitePersistentDataStore'
import { nativePersistentRevisionLease } from './nativePersistentExport'
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
        const { chats: _chats, ...characterDetail } = fixtureDatabase.characters[0]
        const commit = {
            expectedRevision: 8,
            deleteCharacterId: 'char-c',
            characterDetails: [characterDetail],
        }
        const alias = {
            key: 'assets/native.bin',
            objectHash: '44'.repeat(32),
            kind: 'asset' as const,
            size: 4,
            mime: 'application/octet-stream',
            name: 'Native',
            ext: 'bin',
        }
        const owner = { kind: 'root-module-assets' as const, index: 0 }

        await store.open()
        await store.readRoot()
        await store.queryPresets()
        await store.readPreset('1')
        await store.queryCharacters(characterQuery)
        await store.readCharacter('char-a')
        await store.queryConversations(conversationQuery)
        await store.readConversation('char-a', 'conv-long')
        await store.readConversationWindow(windowQuery)
        await store.queryPluginStorage()
        await store.readPluginStorage('memory')
        await store.readAssetAlias(alias.key)
        await store.readAssetOwnerHead(owner)
        await store.commitAssetAlias(alias, 8)
        await store.commit(commit)
        await store.materializeDatabase(9)

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_open'],
            ['pds_read_root', {}],
            ['pds_query_presets', {}],
            ['pds_read_preset', { id: '1' }],
            ['pds_query_characters', { query: characterQuery }],
            ['pds_read_character', { id: 'char-a' }],
            ['pds_query_conversations', { query: conversationQuery }],
            [
                'pds_read_conversation',
                { characterId: 'char-a', conversationId: 'conv-long' },
            ],
            ['pds_read_conversation_window', { query: windowQuery }],
            ['pds_query_plugin_storage', {}],
            ['pds_read_plugin_storage', { key: 'memory' }],
            ['pds_read_asset_alias', { key: alias.key }],
            ['pds_read_asset_owner_head', { owner }],
            ['pds_commit_asset_alias', { alias, expectedRevision: 8 }],
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

    it('forwards valid absolute ranges and rejects invalid ranges before native IPC', async () => {
        mocks.invoke.mockResolvedValue({ revision: 9, value: null })
        const store = new SqlitePersistentDataStore()
        const validRange = {
            characterId: 'char-a',
            conversationId: 'conv-long',
            startIndex: 127,
            limit: 2,
        }

        await store.readConversationWindow(validRange)
        expect(mocks.invoke).toHaveBeenCalledWith('pds_read_conversation_window', {
            query: validRange,
        })

        mocks.invoke.mockClear()
        await expect(store.readConversationWindow({
            ...validRange,
            startIndex: -1,
        })).rejects.toBeInstanceOf(RangeError)
        await expect(store.readConversationWindow({
            ...validRange,
            limit: 4_097,
        })).rejects.toBeInstanceOf(RangeError)
        await expect(store.readConversationWindow({
            ...validRange,
            anchorMessageId: 'msg-127',
        })).rejects.toBeInstanceOf(RangeError)
        expect(mocks.invoke).not.toHaveBeenCalled()
    })

    it('replaces a database in staged 16-character batches before committing', async () => {
        mocks.invoke
            .mockResolvedValueOnce({ stagingId: 'staging-1' })
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce(undefined)
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
        const { characters, botPresets, ...root } = database
        const aliases = [{
            key: 'assets/staged.bin',
            objectHash: '55'.repeat(32),
            kind: 'asset' as const,
            size: 5,
            mime: 'application/octet-stream',
            name: 'Staged',
            ext: 'bin',
        }]
        const store = new SqlitePersistentDataStore()

        await expect(store.replaceFromDatabase(database, 3, aliases)).resolves.toEqual({ revision: 4 })

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_replace_begin'],
            ['pds_replace_put_root', { stagingId: 'staging-1', root }],
            ['pds_replace_put_presets', { stagingId: 'staging-1', presets: botPresets }],
            [
                'pds_replace_add_characters',
                { stagingId: 'staging-1', characters: characters.slice(0, 16) },
            ],
            [
                'pds_replace_add_characters',
                { stagingId: 'staging-1', characters: characters.slice(16) },
            ],
            ['pds_replace_put_asset_aliases', { stagingId: 'staging-1', aliases }],
            ['pds_replace_commit', { stagingId: 'staging-1', expectedRevision: 3 }],
        ])
    })

    it('aborts a failed staged replacement without replacing its primary error', async () => {
        const primaryError = new Error('character batch failed')
        mocks.invoke
            .mockResolvedValueOnce({ stagingId: 'staging-2' })
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce(undefined)
            .mockRejectedValueOnce(primaryError)
            .mockRejectedValueOnce(new Error('abort failed'))
        const store = new SqlitePersistentDataStore()
        const { characters, botPresets, ...root } = fixtureDatabase

        await expect(store.replaceFromDatabase(fixtureDatabase)).rejects.toBe(primaryError)
        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_replace_begin'],
            ['pds_replace_put_root', { stagingId: 'staging-2', root }],
            ['pds_replace_put_presets', { stagingId: 'staging-2', presets: botPresets }],
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

        expect(lease[nativePersistentRevisionLease]).toBe('lease-7')

        await lease.readRoot()
        await lease.queryPresets()
        await lease.readPreset('0')
        await lease.queryCharacters({ order: 'configured', trash: false, limit: 10 })
        await lease.readCharacter('char-a')
        await lease.queryConversations({ characterId: 'char-a', order: 'recent', limit: 10 })
        await lease.readConversation('char-a', 'conv-long')
        await lease.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            limit: 10,
        })
        await lease.queryPluginStorage()
        await lease.readPluginStorage('memory')
        await lease.readAssetAlias('assets/pinned.bin')
        await lease.readAssetOwnerHead({ kind: 'root-module-assets', index: 0 })
        await lease.release()
        await lease.release()

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_acquire_revision', { revision: 7 }],
            ['pds_read_root', { lease: 'lease-7' }],
            ['pds_query_presets', { lease: 'lease-7' }],
            ['pds_read_preset', { id: '0', lease: 'lease-7' }],
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
            ['pds_query_plugin_storage', { lease: 'lease-7' }],
            ['pds_read_plugin_storage', { key: 'memory', lease: 'lease-7' }],
            ['pds_read_asset_alias', { key: 'assets/pinned.bin', lease: 'lease-7' }],
            [
                'pds_read_asset_owner_head',
                { owner: { kind: 'root-module-assets', index: 0 }, lease: 'lease-7' },
            ],
            ['pds_release_revision', { lease: 'lease-7' }],
        ])
        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        expect(mocks.invoke).toHaveBeenCalledTimes(14)
    })

    it('keeps a lease active and retries native cleanup after release fails', async () => {
        const releaseError = new Error('native release failed')
        let releaseCalls = 0
        mocks.invoke.mockImplementation(async (command: string) => {
            if (command === 'pds_acquire_revision') return { lease: 'lease-retry' }
            if (command === 'pds_release_revision') {
                releaseCalls += 1
                if (releaseCalls === 1) throw releaseError
                return undefined
            }
            if (command === 'pds_read_root') return { revision: 7, value: {} }
            throw new Error(`Unexpected command ${command}`)
        })
        const store = new SqlitePersistentDataStore()
        const lease = await store.acquireRevision(7)

        await expect(lease.release()).rejects.toBe(releaseError)
        await expect(lease.readRoot()).resolves.toMatchObject({ revision: 7 })
        await expect(lease.release()).resolves.toBeUndefined()
        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        expect(releaseCalls).toBe(2)
    })
})
