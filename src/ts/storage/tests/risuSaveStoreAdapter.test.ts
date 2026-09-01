import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import localforage from 'localforage'
import { describe, expect, it, vi } from 'vitest'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { nativePersistentRevisionLease } from '../nativePersistentExport'
import {
    RevisionConflictError,
    SnapshotReleasedError,
    type PersistentDataStore,
    type PersistentRevisionLease,
} from '../persistentDataStore'
import { decodeRisuSave, encodeRisuSaveBlock, RisuSaveType } from '../risuSave'
import {
    importRisuSaveToStore,
    streamRisuSaveFromLease,
    streamRisuSaveFromStore,
    withFlushedRisuSaveExport,
} from '../risuSaveStoreAdapter'
import { iteratePinnedCharacters } from '../persistentRecordIterator'
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

async function snapshotLeases(indexedDB: IDBFactory, databaseName: string): Promise<string[]> {
    const request = indexedDB.open(databaseName)
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        request.onsuccess = () => resolve(request.result)
        request.onerror = () => reject(request.error)
    })
    const transaction = database.transaction('meta', 'readonly')
    const records = await new Promise<IDBValidKey[]>((resolve, reject) => {
        const values = transaction.objectStore('meta').getAllKeys(
            IDBKeyRange.bound('snapshotLease:', 'snapshotLease:\uffff'),
        )
        values.onsuccess = () => resolve(values.result)
        values.onerror = () => reject(values.error)
    })
    database.close()
    return records.map(String)
}

async function deleteSnapshotLeases(indexedDB: IDBFactory, databaseName: string): Promise<void> {
    const keys = await snapshotLeases(indexedDB, databaseName)
    const request = indexedDB.open(databaseName)
    const database = await new Promise<IDBDatabase>((resolve, reject) => {
        request.onsuccess = () => resolve(request.result)
        request.onerror = () => reject(request.error)
    })
    const transaction = database.transaction('meta', 'readwrite')
    const meta = transaction.objectStore('meta')
    for (const key of keys) meta.delete(key)
    await new Promise<void>((resolve, reject) => {
        transaction.oncomplete = () => resolve()
        transaction.onabort = () => reject(transaction.error)
        transaction.onerror = () => reject(transaction.error)
    })
    database.close()
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
            pluginStorage: [
                { type: 'set', key: 'fixture', value: { value: 'later' } },
            ],
        })
        const remaining = await concatenate({
            [Symbol.asyncIterator]: () => iterator,
        })
        const exported = new Uint8Array(header.value.length + remaining.length)
        exported.set(header.value)
        exported.set(remaining, header.value.length)

        const decoded = await decodeRisuSave(exported)
        expect(decoded.username).toBe('Snapshot User')
        expect(decoded.pluginCustomStorage).toEqual({ fixture: { value: 'stored' } })
        expect((await store.readRoot()).value.username).toBe('Later User')
        expect((await store.readPluginStorage('fixture'))?.value).toEqual({ value: 'later' })
        expect(materialize).not.toHaveBeenCalled()
    })

    it('exports plugin storage in legacy Object.keys order from the pinned catalog', async () => {
        const database = structuredClone(risuSaveFixtureDatabase)
        const storage = JSON.parse(
            '{"zeta":"first string","10":"ten","2":0,"01":"non-index",' +
            '"4294967294":true,"4294967295":false,"__proto__":{"safe":true},' +
            '"\\uffffx":"unicode"}',
        ) as Record<string, unknown>
        database.pluginCustomStorage = storage
        const store = new IndexedDbPersistentDataStore(
            'risu-save-plugin-order',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)

        const decoded = await decodeRisuSave(
            await concatenate(streamRisuSaveFromStore(store, imported.revision)),
        )

        expect(Object.keys(decoded.pluginCustomStorage)).toEqual(Object.keys(storage))
        expect(Object.hasOwn(decoded.pluginCustomStorage, '__proto__')).toBe(true)
        expect(decoded.pluginCustomStorage.__proto__).toEqual({ safe: true })
        expect(Object.getPrototypeOf(decoded.pluginCustomStorage)).toBe(Object.prototype)
        expect(decoded.pluginCustomStorage['2']).toBe(0)
        expect(decoded.pluginCustomStorage['\uffffx']).toBe('unicode')
    })

    it('streams a supplied lease without releasing it or materializing a database', async () => {
        const store = new IndexedDbPersistentDataStore(
            'risu-save-injected-lease',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const lease = await store.acquireRevision(imported.revision)
        const release = vi.spyOn(lease, 'release')
        const materialize = vi.spyOn(store, 'materializeDatabase')

        const exported = await concatenate(streamRisuSaveFromLease(lease))

        await expect(decodeRisuSave(exported)).resolves.toEqual(risuSaveFixtureDatabase)
        expect(release).not.toHaveBeenCalled()
        expect(materialize).not.toHaveBeenCalled()
        await lease.release()
    })

    it('rejects materialization and export through an externally expired lease', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'risu-save-externally-expired-lease'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const lease = await store.acquireRevision(imported.revision)
        await deleteSnapshotLeases(indexedDB, databaseName)
        const root = (await store.readRoot()).value
        await store.commit({
            expectedRevision: imported.revision,
            root: { ...root, username: 'Committed after external lease expiry' },
        })

        const expiredLeaseStore = {
            acquireRevision: async () => lease,
        } as unknown as PersistentDataStore
        await withFlushedRisuSaveExport(
            {
                store: expiredLeaseStore,
                capturePersistentMutationToken: async () => ({
                    revision: imported.revision,
                    mutationGeneration: 0,
                }),
            },
            'external lease expiry regression',
            async (pinned) => {
                await expect(pinned.materializeDatabase()).rejects.toBeInstanceOf(
                    SnapshotReleasedError,
                )
                await expect(pinned.collectBytes()).rejects.toBeInstanceOf(
                    SnapshotReleasedError,
                )
            },
        )
        expect((await store.readRoot()).value.username).toBe(
            'Committed after external lease expiry',
        )
    })

    it('projects root and character resources while streaming from a lease', async () => {
        const database = structuredClone(risuSaveFixtureDatabase) as any
        database.pluginCustomStorage = JSON.parse(
            '{"projection":{' +
            '"direct":"assets/plugin.bin",' +
            '"nested":["assets/plugin-chain.bin","prefix assets/plugin.bin"],' +
            '"legacyNested":"assets/folder/plugin-legacy.bin",' +
            '"assets/plugin.bin":"object-key",' +
            '"__proto__":"assets/plugin-proto.bin"}}',
        )
        database.customBackground = 'assets/background.png'
        database.userIcon = 'assets/user.png'
        database.modules = [{
            assets: [['module', 'assets/module.png', 'png']],
            icon: 'assets/module-icon.png',
        }]
        database.personas = [{
            icon: 'assets/persona.png',
            embeddedModule: {
                assets: [['embedded', 'assets/embedded.png', 'png']],
                icon: 'assets/embedded-icon.png',
            },
        }]
        database.characterOrder = [{ name: 'Folder', imgFile: 'assets/folder.png' }]
        Object.assign(database.characters[0], {
            image: 'assets/character.png',
            emotionImages: [['happy', 'assets/emotion.png']],
            additionalAssets: [['prop', 'assets/prop.png', 'png']],
            vits: { files: { model: 'assets/model.onnx' } },
            ccAssets: [{ type: 'icon', uri: 'assets/card.png', name: 'card', ext: 'png' }],
        })
        const resources = [
            'assets/background.png',
            'assets/user.png',
            'assets/module.png',
            'assets/module-icon.png',
            'assets/persona.png',
            'assets/embedded.png',
            'assets/embedded-icon.png',
            'assets/folder.png',
            'assets/character.png',
            'assets/emotion.png',
            'assets/prop.png',
            'assets/model.onnx',
            'assets/card.png',
        ]
        const replacements = Object.fromEntries(
            resources.map((resource) => [resource, `remote/${resource}`]),
        )
        Object.assign(replacements, {
            'assets/plugin.bin': 'remote/assets/plugin.bin',
            'assets/plugin-chain.bin': 'assets/plugin-chain-step.bin',
            'assets/plugin-chain-step.bin': 'remote/assets/plugin-chain-final.bin',
            'assets/folder/plugin-legacy.bin': 'remote/assets/folder/plugin-legacy.bin',
            'assets/plugin-proto.bin': 'remote/assets/plugin-proto.bin',
        })
        const store = new IndexedDbPersistentDataStore(
            'risu-save-resource-projection',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(database)
        const lease = await store.acquireRevision(imported.revision)
        const release = vi.spyOn(lease, 'release')

        const exported = await concatenate(streamRisuSaveFromLease(lease, {
            replaceResources: replacements,
        }))
        const projected = await decodeRisuSave(exported) as any

        expect(projected.customBackground).toBe('remote/assets/background.png')
        expect(projected.userIcon).toBe('remote/assets/user.png')
        expect(projected.modules[0].assets[0][1]).toBe('remote/assets/module.png')
        expect(projected.modules[0].icon).toBe('remote/assets/module-icon.png')
        expect(projected.personas[0].icon).toBe('remote/assets/persona.png')
        expect(projected.personas[0].embeddedModule.assets[0][1]).toBe('remote/assets/embedded.png')
        expect(projected.personas[0].embeddedModule.icon).toBe('remote/assets/embedded-icon.png')
        expect(projected.characterOrder[0].imgFile).toBe('remote/assets/folder.png')
        expect(projected.characters[0].image).toBe('remote/assets/character.png')
        expect(projected.characters[0].emotionImages[0][1]).toBe('remote/assets/emotion.png')
        expect(projected.characters[0].additionalAssets[0][1]).toBe('remote/assets/prop.png')
        expect(projected.characters[0].vits.files.model).toBe('remote/assets/model.onnx')
        expect(projected.characters[0].ccAssets[0].uri).toBe('remote/assets/card.png')
        expect(projected.pluginCustomStorage.projection.direct).toBe('remote/assets/plugin.bin')
        expect(projected.pluginCustomStorage.projection.nested).toEqual([
            'assets/plugin-chain-step.bin',
            'prefix assets/plugin.bin',
        ])
        expect(projected.pluginCustomStorage.projection.legacyNested).toBe(
            'remote/assets/folder/plugin-legacy.bin',
        )
        expect(projected.pluginCustomStorage.projection['assets/plugin.bin']).toBe('object-key')
        expect(Object.hasOwn(projected.pluginCustomStorage.projection, '__proto__')).toBe(true)
        expect(projected.pluginCustomStorage.projection.__proto__).toBe(
            'remote/assets/plugin-proto.bin',
        )
        expect(database.customBackground).toBe('assets/background.png')
        expect(database.characters[0].image).toBe('assets/character.png')
        expect(database.pluginCustomStorage.projection.direct).toBe('assets/plugin.bin')
        expect(database.pluginCustomStorage.projection.nested[0]).toBe('assets/plugin-chain.bin')
        expect(database.pluginCustomStorage.projection.legacyNested).toBe(
            'assets/folder/plugin-legacy.bin',
        )
        expect(release).not.toHaveBeenCalled()
        await lease.release()
    })

    it('keeps an empty resource projection byte-identical', async () => {
        const store = new IndexedDbPersistentDataStore(
            'risu-save-empty-resource-projection',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const lease = await store.acquireRevision(imported.revision)

        const ordinary = await concatenate(streamRisuSaveFromLease(lease))
        const projected = await concatenate(streamRisuSaveFromLease(lease, {
            replaceResources: {},
        }))

        expect(projected).toEqual(ordinary)
        await lease.release()
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

    it('releases its temporary lease when iteration is cancelled', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'risu-save-cancelled-export'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const iterator = streamRisuSaveFromStore(store, imported.revision)[Symbol.asyncIterator]()

        await iterator.next()
        expect(await snapshotLeases(indexedDB, databaseName)).toHaveLength(1)
        await iterator.return?.(undefined)

        expect(await snapshotLeases(indexedDB, databaseName)).toEqual([])
    })

    it('exports a flushed pinned revision instead of released live message arrays', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'risu-save-flushed-pinned-export'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        let revision = imported.revision
        const liveDatabase = structuredClone(risuSaveFixtureDatabase)
        liveDatabase.characters[0].chats[0].message = []
        const capturePersistentMutationToken = vi.fn(async () => {
            const root = await store.readRoot()
            revision = (await store.commit({
                expectedRevision: revision,
                root: { ...root.value, username: 'Flushed User' },
            })).revision
            return { revision, mutationGeneration: 7 }
        })

        const exported = await withFlushedRisuSaveExport({
            store,
            capturePersistentMutationToken,
        }, 'local-backup', async (pinned) => {
            expect(pinned.mutationGeneration).toBe(7)
            expect(pinned.reader.revision).toBe(revision)
            const ids: string[] = []
            for await (const record of iteratePinnedCharacters(pinned.reader)) {
                ids.push(record.summary.id)
            }
            expect(ids).toEqual(
                risuSaveFixtureDatabase.characters.map((character) => character.chaId),
            )
            const snapshot = await pinned.materializeDatabase()
            expect(snapshot.characters[0].chats[0].message).toEqual(
                risuSaveFixtureDatabase.characters[0].chats[0].message,
            )
            expect(snapshot.characters[0].chats[0].message).not.toEqual(
                liveDatabase.characters[0].chats[0].message,
            )
            return pinned.collectBytes()
        })

        expect(capturePersistentMutationToken).toHaveBeenCalledWith('local-backup')
        expect((await decodeRisuSave(exported)).username).toBe('Flushed User')
        expect((await decodeRisuSave(exported)).characters[0].chats[0].message).toEqual(
            risuSaveFixtureDatabase.characters[0].chats[0].message,
        )
        expect(await snapshotLeases(indexedDB, databaseName)).toEqual([])
    })

    it('releases a flushed export lease when the backup fails', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'risu-save-failed-pinned-export'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const error = new Error('backup failed')

        await expect(withFlushedRisuSaveExport({
            store,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        }, 'drive-backup', async (pinned) => {
            await pinned.materializeDatabase()
            throw error
        })).rejects.toBe(error)

        expect(await snapshotLeases(indexedDB, databaseName)).toEqual([])
    })

    it('preserves the export failure when lease cleanup also fails', async () => {
        const exportError = new Error('export failed')
        const releaseError = new Error('lease cleanup failed')
        const release = vi.fn()
            .mockRejectedValueOnce(releaseError)
            .mockResolvedValueOnce(undefined)
        const lease = { release } as unknown as PersistentRevisionLease
        const store = { acquireRevision: vi.fn(async () => lease) }

        await expect(withFlushedRisuSaveExport({
            store: store as never,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: 4,
                mutationGeneration: 9,
            })),
        }, 'local-backup', async () => {
            throw exportError
        })).rejects.toBe(exportError)

        expect(release).toHaveBeenCalledTimes(2)
    })

    it('preserves a streaming export failure when lease cleanup also fails', async () => {
        const exportError = new Error('stream failed')
        const release = vi.fn()
            .mockRejectedValueOnce(new Error('lease cleanup failed'))
            .mockResolvedValueOnce(undefined)
        const lease = {
            readRoot: vi.fn(async () => {
                throw exportError
            }),
            release,
        } as unknown as PersistentRevisionLease
        const store = { acquireRevision: vi.fn(async () => lease) }

        await expect(concatenate(streamRisuSaveFromStore(
            store as never,
            4,
        ))).rejects.toBe(exportError)
        expect(release).toHaveBeenCalledTimes(2)
    })

    it('rejects a reader whose root reports a different revision before yielding bytes', async () => {
        const lease = {
            revision: 4,
            readRoot: vi.fn(async () => ({ revision: 5, value: {} })),
        } as unknown as PersistentRevisionLease
        const iterator = streamRisuSaveFromLease(lease)[Symbol.asyncIterator]()

        await expect(iterator.next()).rejects.toThrow('Root returned revision 5, expected 4')
    })

    it('counts characters from the pinned catalog without materializing the database', async () => {
        const store = new IndexedDbPersistentDataStore(
            'risu-save-pinned-character-count',
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const materialize = vi.spyOn(store, 'materializeDatabase').mockRejectedValue(
            new Error('character count must use the pinned catalog'),
        )

        const count = await withFlushedRisuSaveExport({
            store,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        }, 'sync-conflict-backup', (pinned) => pinned.countCharacters())

        expect(count).toBe(risuSaveFixtureDatabase.characters.length)
        expect(materialize).not.toHaveBeenCalled()
    })

    it('exposes the native file boundary only for a native-capable pinned lease', async () => {
        const release = vi.fn(async () => undefined)
        const lease = {
            revision: 12,
            [nativePersistentRevisionLease]: 'native-lease-12',
            release,
        } as unknown as PersistentRevisionLease
        const store = {
            acquireRevision: vi.fn(async () => lease),
        }

        await withFlushedRisuSaveExport({
            store: store as never,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: 12,
                mutationGeneration: 0,
            })),
        }, 'local-backup', async (pinned) => {
            expect(pinned.withNativeFile).toBeTypeOf('function')
        })

        expect(store.acquireRevision).toHaveBeenCalledWith(12)
        expect(release).toHaveBeenCalledOnce()
    })

    it('rejects a stale revision before yielding any bytes', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'risu-save-stale-export'
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        const iterator = streamRisuSaveFromStore(store, imported.revision - 1)[Symbol.asyncIterator]()

        await expect(iterator.next()).rejects.toBeInstanceOf(RevisionConflictError)
        expect(await snapshotLeases(indexedDB, databaseName)).toEqual([])
    })
})
