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
            anchorOccurrence: 'last' as const,
            before: 4,
            after: 5,
        }
        const { chats: _chats, ...characterDetail } = fixtureDatabase.characters[0]
        const alias = {
            key: 'assets/native.bin',
            objectHash: '44'.repeat(32),
            kind: 'asset' as const,
            size: 4,
            mime: 'application/octet-stream',
            name: 'Native',
            ext: 'bin',
        }
        const commit = {
            expectedRevision: 8,
            deleteCharacterId: 'char-c',
            characterDetails: [characterDetail],
            assetAliases: [alias],
        }
        const owner = { kind: 'root-module-assets' as const, index: 0 }
        const coldAlias = {
            key: 'cold/native',
            objectHash: '88'.repeat(32),
            size: 4,
            metadata: { source: 'native' },
        }

        await store.open()
        await store.readRoot()
        await store.queryPresets()
        await store.readPreset('1')
        await store.queryCharacters(characterQuery)
        await store.readCharacter('char-a')
        await store.queryConversations(conversationQuery)
        await store.readConversation('char-a', 'conv-long')
        await store.readConversationMetadata('char-a', 'conv-long')
        await store.readConversationWindow(windowQuery)
        await store.queryPluginStorage()
        await store.readPluginStorage('memory')
        await store.readAssetAlias({ kind: alias.kind, key: alias.key })
        await store.listAssetAliases({ kind: 'asset', limit: 2, cursor: 'alias-cursor' })
        await store.readAssetRepositoryAuthority()
        await store.readAssetOwnerHead(owner)
        await store.readColdPayloadAuthority()
        await store.readColdAlias(coldAlias.key)
        await store.listColdAliases()
        await store.commitAssetAlias(alias, 8)
        await store.deleteAssetAlias({ kind: alias.kind, key: alias.key }, 9)
        await store.commitColdAlias(coldAlias, 10)
        await store.deleteColdAlias(coldAlias.key, 11)
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
            [
                'pds_read_conversation_metadata',
                { characterId: 'char-a', conversationId: 'conv-long' },
            ],
            ['pds_read_conversation_window', { query: windowQuery }],
            ['pds_query_plugin_storage', {}],
            ['pds_read_plugin_storage', { key: 'memory' }],
            ['pds_read_asset_alias', { kind: 'asset', key: alias.key }],
            [
                'pds_list_asset_aliases',
                { query: { kind: 'asset', limit: 2, cursor: 'alias-cursor' } },
            ],
            ['pds_read_asset_repository_authority', {}],
            ['pds_read_asset_owner_head', { owner }],
            ['pds_read_cold_payload_authority', {}],
            ['pds_read_cold_alias', { key: coldAlias.key }],
            ['pds_list_cold_aliases', {}],
            ['pds_commit_asset_alias', { alias, expectedRevision: 8 }],
            [
                'pds_delete_asset_alias',
                { kind: 'asset', key: alias.key, expectedRevision: 9 },
            ],
            ['pds_commit_cold_alias', { alias: coldAlias, expectedRevision: 10 }],
            ['pds_delete_cold_alias', { key: coldAlias.key, expectedRevision: 11 }],
            [
                'pds_commit',
                {
                    commit: {
                        expectedRevision: 8,
                        deleteCharacterId: 'char-c',
                        characterDetails: [characterDetail],
                    },
                    assetAliases: [alias],
                },
            ],
            ['pds_materialize', { revision: 9 }],
        ])
    })

    it('forwards asset alias kind for current and leased reads', async () => {
        mocks.invoke.mockResolvedValueOnce({ lease: 'lease-alias-kind' }).mockResolvedValue(null)
        const store = new SqlitePersistentDataStore()
        const lease = await store.acquireRevision(9)
        const key = 'shared/same-key.bin'

        await store.readAssetAlias({ kind: 'asset', key })
        await store.readAssetAlias({ kind: 'inlay', key })
        await lease.readAssetAlias({ kind: 'asset', key })
        await lease.readAssetAlias({ kind: 'inlay', key })

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_acquire_revision', { revision: 9 }],
            ['pds_read_asset_alias', { kind: 'asset', key }],
            ['pds_read_asset_alias', { kind: 'inlay', key }],
            ['pds_read_asset_alias', { kind: 'asset', key, lease: 'lease-alias-kind' }],
            ['pds_read_asset_alias', { kind: 'inlay', key, lease: 'lease-alias-kind' }],
        ])
    })

    it('forwards each current and leased asset alias batch with one native call', async () => {
        mocks.invoke.mockResolvedValueOnce({ lease: 'lease-alias-batch' }).mockResolvedValue({
            revision: 9,
            value: [],
        })
        const store = new SqlitePersistentDataStore()
        const lease = await store.acquireRevision(9)
        const keys = ['assets/first.bin', 'assets/second.bin']

        await store.readAssetAliasesByKeys('asset', keys)
        await lease.readAssetAliasesByKeys('inlay', keys)

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_acquire_revision', { revision: 9 }],
            ['pds_read_asset_aliases_by_keys', { kind: 'asset', keys }],
            [
                'pds_read_asset_aliases_by_keys',
                { kind: 'inlay', keys, lease: 'lease-alias-batch' },
            ],
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
            .mockResolvedValueOnce({ revision: 3 })
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
            ['pds_replace_preserve_repositories', {
                stagingId: 'staging-1',
                expectedRevision: 3,
            }],
            ['pds_replace_put_asset_aliases', { stagingId: 'staging-1', aliases }],
            ['pds_replace_commit', { stagingId: 'staging-1', expectedRevision: 3 }],
        ])
    })

    it('preserves active repositories after staging a database replacement', async () => {
        mocks.invoke.mockImplementation(async (command: string) => {
            if (command === 'pds_replace_begin') return { stagingId: 'staging-cold-preserved' }
            if (command === 'pds_replace_preserve_repositories') return { revision: 7 }
            if (command === 'pds_replace_commit') return { revision: 8 }
            return undefined
        })
        const { characters, botPresets, ...root } = fixtureDatabase
        const store = new SqlitePersistentDataStore()

        await expect(store.replaceFromDatabase(fixtureDatabase, 7)).resolves.toEqual({ revision: 8 })

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_replace_begin'],
            ['pds_replace_put_root', { stagingId: 'staging-cold-preserved', root }],
            ['pds_replace_put_presets', {
                stagingId: 'staging-cold-preserved',
                presets: botPresets,
            }],
            ['pds_replace_add_characters', {
                stagingId: 'staging-cold-preserved',
                characters,
            }],
            ['pds_replace_preserve_repositories', {
                stagingId: 'staging-cold-preserved',
                expectedRevision: 7,
            }],
            ['pds_replace_commit', {
                stagingId: 'staging-cold-preserved',
                expectedRevision: 7,
            }],
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

    it('activates a complete asset repository migration through one staged generation', async () => {
        mocks.invoke.mockImplementation(async (command: string) => {
            if (command === 'pds_replace_begin') return { stagingId: 'staging-migration' }
            if (command === 'pds_replace_commit') return { revision: 8 }
            return undefined
        })
        const database = structuredClone(fixtureDatabase)
        database.modules = [{
            id: 'module',
            name: 'Module',
            description: '',
            assets: [['asset', 'assets/migrated.bin', 'BIN']],
        }]
        const alias = {
            key: 'assets/migrated.bin',
            objectHash: '55'.repeat(32),
            kind: 'asset' as const,
            size: 5,
            mime: 'application/octet-stream',
            name: 'Migrated',
            ext: 'BIN',
        }
        const head = {
            owner: { kind: 'root-module-assets' as const, index: 0 },
            present: true as const,
            manifestHash: '66'.repeat(32),
            entryCount: 1,
        }
        const { characters, botPresets, ...root } = database
        const store = new SqlitePersistentDataStore()

        await expect(store.activateAssetRepositoryMigration({
            sourceRevision: 7,
            migrationId: 'migration-atomic',
            compatibilityHash: '77'.repeat(32),
            database,
            assetAliases: [alias],
            assetOwnerHeads: [head],
        })).resolves.toEqual({ revision: 8 })

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_replace_begin'],
            ['pds_replace_put_asset_repository_authority', {
                stagingId: 'staging-migration',
                authority: {
                    format: 'preparing',
                    migrationId: 'migration-atomic',
                    sourceRevision: 7,
                },
            }],
            ['pds_replace_put_root', { stagingId: 'staging-migration', root }],
            ['pds_replace_put_presets', { stagingId: 'staging-migration', presets: botPresets }],
            ['pds_replace_add_characters', { stagingId: 'staging-migration', characters }],
            ['pds_replace_put_asset_aliases', {
                stagingId: 'staging-migration',
                aliases: [alias],
            }],
            ['pds_replace_put_asset_owner_heads', {
                stagingId: 'staging-migration',
                heads: [head],
            }],
            ['pds_replace_put_asset_repository_authority', {
                stagingId: 'staging-migration',
                authority: {
                    format: 'v2',
                    migrationId: 'migration-atomic',
                    compatibilityHash: '77'.repeat(32),
                },
            }],
            ['pds_replace_commit', {
                stagingId: 'staging-migration',
                expectedRevision: 7,
            }],
        ])
    })

    it('activates a complete cold payload migration through one atomic command', async () => {
        mocks.invoke.mockResolvedValue({ revision: 8 })
        const alias = {
            key: 'cold/migrated',
            objectHash: '88'.repeat(32),
            size: 3,
            metadata: {},
        }
        const input = {
            sourceRevision: 7,
            migrationId: 'cold-migration',
            compatibilityHash: '99'.repeat(32),
            coldAliases: [alias],
        }
        const store = new SqlitePersistentDataStore()

        await expect(store.activateColdPayloadMigration(input)).resolves.toEqual({ revision: 8 })
        expect(mocks.invoke).toHaveBeenCalledWith('pds_activate_cold_payload_migration', { input })
    })

    it('splits staged character batches at approximately four MiB', async () => {
        mocks.invoke.mockImplementation(async (command: string) => {
            if (command === 'pds_replace_begin') return { stagingId: 'staging-large' }
            if (command === 'pds_replace_preserve_repositories') return { revision: 4 }
            if (command === 'pds_replace_commit') return { revision: 5 }
            return undefined
        })
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
        await lease.readConversationMetadata('char-a', 'conv-long')
        await lease.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            limit: 10,
        })
        await lease.queryPluginStorage()
        await lease.readPluginStorage('memory')
        await lease.readAssetAlias({ kind: 'asset', key: 'assets/pinned.bin' })
        await lease.listAssetAliases({ kind: 'asset', limit: 2 })
        await lease.readAssetRepositoryAuthority()
        await lease.readAssetOwnerHead({ kind: 'root-module-assets', index: 0 })
        await lease.readColdPayloadAuthority()
        await lease.readColdAlias('cold/pinned')
        await lease.listColdAliases()
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
                'pds_read_conversation_metadata',
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
            [
                'pds_read_asset_alias',
                { kind: 'asset', key: 'assets/pinned.bin', lease: 'lease-7' },
            ],
            [
                'pds_list_asset_aliases',
                { query: { kind: 'asset', limit: 2 }, lease: 'lease-7' },
            ],
            ['pds_read_asset_repository_authority', { lease: 'lease-7' }],
            [
                'pds_read_asset_owner_head',
                { owner: { kind: 'root-module-assets', index: 0 }, lease: 'lease-7' },
            ],
            ['pds_read_cold_payload_authority', { lease: 'lease-7' }],
            ['pds_read_cold_alias', { key: 'cold/pinned', lease: 'lease-7' }],
            ['pds_list_cold_aliases', { lease: 'lease-7' }],
            ['pds_release_revision', { lease: 'lease-7' }],
        ])
        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readConversationMetadata('char-a', 'conv-long')).rejects.toBeInstanceOf(
            SnapshotReleasedError,
        )
        expect(mocks.invoke).toHaveBeenCalledTimes(20)
    })

    it('retains the native open report and warns when a snapshot restore was skipped', async () => {
        const openResult = {
            revision: 12,
            restoreFailure: 'persistent snapshot restore skipped: integrity check failed',
        }
        mocks.invoke.mockResolvedValue(openResult)
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        const store = new SqlitePersistentDataStore()

        expect(store.lastOpenResult).toBeNull()
        await store.open()

        expect(store.lastOpenResult).toEqual(openResult)
        expect(warn).toHaveBeenCalledOnce()
        expect(warn.mock.calls[0][0]).toContain(openResult.restoreFailure)
        warn.mockRestore()
    })

    it('keeps the open report without warning when no snapshot restore was skipped', async () => {
        const openResult = { revision: 3 }
        mocks.invoke.mockResolvedValue(openResult)
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        const store = new SqlitePersistentDataStore()

        await store.open()

        expect(mocks.invoke).toHaveBeenCalledWith('pds_open')
        expect(store.lastOpenResult).toEqual(openResult)
        expect(store.lastOpenResult?.restoreFailure).toBeUndefined()
        mocks.invoke.mockResolvedValueOnce({ revision: 4 })
        await store.open()
        expect(store.lastOpenResult).toEqual({ revision: 4 })
        expect(mocks.invoke).toHaveBeenCalledTimes(2)
        expect(warn).not.toHaveBeenCalled()
        warn.mockRestore()
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
