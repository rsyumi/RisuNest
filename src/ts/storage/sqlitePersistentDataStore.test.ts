import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))

import { createMutationGatedPersistentDataStore } from './mutationGatedPersistentDataStore'
import { createStorageMutationGate, createInRealmStorageLockManager } from './storageMutationGate'
import { SqlitePersistentDataStore } from './sqlitePersistentDataStore'
import { rendererDocument } from './rendererDocument'
import { nativePersistentRevisionLease } from './nativePersistentExport'
import { RevisionConflictError, SnapshotReleasedError } from './persistentDataStore'
import { fixtureDatabase } from './tests/persistentDataFixtures'
import { PayloadTooLargeError } from './nativePersistenceValue'
import { encodeNativeCommit, NativeCommitTransport, STAGED_REQUEST_BYTES } from './nativeCommitTransport'
import type { botPreset, character, Database, Message } from './database.svelte'
import type { PluginStorageValue } from './persistentDataStore'

describe('SqlitePersistentDataStore', () => {
    it('activates a bound upstream staging generation through an immutable LWW replacement request', async () => {
        const header={bindingAuthority:'9007199254740993',requestId:'synthetic-import'}
        mocks.invoke.mockImplementation(async (command: string) => {
            if(command==='pds_replace_begin') return {stagingId:'upstream-stage'}
            if(command==='pds_replace_preserve_repositories') return {revision:7}
            if(command==='pds_lww_commit_replacement') return {revision:8}
            return undefined
        })
        await expect(new SqlitePersistentDataStore().replaceFromDatabase(structuredClone(fixtureDatabase),7,[],undefined,header)).resolves.toEqual({revision:8})
        expect(mocks.invoke).toHaveBeenLastCalledWith('pds_lww_commit_replacement',{request:{...header,stagingId:'upstream-stage'}})
        expect(mocks.invoke.mock.calls.some(([command])=>command==='pds_replace_commit')).toBe(false)
        expect(mocks.invoke).toHaveBeenCalledWith('pds_replace_preserve_repositories',{stagingId:'upstream-stage',expectedRevision:7})
    })

    it('preserves the canonical native trash stamp beyond the safe integer range', async () => {
        const summary={id:'synthetic',name:'Synthetic',trashed:true,trashTime:1,trashStampMs:'9007199254740993'}
        mocks.invoke.mockResolvedValueOnce({revision:7,items:[summary]})
        await expect(new SqlitePersistentDataStore().queryCharacters({trash:true,order:'configured',limit:200})).resolves.toEqual({revision:7,items:[summary]})
    })

    it('stages outside admission and gates only final native activation, with an immutable bound header',async()=>{
        const latch=()=>{let resolve!:()=>void;const promise=new Promise<void>(done=>{resolve=done});return {promise,resolve}}
        const stagingStarted=latch(),continueStaging=latch(),writeStarted=latch(),continueWrite=latch(),activationStarted=latch(),continueActivation=latch()
        let commits=0
        mocks.invoke.mockImplementation(async(command:string)=>{
            if(command==='pds_replace_begin')return {stagingId:'isolated-native-stage'}
            if(command==='pds_replace_put_root'){stagingStarted.resolve();await continueStaging.promise;return}
            if(command==='pds_replace_preserve_repositories')return {revision:3}
            if(command==='pds_commit'){
                commits++
                if(commits===2){writeStarted.resolve();await continueWrite.promise}
                return {revision:commits===1?2:commits===2?3:5}
            }
            if(command==='pds_lww_commit_replacement'){activationStarted.resolve();await continueActivation.promise;return {revision:4}}
        })
        const gated=createMutationGatedPersistentDataStore(new SqlitePersistentDataStore(),createStorageMutationGate({locks:createInRealmStorageLockManager()}))
        const header={bindingAuthority:'synthetic-authority',requestId:'immutable-native-request'}
        const replacement=gated.replaceFromDatabase(structuredClone(fixtureDatabase),undefined,[],undefined,header)
        await stagingStarted.promise
        header.requestId='mutated-caller-request'
        await expect(gated.commit({expectedRevision:1})).resolves.toEqual({revision:2})
        const earlierWrite=gated.commit({expectedRevision:2})
        await writeStarted.promise
        continueStaging.resolve()
        await vi.waitFor(()=>expect(mocks.invoke).toHaveBeenCalledWith('pds_replace_preserve_repositories',{stagingId:'isolated-native-stage'}))
        expect(mocks.invoke.mock.calls.some(([command])=>command==='pds_lww_commit_replacement')).toBe(false)
        continueWrite.resolve();await earlierWrite;await activationStarted.promise
        const laterWrite=gated.commit({expectedRevision:4})
        await Promise.resolve();expect(commits).toBe(2)
        continueActivation.resolve()
        await expect(replacement).resolves.toEqual({revision:4})
        await expect(laterWrite).resolves.toEqual({revision:5})
        expect(mocks.invoke).toHaveBeenCalledWith('pds_lww_commit_replacement',{request:{bindingAuthority:'synthetic-authority',requestId:'immutable-native-request',stagingId:'isolated-native-stage'}})
        expect(mocks.invoke.mock.calls.some(([command])=>command==='pds_replace_abort')).toBe(false)
    })

    it('does not abort or submit activation twice after an uncertain native activation response',async()=>{
        mocks.invoke.mockImplementation(async(command:string)=>{
            if(command==='pds_replace_begin')return {stagingId:'uncertain-stage'}
            if(command==='pds_replace_preserve_repositories')return {revision:7}
            if(command==='pds_lww_commit_replacement')throw new Error('response unavailable')
        })
        const store=new SqlitePersistentDataStore()
        const stage=await store.stageDatabaseReplacement(structuredClone(fixtureDatabase),7,[],undefined,{bindingAuthority:'authority',requestId:'request'})
        await expect(stage.activate()).rejects.toThrow('response unavailable')
        await stage.abort()
        await expect(stage.activate()).rejects.toThrow('already submitted')
        expect(mocks.invoke.mock.calls.filter(([command])=>command==='pds_lww_commit_replacement')).toHaveLength(1)
        expect(mocks.invoke.mock.calls.some(([command])=>command==='pds_replace_abort')).toBe(false)
    })

    beforeEach(() => {
        mocks.invoke.mockReset()
    })

    it('reads current binding authority through the existing native state command', async () => {
        mocks.invoke.mockResolvedValue({targetAuthority:'9007199254740993'})
        expect(await new SqlitePersistentDataStore().lwwBindingState()).toEqual({targetAuthority:'9007199254740993'})
        expect(mocks.invoke).toHaveBeenCalledWith('pds_lww_binding_state', {})
    })

    it('forwards LWW commands with decimal controls and the exact shared request envelope', async () => {
        const store = new SqlitePersistentDataStore()
        const header = {bindingAuthority:'authority', requestId:'request'}
        mocks.invoke.mockResolvedValue(undefined)
        await store.lwwReadOutbox({...header, limit:'64'})
        await store.lwwStageReceive({...header, changes:[], progress:{kind:'external',cursor:'9007199254740993',writerId:'writer'}, admittedTimeUpperMs:'9007199254740993'})
        await store.lwwApplyReceive({...header, generating:[{characterId:'a',conversationId:'same'},{characterId:'b',conversationId:'same'}]})
        await store.lwwFinishReceive(header)
        await store.lwwDrainDeferred({...header,generating:[]})
        await store.lwwCommitReplacement({...header,stagingId:'staged'})
        expect(mocks.invoke.mock.calls.map(([command,args]) => [command,Object.keys(args)])).toEqual([
            'pds_lww_read_outbox','pds_lww_stage_receive',
            'pds_lww_apply_receive','pds_lww_finish_receive','pds_lww_drain_deferred','pds_lww_commit_replacement',
        ].map((command) => [command,['request']]))
        expect(mocks.invoke.mock.calls[2][1]).toEqual({request:{...header,generating:[{characterId:'a',conversationId:'same'},{characterId:'b',conversationId:'same'}]}})
    })

    it('reads owner pages with one native call per page and a pinned lease', async () => {
        mocks.invoke.mockResolvedValueOnce({ lease: 'plugin-page-lease' })
        const store = new SqlitePersistentDataStore()
        const lease = await store.acquireRevision(9)
        const query = { owner: 'owner-a', afterKey: { owner: 'owner-a', key: 'zeta', ordinal: 7 }, limit: 128 }
        const page = { revision: 9, items: [{ owner: 'owner-a', key: 'alpha', value: false }], nextCursor: null }
        mocks.invoke.mockResolvedValue(page)
        await expect(store.readPluginStorageValues(query)).resolves.toEqual(page)
        await expect(lease.readPluginStorageValues(query)).resolves.toEqual(page)
        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_acquire_revision', { revision: 9, document: rendererDocument() }],
            ['pds_read_plugin_storage_page', { query }],
            ['pds_read_plugin_storage_page', { query, lease: 'plugin-page-lease' }],
        ])
        await lease.release()
        mocks.invoke.mockClear()
        await expect(lease.readPluginStorageValues(query)).rejects.toBeInstanceOf(SnapshotReleasedError)
        expect(mocks.invoke).not.toHaveBeenCalled()
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
            deleteCharacterIds: ['char-c'],
            characterDetails: [characterDetail],
            assetAliases: [alias],
        }
        const owner = { kind: 'root-module-assets' as const, moduleId: 'module-0' }

        await store.open()
        await store.readRoot()
        await store.readRevision()
        await store.queryPresets()
        await store.readPreset('1')
        await store.queryCharacters(characterQuery)
        await store.readCharacter('char-a')
        await store.queryConversations(conversationQuery)
        await store.readConversation('char-a', 'conv-long')
        await store.readConversationMetadata('char-a', 'conv-long')
        await store.readConversationWindow(windowQuery)
        await store.queryAuthorNotePresetUsage('preset')
        await store.queryPluginStorage()
        await store.queryPluginStorage('test-plugin')
        await store.readPluginStorage('test-plugin', 'memory')
        await store.readAssetAlias({ kind: alias.kind, key: alias.key })
        await store.listAssetAliases({ kind: 'asset', limit: 2, cursor: 'alias-cursor' })
        await store.readAssetOwnerHead(owner)
        await store.commitAssetAlias(alias, 8)
        await store.deleteAssetAlias({ kind: alias.kind, key: alias.key }, 9)
        await store.commit(commit)
        await store.materializeDatabase(9)

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_open', { document: rendererDocument() }],
            ['pds_read_root', {}],
            ['pds_read_revision', {}],
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
            ['pds_query_author_note_preset_usage', { presetId: 'preset' }],
            ['pds_query_plugin_storage', {}],
            ['pds_query_plugin_storage', { owner: 'test-plugin' }],
            ['pds_read_plugin_storage', { owner: 'test-plugin', key: 'memory' }],
            ['pds_read_asset_alias', { kind: 'asset', key: alias.key }],
            [
                'pds_list_asset_aliases',
                { query: { kind: 'asset', limit: 2, cursor: 'alias-cursor' } },
            ],
            ['pds_read_asset_owner_head', { owner }],
            ['pds_commit_asset_alias', { alias, expectedRevision: 8 }],
            [
                'pds_delete_asset_alias',
                { kind: 'asset', key: alias.key, expectedRevision: 9 },
            ],
            [
                'pds_commit',
                {
                    commit: {
                        expectedRevision: 8,
                        deleteCharacterIds: ['char-c'],
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
            ['pds_acquire_revision', { revision: 9, document: rendererDocument() }],
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
            ['pds_acquire_revision', { revision: 9, document: rendererDocument() }],
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

    it.each([
        { code: 'validation', message: 'request-id-integrity' },
        { code: 'store-error', message: 'disk I/O error' },
        { code: 'commit-decode', message: 'invalid commit body' },
        { code: 'schema-mismatch', message: 'invalid-lww-schema' },
    ])('restores a native $code error as an Error that keeps its code', async (native) => {
        mocks.invoke.mockRejectedValue(native)
        const store = new SqlitePersistentDataStore()

        const error = await store.readRoot().catch((value: unknown) => value)
        expect(error).toBeInstanceOf(Error)
        expect(error).toMatchObject(native)
    })

    it('cancels an in-flight native character archive operation with the same operation id', async () => {
        let rejectArchive!: (error: unknown) => void
        mocks.invoke.mockImplementation((command: string, args?: Record<string, unknown>) => {
            if (command === 'pds_archive_character') {
                return new Promise((_resolve, reject) => { rejectArchive = reject })
            }
            if (command === 'pds_cancel_character_archive_operation') {
                rejectArchive({
                    code: 'validation',
                    message: 'character archive operation cancelled',
                })
                return Promise.resolve(true)
            }
            throw new Error(`unexpected command ${command}`)
        })
        const store = new SqlitePersistentDataStore()
        const controller = new AbortController()

        const archive = store.archiveCharacter('char-a', 12, controller.signal)
        controller.abort()

        await expect(archive).rejects.toMatchObject({ name: 'AbortError' })
        const archiveArgs = mocks.invoke.mock.calls[0][1] as Record<string, unknown>
        expect(mocks.invoke.mock.calls).toEqual([
            [
                'pds_archive_character',
                {
                    characterId: 'char-a',
                    expectedRevision: 12,
                    operationId: archiveArgs.operationId,
                },
            ],
            [
                'pds_cancel_character_archive_operation',
                { operationId: archiveArgs.operationId },
            ],
        ])
    })

    it('does not start a character restore whose signal is already cancelled', async () => {
        const store = new SqlitePersistentDataStore()
        const controller = new AbortController()
        controller.abort()

        await expect(store.restoreCharacter('char-a', 12, controller.signal))
            .rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.invoke).not.toHaveBeenCalled()
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

    it('passes owner-scoped plugin values through a staged replacement', async () => {
        mocks.invoke.mockImplementation(async (command: string) => {
            if (command === 'pds_replace_begin') return { stagingId: 'staging-plugin-values' }
            if (command === 'pds_replace_preserve_repositories') return { revision: 4 }
            if (command === 'pds_replace_commit') return { revision: 5 }
            return undefined
        })
        const pluginStorageValues = [
            { owner: 'plugin-a', key: 'shared', value: 'a' },
            { owner: 'plugin-b', key: 'shared', value: 'b' },
        ]
        const store = new SqlitePersistentDataStore()

        await store.replaceFromDatabase(fixtureDatabase, 4, [], pluginStorageValues)

        expect(mocks.invoke).toHaveBeenCalledWith('pds_replace_put_root', expect.objectContaining({
            stagingId: 'staging-plugin-values',
            pluginStorageValues,
        }))
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
        await lease.queryPluginStorage('test-plugin')
        await lease.readPluginStorage('test-plugin', 'memory')
        await lease.readAssetAlias({ kind: 'asset', key: 'assets/pinned.bin' })
        await lease.listAssetAliases({ kind: 'asset', limit: 2 })
        await lease.readAssetOwnerHead({ kind: 'root-module-assets', moduleId: 'module-0' })
        await lease.release()
        await lease.release()

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_acquire_revision', { revision: 7, document: rendererDocument() }],
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
            ['pds_query_plugin_storage', { lease: 'lease-7', owner: 'test-plugin' }],
            ['pds_read_plugin_storage', { owner: 'test-plugin', key: 'memory', lease: 'lease-7' }],
            [
                'pds_read_asset_alias',
                { kind: 'asset', key: 'assets/pinned.bin', lease: 'lease-7' },
            ],
            [
                'pds_list_asset_aliases',
                { query: { kind: 'asset', limit: 2 }, lease: 'lease-7' },
            ],
            [
                'pds_read_asset_owner_head',
                { owner: { kind: 'root-module-assets', moduleId: 'module-0' }, lease: 'lease-7' },
            ],
            ['pds_release_revision', { lease: 'lease-7' }],
        ])
        mocks.invoke.mockClear()
        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readConversationMetadata('char-a', 'conv-long')).rejects.toBeInstanceOf(
            SnapshotReleasedError,
        )
        expect(mocks.invoke).not.toHaveBeenCalled()
    })


    it('keeps each native revision report without warning', async () => {
        const openResult = { revision: 3 }
        mocks.invoke.mockResolvedValue(openResult)
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        const store = new SqlitePersistentDataStore()

        await store.open()

        expect(mocks.invoke).toHaveBeenCalledWith('pds_open', { document: rendererDocument() })
        expect(store.lastOpenResult).toEqual(openResult)
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

describe('SqlitePersistentDataStore staged replacement pieces', () => {
    const MiB = 1024 * 1024
    const stagingId = 'staging-pieces'
    const store = new SqlitePersistentDataStore()
    // Above the staging budget, so an oversized value is split into its own request as under the native limit.
    const requestLimit = 2 * STAGED_REQUEST_BYTES
    const limited = new SqlitePersistentDataStore(new NativeCommitTransport({
        windows: () => false, invoke: mocks.invoke, encode: encodeNativeCommit, shared: () => undefined, maxRequestBytes: requestLimit,
    }))
    const huge = () => 'x'.repeat(requestLimit)

    beforeEach(() => {
        mocks.invoke.mockReset()
        mocks.invoke.mockImplementation(async (command: string) => {
            if (command === 'pds_replace_begin') return { stagingId }
            if (command === 'pds_replace_preserve_repositories') return { revision: 4 }
            if (command === 'pds_replace_commit') return { revision: 5 }
            return undefined
        })
    })
    const calls = (match: (command: string) => boolean) =>
        mocks.invoke.mock.calls.filter(([command]) => match(command))

    it('stages a character above the request budget as its detail, each chat and message pages', async () => {
        const database = structuredClone(fixtureDatabase)
        const [beta, alpha, gamma] = database.characters as character[]
        const messages = ['a', 'b', 'c'].map((mark, index) => ({
            role: 'user', data: mark.repeat(1.5 * MiB), time: 1_700_000_000_000 + index,
        })) as Message[]
        alpha.chats[0].message = messages
        alpha.chats[1].message = [{ role: 'char', data: 'reply' }] as Message[]

        await store.replaceFromDatabase(database, 4)

        const { chats, ...detail } = alpha
        const [long, short] = chats.map(({ message: _message, ...conversation }) => conversation)
        const conversation = { stagingId, characterId: 'char-a' }
        expect(calls((command) => /character|conversation/.test(command))).toEqual([
            ['pds_replace_add_characters', { stagingId, characters: [beta] }],
            ['pds_replace_put_character_detail', { stagingId, detail, conversationCount: 2 }],
            ['pds_replace_put_conversation', {
                ...conversation, configuredIndex: 0, conversation: long, messageCount: 3, lastMessageTime: 1_700_000_000_002,
            }],
            ['pds_replace_add_conversation_messages', {
                ...conversation, conversationId: 'conv-long', start: 0, messages: messages.slice(0, 2),
            }],
            ['pds_replace_add_conversation_messages', {
                ...conversation, conversationId: 'conv-long', start: 2, messages: messages.slice(2),
            }],
            ['pds_replace_put_conversation', { ...conversation, configuredIndex: 1, conversation: short, messageCount: 1 }],
            ['pds_replace_add_conversation_messages', {
                ...conversation, conversationId: 'conv-short', start: 0, messages: alpha.chats[1].message,
            }],
            ['pds_replace_add_characters', { stagingId, characters: [gamma] }],
        ])
        expect(calls((command) => command === 'pds_replace_put_conversation')[1][1]).not.toHaveProperty('lastMessageTime')
    })

    it('stages presets above the request budget in append batches', async () => {
        const database = structuredClone(fixtureDatabase)
        database.botPresets = ['a', 'b', 'c'].map((mark) => ({
            id: `preset-${mark}`, name: mark, mainPrompt: mark.repeat(1.5 * MiB),
        })) as unknown as botPreset[]

        await store.replaceFromDatabase(database, 4)

        expect(calls((command) => command.includes('presets'))).toEqual([
            ['pds_replace_put_presets', { stagingId, presets: database.botPresets.slice(0, 2) }],
            ['pds_replace_add_presets', { stagingId, presets: database.botPresets.slice(2) }],
        ])
    })

    it('stages plugin values that do not fit beside the root in append batches', async () => {
        const values = ['a', 'b', 'c'].map((mark) => ({
            owner: 'synthetic-plugin', key: mark, value: mark.repeat(1.5 * MiB),
        }))

        await store.replaceFromDatabase(fixtureDatabase, 4, [], values)

        const { characters: _characters, botPresets: _presets, ...root } = fixtureDatabase
        expect(calls((command) => /root|plugin/.test(command))).toEqual([
            ['pds_replace_put_root', { stagingId, root, pluginStorageValues: [] }],
            ['pds_replace_add_plugin_storage_values', { stagingId, values: values.slice(0, 2) }],
            ['pds_replace_add_plugin_storage_values', { stagingId, values: values.slice(2) }],
        ])
    })

    it('moves the plugin storage of a root above the request budget into append batches', async () => {
        const database = structuredClone(fixtureDatabase)
        const [a, b, c] = ['a', 'b', 'c'].map((mark) => mark.repeat(1.5 * MiB))
        database.pluginCustomStorage = { a, b, c }
        database.pluginStorageMeta = { a: { plugin: 'synthetic-plugin', updatedAt: 1 }, c: { plugin: 'other-plugin', updatedAt: 2 } }

        await store.replaceFromDatabase(database, 4)

        const {
            characters: _characters, botPresets: _presets, pluginCustomStorage: _storage, pluginStorageMeta: meta, ...root
        } = database
        expect(calls((command) => /root|plugin/.test(command))).toEqual([
            ['pds_replace_put_root', { stagingId, root }],
            ['pds_replace_add_plugin_storage', { stagingId, storage: { a, b }, meta: { a: meta.a } }],
            ['pds_replace_add_plugin_storage', { stagingId, storage: { c }, meta: { c: meta.c } }],
        ])
    })

    it.each([
        ['root', 'pds_replace_put_root', (database: any) => { database.username = huge() }],
        ['preset', 'pds_replace_put_presets', (database: any) => { database.botPresets[0].mainPrompt = huge() }],
        ['character', 'pds_replace_put_character_detail', (database: any) => { database.characters[1].desc = huge() }],
        ['conversation', 'pds_replace_put_conversation', (database: any) => { database.characters[1].chats[0].note = huge() }],
        ['message', 'pds_replace_add_conversation_messages', (database: any) => {
            database.characters[1].chats[0].message[0].data = huge()
        }],
        ['plugin-value', 'pds_replace_add_plugin_storage', (database: any) => { database.pluginCustomStorage = { big: huge() } }],
        ['plugin-value', 'pds_replace_add_plugin_storage_values', () => [{ owner: 'synthetic-plugin', key: 'big', value: huge() }]],
    ] as const)('refuses a %s above the request limit with %s unsent and aborts the staging', async (kind, command, grow) => {
        const database = structuredClone(fixtureDatabase)
        const values = grow(database) as PluginStorageValue[] | undefined

        const error = await limited.replaceFromDatabase(database, 4, [], values).catch((caught) => caught)

        expect(error).toBeInstanceOf(PayloadTooLargeError)
        expect(error.kind).toBe(kind)
        expect(error.byteLength).toBeGreaterThan(requestLimit)
        const commands = mocks.invoke.mock.calls.map(([sent]) => sent)
        expect(commands).not.toContain(command)
        expect(commands).not.toContain('pds_replace_commit')
        expect(commands.at(-1)).toBe('pds_replace_abort')
    })
})
