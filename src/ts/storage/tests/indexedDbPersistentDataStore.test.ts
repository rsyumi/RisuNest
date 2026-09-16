import { UNOWNED_PLUGIN_OWNER } from '../../plugins/pluginOwner'
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

async function createPreviousSchemaDatabase(
    indexedDB: IDBFactory,
    databaseName: string,
    version: 1 | 15,
): Promise<void> {
    const request = indexedDB.open(databaseName, version)
    request.onupgradeneeded = () => {
        const currentStoreNames = [
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
        ]
        for (const storeName of version === 1 ? ['meta', 'root'] : currentStoreNames) {
            request.result.createObjectStore(storeName, { keyPath: 'key' })
        }
        request.transaction!.objectStore('meta').put({ key: 'schemaVersion', value: version })
    }
    const database = await requestResultForTest(request)
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

    it('settles a persisted catalog value that throws in a cursor predicate', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `catalog-cursor-predicate-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await writeRawRecords(indexedDB, databaseName, 'catalog', [{
            key: 'revision-0:character:corrupt',
            generation: 'revision-0',
            configuredIndex: 0,
            recentSortValue: 0,
            value: {
                id: 'corrupt',
                name: null,
                configuredIndex: 0,
                recentAt: 0,
                trashed: false,
                conversationCount: 0,
            },
        }])

        await expect(Promise.race([
            store.queryCharacters({
                order: 'configured',
                trash: false,
                search: 'corrupt',
                limit: 8,
            }),
            new Promise((_, reject) => setTimeout(
                () => reject(new Error('catalog query did not settle')),
                100,
            )),
        ])).rejects.toBeInstanceOf(TypeError)
    })

    it('keeps ordinary owner preservation bounded with a large head set', async () => {
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
        expect(await store.readAssetOwnerHead(characterHead.owner)).toMatchObject({
            value: characterHead,
        })
        expect(await store.readAssetOwnerHead(rootHeads.at(-1)!.owner)).toMatchObject({
            value: rootHeads.at(-1),
        })
        console.info('large-owner-head-save-measurement', JSON.stringify({
            ownerHeads: ownerCount + 1,
            headGetAllCalls,
            rootOwnerRangeDeletes: headDeleteQueries.length - 1,
            characterSaveMs,
            rootSaveMs,
        }))
    }, 15_000)

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
                    { owner: UNOWNED_PLUGIN_OWNER, key: 'alpha', byteSize: 2 * 1024 * 1024 + 2 },
                    { owner: UNOWNED_PLUGIN_OWNER, key: 'beta', byteSize: 2 * 1024 * 1024 + 2 },
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
                pluginStorage: [{ type: 'set', owner: 'test-plugin', key: 'third', value: 3 }],
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

    it.each([1, 15] as const)(
        'rejects previous RisuNest-owned schema v%s without modifying it',
        async (version) => {
            const indexedDB = new IDBFactory()
            const databaseName = `previous-schema-v${version}-${databaseSequence++}`
            await createPreviousSchemaDatabase(indexedDB, databaseName, version)
            const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)

            if (version === 1) {
                await expect(store.open()).rejects.toThrow(
                    'Unsupported RisuNest IndexedDB schema',
                )
            } else {
                await expect(store.open()).rejects.toMatchObject({ name: 'VersionError' })
            }

            const unchanged = await openDatabase(indexedDB, databaseName)
            expect(unchanged.version).toBe(version)
            expect(Array.from(unchanged.objectStoreNames)).toEqual(
                version === 1
                    ? ['meta', 'root']
                    : [
                        'assetAliases', 'assetOwnerHeads', 'assetRepositoryAuthority', 'catalog',
                        'characters', 'conversations',
                        'messageOccurrences', 'messagePages', 'meta', 'pluginStorage',
                        'pluginStorageMetadata', 'presets', 'root',
                    ],
            )
            const metaTransaction = unchanged.transaction('meta', 'readonly')
            const meta = metaTransaction.objectStore('meta')
            expect(await requestResultForTest(meta.get('schemaVersion'))).toEqual({
                key: 'schemaVersion',
                value: version,
            })
            expect(await requestResultForTest(meta.get('schemaIdentity'))).toBeUndefined()
            await completeTransaction(metaTransaction)
            unchanged.close()
        },
    )

    it('sweeps asset aliases from an abandoned revision generation on reopen', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `asset-alias-revision-sweep-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const generation = 'revision-99'
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

    it('settles a corrupt persisted asset alias during paged listing', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `asset-alias-list-integrity-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await writeRawRecords(indexedDB, databaseName, 'assetAliases', [{
            key: 'revision-0:asset-alias:asset:assets/corrupt-list.bin',
            generation: 'revision-0',
            value: {
                key: 'assets/corrupt-list.bin',
                objectHash: 'CORRUPT',
                kind: 'asset',
                size: 1,
                mime: 'application/octet-stream',
                name: 'Corrupt list',
                ext: 'bin',
            },
        }])

        await expect(Promise.race([
            store.listAssetAliases({ limit: 8 }),
            new Promise((_, reject) => setTimeout(
                () => reject(new Error('asset alias listing did not settle')),
                100,
            )),
        ])).rejects.toThrow('objectHash')
    })

    it('consumes transaction failure after a batch alias request rejects', async () => {
        const store = new IndexedDbPersistentDataStore(
            `asset-alias-batch-request-failure-${databaseSequence++}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        const requestError = new Error('injected alias request failure')
        const transactionError = new Error('injected transaction failure')
        let requestErrorHandler: (() => void) | null = null
        const request = {
            error: requestError,
            onsuccess: null,
            set onerror(handler: (() => void) | null) {
                requestErrorHandler = handler
                queueMicrotask(() => {
                    requestErrorHandler?.()
                    queueMicrotask(() => transaction.onabort?.(new Event('abort')))
                })
            },
        }
        const transaction = {
            error: transactionError,
            oncomplete: null,
            onabort: null as ((event: Event) => void) | null,
            onerror: null,
            objectStore: () => ({ get: () => request }),
        }
        const unhandled: unknown[] = []
        const onUnhandled = (error: unknown) => unhandled.push(error)
        process.on('unhandledRejection', onUnhandled)

        try {
            const operation = (
                store as unknown as {
                    readAssetAliasesByKeysFromTransaction(
                        transaction: IDBTransaction,
                        revision: number,
                        generation: string,
                        kind: 'asset',
                        keys: string[],
                    ): Promise<unknown>
                }
            ).readAssetAliasesByKeysFromTransaction(
                transaction as unknown as IDBTransaction,
                0,
                'revision-0',
                'asset',
                ['assets/failure.bin'],
            )
            await expect(operation).rejects.toBe(requestError)
            await new Promise((resolve) => setTimeout(resolve, 0))
            expect(unhandled).toEqual([])
        } finally {
            process.off('unhandledRejection', onUnhandled)
        }
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
            pluginStorage: [{ type: 'set', owner: 'test-plugin', key: 'counted-zero', value: 0 }],
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
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
        const database = await new Promise<IDBDatabase>((resolve, reject) => {
            const request = indexedDB.open(databaseName)
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })
        const transaction = database.transaction('catalog', 'readwrite')
        const recordRequest = transaction.objectStore('catalog').get('revision-1:character:char-c')
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
        await store.commit({ expectedRevision: imported.revision, addCharacter: added })

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
        await expect(lease.readPluginStorage('test-plugin', 'missing')).rejects.toBeInstanceOf(
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
            pluginStorage: [{ type: 'set', owner: 'test-plugin', key: 'pinned-zero', value: 0 }],
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
            pluginStorage: [{ type: 'set', owner: 'test-plugin', key: 'pinned-zero', value: 1 }],
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
        await expect(lease.readPluginStorage('test-plugin', 'pinned-zero')).rejects.toBeInstanceOf(
            SnapshotReleasedError,
        )

        expect((await store.readRoot()).value).toMatchObject({
            username: 'Committed after external lease expiry',
        })
        expect((await store.readRoot()).revision).toBe(committed.revision)
        await expect(store.readPluginStorage('test-plugin', 'pinned-zero')).resolves.toMatchObject({
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
            pluginStorage: [{ type: 'set', owner: 'test-plugin', key: 'rollback-zero', value: 0 }],
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
        await expect(store.readPluginStorage('test-plugin', 'rollback-zero')).resolves.toMatchObject({
            revision: imported.revision,
            value: 0,
        })
        await expect(lease.readPluginStorage('test-plugin', 'rollback-zero')).resolves.toMatchObject({
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

    it('sweeps inactive revision generations on open', async () => {
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
            key: 'revision-999',
            generation: 'revision-999',
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
            const request = verifyTransaction.objectStore('root').get('revision-999')
            request.onsuccess = () => resolve(request.result)
            request.onerror = () => reject(request.error)
        })
        expect(orphan).toBeUndefined()
    })

    it('rolls back stale lease deletion when generation cleanup fails', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `atomic-generation-sweep-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const generation = 'revision-903'
        const lease = 'snapshot-903-crashed'
        await writeRawRecords(indexedDB, databaseName, 'root', [
            { key: generation, generation, value: {} },
        ])
        await writeRawRecords(indexedDB, databaseName, 'meta', [{
            key: `snapshotLease:${lease}`,
            value: { generation, revision: 903 },
            createdAt: Date.now() - 25 * 60 * 60 * 1000,
        }])
        const cleanupError = new Error('injected generation cleanup failure')
        const originalOpenCursor = IDBIndex.prototype.openCursor
        const cursorSpy = vi.spyOn(IDBIndex.prototype, 'openCursor').mockImplementation(
            function (this: IDBIndex, ...args: Parameters<IDBIndex['openCursor']>) {
                if (this.objectStore.name === 'presets') throw cleanupError
                return originalOpenCursor.apply(this, args)
            },
        )

        try {
            const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
            await expect(reopened.open()).rejects.toBe(cleanupError)
        } finally {
            cursorSpy.mockRestore()
        }

        expect(await readRawRecord(indexedDB, databaseName, 'root', generation)).toMatchObject({
            generation,
        })
        expect(
            await readRawRecord(indexedDB, databaseName, 'meta', `snapshotLease:${lease}`),
        ).toMatchObject({ value: { generation, revision: 903 } })
    })

    it('rejects obsolete snapshot lease records without deleting them', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `obsolete-snapshot-lease-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const leaseKey = 'snapshotLease:snapshot-904-obsolete'
        await writeRawRecords(indexedDB, databaseName, 'meta', [{
            key: leaseKey,
            value: 'revision-904',
            createdAt: Date.now(),
        }])

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await expect(reopened.open()).rejects.toThrow('Snapshot lease target is invalid')
        expect(await readRawRecord(indexedDB, databaseName, 'meta', leaseKey)).toMatchObject({
            value: 'revision-904',
        })
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
        const generation = 'revision-777'
        await writeRawRecords(indexedDB, databaseName, 'root', [
            { key: generation, generation, value: { username: 'Crashed' } },
        ])
        await writeRawRecords(indexedDB, databaseName, 'meta', [
            {
                key: 'snapshotLease:snapshot-777-crashed-fresh',
                value: { generation, revision: 777 },
                createdAt: Date.now(),
            },
        ])

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()

        expect(await readRawRecord(indexedDB, databaseName, 'root', generation)).toMatchObject({
            generation,
        })
        expect(
            await readRawRecord(
                indexedDB,
                databaseName,
                'meta',
                'snapshotLease:snapshot-777-crashed-fresh',
            ),
        ).toMatchObject({ value: { generation, revision: 777 } })
    })

    it('reclaims stale snapshot leases with their generations on open', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `stale-leased-snapshot-${databaseSequence++}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const stale = 'revision-901'
        const cow = 'revision-902'
        const staleLease = 'snapshot-901-crashed-stale'
        const cowLease = 'snapshot-902-cow-stale'
        await writeRawRecords(indexedDB, databaseName, 'root', [
            { key: stale, generation: stale, value: {} },
            { key: cow, generation: cow, value: {} },
        ])
        await writeRawRecords(indexedDB, databaseName, 'meta', [
            {
                key: `snapshotLease:${staleLease}`,
                value: { generation: stale, revision: 901 },
                createdAt: Date.now() - 25 * 60 * 60 * 1000,
            },
            {
                key: `snapshotLease:${cowLease}`,
                value: { generation: cow, revision: 902 },
                createdAt: Date.now() - 25 * 60 * 60 * 1000,
            },
        ])

        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()

        for (const generation of [stale, cow]) {
            expect(await readRawRecord(indexedDB, databaseName, 'root', generation)).toBeUndefined()
        }
        for (const lease of [staleLease, cowLease]) {
            expect(
                await readRawRecord(indexedDB, databaseName, 'meta', `snapshotLease:${lease}`),
            ).toBeUndefined()
        }
    })
})
