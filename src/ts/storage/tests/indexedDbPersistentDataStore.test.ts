import { IDBFactory, IDBIndex, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import {
    RevisionConflictError,
    SnapshotReleasedError,
    type AssetOwnerHead,
} from '../persistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'
import { persistentDataStoreContract } from './persistentDataStoreContract'

let databaseSequence = 0

async function openDatabase(indexedDB: IDBFactory, databaseName: string): Promise<IDBDatabase> {
    const request = indexedDB.open(databaseName)
    return new Promise((resolve, reject) => {
        request.onsuccess = () => resolve(request.result)
        request.onerror = () => reject(request.error)
    })
}

async function completeTransaction(transaction: IDBTransaction): Promise<void> {
    return new Promise((resolve, reject) => {
        transaction.oncomplete = () => resolve()
        transaction.onabort = () => reject(transaction.error)
        transaction.onerror = () => reject(transaction.error)
    })
}

async function requestResultForTest<T>(request: IDBRequest<T>): Promise<T> {
    return new Promise((resolve, reject) => {
        request.onsuccess = () => resolve(request.result)
        request.onerror = () => reject(request.error)
    })
}

async function readRawRecord(
    indexedDB: IDBFactory,
    databaseName: string,
    storeName: string,
    key: IDBValidKey,
): Promise<unknown> {
    const database = await openDatabase(indexedDB, databaseName)
    const transaction = database.transaction(storeName, 'readonly')
    const request = transaction.objectStore(storeName).get(key)
    const value = await new Promise((resolve, reject) => {
        request.onsuccess = () => resolve(request.result)
        request.onerror = () => reject(request.error)
    })
    await completeTransaction(transaction)
    database.close()
    return value
}

async function writeRawRecords(
    indexedDB: IDBFactory,
    databaseName: string,
    storeName: string,
    records: Array<Record<string, unknown>>,
): Promise<void> {
    const database = await openDatabase(indexedDB, databaseName)
    const transaction = database.transaction(storeName, 'readwrite')
    for (const record of records) transaction.objectStore(storeName).put(record)
    await completeTransaction(transaction)
    database.close()
}

async function countPersistentDataRecords(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<Record<string, number>> {
    const storeNames = [
        'root',
        'presets',
        'catalog',
        'characters',
        'conversations',
        'messagePages',
        'messageOccurrences',
        'pluginStorage',
        'pluginStorageMetadata',
        'assetAliases',
        'assetOwnerHeads',
    ]
    const database = await openDatabase(indexedDB, databaseName)
    const transaction = database.transaction(storeNames, 'readonly')
    const entries = await Promise.all(
        storeNames.map(async (storeName) => {
            const request = transaction.objectStore(storeName).count()
            const count = await new Promise<number>((resolve, reject) => {
                request.onsuccess = () => resolve(request.result)
                request.onerror = () => reject(request.error)
            })
            return [storeName, count] as const
        }),
    )
    await completeTransaction(transaction)
    database.close()
    return Object.fromEntries(entries)
}

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
    root.pluginCustomStorage = { 'active-memory': { turns: [1, 2, 3] } }
    transaction.objectStore('root').put({
        key: generation,
        generation,
        value: root,
    })
    transaction.objectStore('root').put({
        key: 'revision-legacy',
        generation: 'revision-legacy',
        value: {
            username: 'Legacy generation',
            botPresets: [{ name: 'Legacy preset', image: 'legacy.png' }],
            pluginCustomStorage: { 'old-memory': 'preserved' },
        },
    })
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

async function createVersion5PluginDatabase(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    const generation = 'revision-5'
    const openRequest = indexedDB.open(databaseName, 5)
    openRequest.onupgradeneeded = () => {
        for (const storeName of [
            'meta',
            'root',
            'presets',
            'catalog',
            'characters',
            'conversations',
            'messagePages',
            'pluginStorage',
        ]) {
            openRequest.result.createObjectStore(storeName, { keyPath: 'key' })
        }
        const pluginStorage = openRequest.transaction!.objectStore('pluginStorage')
        pluginStorage.createIndex('byGenerationKey', ['generation', 'storageKey'])
        pluginStorage.createIndex('byGeneration', 'generation')
    }
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        openRequest.onsuccess = () => resolve(openRequest.result)
        openRequest.onerror = () => reject(openRequest.error)
    })
    const transaction = database.transaction(['meta', 'root', 'pluginStorage'], 'readwrite')
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 5 })
    transaction.objectStore('meta').put({ key: 'activeGeneration', value: generation })
    transaction.objectStore('meta').put({ key: 'currentRevision', value: 5 })
    transaction.objectStore('root').put({
        key: generation,
        generation,
        value: { username: 'Version 5' },
    })
    for (const [ordinal, [storageKey, value]] of [
        ['zeta', 'first'],
        ['alpha', 'second'],
    ].entries()) {
        transaction.objectStore('pluginStorage').put({
            key: `${generation}:plugin-storage:${storageKey}`,
            generation,
            storageKey,
            byteSize: JSON.stringify(value).length,
            ordinal,
            value,
        })
    }
    await completeTransaction(transaction)
    database.close()
}

async function createVersion6PluginSnapshotDatabase(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    await createVersion5PluginDatabase(indexedDB, databaseName)
    const openRequest = indexedDB.open(databaseName, 6)
    openRequest.onupgradeneeded = () => {
        const metadata = openRequest.result.createObjectStore('pluginStorageMetadata', {
            keyPath: 'key',
        })
        metadata.createIndex('byGenerationOrdinal', ['generation', 'ordinal'])
        metadata.createIndex('byGeneration', 'generation')
        const values = openRequest.transaction!.objectStore('pluginStorage')
        const cursorRequest = values.openCursor()
        cursorRequest.onsuccess = () => {
            const cursor = cursorRequest.result
            if (!cursor) return
            const { value: _value, ...record } = cursor.value as Record<string, unknown>
            metadata.put(record)
            cursor.continue()
        }
    }
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        openRequest.onsuccess = () => resolve(openRequest.result)
        openRequest.onerror = () => reject(openRequest.error)
    })
    const snapshotGeneration = 'snapshot-5-migrated'
    const transaction = database.transaction(
        ['meta', 'root', 'pluginStorage', 'pluginStorageMetadata'],
        'readwrite',
    )
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 6 })
    transaction.objectStore('meta').put({
        key: `snapshotLease:${snapshotGeneration}`,
        value: snapshotGeneration,
        createdAt: Date.now(),
    })
    transaction.objectStore('root').put({
        key: snapshotGeneration,
        generation: snapshotGeneration,
        value: { username: 'Version 6 snapshot' },
    })
    const metadata = {
        key: `${snapshotGeneration}:plugin-storage:legacy`,
        generation: snapshotGeneration,
        storageKey: 'legacy',
        byteSize: JSON.stringify(0).length,
        ordinal: 0,
    }
    transaction.objectStore('pluginStorage').put({ ...metadata, value: 0 })
    transaction.objectStore('pluginStorageMetadata').put(metadata)
    await completeTransaction(transaction)
    database.close()
}

async function createVersion7Database(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    await createVersion6PluginSnapshotDatabase(indexedDB, databaseName)
    const openRequest = indexedDB.open(databaseName, 7)
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        openRequest.onsuccess = () => resolve(openRequest.result)
        openRequest.onerror = () => reject(openRequest.error)
    })
    const transaction = database.transaction('meta', 'readwrite')
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 7 })
    await completeTransaction(transaction)
    database.close()
}

async function createVersion8Database(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    await createVersion7Database(indexedDB, databaseName)
    const openRequest = indexedDB.open(databaseName, 8)
    openRequest.onupgradeneeded = () => {
        const aliases = openRequest.result.createObjectStore('assetAliases', { keyPath: 'key' })
        aliases.createIndex('byGeneration', 'generation')
    }
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        openRequest.onsuccess = () => resolve(openRequest.result)
        openRequest.onerror = () => reject(openRequest.error)
    })
    const transaction = database.transaction('meta', 'readwrite')
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 8 })
    await completeTransaction(transaction)
    database.close()
}

async function createVersion9AliasDatabase(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    await createVersion8Database(indexedDB, databaseName)
    const openRequest = indexedDB.open(databaseName, 9)
    openRequest.onupgradeneeded = () => {
        const heads = openRequest.result.createObjectStore('assetOwnerHeads', { keyPath: 'key' })
        heads.createIndex('byGeneration', 'generation')
    }
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        openRequest.onsuccess = () => resolve(openRequest.result)
        openRequest.onerror = () => reject(openRequest.error)
    })
    const transaction = database.transaction(['meta', 'assetAliases'], 'readwrite')
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 9 })
    transaction.objectStore('assetAliases').put({
        key: 'revision-5:asset-alias:shared/migrated.bin',
        generation: 'revision-5',
        value: {
            key: 'shared/migrated.bin',
            objectHash: '61'.repeat(32),
            kind: 'asset',
            size: 6,
            mime: 'application/octet-stream',
            name: 'Migrated asset',
            ext: 'bin',
        },
    })
    transaction.objectStore('assetAliases').put({
        key: 'revision-5:asset-alias:0',
        generation: 'revision-5',
        value: {
            key: '0',
            objectHash: '62'.repeat(32),
            kind: 'asset',
            size: 1,
            mime: 'application/octet-stream',
            name: 'Collision asset',
            ext: 'bin',
        },
    })
    transaction.objectStore('assetAliases').put({
        key: 'revision-5:asset-alias:asset:0',
        generation: 'revision-5',
        value: {
            key: 'asset:0',
            objectHash: '63'.repeat(32),
            kind: 'inlay',
            size: 1,
            mime: 'image/webp',
            name: 'Collision inlay',
            ext: 'webp',
            inlayType: 'image',
        },
    })
    await completeTransaction(transaction)
    database.close()
}

async function createVersion11ColdlessDatabase(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    await createVersion7Database(indexedDB, databaseName)
    const openRequest = indexedDB.open(databaseName, 11)
    openRequest.onupgradeneeded = () => {
        const aliases = openRequest.result.createObjectStore('assetAliases', { keyPath: 'key' })
        aliases.createIndex('byGeneration', 'generation')
        aliases.createIndex('byGenerationKindKey', ['generation', 'value.kind', 'value.key'])
        const heads = openRequest.result.createObjectStore('assetOwnerHeads', { keyPath: 'key' })
        heads.createIndex('byGeneration', 'generation')
        const authority = openRequest.result.createObjectStore('assetRepositoryAuthority', {
            keyPath: 'key',
        })
        authority.createIndex('byGeneration', 'generation')
        authority.put({
            key: 'revision-5',
            generation: 'revision-5',
            value: { format: 'legacy' },
        })
        authority.put({
            key: 'snapshot-5-migrated',
            generation: 'snapshot-5-migrated',
            value: { format: 'legacy' },
        })
    }
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        openRequest.onsuccess = () => resolve(openRequest.result)
        openRequest.onerror = () => reject(openRequest.error)
    })
    const transaction = database.transaction('meta', 'readwrite')
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 11 })
    await completeTransaction(transaction)
    database.close()
}

async function createVersion14ColonKeyDatabase(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    const openRequest = indexedDB.open(databaseName, 14)
    openRequest.onupgradeneeded = () => {
        for (const storeName of [
            'meta',
            'root',
            'conversations',
            'messagePages',
            'messageOccurrences',
        ]) {
            openRequest.result.createObjectStore(storeName, { keyPath: 'key' })
        }
    }
    const database = await requestResultForTest(openRequest)
    const generation = 'revision-5'
    const transaction = database.transaction(
        ['meta', 'root', 'conversations', 'messagePages', 'messageOccurrences'],
        'readwrite',
    )
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 14 })
    transaction.objectStore('meta').put({ key: 'activeGeneration', value: generation })
    transaction.objectStore('meta').put({ key: 'currentRevision', value: 5 })
    transaction.objectStore('root').put({ key: generation, generation, value: {} })
    // Written by version 14 with the raw ':'-joined key layout.
    transaction.objectStore('conversations').put({
        key: `${generation}:conversation:char:x:conv`,
        generation,
        configuredIndex: 0,
        recentSortValue: -100,
        value: {
            summary: {
                id: 'conv',
                characterId: 'char:x',
                name: 'Colon chat',
                configuredIndex: 0,
                recentAt: 100,
                messageCount: 1,
            },
            detail: { id: 'conv', name: 'Colon chat', note: '', localLore: [] },
        },
    })
    transaction.objectStore('conversations').put({
        key: `${generation}:conversation:plain:conv`,
        generation,
        configuredIndex: 1,
        recentSortValue: -90,
        value: {
            summary: {
                id: 'conv',
                characterId: 'plain',
                name: 'Plain chat',
                configuredIndex: 1,
                recentAt: 90,
                messageCount: 0,
            },
            detail: { id: 'conv', name: 'Plain chat', note: '', localLore: [] },
        },
    })
    transaction.objectStore('messagePages').put({
        key: `${generation}:message-page:char:x:conv:0`,
        generation,
        characterId: 'char:x',
        conversationId: 'conv',
        pageIndex: 0,
        value: [{ role: 'user', data: 'colon payload', chatId: 'msg:1', time: 100 }],
    })
    transaction.objectStore('messageOccurrences').put({
        key: `${generation}:message-occurrence-page:char:x:conv:0000000000000000`,
        generation,
        characterId: 'char:x',
        conversationId: 'conv',
        pageIndex: 0,
        lookupKeys: [JSON.stringify([generation, 'char:x', 'conv', 'msg:1'])],
    })
    await completeTransaction(transaction)
    database.close()
}

async function createVersion13OccurrenceDatabase(
    indexedDB: IDBFactory,
    databaseName: string,
): Promise<void> {
    const openRequest = indexedDB.open(databaseName, 13)
    openRequest.onupgradeneeded = () => {
        for (const storeName of [
            'meta',
            'root',
            'presets',
            'catalog',
            'characters',
            'conversations',
            'messagePages',
            'messageOccurrences',
            'pluginStorage',
            'pluginStorageMetadata',
            'assetAliases',
            'assetOwnerHeads',
            'assetRepositoryAuthority',
            'coldAliases',
            'coldPayloadAuthority',
        ]) {
            openRequest.result.createObjectStore(storeName, { keyPath: 'key' })
        }
        const occurrences = openRequest.transaction!.objectStore('messageOccurrences')
        occurrences.createIndex(
            'byConversationMessageIndex',
            ['generation', 'characterId', 'conversationId', 'messageId', 'absoluteIndex'],
        )
        occurrences.createIndex(
            'byConversationIndex',
            ['generation', 'characterId', 'conversationId', 'absoluteIndex'],
        )
    }
    const database = await requestResultForTest(openRequest)
    const generation = 'revision-1'
    const transaction = database.transaction(
        ['meta', 'root', 'conversations', 'messagePages', 'messageOccurrences'],
        'readwrite',
    )
    transaction.objectStore('meta').put({ key: 'schemaVersion', value: 13 })
    transaction.objectStore('meta').put({ key: 'activeGeneration', value: generation })
    transaction.objectStore('meta').put({ key: 'currentRevision', value: 1 })
    transaction.objectStore('root').put({ key: generation, generation, value: {} })
    transaction.objectStore('conversations').put({
        key: `${generation}:conversation:char-a:conv-long`,
        generation,
        value: {
            summary: {
                id: 'conv-long',
                characterId: 'char-a',
                name: 'Legacy occurrence chat',
                configuredIndex: 0,
                recentAt: 0,
                messageCount: 3,
            },
            detail: { id: 'conv-long', name: 'Legacy occurrence chat', note: '', localLore: [] },
        },
    })
    const messages = [
        { role: 'user', data: 'first', chatId: 'duplicate' },
        { role: 'char', data: 'middle', chatId: 'middle' },
        { role: 'user', data: 'last', chatId: 'duplicate' },
    ]
    transaction.objectStore('messagePages').put({
        key: `${generation}:message-page:char-a:conv-long:0`,
        generation,
        characterId: 'char-a',
        conversationId: 'conv-long',
        pageIndex: 0,
        value: messages,
    })
    for (let absoluteIndex = 0; absoluteIndex < messages.length; absoluteIndex++) {
        transaction.objectStore('messageOccurrences').put({
            key: `${generation}:message-occurrence:char-a:conv-long:${absoluteIndex}`,
            generation,
            characterId: 'char-a',
            conversationId: 'conv-long',
            messageId: messages[absoluteIndex].chatId,
            absoluteIndex,
        })
    }
    await completeTransaction(transaction)
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
    it('preserves conversation order when appending and inserting after a deletion', async () => {
        const store = new IndexedDbPersistentDataStore(
            `conversation-order-${databaseSequence++}`, new IDBFactory(), IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(fixtureDatabase)
        const deleted = await store.commit({
            expectedRevision: imported.revision,
            conversations: [{ type: 'delete', characterId: 'char-a', conversationId: 'conv-long' }],
        })
        const append = await store.commit({
            expectedRevision: deleted.revision,
            conversations: [{
                type: 'replace-range', characterId: 'char-a', conversationId: 'conv-appended',
                start: 0, deleteCount: 0, messages: [],
                conversation: { id: 'conv-appended', name: 'Appended', note: '', localLore: [] },
            }],
        })
        const query = () => store.queryConversations({ characterId: 'char-a', order: 'configured', limit: 10 })
        expect((await query()).items.map(({ id }) => id)).toEqual(['conv-short', 'conv-appended'])
        await store.commit({
            expectedRevision: append.revision,
            conversations: [{
                type: 'replace-range', characterId: 'char-a', conversationId: 'conv-inserted',
                start: 0, deleteCount: 0, messages: [], configuredIndex: 1,
                conversation: { id: 'conv-inserted', name: 'Inserted', note: '', localLore: [] },
            }],
        })
        expect((await query()).items.map(({ id }) => id)).toEqual([
            'conv-short', 'conv-inserted', 'conv-appended',
        ])
        expect((await store.materializeDatabase()).characters[1].chats.map(({ id }) => id)).toEqual([
            'conv-short', 'conv-inserted', 'conv-appended',
        ])
    })

    function rejectPluginPayloadIndexScans() {
        const original = IDBIndex.prototype.getAll
        return vi.spyOn(IDBIndex.prototype, 'getAll').mockImplementation(function (
            this: IDBIndex,
            query?: IDBValidKey | IDBKeyRange | null,
            count?: number,
        ) {
            if (this.objectStore.name === 'pluginStorage') {
                throw new Error('plugin payload index scan')
            }
            return count === undefined
                ? original.call(this, query)
                : original.call(this, query, count)
        })
    }

    it('uses only the occurrence index for present and absent first/last anchor lookup', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `message-occurrence-io-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const database = structuredClone(fixtureDatabase)
        const conversation = database.characters
            .find((character) => character.chaId === 'char-a')!
            .chats.find((chat) => chat.id === 'conv-long')!
        conversation.message[1].chatId = 'far-duplicate'
        conversation.message[128].chatId = 'far-duplicate'
        await store.replaceFromDatabase(database)

        const cursorCalls: Array<{ store: string; index: string }> = []
        const pageRangeReads: string[] = []
        const originalOpenCursor = IDBIndex.prototype.openCursor
        const originalGetAll = IDBIndex.prototype.getAll
        const cursorSpy = vi.spyOn(IDBIndex.prototype, 'openCursor').mockImplementation(function (
            this: IDBIndex,
            ...args: Parameters<IDBIndex['openCursor']>
        ) {
            cursorCalls.push({ store: this.objectStore.name, index: this.name })
            return originalOpenCursor.apply(this, args)
        })
        const getAllSpy = vi.spyOn(IDBIndex.prototype, 'getAll').mockImplementation(function (
            this: IDBIndex,
            ...args: Parameters<IDBIndex['getAll']>
        ) {
            if (this.objectStore.name === 'messagePages') pageRangeReads.push(this.name)
            return originalGetAll.apply(this, args)
        })

        try {
            for (const [messageId, anchorOccurrence] of [
                ['far-duplicate', 'first'],
                ['far-duplicate', 'last'],
                ['absent', 'first'],
                ['absent', 'last'],
            ] as const) {
                await store.readConversationWindow({
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    anchorMessageId: messageId,
                    anchorOccurrence,
                    before: 0,
                    after: 0,
                })
            }
        } finally {
            cursorSpy.mockRestore()
            getAllSpy.mockRestore()
        }

        expect(cursorCalls).toEqual(Array.from({ length: 4 }, () => ({
            store: 'messageOccurrences',
            index: 'byLookupKey',
        })))
        expect(pageRangeReads).toEqual([])
    })

    it('reads conversation metadata without opening either message store', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `conversation-metadata-io-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(
            databaseName,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(
            structuredClone(fixtureDatabase),
        )
        const lease = await store.acquireRevision(imported.revision)
        const rawDatabase = await openDatabase(indexedDB, databaseName)
        const databasePrototype = Object.getPrototypeOf(
            rawDatabase,
        ) as IDBDatabase
        rawDatabase.close()
        const openedStores: string[][] = []
        const originalTransaction = databasePrototype.transaction
        const transactionSpy = vi
            .spyOn(databasePrototype, 'transaction')
            .mockImplementation(function (
                this: IDBDatabase,
                storeNames,
                ...args
            ) {
                openedStores.push(
                    typeof storeNames === 'string'
                        ? [storeNames]
                        : [...storeNames],
                )
                return originalTransaction.call(this, storeNames, ...args)
            })

        try {
            await expect(
                store.readConversationMetadata('char-a', 'conv-long'),
            ).resolves.toMatchObject({
                value: { totalMessages: 130 },
            })
            await expect(
                lease.readConversationMetadata('char-a', 'conv-long'),
            ).resolves.toMatchObject({
                revision: imported.revision,
                value: { totalMessages: 130 },
            })
        } finally {
            transactionSpy.mockRestore()
        }

        expect(openedStores).toEqual([
            ['meta', 'conversations'],
            ['meta', 'conversations'],
        ])
        expect(openedStores.flat()).not.toContain('messagePages')
        expect(openedStores.flat()).not.toContain('messageOccurrences')
        await lease.release()
    })

    it('rejects an invalid persisted conversation metadata message count', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `conversation-metadata-count-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(
            databaseName,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await writeRawRecords(indexedDB, databaseName, 'conversations', [
            {
                key: 'revision-0:conversation:char-a:conv-invalid',
                generation: 'revision-0',
                value: {
                    summary: {
                        id: 'conv-invalid',
                        characterId: 'char-a',
                        name: 'Invalid count',
                        configuredIndex: 0,
                        recentAt: 0,
                        messageCount: -1,
                    },
                    detail: { id: 'conv-invalid', name: 'Invalid count' },
                },
            },
        ])

        await expect(
            store.readConversationMetadata('char-a', 'conv-invalid'),
        ).rejects.toThrow('nonnegative safe integer')
    })

    it('backfills the occurrence index atomically when upgrading legacy message pages', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `message-occurrence-upgrade-${databaseSequence++}`
        await createVersion1Database(indexedDB, databaseName)
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        await store.open()

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(15)
        const transaction = database.transaction('messageOccurrences', 'readonly')
        expect(transaction.objectStore('messageOccurrences').indexNames.contains(
            'byLookupKey',
        )).toBe(true)
        expect(await requestResultForTest(
            transaction.objectStore('messageOccurrences').count(),
        )).toBeGreaterThan(0)
        await completeTransaction(transaction)
        database.close()

        await expect(store.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            anchorMessageId: 'msg-127',
            anchorOccurrence: 'last',
            before: 0,
            after: 0,
        })).resolves.toMatchObject({ value: { startIndex: 127, endIndex: 128 } })
        await expect(store.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            anchorMessageId: 'absent-after-upgrade',
            anchorOccurrence: 'last',
            before: 0,
            after: 0,
        })).resolves.toBeNull()
    })

    it('rolls back the schema upgrade when occurrence backfill cannot read legacy pages', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `message-occurrence-upgrade-rollback-${databaseSequence++}`
        await createVersion1Database(indexedDB, databaseName)
        const originalOpenCursor = IDBObjectStore.prototype.openCursor
        const cursorSpy = vi.spyOn(IDBObjectStore.prototype, 'openCursor').mockImplementation(
            function (
                this: IDBObjectStore,
                ...args: Parameters<IDBObjectStore['openCursor']>
            ) {
                if (this.name === 'messagePages') throw new Error('injected occurrence backfill failure')
                return originalOpenCursor.apply(this, args)
            },
        )
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        try {
            await expect(store.open()).rejects.toThrow()
        } finally {
            cursorSpy.mockRestore()
        }

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(1)
        expect(database.objectStoreNames.contains('messageOccurrences')).toBe(false)
        database.close()

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()
        await expect(reopened.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            anchorMessageId: 'msg-127',
            anchorOccurrence: 'last',
            before: 0,
            after: 0,
        })).resolves.toMatchObject({ value: { startIndex: 127 } })
    })

    it('compacts version 13 per-message occurrences into indexed page rows', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `message-occurrence-v13-upgrade-${databaseSequence++}`
        await createVersion13OccurrenceDatabase(indexedDB, databaseName)
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        await store.open()

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(15)
        const transaction = database.transaction('messageOccurrences', 'readonly')
        const occurrences = transaction.objectStore('messageOccurrences')
        expect(occurrences.indexNames.contains('byConversationMessageIndex')).toBe(false)
        expect(occurrences.indexNames.contains('byLookupKey')).toBe(true)
        expect(await requestResultForTest(occurrences.count())).toBe(1)
        await completeTransaction(transaction)
        database.close()

        const query = (anchorOccurrence: 'first' | 'last') => store.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            anchorMessageId: 'duplicate',
            anchorOccurrence,
            before: 0,
            after: 0,
        })
        await expect(query('first')).resolves.toMatchObject({ value: { startIndex: 0 } })
        await expect(query('last')).resolves.toMatchObject({ value: { startIndex: 2 } })
    })

    it('keeps ordinary owner invalidation bounded with a large head set', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `large-owner-head-save-${databaseSequence++}`
        const database = structuredClone(fixtureDatabase)
        const ownerCount = 4_096
        database.modules = Array.from({ length: ownerCount }, (_, index) => ({
            id: index % 2 === 0 ? '' : 'duplicate',
            name: `Module ${index}`,
            description: '',
            assets: [],
        }))
        const character = database.characters.find(({ chaId }) => chaId === 'char-a')!
        character.additionalAssets = []
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(database)
        const root = (await store.readRoot()).value
        const detail = (await store.readCharacter('char-a'))!.value
        const characterHead: AssetOwnerHead = {
            owner: { kind: 'character-additional-assets', characterId: 'char-a' },
            present: true,
            manifestHash: '88'.repeat(32),
            entryCount: 0,
        }
        const rootHeads: AssetOwnerHead[] = database.modules.map((_, index) => ({
            owner: { kind: 'root-module-assets', index },
            present: true,
            manifestHash: '99'.repeat(32),
            entryCount: 0,
        }))
        const shadowed = await store.commit({
            expectedRevision: imported.revision,
            root,
            character: detail,
            assetOwnerHeads: [...rootHeads, characterHead],
        })

        let headGetAllCalls = 0
        const headCursorRanges: Array<{
            lower: IDBValidKey | undefined
            upper: IDBValidKey | undefined
        }> = []
        const headDeleteQueries: Array<
            IDBValidKey | { lower: IDBValidKey | undefined; upper: IDBValidKey | undefined }
        > = []
        const originalGetAll = IDBIndex.prototype.getAll
        const originalOpenCursor = IDBObjectStore.prototype.openCursor
        const originalDelete = IDBObjectStore.prototype.delete
        const getAllSpy = vi.spyOn(IDBIndex.prototype, 'getAll').mockImplementation(function (
            this: IDBIndex,
            ...args: Parameters<IDBIndex['getAll']>
        ) {
            if (this.objectStore.name === 'assetOwnerHeads') headGetAllCalls++
            return originalGetAll.apply(this, args)
        })
        const cursorSpy = vi.spyOn(IDBObjectStore.prototype, 'openCursor').mockImplementation(
            function (
                this: IDBObjectStore,
                ...args: Parameters<IDBObjectStore['openCursor']>
            ) {
                if (this.name === 'assetOwnerHeads') {
                    const range = args[0] instanceof IDBKeyRange ? args[0] : undefined
                    headCursorRanges.push({ lower: range?.lower, upper: range?.upper })
                }
                return originalOpenCursor.apply(this, args)
            },
        )
        const deleteSpy = vi.spyOn(IDBObjectStore.prototype, 'delete').mockImplementation(
            function (
                this: IDBObjectStore,
                ...args: Parameters<IDBObjectStore['delete']>
            ) {
                if (this.name === 'assetOwnerHeads') {
                    const query = args[0]
                    headDeleteQueries.push(query instanceof IDBKeyRange
                        ? { lower: query.lower, upper: query.upper }
                        : query)
                }
                return originalDelete.apply(this, args)
            },
        )

        let characterSaveMs: number
        let rootSaveMs: number
        try {
            const characterStart = performance.now()
            const characterSaved = await store.commit({
                expectedRevision: shadowed.revision,
                character: { ...detail, name: 'Ordinary character save' },
            })
            characterSaveMs = performance.now() - characterStart
            const rootStart = performance.now()
            await store.commit({
                expectedRevision: characterSaved.revision,
                root: { ...root, username: 'Ordinary root save' },
            })
            rootSaveMs = performance.now() - rootStart
        } finally {
            getAllSpy.mockRestore()
            cursorSpy.mockRestore()
            deleteSpy.mockRestore()
        }

        expect(headGetAllCalls).toBe(0)
        expect(headCursorRanges).toEqual([])
        expect(headDeleteQueries).toEqual([
            'revision-1:asset-owner-head:character-additional-assets:char-a',
            {
                lower: 'revision-1:asset-owner-head:root-module-assets:',
                upper: 'revision-1:asset-owner-head:root-module-assets:\uffff',
            },
            {
                lower: 'revision-1:asset-owner-head:persona-embedded-module-assets:',
                upper: 'revision-1:asset-owner-head:persona-embedded-module-assets:\uffff',
            },
        ])
        expect(await store.readAssetOwnerHead(characterHead.owner)).toBeNull()
        expect(await store.readAssetOwnerHead(rootHeads.at(-1)!.owner)).toBeNull()
        console.info('large-owner-head-save-measurement', JSON.stringify({
            ownerHeads: ownerCount + 1,
            headGetAllCalls,
            rootOwnerRangeDeletes: headDeleteQueries.length - 1,
            characterSaveMs,
            rootSaveMs,
        }))
    }, 15_000)

    it('rewrites colon-bearing composite keys when upgrading to version 15', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-14-colon-keys-${databaseSequence++}`
        await createVersion14ColonKeyDatabase(indexedDB, databaseName)
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()

        const conversation = await store.readConversation('char:x', 'conv')
        expect(conversation?.value.message).toMatchObject([
            { data: 'colon payload', chatId: 'msg:1' },
        ])
        expect((await store.readConversation('plain', 'conv'))?.value.name).toBe('Plain chat')

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(15)
        const transaction = database.transaction(
            ['conversations', 'messagePages', 'messageOccurrences'],
            'readonly',
        )
        const generation = 'revision-5'
        const readKey = (storeName: string, key: string) =>
            new Promise<unknown>((resolve, reject) => {
                const request = transaction.objectStore(storeName).get(key)
                request.onsuccess = () => resolve(request.result)
                request.onerror = () => reject(request.error)
            })
        await expect(readKey('conversations', `${generation}:conversation:char%3Ax:conv`))
            .resolves.toMatchObject({ generation })
        await expect(readKey('conversations', `${generation}:conversation:char:x:conv`))
            .resolves.toBeUndefined()
        await expect(readKey('conversations', `${generation}:conversation:plain:conv`))
            .resolves.toMatchObject({ generation })
        await expect(readKey('messagePages', `${generation}:message-page:char%3Ax:conv:0`))
            .resolves.toMatchObject({ characterId: 'char:x' })
        await expect(readKey('messagePages', `${generation}:message-page:char:x:conv:0`))
            .resolves.toBeUndefined()
        await expect(readKey(
            'messageOccurrences',
            `${generation}:message-occurrence-page:char%3Ax:conv:0000000000000000`,
        )).resolves.toMatchObject({ conversationId: 'conv' })
        await completeTransaction(transaction)
        database.close()
    })

    it('upgrades version 7 with empty asset alias and owner-head stores without scans', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-7-asset-alias-schema-${databaseSequence++}`
        await createVersion7Database(indexedDB, databaseName)
        const originalOpenCursor = IDBObjectStore.prototype.openCursor
        const cursorSpy = vi.spyOn(IDBObjectStore.prototype, 'openCursor')
            .mockImplementation(function (
                this: IDBObjectStore,
                ...args: Parameters<IDBObjectStore['openCursor']>
            ) {
                if (
                    this.name !== 'meta' &&
                    this.name !== 'messagePages' &&
                    this.name !== 'conversations'
                ) {
                    throw new Error('version 7 upgrade must scan only message pages and conversations')
                }
                return originalOpenCursor.apply(this, args)
            })
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        try {
            await store.open()
        } finally {
            cursorSpy.mockRestore()
        }

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(15)
        const transaction = database.transaction(
            ['assetAliases', 'assetOwnerHeads', 'assetRepositoryAuthority'],
            'readonly',
        )
        const aliases = transaction.objectStore('assetAliases')
        expect(aliases.indexNames.contains('byGeneration')).toBe(true)
        await expect(new Promise<number>((resolve, reject) => {
            const request = aliases.count()
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })).resolves.toBe(0)
        expect(
            transaction.objectStore('assetRepositoryAuthority').indexNames.contains('byGeneration'),
        ).toBe(true)
        const heads = transaction.objectStore('assetOwnerHeads')
        expect(heads.indexNames.contains('byGeneration')).toBe(true)
        await expect(new Promise<number>((resolve, reject) => {
            const request = heads.count()
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })).resolves.toBe(0)
        await completeTransaction(transaction)
        database.close()
    })

    it('upgrades version 8 with an owner-head store and kind-aware alias migration', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-8-owner-head-schema-${databaseSequence++}`
        await createVersion8Database(indexedDB, databaseName)
        const originalOpenCursor = IDBObjectStore.prototype.openCursor
        const cursorSpy = vi.spyOn(IDBObjectStore.prototype, 'openCursor')
            .mockImplementation(function (
                this: IDBObjectStore,
                ...args: Parameters<IDBObjectStore['openCursor']>
            ) {
                if (
                    this.name !== 'meta' &&
                    this.name !== 'assetAliases' &&
                    this.name !== 'messagePages' &&
                    this.name !== 'conversations'
                ) {
                    throw new Error('version 8 upgrade scanned an unrelated record family')
                }
                return originalOpenCursor.apply(this, args)
            })
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        try {
            await store.open()
        } finally {
            cursorSpy.mockRestore()
        }

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(15)
        const transaction = database.transaction('assetOwnerHeads', 'readonly')
        const heads = transaction.objectStore('assetOwnerHeads')
        expect(heads.indexNames.contains('byGeneration')).toBe(true)
        await expect(new Promise<number>((resolve, reject) => {
            const request = heads.count()
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })).resolves.toBe(0)
        await completeTransaction(transaction)
        database.close()
    })

    it('upgrades version 11 with cold stores and legacy markers for active and leased generations', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-11-cold-authority-${databaseSequence++}`
        await createVersion11ColdlessDatabase(indexedDB, databaseName)
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        await store.open()

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(15)
        const transaction = database.transaction(
            ['coldAliases', 'coldPayloadAuthority'],
            'readonly',
        )
        expect(transaction.objectStore('coldAliases').indexNames.contains('byGeneration')).toBe(true)
        expect(
            transaction.objectStore('coldPayloadAuthority').indexNames.contains('byGeneration'),
        ).toBe(true)
        await completeTransaction(transaction)
        database.close()

        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'coldPayloadAuthority',
            'revision-5',
        )).toEqual({
            key: 'revision-5',
            generation: 'revision-5',
            value: { format: 'legacy' },
        })
        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'coldPayloadAuthority',
            'snapshot-5-migrated',
        )).toEqual({
            key: 'snapshot-5-migrated',
            generation: 'snapshot-5-migrated',
            value: { format: 'legacy' },
        })
    })

    it('fails closed when cold authority is preparing or malformed', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `invalid-cold-authority-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const alias = {
            key: 'conversation/blocked',
            objectHash: '81'.repeat(32),
            size: 1,
            metadata: {},
        }
        await writeRawRecords(indexedDB, databaseName, 'coldPayloadAuthority', [{
            key: 'revision-0',
            generation: 'revision-0',
            value: { format: 'preparing', migrationId: 'blocked', sourceRevision: 0 },
        }])

        await expect(store.commitColdAlias(alias, 0)).rejects.toThrow('v2')
        await expect(store.activateColdPayloadMigration({
            sourceRevision: 0,
            migrationId: 'cannot-reenter',
            compatibilityHash: '82'.repeat(32),
            coldAliases: [alias],
        })).rejects.toThrow('legacy')
        expect((await store.readRoot()).revision).toBe(0)

        await writeRawRecords(indexedDB, databaseName, 'coldPayloadAuthority', [{
            key: 'revision-0',
            generation: 'revision-0',
            value: { format: 'v2', migrationId: 'malformed', compatibilityHash: 'no' },
        }])
        await expect(store.readColdPayloadAuthority()).rejects.toThrow('compatibilityHash')
        await expect(store.deleteColdAlias(alias.key, 0)).rejects.toThrow('compatibilityHash')
        expect((await store.readRoot()).revision).toBe(0)
    })

    it('migrates version 9 aliases to kind-aware keys without losing same-key siblings', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-9-kind-aware-alias-${databaseSequence++}`
        await createVersion9AliasDatabase(indexedDB, databaseName)
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const key = 'shared/migrated.bin'
        const inlay = {
            key,
            objectHash: '71'.repeat(32),
            kind: 'inlay' as const,
            size: 7,
            mime: 'image/webp',
            name: 'Sibling inlay',
            ext: 'webp',
            inlayType: 'image' as const,
        }

        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'assetAliases',
            'revision-5:asset-alias:shared/migrated.bin',
        )).toBeUndefined()
        expect((await store.readAssetAlias({ kind: 'asset', key }))?.value)
            .toMatchObject({ kind: 'asset', key, name: 'Migrated asset' })

        const committed = await store.commitAssetAlias(inlay, 5)

        expect((await store.readAssetAlias({ kind: 'asset', key }))?.value)
            .toMatchObject({ kind: 'asset', key, name: 'Migrated asset' })
        expect(await store.readAssetAlias({ kind: 'inlay', key })).toEqual({
            revision: committed.revision,
            value: inlay,
        })
    })

    it('migrates version 9 aliases without typed-key collisions', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-9-alias-key-collision-${databaseSequence++}`
        await createVersion9AliasDatabase(indexedDB, databaseName)
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        await store.open()

        expect((await store.readAssetAlias({ kind: 'asset', key: '0' }))?.value)
            .toMatchObject({ kind: 'asset', key: '0', name: 'Collision asset' })
        expect((await store.readAssetAlias({ kind: 'inlay', key: 'asset:0' }))?.value)
            .toMatchObject({ kind: 'inlay', key: 'asset:0', name: 'Collision inlay' })
    })

    it('boots the plugin catalog without scanning large plugin payload rows', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            `plugin-metadata-catalog-${databaseSequence++}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const database = structuredClone(fixtureDatabase)
        database.pluginCustomStorage = {
            alpha: 'a'.repeat(2 * 1024 * 1024),
            beta: 'b'.repeat(2 * 1024 * 1024),
        }
        const imported = await store.replaceFromDatabase(database)
        const payloadScan = rejectPluginPayloadIndexScans()

        try {
            await expect(store.queryPluginStorage()).resolves.toEqual({
                revision: imported.revision,
                items: [
                    { key: 'alpha', byteSize: 2 * 1024 * 1024 + 2 },
                    { key: 'beta', byteSize: 2 * 1024 * 1024 + 2 },
                ],
            })
        } finally {
            payloadScan.mockRestore()
        }
    })

    it('allocates a new plugin ordinal without scanning existing payload rows', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            `plugin-metadata-ordinal-${databaseSequence++}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const database = structuredClone(fixtureDatabase)
        database.pluginCustomStorage = {
            first: 'a'.repeat(2 * 1024 * 1024),
            second: 'b'.repeat(2 * 1024 * 1024),
        }
        const imported = await store.replaceFromDatabase(database)
        const payloadScan = rejectPluginPayloadIndexScans()

        try {
            await expect(store.commit({
                expectedRevision: imported.revision,
                pluginStorage: [{ type: 'set', key: 'third', value: 3 }],
            })).resolves.toEqual({ revision: imported.revision + 1 })
            await expect(store.queryPluginStorage()).resolves.toMatchObject({
                items: [
                    { key: 'first' },
                    { key: 'second' },
                    { key: 'third' },
                ],
            })
        } finally {
            payloadScan.mockRestore()
        }
    })

    it('shares a single in-flight open across concurrent callers', async () => {
        const indexedDB = new IDBFactory()
        const openSpy = vi.spyOn(indexedDB, 'open')
        const store = new IndexedDbPersistentDataStore(
            `concurrent-open-${databaseSequence++}`,
            indexedDB,
            IDBKeyRange,
        )

        await Promise.all([store.open(), store.open()])

        expect(openSpy).toHaveBeenCalledTimes(1)
        openSpy.mockRestore()
        expect((await store.readRoot()).revision).toBe(0)
    })

    it('sweeps asset aliases from an abandoned staging generation on reopen', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `asset-alias-staging-sweep-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const generation = 'staging-abandoned-alias'
        const aliasKey = `${generation}:asset-alias:asset:assets/abandoned.bin`
        await writeRawRecords(indexedDB, databaseName, 'root', [{
            key: generation,
            generation,
            value: {},
        }])
        await writeRawRecords(indexedDB, databaseName, 'assetAliases', [{
            key: aliasKey,
            generation,
            value: {
                key: 'assets/abandoned.bin',
                objectHash: '77'.repeat(32),
                kind: 'asset',
                size: 7,
                mime: 'application/octet-stream',
                name: 'Abandoned',
                ext: 'bin',
            },
        }])

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()

        expect(await readRawRecord(indexedDB, databaseName, 'assetAliases', aliasKey)).toBeUndefined()
    })

    it('rejects a corrupt persisted asset alias during direct lookup', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `asset-alias-integrity-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await writeRawRecords(indexedDB, databaseName, 'assetAliases', [{
            key: 'revision-0:asset-alias:asset:assets/corrupt.bin',
            generation: 'revision-0',
            value: {
                key: 'assets/corrupt.bin',
                objectHash: 'CORRUPT',
                kind: 'asset',
                size: 1,
                mime: 'application/octet-stream',
                name: 'Corrupt',
                ext: 'bin',
            },
        }])

        await expect(store.readAssetAlias({
            kind: 'asset',
            key: 'assets/corrupt.bin',
        })).rejects.toThrow('objectHash')
    })

    it('rejects a corrupt persisted asset alias during batch lookup', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `asset-alias-batch-integrity-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await writeRawRecords(indexedDB, databaseName, 'assetAliases', [{
            key: 'revision-0:asset-alias:asset:assets/corrupt-batch.bin',
            generation: 'revision-0',
            value: {
                key: 'assets/corrupt-batch.bin',
                objectHash: 'CORRUPT',
                kind: 'asset',
                size: 1,
                mime: 'application/octet-stream',
                name: 'Corrupt batch',
                ext: 'bin',
            },
        }])

        await expect(store.readAssetAliasesByKeys('asset', ['assets/corrupt-batch.bin']))
            .rejects.toThrow('objectHash')
    })

    it('fails closed when a persisted alias row identity does not match its lookup key', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `asset-alias-identity-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const requestedKey = 'assets/requested.bin'
        const recordKey = `revision-0:asset-alias:asset:${requestedKey}`
        const value = {
            key: requestedKey,
            objectHash: '88'.repeat(32),
            kind: 'asset',
            size: 1,
            mime: 'application/octet-stream',
            name: 'Requested',
            ext: 'bin',
        }
        await writeRawRecords(indexedDB, databaseName, 'assetAliases', [{
            key: recordKey,
            generation: 'revision-other',
            value,
        }])

        await expect(store.readAssetAlias({ kind: 'asset', key: requestedKey }))
            .rejects.toThrow('generation')

        await writeRawRecords(indexedDB, databaseName, 'assetAliases', [{
            key: recordKey,
            generation: 'revision-0',
            value: { ...value, key: 'assets/other.bin' },
        }])
        await expect(store.readAssetAlias({ kind: 'asset', key: requestedKey }))
            .rejects.toThrow('logical key')
    })

    it('acquires a revision by reference without copying persistent records', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `revision-reference-count-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        let imported = await store.replaceFromDatabase(fixtureDatabase)
        imported = await store.commit({
            expectedRevision: imported.revision,
            pluginStorage: [{ type: 'set', key: 'counted-zero', value: 0 }],
        })
        const before = await countPersistentDataRecords(indexedDB, databaseName)

        const lease = await store.acquireRevision(imported.revision)

        expect(await countPersistentDataRecords(indexedDB, databaseName)).toEqual(before)
        await lease.release()
    })

    it('rejects non-positive query limits instead of returning a stuck cursor', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            `non-positive-limit-${databaseSequence++}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        await store.replaceFromDatabase(structuredClone(fixtureDatabase))

        await expect(
            store.queryCharacters({ order: 'configured', trash: false, limit: 0 }),
        ).rejects.toThrow(RangeError)
        await expect(
            store.queryConversations({ characterId: 'char-a', order: 'configured', limit: -1 }),
        ).rejects.toThrow(RangeError)
    })

    it('keeps synchronous chat-list metadata in conversation summaries', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            `conversation-summary-list-metadata-${databaseSequence++}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const database = structuredClone(fixtureDatabase)
        database.characters[1].chats[0].folderId = 'folder-a'
        database.characters[1].chats[0].bindedPersona = 'persona-a'
        await store.replaceFromDatabase(database)

        const page = await store.queryConversations({
            characterId: 'char-a',
            order: 'configured',
            limit: 10,
        })

        expect(page.items[0]).toMatchObject({
            id: 'conv-long',
            folderId: 'folder-a',
            bindedPersona: 'persona-a',
        })
    })

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
        expect((await store.readRoot()).value).not.toHaveProperty('pluginCustomStorage')
        expect((await store.readPluginStorage('active-memory'))?.value).toEqual({
            turns: [1, 2, 3],
        })
        expect(await store.queryPresets()).toEqual({
            revision: 7,
            items: [
                {
                    id: '0',
                    name: 'Preset Beta',
                    image: 'preset-beta.png',
                    configuredIndex: 0,
                },
                { id: '1', name: 'Preset Alpha', image: undefined, configuredIndex: 1 },
            ],
        })
        expect(await readRawRecord(indexedDB, databaseName, 'root', 'revision-legacy')).toEqual({
            key: 'revision-legacy',
            generation: 'revision-legacy',
            value: { username: 'Legacy generation' },
        })
        expect(
            await readRawRecord(
                indexedDB,
                databaseName,
                'pluginStorage',
                'revision-legacy:plugin-storage:old-memory',
            ),
        ).toMatchObject({
            generation: 'revision-legacy',
            storageKey: 'old-memory',
            value: 'preserved',
        })
        expect(
            await readRawRecord(
                indexedDB,
                databaseName,
                'presets',
                'revision-legacy:preset:0',
            ),
        ).toMatchObject({
            generation: 'revision-legacy',
            value: {
                summary: { name: 'Legacy preset', image: 'legacy.png', configuredIndex: 0 },
                preset: { name: 'Legacy preset', image: 'legacy.png' },
            },
        })
        expect(
            (await store.queryCharacters({ order: 'configured', trash: false, limit: 10 })).items.map(
                (item) => item.id,
            ),
        ).toEqual(['char-b', 'char-a'])
        expect(
            (await store.queryCharacters({ order: 'configured', trash: true, limit: 10 })).items[0],
        ).toMatchObject({
            id: 'char-c',
            type: 'character',
            creatorNotes: '',
            trashTime: 350,
        })
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

    it('migrates version 1 plugin insertion ordinals and array-index ordering', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'version-1-plugin-order-upgrade'
        await createVersion1Database(indexedDB, databaseName)
        const record = await readRawRecord(
            indexedDB,
            databaseName,
            'root',
            'revision-7',
        ) as { key: string; generation: string; value: Record<string, unknown> }
        const storage: Record<string, unknown> = {}
        storage.zeta = 'first string'
        storage['10'] = 'ten'
        storage['2'] = 'two'
        storage['01'] = 'non-index'
        storage['\uffffx'] = 'unicode'
        record.value.pluginCustomStorage = storage
        await writeRawRecords(indexedDB, databaseName, 'root', [record])

        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()

        expect((await store.queryPluginStorage()).items.map((item) => item.key)).toEqual(
            Object.keys(storage),
        )
        expect(Object.keys((await store.materializeDatabase()).pluginCustomStorage)).toEqual(
            Object.keys(storage),
        )
        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'pluginStorage',
            'revision-7:plugin-storage:zeta',
        )).toMatchObject({ ordinal: 2 })
    })

    it('backfills metadata from version 5 plugin value rows atomically', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-5-plugin-metadata-${databaseSequence++}`
        await createVersion5PluginDatabase(indexedDB, databaseName)

        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()

        expect((await store.queryPluginStorage()).items.map((item) => item.key)).toEqual([
            'zeta',
            'alpha',
        ])
        expect(await store.readPluginStorage('zeta')).toMatchObject({ value: 'first' })
        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'pluginStorageMetadata',
            'revision-5:plugin-storage:zeta',
        )).toMatchObject({
            generation: 'revision-5',
            storageKey: 'zeta',
            ordinal: 0,
        })
    })

    it('rolls back a version 5 metadata upgrade when backfill fails', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-5-plugin-metadata-rollback-${databaseSequence++}`
        await createVersion5PluginDatabase(indexedDB, databaseName)
        const backfillError = new Error('injected metadata backfill failure')
        const originalOpenCursor = IDBObjectStore.prototype.openCursor
        const cursorSpy = vi
            .spyOn(IDBObjectStore.prototype, 'openCursor')
            .mockImplementation(function (
                this: IDBObjectStore,
                ...args: Parameters<IDBObjectStore['openCursor']>
            ) {
                if (this.name === 'pluginStorage') throw backfillError
                return originalOpenCursor.apply(this, args)
            })
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

        try {
            await expect(store.open()).rejects.toBeTruthy()
        } finally {
            cursorSpy.mockRestore()
        }

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(5)
        expect(database.objectStoreNames.contains('pluginStorageMetadata')).toBe(false)
        database.close()
        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'pluginStorage',
            'revision-5:plugin-storage:zeta',
        )).toMatchObject({ value: 'first', ordinal: 0 })

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()
        await expect(reopened.readPluginStorage('zeta')).resolves.toMatchObject({
            revision: 5,
            value: 'first',
        })
    })

    it('opens a version 6 metadata split and preserves legacy snapshot rows', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `version-6-plugin-snapshot-${databaseSequence++}`
        await createVersion6PluginSnapshotDatabase(indexedDB, databaseName)

        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()

        await expect(store.queryPluginStorage()).resolves.toMatchObject({
            revision: 5,
            items: [{ key: 'zeta' }, { key: 'alpha' }],
        })
        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'pluginStorage',
            'snapshot-5-migrated:plugin-storage:legacy',
        )).toMatchObject({ value: 0 })
        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'pluginStorageMetadata',
            'snapshot-5-migrated:plugin-storage:legacy',
        )).toMatchObject({ generation: 'snapshot-5-migrated', ordinal: 0 })

        await (store as unknown as { releaseSnapshotLease(lease: string): Promise<void> })
            .releaseSnapshotLease('snapshot-5-migrated')

        expect(await readRawRecord(
            indexedDB,
            databaseName,
            'pluginStorageMetadata',
            'snapshot-5-migrated:plugin-storage:legacy',
        )).toBeUndefined()
        await expect(store.readPluginStorage('zeta')).resolves.toMatchObject({
            revision: 5,
            value: 'first',
        })
    })

    it('rolls back a version 1 upgrade when botPresets exists but is not an array', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'version-1-invalid-presets-upgrade'
        const invalidPresets = { legacy: 'unsupported' }
        await createVersion1Database(indexedDB, databaseName)
        await writeRawRecords(indexedDB, databaseName, 'root', [
            {
                key: 'revision-legacy',
                generation: 'revision-legacy',
                value: { username: 'Legacy generation', botPresets: invalidPresets },
            },
        ])

        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await expect(store.open()).rejects.toBeTruthy()

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(1)
        expect(database.objectStoreNames.contains('presets')).toBe(false)
        database.close()
        const record = (await readRawRecord(
            indexedDB,
            databaseName,
            'root',
            'revision-legacy',
        )) as Record<string, unknown>
        expect((record.value as Record<string, unknown>).botPresets).toEqual(invalidPresets)
        const activeRecord = (await readRawRecord(
            indexedDB,
            databaseName,
            'root',
            'revision-7',
        )) as Record<string, unknown>
        expect((activeRecord.value as Record<string, unknown>).botPresets).toEqual(
            fixtureDatabase.botPresets,
        )
    })

    it('rolls back a version 1 upgrade when plugin storage is not a plain record', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'version-1-invalid-plugin-storage-upgrade'
        const invalidPluginStorage = ['unsupported']
        await createVersion1Database(indexedDB, databaseName)
        await writeRawRecords(indexedDB, databaseName, 'root', [
            {
                key: 'revision-legacy',
                generation: 'revision-legacy',
                value: {
                    username: 'Legacy generation',
                    botPresets: [{ name: 'Legacy preset' }],
                    pluginCustomStorage: invalidPluginStorage,
                },
            },
        ])

        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await expect(store.open()).rejects.toBeTruthy()

        const database = await openDatabase(indexedDB, databaseName)
        expect(database.version).toBe(1)
        expect(database.objectStoreNames.contains('pluginStorage')).toBe(false)
        database.close()
        const record = (await readRawRecord(
            indexedDB,
            databaseName,
            'root',
            'revision-legacy',
        )) as Record<string, unknown>
        expect((record.value as Record<string, unknown>).pluginCustomStorage).toEqual(
            invalidPluginStorage,
        )
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
            {
                store: 'messageOccurrences',
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
        let occurrenceCursors = 0
        const originalOpenCursor = IDBIndex.prototype.openCursor
        const cursorSpy = vi
            .spyOn(IDBIndex.prototype, 'openCursor')
            .mockImplementation(function (this: IDBIndex, ...args: Parameters<IDBIndex['openCursor']>) {
                if (this.name === 'byConversationPage') conversationPageCursors++
                if (this.name === 'byLookupKey') occurrenceCursors++
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

        expect(conversationPageCursors).toBe(0)
        expect(occurrenceCursors).toBe(1)
    })

    it('removes the previous generation after each successful staged replacement', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'staged-generation-cleanup'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const database = structuredClone(fixtureDatabase)
        database.pluginCustomStorage = { retained: 0 }
        await store.replaceFromDatabase(database)
        const replacement = structuredClone(database)
        replacement.username = 'Replacement User'
        await store.replaceFromDatabase(replacement)

        const openRequest = indexedDB.open(databaseName)
        const rawDatabase = await new Promise<IDBDatabase>((resolve, reject) => {
            openRequest.onsuccess = () => resolve(openRequest.result)
            openRequest.onerror = () => reject(openRequest.error)
        })
        const storeNames = [
            'root',
            'presets',
            'catalog',
            'characters',
            'conversations',
            'messagePages',
            'pluginStorage',
            'pluginStorageMetadata',
        ]
        const transaction = rawDatabase.transaction(storeNames, 'readonly')
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
        await expect(lease.queryPluginStorage()).resolves.toMatchObject({
            revision: imported.revision,
            items: [],
        })

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
        await expect(lease.queryPluginStorage()).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readPluginStorage('missing')).rejects.toBeInstanceOf(
            SnapshotReleasedError,
        )
    })

    it('rewrites occurrence lookup generations during copy-on-write and cleans the source', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `revision-occurrence-copy-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const database = structuredClone(fixtureDatabase)
        const longConversation = database.characters
            .find((character) => character.chaId === 'char-a')!
            .chats.find((conversation) => conversation.id === 'conv-long')!
        longConversation.message[0].chatId = 'cow-duplicate'
        longConversation.message[129].chatId = 'cow-duplicate'
        const imported = await store.replaceFromDatabase(database)
        const lease = await store.acquireRevision(imported.revision)

        const root = (await store.readRoot()).value
        const committed = await store.commit({
            expectedRevision: imported.revision,
            root: { ...root, username: 'Copied root only' },
        })
        const query = (reader: typeof store | typeof lease, occurrence: 'first' | 'last') => (
            reader.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                anchorMessageId: 'cow-duplicate',
                anchorOccurrence: occurrence,
                before: 0,
                after: 0,
            })
        )

        await expect(query(store, 'first')).resolves.toMatchObject({
            revision: committed.revision,
            value: { startIndex: 0 },
        })
        await expect(query(store, 'last')).resolves.toMatchObject({
            revision: committed.revision,
            value: { startIndex: 129 },
        })
        await expect(store.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            anchorMessageId: 'cow-absent',
            before: 0,
            after: 0,
        })).resolves.toBeNull()
        await expect(query(lease, 'first')).resolves.toMatchObject({
            revision: imported.revision,
            value: { startIndex: 0 },
        })
        await expect(query(lease, 'last')).resolves.toMatchObject({
            revision: imported.revision,
            value: { startIndex: 129 },
        })

        const rawDatabase = await openDatabase(indexedDB, databaseName)
        const beforeRelease = rawDatabase.transaction('messageOccurrences', 'readonly')
        const occurrences = beforeRelease.objectStore('messageOccurrences')
        const copiedRows = await requestResultForTest<Array<{
            generation: string
            lookupKeys: string[]
        }>>(occurrences.index('byGeneration').getAll('revision-2'))
        expect(copiedRows).not.toHaveLength(0)
        expect(copiedRows.every((row) => row.lookupKeys.every(
            (lookupKey) => JSON.parse(lookupKey)[0] === 'revision-2',
        ))).toBe(true)
        expect(await requestResultForTest(occurrences.index('byLookupKey').count(
            JSON.stringify(['revision-1', 'char-a', 'conv-long', 'cow-duplicate']),
        ))).toBe(2)
        expect(await requestResultForTest(occurrences.index('byLookupKey').count(
            JSON.stringify(['revision-2', 'char-a', 'conv-long', 'cow-duplicate']),
        ))).toBe(2)
        await completeTransaction(beforeRelease)

        await lease.release()

        const afterRelease = rawDatabase.transaction('messageOccurrences', 'readonly')
        const releasedOccurrences = afterRelease.objectStore('messageOccurrences')
        expect(await requestResultForTest(
            releasedOccurrences.index('byGeneration').count('revision-1'),
        )).toBe(0)
        expect(await requestResultForTest(releasedOccurrences.index('byLookupKey').count(
            JSON.stringify(['revision-1', 'char-a', 'conv-long', 'cow-duplicate']),
        ))).toBe(0)
        expect(await requestResultForTest(releasedOccurrences.index('byLookupKey').count(
            JSON.stringify(['revision-2', 'char-a', 'conv-long', 'cow-duplicate']),
        ))).toBe(2)
        await completeTransaction(afterRelease)
        rawDatabase.close()
    })

    it('rejects and rolls back a malformed occurrence row during generation copy', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `revision-occurrence-copy-invalid-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(fixtureDatabase)
        const lease = await store.acquireRevision(imported.revision)
        const occurrenceKey = [
            'revision-1:message-occurrence-page:char-a:conv-long:',
            '0'.repeat(16),
        ].join('')
        const originalOccurrence = await readRawRecord(
            indexedDB,
            databaseName,
            'messageOccurrences',
            occurrenceKey,
        ) as Record<string, unknown>
        const originalRoot = await store.readRoot()
        const originalCounts = await countPersistentDataRecords(indexedDB, databaseName)
        await writeRawRecords(indexedDB, databaseName, 'messageOccurrences', [{
            ...originalOccurrence,
            lookupKeys: [JSON.stringify([
                'wrong-generation',
                'char-a',
                'conv-long',
                'msg-000',
            ])],
        }])

        const commit = store.commit({
            expectedRevision: imported.revision,
            root: { ...originalRoot.value, username: 'Must roll back' },
        })
        let timeout: ReturnType<typeof setTimeout> | undefined
        const boundedCommit = Promise.race([
            commit,
            new Promise<never>((_resolve, reject) => {
                timeout = setTimeout(
                    () => reject(new Error('generation copy did not settle within 1 second')),
                    1_000,
                )
            }),
        ]).finally(() => {
            if (timeout !== undefined) clearTimeout(timeout)
        })
        await expect(boundedCommit).rejects.toThrow(
            'Persistent message occurrence lookup key does not match its row',
        )

        await expect(store.readRoot()).resolves.toEqual(originalRoot)
        await expect(lease.readConversation('char-a', 'conv-long')).resolves.toMatchObject({
            revision: imported.revision,
            value: { id: 'conv-long' },
        })
        expect(await countPersistentDataRecords(indexedDB, databaseName)).toEqual(originalCounts)

        const rawDatabase = await openDatabase(indexedDB, databaseName)
        const generationStores = [
            'root',
            'presets',
            'catalog',
            'characters',
            'conversations',
            'messagePages',
            'messageOccurrences',
            'pluginStorage',
            'pluginStorageMetadata',
            'assetAliases',
            'assetOwnerHeads',
            'assetRepositoryAuthority',
            'coldAliases',
            'coldPayloadAuthority',
        ]
        const residueTransaction = rawDatabase.transaction(generationStores, 'readonly')
        const targetResidue = await Promise.all(generationStores.map(async (storeName) => {
            const objectStore = residueTransaction.objectStore(storeName)
            if (storeName === 'root') {
                return requestResultForTest(objectStore.count('revision-2'))
            }
            return requestResultForTest(objectStore.index('byGeneration').count('revision-2'))
        }))
        expect(targetResidue).toEqual(generationStores.map(() => 0))
        await completeTransaction(residueTransaction)
        rawDatabase.close()

        await writeRawRecords(
            indexedDB,
            databaseName,
            'messageOccurrences',
            [originalOccurrence],
        )
        const committed = await store.commit({
            expectedRevision: imported.revision,
            root: { ...originalRoot.value, username: 'Retry succeeded' },
        })
        await expect(store.readConversationWindow({
            characterId: 'char-a',
            conversationId: 'conv-long',
            anchorMessageId: 'msg-127',
            before: 0,
            after: 0,
        })).resolves.toMatchObject({
            revision: committed.revision,
            value: { startIndex: 127 },
        })
        await lease.release()
    })

    it('rejects every old lease view after another realm removes its durable lease', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `externally-expired-revision-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        let imported = await store.replaceFromDatabase(fixtureDatabase)
        imported = await store.commit({
            expectedRevision: imported.revision,
            pluginStorage: [{ type: 'set', key: 'pinned-zero', value: 0 }],
        })
        const lease = await store.acquireRevision(imported.revision)

        const database = await openDatabase(indexedDB, databaseName)
        const transaction = database.transaction('meta', 'readwrite')
        const meta = transaction.objectStore('meta')
        const leaseKeys = await new Promise<IDBValidKey[]>((resolve, reject) => {
            const request = meta.getAllKeys(
                IDBKeyRange.bound('snapshotLease:', 'snapshotLease:\uffff'),
            )
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })
        expect(leaseKeys).toHaveLength(1)
        meta.delete(leaseKeys[0])
        await completeTransaction(transaction)
        database.close()

        const root = (await store.readRoot()).value
        const committed = await store.commit({
            expectedRevision: imported.revision,
            root: { ...root, username: 'Committed after external lease expiry' },
            pluginStorage: [{ type: 'set', key: 'pinned-zero', value: 1 }],
        })
        expect((await store.readRoot()).value.username).toBe(
            'Committed after external lease expiry',
        )

        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.queryPresets()).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readPreset('0')).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(
            lease.queryCharacters({ order: 'configured', trash: false, limit: 1 }),
        ).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readCharacter('char-a')).rejects.toBeInstanceOf(
            SnapshotReleasedError,
        )
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
        await expect(lease.queryPluginStorage()).rejects.toBeInstanceOf(SnapshotReleasedError)
        await expect(lease.readPluginStorage('pinned-zero')).rejects.toBeInstanceOf(
            SnapshotReleasedError,
        )

        expect((await store.readRoot()).value).toMatchObject({
            username: 'Committed after external lease expiry',
        })
        expect((await store.readRoot()).revision).toBe(committed.revision)
        await expect(store.readPluginStorage('pinned-zero')).resolves.toMatchObject({
            revision: committed.revision,
            value: 1,
        })
    })

    it('rolls back an injected generation copy failure without invalidating the lease', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `revision-copy-rollback-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        let imported = await store.replaceFromDatabase(fixtureDatabase)
        imported = await store.commit({
            expectedRevision: imported.revision,
            pluginStorage: [{ type: 'set', key: 'rollback-zero', value: 0 }],
        })
        const lease = await store.acquireRevision(imported.revision)
        const countsBefore = await countPersistentDataRecords(indexedDB, databaseName)
        const copyError = new Error('injected generation copy failure')
        const copyGeneration = vi.spyOn(
            store as unknown as {
                copyGeneration(
                    source: IDBObjectStore,
                    sourceGeneration: string,
                    targetGeneration: string,
                ): Promise<void>
            },
            'copyGeneration',
        )
        copyGeneration.mockRejectedValueOnce(copyError)

        const root = (await store.readRoot()).value
        await expect(
            store.commit({
                expectedRevision: imported.revision,
                root: { ...root, username: 'Must roll back' },
            }),
        ).rejects.toBe(copyError)
        copyGeneration.mockRestore()

        expect(await countPersistentDataRecords(indexedDB, databaseName)).toEqual(countsBefore)
        await expect(store.readRoot()).resolves.toMatchObject({
            revision: imported.revision,
            value: { username: fixtureDatabase.username },
        })
        await expect(lease.readRoot()).resolves.toMatchObject({
            revision: imported.revision,
            value: { username: fixtureDatabase.username },
        })
        await expect(store.readPluginStorage('rollback-zero')).resolves.toMatchObject({
            revision: imported.revision,
            value: 0,
        })
        await expect(lease.readPluginStorage('rollback-zero')).resolves.toMatchObject({
            revision: imported.revision,
            value: 0,
        })
        await lease.release()
    })

    it('keeps a lease active and retries cleanup after release fails', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            `revision-release-retry-${databaseSequence++}`,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(fixtureDatabase)
        const releaseError = new Error('release failed')
        const releaseSnapshotLease = vi.spyOn(
            store as unknown as { releaseSnapshotLease(generation: string): Promise<void> },
            'releaseSnapshotLease',
        ).mockRejectedValueOnce(releaseError).mockResolvedValueOnce(undefined)
        const lease = await store.acquireRevision(imported.revision)

        await expect(lease.release()).rejects.toBe(releaseError)
        await expect(lease.readRoot()).resolves.toMatchObject({ revision: imported.revision })
        await expect(lease.release()).resolves.toBeUndefined()
        await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        expect(releaseSnapshotLease).toHaveBeenCalledTimes(2)
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

    it('records a durable lease so another document cannot sweep a live snapshot', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'leased-revision-snapshot'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const { revision } = await store.replaceFromDatabase(fixtureDatabase)

        const readLeases = async () => {
            const request = indexedDB.open(databaseName)
            const database = await new Promise<IDBDatabase>((resolve, reject) => {
                request.onsuccess = () => resolve(request.result)
                request.onerror = () => reject(request.error)
            })
            const transaction = database.transaction('meta', 'readonly')
            const keys = await new Promise<IDBValidKey[]>((resolve, reject) => {
                const cursorRequest = transaction.objectStore('meta').getAllKeys(
                    IDBKeyRange.bound('snapshotLease:', 'snapshotLease:￿'),
                )
                cursorRequest.onsuccess = () => resolve(cursorRequest.result)
                cursorRequest.onerror = () => reject(cursorRequest.error)
            })
            database.close()
            return keys
        }

        const lease = await store.acquireRevision(revision)
        expect(await readLeases()).toHaveLength(1)

        const second = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await second.open()
        expect((await lease.readRoot()).value.username).toBe(fixtureDatabase.username)

        await lease.release()
        expect(await readLeases()).toHaveLength(0)
    })

    it('keeps a fresh crash-leaked snapshot lease and its generation across open', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `fresh-leased-snapshot-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const generation = 'snapshot-crashed-fresh'
        await writeRawRecords(indexedDB, databaseName, 'root', [
            { key: generation, generation, value: { username: 'Crashed' } },
        ])
        await writeRawRecords(indexedDB, databaseName, 'meta', [
            { key: `snapshotLease:${generation}`, value: generation, createdAt: Date.now() },
        ])

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()

        expect(await readRawRecord(indexedDB, databaseName, 'root', generation)).toMatchObject({
            generation,
        })
        expect(
            await readRawRecord(indexedDB, databaseName, 'meta', `snapshotLease:${generation}`),
        ).toMatchObject({ value: generation })
    })

    it('reclaims stale and legacy snapshot leases with their generations on open', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `stale-leased-snapshot-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const stale = 'snapshot-crashed-stale'
        const legacy = 'snapshot-crashed-legacy'
        const cow = 'revision-cow-stale'
        const cowLease = 'snapshot-9-cow-stale'
        await writeRawRecords(indexedDB, databaseName, 'root', [
            { key: stale, generation: stale, value: {} },
            { key: legacy, generation: legacy, value: {} },
            { key: cow, generation: cow, value: {} },
        ])
        await writeRawRecords(indexedDB, databaseName, 'meta', [
            {
                key: `snapshotLease:${stale}`,
                value: stale,
                createdAt: Date.now() - 25 * 60 * 60 * 1000,
            },
            { key: `snapshotLease:${legacy}`, value: legacy },
            {
                key: `snapshotLease:${cowLease}`,
                value: { generation: cow, revision: 9 },
                createdAt: Date.now() - 25 * 60 * 60 * 1000,
            },
        ])

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()

        for (const generation of [stale, legacy, cow]) {
            expect(await readRawRecord(indexedDB, databaseName, 'root', generation)).toBeUndefined()
        }
        for (const lease of [stale, legacy, cowLease]) {
            expect(
                await readRawRecord(indexedDB, databaseName, 'meta', `snapshotLease:${lease}`),
            ).toBeUndefined()
        }
    })
})
