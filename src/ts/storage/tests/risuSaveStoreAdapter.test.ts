import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import localforage from 'localforage'
import { describe, expect, it, vi } from 'vitest'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { RevisionConflictError } from '../persistentDataStore'
import { decodeRisuSave, encodeRisuSaveBlock, RisuSaveType } from '../risuSave'
import { importRisuSaveToStore, streamRisuSaveFromStore } from '../risuSaveStoreAdapter'
import { risuSaveFixtureDatabase, risuSaveFixtures } from './risuSaveFixtures'

vi.mock('../database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No live database in storage adapter tests')
    },
    presetTemplate: {},
}))
vi.mock('../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

async function concatenate(chunks: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
    const values: Uint8Array[] = []
    let length = 0
    for await (const chunk of chunks) {
        values.push(chunk)
        length += chunk.length
    }
    const result = new Uint8Array(length)
    let offset = 0
    for (const value of values) {
        result.set(value, offset)
        offset += value.length
    }
    return result
}

async function snapshotGenerations(indexedDB: IDBFactory, databaseName: string): Promise<string[]> {
    const request = indexedDB.open(databaseName)
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        request.onsuccess = () => resolve(request.result)
        request.onerror = () => reject(request.error)
    })
    const transaction = database.transaction('root', 'readonly')
    const records = await new Promise<Array<{ generation: string }>>((resolve, reject) => {
        const values = transaction.objectStore('root').getAll()
        values.onsuccess = () => resolve(values.result)
        values.onerror = () => reject(values.error)
    })
    database.close()
    return records
        .map((record) => record.generation)
        .filter((generation) => generation.startsWith('snapshot-'))
}

describe('RisuSave persistent store adapter', () => {
    it('preserves the existing raw block framing bytes', async () => {
        await expect(
            encodeRisuSaveBlock({
                compression: false,
                data: '{}',
                type: RisuSaveType.ROOT,
                name: 'root',
            }),
        ).resolves.toEqual(
            Uint8Array.from([1, 0, 4, 114, 111, 111, 116, 2, 0, 0, 0, 123, 125]),
        )
    })

    it.each(risuSaveFixtures)(
        'imports a %s save and exports a self-contained block save',
        async (format, fixture) => {
        const store = new IndexedDbPersistentDataStore(
            `risu-save-${format}-round-trip`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()

        const imported = await importRisuSaveToStore(fixture, store)
        await localforage.dropInstance({ name: 'risuSaveCache' })
        const exported = await concatenate(streamRisuSaveFromStore(store, imported.revision))

        await expect(decodeRisuSave(exported)).resolves.toEqual(risuSaveFixtureDatabase)
        },
    )

    it('pins one revision while a later commit completes during streaming', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore('risu-save-pinned-revision', indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const materialize = vi.spyOn(store, 'materializeDatabase').mockRejectedValue(
            new Error('streaming export must not materialize the database'),
        )
        const iterator = streamRisuSaveFromStore(store, imported.revision)[Symbol.asyncIterator]()

        const header = await iterator.next()
        expect(new TextDecoder().decode(header.value)).toBe('RISUSAVE\0')
        const root = (await store.readRoot()).value
        await store.commit({
            expectedRevision: imported.revision,
            root: { ...root, username: 'Later User' },
        })
        const remaining = await concatenate({
            [Symbol.asyncIterator]: () => iterator,
        })
        const exported = new Uint8Array(header.value.length + remaining.length)
        exported.set(header.value)
        exported.set(remaining, header.value.length)

        expect((await decodeRisuSave(exported)).username).toBe('Snapshot User')
        expect((await store.readRoot()).value.username).toBe('Later User')
        expect(materialize).not.toHaveBeenCalled()
    })

    it('exports trashed characters in configured order', async () => {
        const database = structuredClone(risuSaveFixtureDatabase)
        const trashed = structuredClone(database.characters[0])
        trashed.chaId = 'char-trashed'
        trashed.name = 'Trashed Character'
        trashed.trashTime = 200
        database.characters.unshift(trashed)
        const store = new IndexedDbPersistentDataStore(
            'risu-save-trashed-order',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)

        const exported = await concatenate(streamRisuSaveFromStore(store, imported.revision))

        await expect(decodeRisuSave(exported)).resolves.toEqual(database)
    })

    it('streams deterministic bytes for the same revision', async () => {
        const store = new IndexedDbPersistentDataStore(
            'risu-save-deterministic-export',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))

        const first = await concatenate(streamRisuSaveFromStore(store, imported.revision))
        const second = await concatenate(streamRisuSaveFromStore(store, imported.revision))

        expect(second).toEqual(first)
    })

    it('releases its temporary generation when iteration is cancelled', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'risu-save-cancelled-export'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const iterator = streamRisuSaveFromStore(store, imported.revision)[Symbol.asyncIterator]()

        await iterator.next()
        expect(await snapshotGenerations(indexedDB, databaseName)).toHaveLength(1)
        await iterator.return?.(undefined)

        expect(await snapshotGenerations(indexedDB, databaseName)).toEqual([])
    })

    it('rejects a stale revision before yielding any bytes', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'risu-save-stale-export'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const iterator = streamRisuSaveFromStore(store, imported.revision - 1)[Symbol.asyncIterator]()

        await expect(iterator.next()).rejects.toBeInstanceOf(RevisionConflictError)
        expect(await snapshotGenerations(indexedDB, databaseName)).toEqual([])
    })
})
