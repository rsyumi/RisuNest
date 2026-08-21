import { IDBFactory, IDBIndex, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { SnapshotReleasedError } from '../persistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'
import { persistentDataStoreContract } from './persistentDataStoreContract'

let databaseSequence = 0

async function createVersion1Database(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    const databaseValue = structuredClone(fixtureDatabase)
    databaseValue.characters[1].chats[0].lastDate = 250
    databaseValue.characters[1].chats[1].lastDate = 350
    const generation = 'revision-7'
    const openRequest = indexedDB.open(databaseName, 1)
    openRequest.onupgradeneeded = () => {
        for (const storeName of [
            'meta',
            'root',
            'catalog',
            'characters',
            'conversations',
            'messagePages',
        ]) {
            openRequest.result.createObjectStore(storeName, { keyPath: 'key' })
        }
    }
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        openRequest.onsuccess = () => resolve(openRequest.result)
        openRequest.onerror = () => reject(openRequest.error)
    })
    const transaction = database.transaction(
        ['meta', 'root', 'catalog', 'characters', 'conversations', 'messagePages'],
        'readwrite',
    )
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 1 })
    transaction.objectStore('meta').put({ key: 'activeGeneration', value: generation })
    transaction.objectStore('meta').put({ key: 'currentRevision', value: 7 })
    const { characters, ...root } = databaseValue
    transaction.objectStore('root').put({ key: generation, generation, value: root })
    for (let configuredIndex = 0; configuredIndex < characters.length; configuredIndex++) {
        const character = characters[configuredIndex]
        const { chats, ...detail } = character
        const summary = {
            id: character.chaId,
            name: character.name,
            image: character.image,
            configuredIndex,
            recentAt: character.lastInteraction ?? 0,
            trashed: character.trashTime !== undefined,
            conversationCount: chats.length,
        }
        transaction.objectStore('catalog').put({
            key: `${generation}:character:${character.chaId}`,
            generation,
            value: summary,
        })
        transaction.objectStore('characters').put({
            key: `${generation}:character:${character.chaId}`,
            generation,
            value: detail,
        })
        for (let conversationIndex = 0; conversationIndex < chats.length; conversationIndex++) {
            const conversation = chats[conversationIndex]
            const { message, ...conversationDetail } = conversation
            const conversationSummary = {
                id: conversation.id!,
                characterId: character.chaId,
                name: conversation.name,
                configuredIndex: conversationIndex,
                recentAt: conversation.lastDate ?? message.at(-1)?.time ?? 0,
                messageCount: message.length,
            }
            transaction.objectStore('conversations').put({
                key: `${generation}:conversation:${character.chaId}:${conversation.id}`,
                generation,
                value: { summary: conversationSummary, detail: conversationDetail },
            })
            for (let offset = 0; offset < message.length; offset += 128) {
                const pageIndex = offset / 128
                transaction.objectStore('messagePages').put({
                    key: `${generation}:message-page:${character.chaId}:${conversation.id}:${pageIndex}`,
                    generation,
                    characterId: character.chaId,
                    conversationId: conversation.id,
                    pageIndex,
                    value: message.slice(offset, offset + 128),
                })
            }
        }
    }
    await new Promise<void>((resolve, reject) => {
        transaction.oncomplete = () => resolve()
        transaction.onabort = () => reject(transaction.error)
        transaction.onerror = () => reject(transaction.error)
    })
    database.close()
}

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
    it('atomically adds one complete character with root and selected edits across reopen', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `atomic-character-addition-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
        const { characters: _characters, ...root } = structuredClone(fixtureDatabase)
        root.username = 'Root changed with addition'
        const selected = structuredClone(fixtureDatabase.characters[0])
        selected.name = 'Selected changed with addition'
        const added = structuredClone(fixtureDatabase.characters[1])
        added.chaId = 'char-added'
        added.name = 'Added character'
        added.chats.forEach((chat, index) => {
            chat.id = `added-chat-${index}`
        })

        const committed = await store.commit({
            expectedRevision: imported.revision,
            root,
            replaceCharacter: selected,
            addCharacter: added,
        })

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()
        const materialized = await reopened.materializeDatabase(committed.revision)
        expect(committed.revision).toBe(imported.revision + 1)
        expect(materialized.username).toBe('Root changed with addition')
        expect(materialized.characters.map((character) => character.chaId)).toEqual([
            'char-b',
            'char-a',
            'char-c',
            'char-added',
        ])
        expect(materialized.characters[0].name).toBe('Selected changed with addition')
        expect(materialized.characters[3]).toEqual(added)
    })

    it('appends a character after the maximum configured index when catalog indices have gaps', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `character-addition-index-gap-${databaseSequence++}`
        await createVersion1Database(indexedDB, databaseName)
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const database = await new Promise<IDBDatabase>((resolve, reject) => {
            const request = indexedDB.open(databaseName)
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })
        const transaction = database.transaction('catalog', 'readwrite')
        const recordRequest = transaction.objectStore('catalog').get('revision-7:character:char-c')
        const record = await new Promise<Record<string, unknown>>((resolve, reject) => {
            recordRequest.onsuccess = () => resolve(recordRequest.result)
            recordRequest.onerror = () => reject(recordRequest.error)
        })
        record.configuredIndex = 8
        ;(record.value as { configuredIndex: number }).configuredIndex = 8
        transaction.objectStore('catalog').put(record)
        await new Promise<void>((resolve, reject) => {
            transaction.oncomplete = () => resolve()
            transaction.onabort = () => reject(transaction.error)
            transaction.onerror = () => reject(transaction.error)
        })
        database.close()

        const added = structuredClone(fixtureDatabase.characters[1])
        added.chaId = 'char-added'
        added.chats.forEach((chat, index) => {
            chat.id = `added-chat-${index}`
        })
        await store.commit({ expectedRevision: 7, addCharacter: added })

        const page = await store.queryCharacters({ order: 'configured', trash: false, limit: 20 })
        expect(page.items.find((item) => item.id === 'char-added')?.configuredIndex).toBe(9)
    })

    it.each([
        ['duplicate character ID', (character: typeof fixtureDatabase.characters[number]) => {
            character.chaId = 'char-a'
        }],
        ['empty character ID', (character: typeof fixtureDatabase.characters[number]) => {
            character.chaId = ''
        }],
        ['empty chat ID', (character: typeof fixtureDatabase.characters[number]) => {
            character.chaId = 'char-added'
            character.chats[0].id = ''
        }],
        ['duplicate chat ID', (character: typeof fixtureDatabase.characters[number]) => {
            character.chaId = 'char-added'
            character.chats[1].id = character.chats[0].id
        }],
    ])('rolls back a character addition with a %s across reopen', async (_case, mutate) => {
        const indexedDB = new IDBFactory()
        const databaseName = `invalid-character-addition-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
        const added = structuredClone(fixtureDatabase.characters[1])
        mutate(added)

        await expect(store.commit({
            expectedRevision: imported.revision,
            addCharacter: added,
        })).rejects.toThrow()

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()
        expect((await reopened.readRoot()).revision).toBe(imported.revision)
        expect(await reopened.materializeDatabase()).toEqual(fixtureDatabase)
    })

    it('upgrades version 1 records, backfills ordering, commits, and reopens', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'version-1-upgrade'
        await createVersion1Database(indexedDB, databaseName)

        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()

        expect(await store.readRoot()).toMatchObject({
            revision: 7,
            value: { username: 'Fixture User' },
        })
        expect(
            (await store.queryCharacters({ order: 'configured', trash: false, limit: 10 })).items.map(
                (item) => item.id,
            ),
        ).toEqual(['char-b', 'char-a'])
        expect(
            (await store.queryCharacters({ order: 'recent', trash: false, limit: 10 })).items.map(
                (item) => item.id,
            ),
        ).toEqual(['char-a', 'char-b'])
        expect(
            (
                await store.queryConversations({
                    characterId: 'char-a',
                    order: 'configured',
                    limit: 10,
                })
            ).items.map((item) => item.id),
        ).toEqual(['conv-long', 'conv-short'])
        expect(
            (
                await store.queryConversations({
                    characterId: 'char-a',
                    order: 'recent',
                    limit: 10,
                })
            ).items.map((item) => item.id),
        ).toEqual(['conv-short', 'conv-long'])
        expect(
            (
                await store.readConversationWindow({
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    limit: 4,
                })
            )?.value.messages.map((message) => message.chatId),
        ).toEqual(['msg-126', 'msg-127', 'msg-128', 'msg-129'])

        const detail = (await store.readCharacter('char-a'))!.value
        const committed = await store.commit({
            expectedRevision: 7,
            character: { ...detail, name: 'Alpha Upgraded' },
        })
        expect(committed.revision).toBe(8)

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()
        expect(await reopened.readCharacter('char-a')).toMatchObject({
            revision: 8,
            value: { name: 'Alpha Upgraded' },
        })
        expect(
            (await reopened.queryCharacters({ order: 'configured', trash: false, limit: 10 })).items.map(
                (item) => item.id,
            ),
        ).toEqual(['char-b', 'char-a'])
    })

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

    it('scopes selected-character replacement deletes to that character', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'selected-character-write-shape',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(fixtureDatabase)
        const replacement = structuredClone(fixtureDatabase.characters[1])
        replacement.chats[0].message = replacement.chats[0].message.slice(0, 1)
        const cursorRanges: Array<{
            store: string
            index: string
            lower: IDBValidKey | undefined
            upper: IDBValidKey | undefined
            direction: IDBCursorDirection | undefined
        }> = []
        const objectStoreIo: Array<{ store: string; operation: 'openCursor' | 'clear' }> = []
        const originalOpenCursor = IDBIndex.prototype.openCursor
        const originalObjectStoreOpenCursor = IDBObjectStore.prototype.openCursor
        const originalClear = IDBObjectStore.prototype.clear
        const cursorSpy = vi
            .spyOn(IDBIndex.prototype, 'openCursor')
            .mockImplementation(function (
                this: IDBIndex,
                ...args: Parameters<IDBIndex['openCursor']>
            ) {
                const range = args[0] instanceof IDBKeyRange ? args[0] : undefined
                cursorRanges.push({
                    store: this.objectStore.name,
                    index: this.name,
                    lower: range?.lower,
                    upper: range?.upper,
                    direction: args[1],
                })
                return originalOpenCursor.apply(this, args)
            })
        const objectStoreCursorSpy = vi
            .spyOn(IDBObjectStore.prototype, 'openCursor')
            .mockImplementation(function (
                this: IDBObjectStore,
                ...args: Parameters<IDBObjectStore['openCursor']>
            ) {
                if (this.name === 'conversations' || this.name === 'messagePages') {
                    objectStoreIo.push({ store: this.name, operation: 'openCursor' })
                }
                return originalObjectStoreOpenCursor.apply(this, args)
            })
        const clearSpy = vi
            .spyOn(IDBObjectStore.prototype, 'clear')
            .mockImplementation(function (this: IDBObjectStore) {
                if (this.name === 'conversations' || this.name === 'messagePages') {
                    objectStoreIo.push({ store: this.name, operation: 'clear' })
                }
                return originalClear.apply(this)
            })

        try {
            await store.commit({
                expectedRevision: imported.revision,
                replaceCharacter: replacement,
            })
        } finally {
            cursorSpy.mockRestore()
            objectStoreCursorSpy.mockRestore()
            clearSpy.mockRestore()
        }

        expect(cursorRanges).toEqual([
            {
                store: 'conversations',
                index: 'byGenerationCharacterConfigured',
                lower: ['revision-1', 'char-a', 0],
                upper: ['revision-1', 'char-a', Number.MAX_SAFE_INTEGER],
                direction: undefined,
            },
            {
                store: 'messagePages',
                index: 'byGenerationCharacter',
                lower: ['revision-1', 'char-a'],
                upper: ['revision-1', 'char-a'],
                direction: undefined,
            },
        ])
        expect(objectStoreIo).toEqual([])
        expect((await store.readConversation('char-b', 'conv-beta'))?.value.message).toHaveLength(3)

        const openRequest = indexedDB.open('selected-character-write-shape')
        const database = await new Promise<IDBDatabase>((resolve, reject) => {
            openRequest.onsuccess = () => resolve(openRequest.result)
            openRequest.onerror = () => reject(openRequest.error)
        })
        const transaction = database.transaction('messagePages', 'readonly')
        const pageCountRequest = transaction
            .objectStore('messagePages')
            .index('byConversationPage')
            .count(
                IDBKeyRange.bound(
                    ['revision-1', 'char-a', 'conv-long', 0],
                    ['revision-1', 'char-a', 'conv-long', Number.MAX_SAFE_INTEGER],
                ),
            )
        const pageCount = await new Promise<number>((resolve, reject) => {
            pageCountRequest.onsuccess = () => resolve(pageCountRequest.result)
            pageCountRequest.onerror = () => reject(pageCountRequest.error)
        })
        expect(pageCount).toBe(1)
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

    it('copies an immutable revision with cursors and releases it idempotently', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore('revision-snapshot-lease', indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(fixtureDatabase)
        const getAllSpy = vi.spyOn(IDBObjectStore.prototype, 'getAll')
        const materializeSpy = vi.spyOn(store, 'materializeDatabase')

        const lease = await store.acquireRevision(imported.revision)

        expect(getAllSpy).not.toHaveBeenCalled()
        expect(materializeSpy).not.toHaveBeenCalled()
        getAllSpy.mockRestore()
        materializeSpy.mockRestore()

        const root = (await store.readRoot()).value
        await store.commit({
            expectedRevision: imported.revision,
            root: { ...root, username: 'Committed Later' },
        })
        expect((await lease.readRoot()).value.username).toBe('Fixture User')
        expect(
            await lease.queryCharacters({ order: 'configured', trash: false, limit: 1 }),
        ).toHaveProperty('revision', imported.revision)
        expect(
            await lease.queryConversations({
                characterId: 'char-a',
                order: 'configured',
                limit: 1,
            }),
        ).toHaveProperty('revision', imported.revision)
        expect((await lease.readConversation('char-a', 'conv-short'))?.value.message).toHaveLength(2)

        await lease.release()
        await lease.release()
        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(
            lease.queryCharacters({ order: 'configured', trash: false, limit: 1 }),
        ).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readCharacter('char-a')).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(
            lease.queryConversations({ characterId: 'char-a', order: 'configured', limit: 1 }),
        ).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readConversation('char-a', 'conv-short')).rejects.toBeInstanceOf(
            SnapshotReleasedError,
        )
        await expect(
            lease.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-short',
                limit: 1,
            }),
        ).rejects.toBeInstanceOf(SnapshotReleasedError)
    })

    it('sweeps inactive temporary snapshot generations on open', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'orphaned-revision-snapshot'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(fixtureDatabase)

        const openRequest = indexedDB.open(databaseName)
        const database = await new Promise<IDBDatabase>((resolve, reject) => {
            openRequest.onsuccess = () => resolve(openRequest.result)
            openRequest.onerror = () => reject(openRequest.error)
        })
        const transaction = database.transaction('root', 'readwrite')
        transaction.objectStore('root').put({
            key: 'snapshot-orphaned',
            generation: 'snapshot-orphaned',
            value: { username: 'Orphaned' },
        })
        await new Promise<void>((resolve, reject) => {
            transaction.oncomplete = () => resolve()
            transaction.onerror = () => reject(transaction.error)
            transaction.onabort = () => reject(transaction.error)
        })
        database.close()

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()

        const verifyRequest = indexedDB.open(databaseName)
        const verifyDatabase = await new Promise<IDBDatabase>((resolve, reject) => {
            verifyRequest.onsuccess = () => resolve(verifyRequest.result)
            verifyRequest.onerror = () => reject(verifyRequest.error)
        })
        const verifyTransaction = verifyDatabase.transaction('root', 'readonly')
        const orphan = await new Promise<unknown>((resolve, reject) => {
            const request = verifyTransaction.objectStore('root').get('snapshot-orphaned')
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })
        expect(orphan).toBeUndefined()
    })
})
