import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from 'src/ts/storage/database.svelte'
import type { PersistentDataStore, PluginStorageCatalog, PluginStorageMutation } from 'src/ts/storage/persistentDataStore'
import { IndexedDbPersistentDataStore } from 'src/ts/storage/indexedDbPersistentDataStore'
import { createMutationGatedPersistentDataStore } from 'src/ts/storage/mutationGatedPersistentDataStore'
import { createStorageMutationGate, createInRealmStorageLockManager } from 'src/ts/storage/storageMutationGate'
import { createPluginStorageStore } from 'src/ts/plugins/pluginStorageStore'
import { UNOWNED_PLUGIN_OWNER } from 'src/ts/plugins/pluginOwner'
import {
    createPersistentDataRuntime,
    capturePersistentRoot,
    capturePersistentPluginStorage,
    capturePersistentPresets,
    type PersistentDataRuntimeStateAdapter,
} from 'src/ts/storage/persistentDataRuntime'
import { createServerSyncFacade } from 'src/ts/storage/sync/serverSync'
import { SaveCoordinator } from 'src/ts/storage/saveCoordinator'
import {
    NativeCommitTransport, LARGE_COMMIT_BYTES,
    type SharedWebview, type CommitEnvelope,
} from 'src/ts/storage/nativeCommitTransport'

vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => { throw new Error('No global database in isolated regression') },
    presetTemplate: {},
}))
vi.mock('src/ts/globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isTauri: false, isNodeServer: false }))

function database(username = 'Initial'): Database {
    return {
        username,
        characters: [{
            type: 'character', chaId: 'synthetic-a', name: 'Synthetic A', chatPage: 0,
            chats: [{ id: 'synthetic-chat', name: 'First', message: [] }],
        }],
        botPresets: [],
        pluginCustomStorage: {},
    } as unknown as Database
}

async function freshStore() {
    const store = new IndexedDbPersistentDataStore(`boundary-probe-${crypto.randomUUID()}`)
    await store.open()
    return store
}

function stateAdapter(initial: Database, targeted = false): PersistentDataRuntimeStateAdapter & { current(): Database } {
    let current = structuredClone(initial)
    return {
        current: () => current,
        captureRoot: () => capturePersistentRoot(current),
        capturePluginStorage: () => capturePersistentPluginStorage(current),
        capturePresets: () => capturePersistentPresets(current),
        captureSelectedCharacter: () => current.characters[0] ?? null,
        captureCharacter: (id) => current.characters.find((item) => item.chaId === id) ?? null,
        getSelectedCharacterId: () => current.characters[0]?.chaId ?? null,
        getSelectedConversationId: () => current.characters[0]?.chats[0]?.id ?? null,
        replaceDatabase: (value) => { current = targeted ? value : structuredClone(value) },
        ...(targeted ? { captureWorkingSetDatabase: () => current } : {}),
        publishCharacter: (character) => {
            const index = current.characters.findIndex((item) => item.chaId === character.chaId)
            current.characters[index] = structuredClone(character)
        },
        publishConversation: (characterId, conversation) => {
            const character = current.characters.find((item) => item.chaId === characterId)!
            character.chats[character.chatPage ?? 0] = structuredClone(conversation)
        },
    }
}

describe('persistence boundary regressions', () => {
    it('acknowledges a confirmed native commit and the next edit when releasing the JS buffer throws', async () => {
        const raw = await freshStore()
        await raw.replaceFromDatabase(database(), 0)
        let input!: CommitEnvelope
        let listener!: Parameters<SharedWebview['addEventListener']>[1]
        const buffer = new ArrayBuffer(4)
        const webview: SharedWebview = {
            addEventListener: (_event, callback) => { listener = callback },
            removeEventListener: () => undefined,
            releaseBuffer: () => { throw new Error('Synthetic detached buffer') },
        }
        const transport = new NativeCommitTransport({
            windows: () => true,
            shared: () => webview,
            encode: async () => new Uint8Array([1, 2, 3, 4]),
            invoke: (async (command: string, args: any) => {
                if (command === 'pds_commit_shared_open') {
                    listener({
                        additionalData: { kind: 'pds-commit', requestId: args.requestId, id: 'synthetic-buffer' },
                        getBuffer: () => buffer,
                    })
                    return { id: 'synthetic-buffer', capacity: 4 }
                }
                if (command === 'pds_commit_shared_chunk') return args.offset + args.length
                if (command === 'pds_commit_shared_finish') return raw.commit(input.commit)
                if (command === 'pds_commit_shared_cancel') return undefined
                throw new Error(`Unexpected synthetic command ${command}`)
            }) as never,
        })
        const root = { username: 'Initial' }
        const coordinator = new SaveCoordinator({
            store: {
                commit: (commit: CommitEnvelope['commit']) => {
                    input = { commit, assetAliases: [] }
                    return transport.commit(input)
                },
            } as unknown as PersistentDataStore,
            captureRoot: () => root as never,
            captureSelectedCharacter: () => null,
            captureCharacter: () => null,
            replaceDatabase: () => undefined,
        })
        coordinator.initialize(1)
        root.username = 'x'.repeat(LARGE_COMMIT_BYTES + 1)
        let failure: unknown = null
        try { await coordinator.flushPendingData('native-ack-probe') }
        catch (error) { failure = error }
        const persisted = await raw.readRoot()
        expect(persisted.revision).toBe(2)
        expect(persisted.value.username).toBe(root.username)
        expect({ failed: failure !== null, acknowledgedRevision: coordinator.revision })
            .toEqual({ failed: false, acknowledgedRevision: 2 })
        root.username = 'y'.repeat(LARGE_COMMIT_BYTES + 1)
        await coordinator.flushPendingData('next-edit')
        expect(coordinator.revision).toBe(3)
        expect((await raw.readRoot()).value.username).toBe(root.username)
    })

    it('retains both owner-scoped values through the production mutation-gate wrapper', async () => {
        const raw = await freshStore()
        const values = [
            { owner: 'synthetic-plugin-a', key: 'shared', value: 'value-a' },
            { owner: 'synthetic-plugin-b', key: 'shared', value: 'value-b' },
            { owner: 'synthetic-plugin-a', key: 'unique', value: 'unique-value' },
            { owner: UNOWNED_PLUGIN_OWNER, key: 'unowned', value: 'unowned-value' },
        ]
        await raw.replaceFromDatabase(database(), 0, [], values)
        expect((await raw.queryPluginStorage()).items).toHaveLength(4)
        const gated = createMutationGatedPersistentDataStore(raw, createStorageMutationGate({
            locks: createInRealmStorageLockManager(),
        }))
        const candidate = await raw.materializeDatabase(1)
        await gated.replaceFromDatabase(candidate, 1, [], values)
        const actual = await Promise.all(values.map(async ({ owner, key }) => ({
            owner, key, value: (await raw.readPluginStorage(owner, key))?.value,
        })))
        expect(actual).toEqual(values)
        await gated.replaceFromDatabase(candidate, 2, [], [])
        expect((await raw.queryPluginStorage()).items).toEqual([])
    })

    it.each([
        ['set', false], ['delete', false], ['clear', false],
        ['set', true], ['delete', true], ['clear', true],
    ] as const)('rejects a stale plugin catalog for %s, reinitializing=%s', async (type, reinitializing) => {
        const store = await freshStore()
        await store.replaceFromDatabase(database(), 0, [], [
            { owner: 'synthetic-plugin', key: 'old-key', value: 'old-value' },
            { owner: 'other-plugin', key: 'old-key', value: 'other-value' },
        ])
        let resolveCatalog!: (catalog: PluginStorageCatalog) => void
        const oldCatalog = new Promise<PluginStorageCatalog>((resolve) => { resolveCatalog = resolve })
        const storage = createPluginStorageStore({
            store,
            mutate: async () => undefined,
            getStorageAuthorityEpoch: () => 0,
            assertPersistentMutationAllowed: () => undefined,
        })
        const owner = storage.forOwner('synthetic-plugin')
        if (reinitializing) {
            expect(await owner.getItem('old-key')).toBe('old-value')
            storage.invalidateOwner('synthetic-plugin')
        }
        const staleCatalog = await store.queryPluginStorage()
        const queryPluginStorage = vi.spyOn(store, 'queryPluginStorage').mockReturnValueOnce(oldCatalog)
        const loading = owner.keys()
        await vi.waitFor(() => expect(queryPluginStorage).toHaveBeenCalledOnce())
        const mutation: PluginStorageMutation = type === 'set'
            ? { type, owner: 'synthetic-plugin', key: 'new-key', value: 'committed-value' }
            : type === 'delete'
              ? { type, owner: 'synthetic-plugin', key: 'old-key' }
              : { type, owner: 'synthetic-plugin' }
        await store.commit({ expectedRevision: 1, pluginStorage: [mutation] })
        storage.synchronizeCommittedMutation(mutation)
        resolveCatalog(staleCatalog)
        await loading
        const expected = type === 'set' ? { 'old-key': 'old-value', 'new-key': 'committed-value' } : {}
        expect(await owner.snapshot()).toEqual(expected)
        expect(await owner.keys()).toEqual(Object.keys(expected))
        expect(await owner.getItem('old-key')).toBe(type === 'set' ? 'old-value' : null)
        expect(await owner.getItem('new-key')).toBe(type === 'set' ? 'committed-value' : null)
        const other = storage.forOwner('other-plugin')
        expect(await other.keys()).toEqual(['old-key'])
        expect(await other.getItem('old-key')).toBe('other-value')
    })

    it.each([
        ['full', false], ['full', true], ['targeted', false], ['targeted', true],
    ] as const)('keeps later edits after plugin reload failure with %s projection, autosave=%s', async (mode, autosave) => {
        const initial = database()
        const storeName = `sync-retry-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(storeName)
        await store.open()
        await store.replaceFromDatabase(initial, 0)
        if (mode === 'targeted') {
            const acquire = store.acquireRevision.bind(store)
            vi.spyOn(store, 'acquireRevision').mockImplementation(async (revision) => {
                const lease = await acquire(revision)
                return {
                    ...lease,
                    readWorkingSetChangeWindow: async () => ({ revision, afterRevision: revision }),
                    readWorkingSetChangePage: async () => [],
                }
            })
        }
        const state = stateAdapter(initial, mode === 'targeted')
        const runtime = createPersistentDataRuntime({ store, state, prepareDatabase: async (value) => value })
        await runtime.initializeActiveWorkingSet(initial)
        const head = {
            libraryId: 'synthetic-library', epoch: 'synthetic-epoch', seq: '1',
            headId: 'a'.repeat(64), minRetainedSeq: '0', sections: {},
        }
        const invoke = vi.fn(async (command: string) => {
            if (command === 'server_sync_prepare') return {
                kind: 'ready', preparationId: 'synthetic-preparation', localRevision: 1,
                head, appliedRecords: 1,
            }
            if (command === 'server_sync_activate') {
                return {
                    revision: (await store.replaceFromDatabase(database('Remote committed'), 1)).revision,
                    pluginsChanged: true, devicePluginsChanged: false,
                }
            }
            if (command === 'server_sync_publish') return {
                endpoint: 'https://synthetic.invalid', phase: 'idle', localRevision: 2,
                head, conflictCount: 0, conflicts: [], appliedRecords: 1, proposedRecords: 0,
            }
            throw new Error(`Unexpected synthetic command ${command}`)
        })
        const restorePlugins = vi.fn()
            .mockRejectedValueOnce(new Error('Synthetic plugin reload failure'))
            .mockResolvedValue(undefined)
        const facade = createServerSyncFacade({ runtime, invoke: invoke as never, restorePlugins })
        await expect(facade.cycle()).rejects.toMatchObject({ code: 'committed-refresh-pending' })
        expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
        expect(state.current().username).toBe('Remote committed')
        state.current().username = 'New edit after successful projection'
        state.current().characters[0].name = 'Edited after projection'
        state.current().characters[0].chats[0].message.push({
            role: 'user', data: 'Later local turn', chatId: 'later-turn',
        })
        runtime.markPersistentDataDirty(1)
        if (autosave) {
            await vi.waitFor(async () => {
                expect((await store.readRoot()).value.username).toBe('New edit after successful projection')
            }, { timeout: 3000 })
        }
        await facade.cycle()
        const afterRetry = state.current().username
        await runtime.flushPendingData('probe-cleanup')
        expect({ live: afterRetry, durable: (await store.readRoot()).value.username }).toEqual({
            live: 'New edit after successful projection',
            durable: 'New edit after successful projection',
        })
        const reopened = new IndexedDbPersistentDataStore(storeName)
        await reopened.open()
        const durable = await reopened.materializeDatabase()
        expect(durable.username).toBe('New edit after successful projection')
        expect(state.current().characters[0].name).toBe('Edited after projection')
        expect(durable.characters[0].name).toBe('Edited after projection')
        expect(durable.characters[0].chats[0].message).toEqual(state.current().characters[0].chats[0].message)
        expect(durable.characters[0].chats[0].message.at(-1)?.data).toBe('Later local turn')
        expect(invoke.mock.calls.filter(([command]) => command === 'server_sync_activate')).toHaveLength(1)
    })
})
