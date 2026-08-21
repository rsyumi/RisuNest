import { IDBFactory, IDBIndex, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'
import { persistentDataStoreContract } from './persistentDataStoreContract'

let databaseSequence = 0

persistentDataStoreContract(async () => {
    const indexedDB = new IDBFactory()
    const databaseName = `persistent-store-contract-${databaseSequence++}`
    const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
    await store.open()

    return {
        store,
        async reopen() {
            const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
            await reopened.open()
            return reopened
        },
    }
})

describe('IndexedDbPersistentDataStore I/O shape', () => {
    it('scopes catalog, conversation, and latest-window reads to IndexedDB ranges', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'bounded-read-shape',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await store.replaceFromDatabase(fixtureDatabase)

        const getAllCalls: string[] = []
        const cursorRanges: Array<{ index: string; lower: IDBValidKey | undefined; upper: IDBValidKey | undefined }> = []
        const originalGetAll = IDBObjectStore.prototype.getAll
        const originalOpenCursor = IDBIndex.prototype.openCursor
        const getAllSpy = vi
            .spyOn(IDBObjectStore.prototype, 'getAll')
            .mockImplementation(function (this: IDBObjectStore, ...args: Parameters<IDBObjectStore['getAll']>) {
                getAllCalls.push(this.name)
                return originalGetAll.apply(this, args)
            })
        const cursorSpy = vi
            .spyOn(IDBIndex.prototype, 'openCursor')
            .mockImplementation(function (this: IDBIndex, ...args: Parameters<IDBIndex['openCursor']>) {
                const range = args[0] instanceof IDBKeyRange ? args[0] : undefined
                cursorRanges.push({ index: this.name, lower: range?.lower, upper: range?.upper })
                return originalOpenCursor.apply(this, args)
            })

        try {
            await store.queryCharacters({ order: 'configured', trash: false, limit: 2 })
            await store.queryConversations({
                characterId: 'char-a',
                order: 'configured',
                limit: 2,
            })
            await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                limit: 4,
            })
        } finally {
            getAllSpy.mockRestore()
            cursorSpy.mockRestore()
        }

        expect(getAllCalls).toEqual([])
        expect(cursorRanges).toContainEqual({
            index: 'byConversationPage',
            lower: ['revision-1', 'char-a', 'conv-long', 0],
            upper: ['revision-1', 'char-a', 'conv-long', 1],
        })
    })

    it('rewrites only the affected page for an equal-length range edit', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'bounded-write-shape',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(fixtureDatabase)

        const messagePageIo = { getAll: 0, put: 0, delete: 0 }
        const originalGetAll = IDBObjectStore.prototype.getAll
        const originalPut = IDBObjectStore.prototype.put
        const originalDelete = IDBObjectStore.prototype.delete
        const getAllSpy = vi
            .spyOn(IDBObjectStore.prototype, 'getAll')
            .mockImplementation(function (this: IDBObjectStore, ...args: Parameters<IDBObjectStore['getAll']>) {
                if (this.name === 'messagePages') messagePageIo.getAll++
                return originalGetAll.apply(this, args)
            })
        const putSpy = vi
            .spyOn(IDBObjectStore.prototype, 'put')
            .mockImplementation(function (this: IDBObjectStore, ...args: Parameters<IDBObjectStore['put']>) {
                if (this.name === 'messagePages') messagePageIo.put++
                return originalPut.apply(this, args)
            })
        const deleteSpy = vi
            .spyOn(IDBObjectStore.prototype, 'delete')
            .mockImplementation(function (this: IDBObjectStore, ...args: Parameters<IDBObjectStore['delete']>) {
                if (this.name === 'messagePages') messagePageIo.delete++
                return originalDelete.apply(this, args)
            })

        try {
            await store.commit({
                expectedRevision: imported.revision,
                conversations: [
                    {
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'conv-long',
                        start: 5,
                        deleteCount: 1,
                        messages: [{ role: 'char', data: 'edited', chatId: 'msg-edited' }],
                    },
                ],
            })
        } finally {
            getAllSpy.mockRestore()
            putSpy.mockRestore()
            deleteSpy.mockRestore()
        }

        expect(messagePageIo).toEqual({ getAll: 0, put: 1, delete: 0 })
        expect(
            (
                await store.readConversationWindow({
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    anchorMessageId: 'msg-edited',
                    before: 1,
                    after: 1,
                })
            )?.value.messages.map((message) => message.chatId),
        ).toEqual(['msg-004', 'msg-edited', 'msg-006'])
    })

    it('reuses the anchor lookup page when reading an anchored window', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore('bounded-anchor-shape', indexedDB, IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(fixtureDatabase)

        let conversationPageCursors = 0
        const originalOpenCursor = IDBIndex.prototype.openCursor
        const cursorSpy = vi
            .spyOn(IDBIndex.prototype, 'openCursor')
            .mockImplementation(function (this: IDBIndex, ...args: Parameters<IDBIndex['openCursor']>) {
                if (this.name === 'byConversationPage') conversationPageCursors++
                return originalOpenCursor.apply(this, args)
            })
        try {
            await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                anchorMessageId: 'msg-127',
                before: 2,
                after: 1,
            })
        } finally {
            cursorSpy.mockRestore()
        }

        expect(conversationPageCursors).toBe(1)
    })

    it('removes the previous generation after each successful staged replacement', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'staged-generation-cleanup'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(fixtureDatabase)
        const replacement = structuredClone(fixtureDatabase)
        replacement.username = 'Replacement User'
        await store.replaceFromDatabase(replacement)

        const openRequest = indexedDB.open(databaseName)
        const database = await new Promise<IDBDatabase>((resolve, reject) => {
            openRequest.onsuccess = () => resolve(openRequest.result)
            openRequest.onerror = () => reject(openRequest.error)
        })
        const storeNames = ['root', 'catalog', 'characters', 'conversations', 'messagePages']
        const transaction = database.transaction(storeNames, 'readonly')
        const generations = new Map<string, Set<string>>()
        await Promise.all(
            storeNames.map(async (storeName) => {
                const request = transaction.objectStore(storeName).getAll()
                const records = await new Promise<Array<{ generation: string }>>((resolve, reject) => {
                    request.onsuccess = () => resolve(request.result)
                    request.onerror = () => reject(request.error)
                })
                generations.set(storeName, new Set(records.map((record) => record.generation)))
            }),
        )

        expect(generations).toEqual(
            new Map(storeNames.map((storeName) => [storeName, new Set(['revision-2'])])),
        )
        expect((await store.readRoot()).value.username).toBe('Replacement User')
    })
})
