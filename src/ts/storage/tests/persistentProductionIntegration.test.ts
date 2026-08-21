import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import {
    capturePersistentRoot,
    captureSelectedPersistentCharacter,
    createPersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from '../persistentDataRuntime'
import { decodeRisuSave } from '../risuSave'
import { installPersistentSaveNotifications } from '../persistentSaveNotifications'

vi.mock('../database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No live database in runtime integration tests')
    },
    presetTemplate: {},
}))
vi.mock('../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

function makeDatabase(): Database {
    return {
        username: 'Fixture',
        characters: [
            {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 0,
                chats: [
                    {
                        id: 'chat-a',
                        name: 'First',
                        message: [],
                    },
                ],
            },
        ],
    } as unknown as Database
}

function makeAdapter(database: Database): PersistentDataRuntimeStateAdapter & {
    current(): Database
} {
    let workingCopy = structuredClone(database)
    return {
        current: () => workingCopy,
        captureRoot: () => {
            const { characters: _characters, ...root } = structuredClone(workingCopy)
            return root
        },
        captureSelectedCharacter: () => structuredClone(workingCopy.characters[0] ?? null),
        getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
        replaceDatabase: (replacement) => {
            workingCopy = structuredClone(replacement)
        },
        publishCharacter: (character) => {
            const index = workingCopy.characters.findIndex((item) => item.chaId === character.chaId)
            workingCopy.characters[index] = structuredClone(character)
        },
        publishConversation: (characterId, conversation) => {
            const character = workingCopy.characters.find((item) => item.chaId === characterId)!
            const index = character.chats.findIndex((chat) => chat.id === conversation.id)
            character.chats[index] = structuredClone(conversation)
            character.chatPage = index
        },
    }
}

function makeStore(name: string) {
    return new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
}

describe('persistent production runtime', () => {
    it('captures root and the selected character without traversing inactive characters', () => {
        const database = makeDatabase()
        const inactive = structuredClone(database.characters[0])
        Object.defineProperty(inactive, 'chats', {
            enumerable: true,
            get: () => {
                throw new Error('inactive character was traversed')
            },
        })
        database.characters.push(inactive)

        expect(capturePersistentRoot(database).username).toBe('Fixture')
        expect(captureSelectedPersistentCharacter(database, 0)?.chaId).toBe('char-a')
    })

    it('commits ordinary root and selected-character edits without a legacy writer', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const revisions: number[] = []
        const legacyWriter = vi.fn()
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            onLocalRevision: (revision) => revisions.push(revision),
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)

        adapter.current().username = 'Changed root'
        adapter.current().characters[0].name = 'Changed character'
        runtime.markPersistentDataDirty(64)
        await runtime.flushPendingData('test')

        const reopened = await store.materializeDatabase(runtime.revision)
        expect(reopened.username).toBe('Changed root')
        expect(reopened.characters[0].name).toBe('Changed character')
        expect(revisions).toEqual([2])
        expect(legacyWriter).not.toHaveBeenCalled()
    })

    it('retries one exact pinned official snapshot and disposes it after success', async () => {
        const database = makeDatabase()
        const store = makeStore(`publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const writes: Uint8Array[] = []
        const officialStorage = {
            setItem: vi.fn(async (_key: string, bytes: Uint8Array) => {
                writes.push(bytes)
                if (writes.length === 1) throw new Error('offline')
            }),
        }
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            officialStorage,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        adapter.current().username = 'Published'
        runtime.markPersistentDataDirty(64)

        await expect(runtime.flushPendingData('first')).rejects.toThrow('offline')
        await runtime.flushPendingData('retry')

        expect(writes).toHaveLength(2)
        expect(writes[1]).toBe(writes[0])
        expect((await decodeRisuSave(writes[0])).username).toBe('Published')
        expect(runtime.revision).toBe(2)
    })

    it('uses the official account storage selected after local initialization', async () => {
        const database = makeDatabase()
        const store = makeStore(`late-publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const setItem = vi.fn(async (_key: string, _bytes: Uint8Array) => undefined)
        let officialStorage: { setItem: typeof setItem } | null = null
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            getOfficialStorage: () => officialStorage,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        officialStorage = { setItem }
        adapter.current().username = 'Account enabled'
        runtime.markPersistentDataDirty(64)

        await runtime.flushPendingData('account-enabled')

        expect(setItem).toHaveBeenCalledOnce()
        expect(setItem.mock.calls[0][0]).toBe('database/database.bin')
    })

    it('broadcasts successful revisions and warns only once for foreign sessions', async () => {
        const posted: string[] = []
        let onmessage: ((event: MessageEvent) => void) | null = null
        let callbacks: {
            onLocalRevision?: (revision: number) => void
            onFlushPromise?: (promise: Promise<void> | null) => void
        } = {}
        const warning = vi.fn()
        const savingStates: boolean[] = []
        const dispose = installPersistentSaveNotifications({
            sessionId: 'local-session',
            channel: {
                postMessage: (value) => posted.push(value as string),
                close: vi.fn(),
                get onmessage() {
                    return onmessage
                },
                set onmessage(value) {
                    onmessage = value
                },
            },
            configureRuntime: (next) => {
                callbacks = next
            },
            showForeignRevisionWarning: warning,
            setSaving: (value) => savingStates.push(value),
        })

        callbacks.onLocalRevision?.(2)
        onmessage?.({ data: 'local-session' } as MessageEvent)
        onmessage?.({ data: 'foreign-a' } as MessageEvent)
        onmessage?.({ data: 'foreign-b' } as MessageEvent)
        callbacks.onFlushPromise?.(Promise.resolve())
        callbacks.onFlushPromise?.(null)
        dispose()

        expect(posted).toEqual(['local-session'])
        expect(warning).toHaveBeenCalledOnce()
        expect(savingStates).toEqual([true, false])
    })
})
