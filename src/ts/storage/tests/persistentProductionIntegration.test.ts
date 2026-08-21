import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import {
    createPersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from '../persistentDataRuntime'
import { decodeRisuSave } from '../risuSave'

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
})
