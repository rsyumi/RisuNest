import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { BlobStore } from '../storage/blobStore'
import type { Database } from '../storage/database.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'
import { decodeRisuSave } from '../storage/risuSave'
import { risuSaveFixtureDatabase } from '../storage/tests/risuSaveFixtures'
import { getBackupInlayName } from './backupAssets'

const state = vi.hoisted(() => ({
    blobStore: null as BlobStore | null,
    currentDatabase: null as Database | null,
    runtime: null as PersistentDataRuntime | null,
    written: new Map<string, Uint8Array>(),
    nativeFile: new Uint8Array([7, 8, 9]),
    openNativeFile: vi.fn(),
    nativeFileClose: vi.fn(async () => undefined),
    fullReadFile: vi.fn(async () => new Uint8Array([99])),
    snapshotSeenByColdStorage: null as Database | null,
    coldStoragePayloads: [] as Array<{
        key: string
        backupName: string
        value: unknown
    }>,
    missingColdStorageKeys: [] as string[],
    invalidColdStorageKeys: [] as string[],
    confirmColdStorage: vi.fn(async () => true),
    getUncleanables: vi.fn(async () => ['assets/second-read.png']),
}))

vi.mock('../alert', () => ({
    alertConfirm: vi.fn(async () => true),
    alertError: vi.fn(),
    alertMd: vi.fn(),
    alertNormal: vi.fn(),
    alertStore: { set: vi.fn() },
    alertWait: vi.fn(),
}))

vi.mock('../globalApi.svelte', () => ({
    forageStorage: { Init: vi.fn(async () => undefined), isAccount: false },
    getUncleanables: state.getUncleanables,
    LocalWriter: class {
        async init() {
            return true
        }

        async writeBackup(name: string, bytes: Uint8Array) {
            state.written.set(name, bytes.slice())
        }

        async writeBackupStream(
            name: string,
            byteLength: number,
            chunks: AsyncIterable<Uint8Array>,
        ) {
            const value = new Uint8Array(byteLength)
            let offset = 0
            for await (const chunk of chunks) {
                value.set(chunk, offset)
                offset += chunk.byteLength
            }
            if (offset !== byteLength) throw new Error('unexpected test stream length')
            state.written.set(name, value)
        }

        async close() {}
    },
}))

vi.mock('../storage/platformBlobStore', () => ({
    resolveBlobStore: async () => state.blobStore,
}))

vi.mock('../storage/database.svelte', () => ({
    getDatabase: () => state.currentDatabase,
    presetTemplate: {},
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => state.runtime,
    publishCurrentOfficialRevision: vi.fn(async () => undefined),
    replacePersistentDatabase: vi.fn(async () => undefined),
}))

vi.mock('../process/coldstorage.svelte', () => ({
    collectColdStorageBackupPayloads: vi.fn(async (database: Database) => {
        state.snapshotSeenByColdStorage = database
        return {
            payloads: state.coldStoragePayloads,
            missingKeys: state.missingColdStorageKeys,
            invalidKeys: state.invalidColdStorageKeys,
        }
    }),
    confirmIncompleteColdStorageOperation: state.confirmColdStorage,
    getColdStorageBackupKey: vi.fn(),
    getColdStorageItem: vi.fn(),
    isColdStorageBackupData: vi.fn(),
    listColdDataKeys: vi.fn(async () => []),
    setLocalColdStorageItem: vi.fn(),
}))

vi.mock('src/ts/platform', () => ({
    isTauri: true,
    isTauriDesktop: true,
}))

vi.mock('@tauri-apps/plugin-fs', () => ({
    BaseDirectory: {},
    open: state.openNativeFile,
    readFile: state.fullReadFile,
    writeFile: vi.fn(),
}))

vi.mock('@tauri-apps/plugin-process', () => ({ relaunch: vi.fn() }))
vi.mock('../util', () => ({
    decryptBuffer: vi.fn(),
    encryptBuffer: vi.fn(),
    sleep: vi.fn(async () => undefined),
}))
vi.mock('../characterCards', () => ({ hubURL: 'https://hub.invalid' }))
vi.mock('src/lang', () => ({ language: {} }))

function emptyBlobStore(): BlobStore {
    return {
        put: vi.fn(),
        read: vi.fn(async () => null),
        stat: vi.fn(async () => null),
        list: vi.fn(async () => []),
        remove: vi.fn(),
        resolveUrl: vi.fn(async () => null),
    }
}

describe('local backup persistent snapshot', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        state.written.clear()
        state.snapshotSeenByColdStorage = null
        state.coldStoragePayloads = []
        state.missingColdStorageKeys = []
        state.invalidColdStorageKeys = []
        state.blobStore = emptyBlobStore()
        state.nativeFileClose.mockClear()
        state.fullReadFile.mockClear()
        state.openNativeFile.mockReset()
        let offset = 0
        state.openNativeFile.mockResolvedValue({
            read: vi.fn(async (buffer: Uint8Array) => {
                if (offset >= state.nativeFile.byteLength) return null
                const length = Math.min(2, state.nativeFile.byteLength - offset)
                buffer.set(state.nativeFile.subarray(offset, offset + length))
                offset += length
                return length
            }),
            close: state.nativeFileClose,
        })
    })

    it('writes database and cold enumeration from the flushed store revision', async () => {
        const store = new IndexedDbPersistentDataStore(
            `local-backup-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const persisted = structuredClone(risuSaveFixtureDatabase) as Database
        const imported = await store.replaceFromDatabase(persisted)
        const live = structuredClone(persisted)
        live.characters[0].chats[0].message = []
        state.currentDatabase = live
        state.runtime = {
            store,
            revision: imported.revision,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        } as unknown as PersistentDataRuntime

        const { SaveLocalBackup } = await import('./backuplocal')
        await SaveLocalBackup()

        const databaseBytes = state.written.get('database.risudat')
        expect(databaseBytes).toBeDefined()
        const backedUp = await decodeRisuSave(databaseBytes!)
        expect(backedUp.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(backedUp.pluginCustomStorage).toEqual(persisted.pluginCustomStorage)
        expect(state.snapshotSeenByColdStorage?.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(state.runtime.capturePersistentMutationToken).toHaveBeenCalledWith('local-backup')
    })

    it('writes a partial backup database from the flushed store revision', async () => {
        const store = new IndexedDbPersistentDataStore(
            `partial-local-backup-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const persisted = structuredClone(risuSaveFixtureDatabase) as Database
        const imported = await store.replaceFromDatabase(persisted)
        const live = structuredClone(persisted)
        live.characters[0].chats[0].message = []
        state.currentDatabase = live
        state.runtime = {
            store,
            revision: imported.revision,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        } as unknown as PersistentDataRuntime
        state.coldStoragePayloads = [{
            key: 'partial-first',
            backupName: 'coldstorage_partial-first.json',
            value: { message: [{ role: 'user', data: 'first partial payload' }] },
        }, {
            key: 'partial-second',
            backupName: 'coldstorage_partial-second.json',
            value: { message: [{ role: 'user', data: 'second partial payload' }] },
        }]

        const { SavePartialLocalBackup } = await import('./backuplocal')
        await SavePartialLocalBackup()

        const databaseBytes = state.written.get('database.risudat')
        expect(databaseBytes).toBeDefined()
        const backedUp = await decodeRisuSave(databaseBytes!)
        expect(backedUp.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(backedUp.pluginCustomStorage).toEqual(persisted.pluginCustomStorage)
        expect(state.runtime.capturePersistentMutationToken).toHaveBeenCalledWith(
            'partial-local-backup',
        )
        expect([...state.written.entries()].filter(([name]) => name.startsWith('coldstorage_'))).toEqual(
            state.coldStoragePayloads.map((payload) => [
                payload.backupName,
                new TextEncoder().encode(JSON.stringify(payload.value)),
            ]),
        )
    })

    it('uses pinned cold asset and inlay references without rereading mutable cold storage', async () => {
        const store = new IndexedDbPersistentDataStore(
            `cold-inlay-local-backup-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const persisted = structuredClone(risuSaveFixtureDatabase) as Database
        const imported = await store.replaceFromDatabase(persisted)
        state.currentDatabase = persisted
        state.runtime = {
            store,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        } as unknown as PersistentDataRuntime
        state.coldStoragePayloads = [{
            key: 'cold-char',
            backupName: 'coldstorage_cold-char.json',
            value: {
                character: {
                    ...persisted.characters[0],
                    image: 'assets/cold-pinned.png',
                    chats: [{ message: [{ data: '{{inlay::cold-inlay}}' }] }],
                },
            },
        }, {
            key: 'cold-message',
            backupName: 'coldstorage_cold-message.json',
            value: { message: [{ role: 'user', data: 'second payload' }] },
        }]
        const inlayBytes = new Uint8Array([4, 5, 6])
        state.blobStore = {
            ...emptyBlobStore(),
            list: vi.fn(async () => [
                {
                    key: 'cold-inlay', kind: 'inlay' as const, size: 3, mime: 'image/png',
                    name: 'cold.png', ext: 'png', inlayType: 'image' as const,
                },
                {
                    key: 'post-pin-orphan', kind: 'inlay' as const, size: 3, mime: 'image/png',
                    name: 'orphan.png', ext: 'png', inlayType: 'image' as const,
                },
            ]),
            read: vi.fn(async (key: string) => [
                'cold-inlay',
                'assets/cold-pinned.png',
                'assets/second-read.png',
            ].includes(key) ? inlayBytes : null),
        }

        const { SaveLocalBackup } = await import('./backuplocal')
        await SaveLocalBackup()

        expect(state.written.has(getBackupInlayName('cold-inlay'))).toBe(true)
        expect(state.written.has(getBackupInlayName('post-pin-orphan'))).toBe(false)
        expect(state.written.has('cold-pinned.png')).toBe(true)
        expect(state.written.has('second-read.png')).toBe(false)
        expect([...state.written.entries()].filter(([name]) => name.startsWith('coldstorage_'))).toEqual(
            state.coldStoragePayloads.map((payload) => [
                payload.backupName,
                new TextEncoder().encode(JSON.stringify(payload.value)),
            ]),
        )
        expect(state.getUncleanables).not.toHaveBeenCalled()
    })

    it('preserves missing and invalid cold storage confirmation before writing', async () => {
        const store = new IndexedDbPersistentDataStore(
            `cold-confirm-local-backup-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(structuredClone(risuSaveFixtureDatabase))
        state.currentDatabase = structuredClone(risuSaveFixtureDatabase)
        state.runtime = {
            store,
            revision: imported.revision,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        } as unknown as PersistentDataRuntime
        state.missingColdStorageKeys = ['missing-cold']
        state.invalidColdStorageKeys = ['invalid-cold']
        state.confirmColdStorage.mockResolvedValueOnce(false)

        const { SaveLocalBackup } = await import('./backuplocal')
        await SaveLocalBackup()

        expect(state.confirmColdStorage).toHaveBeenCalledWith(
            expect.any(Object),
            ['missing-cold', 'invalid-cold'],
            'backup',
        )
        expect(state.written.size).toBe(0)
    })

    it('streams a native pinned export into the local backup entry', async () => {
        const collectBytes = vi.fn(async () => new Uint8Array([1]))
        const events: string[] = []
        const withNativeFile = vi.fn(async (_options, callback) => {
            try {
                return await callback({
                    path: 'C:\\app\\persistent\\exports\\risusave-test.risudat',
                    bytes: 3,
                })
            } finally {
                events.push('cleanup')
            }
        })
        const writeBackupStream = vi.fn(async (
            name: string,
            byteLength: number,
            chunks: AsyncIterable<Uint8Array>,
        ) => {
            const values: number[] = []
            for await (const chunk of chunks) values.push(...chunk)
            expect(name).toBe('database.risudat')
            expect(byteLength).toBe(3)
            expect(values).toEqual([7, 8, 9])
        })
        const { writePinnedLocalBackupDatabase } = await import('./backuplocal')

        await expect(writePinnedLocalBackupDatabase({ writeBackupStream } as any, {
            revision: 3,
            mutationGeneration: 0,
            countCharacters: vi.fn(),
            materializeDatabase: vi.fn(),
            stream: vi.fn(),
            collectBytes,
            withNativeFile,
        })).resolves.toBeUndefined()

        expect(withNativeFile).toHaveBeenCalledOnce()
        expect(withNativeFile.mock.calls[0][0]).toEqual({ omitAccount: true })
        expect(writeBackupStream).toHaveBeenCalledOnce()
        expect(state.openNativeFile).toHaveBeenCalledWith(
            'C:\\app\\persistent\\exports\\risusave-test.risudat',
            { read: true },
        )
        expect(state.nativeFileClose).toHaveBeenCalledOnce()
        expect(events).toEqual(['cleanup'])
        expect(state.fullReadFile).not.toHaveBeenCalled()
        expect(collectBytes).not.toHaveBeenCalled()
    })

    it('closes and cleans the native export when its source read fails', async () => {
        const sourceError = new Error('source failed')
        state.openNativeFile.mockResolvedValueOnce({
            read: vi.fn().mockRejectedValue(sourceError),
            close: state.nativeFileClose,
        })
        const cleanup = vi.fn()
        const withNativeFile = vi.fn(async (_options, callback) => {
            try {
                return await callback({ path: 'native.risudat', bytes: 3 })
            } finally {
                cleanup()
            }
        })
        const writer = {
            writeBackupStream: async (_name: string, _length: number, chunks: AsyncIterable<Uint8Array>) => {
                for await (const _chunk of chunks) {
                    // consume the source
                }
            },
        }
        const { writePinnedLocalBackupDatabase } = await import('./backuplocal')

        await expect(writePinnedLocalBackupDatabase(writer as any, {
            revision: 3,
            mutationGeneration: 0,
            countCharacters: vi.fn(),
            materializeDatabase: vi.fn(),
            stream: vi.fn(),
            collectBytes: vi.fn(),
            withNativeFile,
        })).rejects.toBe(sourceError)

        expect(state.nativeFileClose).toHaveBeenCalledOnce()
        expect(cleanup).toHaveBeenCalledOnce()
    })

    it('closes and cleans the native export when the destination fails', async () => {
        const destinationError = new Error('destination failed')
        const cleanup = vi.fn()
        const withNativeFile = vi.fn(async (_options, callback) => {
            try {
                return await callback({ path: 'native.risudat', bytes: 3 })
            } finally {
                cleanup()
            }
        })
        const writer = {
            writeBackupStream: async (_name: string, _length: number, chunks: AsyncIterable<Uint8Array>) => {
                for await (const _chunk of chunks) throw destinationError
            },
        }
        const { writePinnedLocalBackupDatabase } = await import('./backuplocal')

        await expect(writePinnedLocalBackupDatabase(writer as any, {
            revision: 3,
            mutationGeneration: 0,
            countCharacters: vi.fn(),
            materializeDatabase: vi.fn(),
            stream: vi.fn(),
            collectBytes: vi.fn(),
            withNativeFile,
        })).rejects.toBe(destinationError)

        expect(state.nativeFileClose).toHaveBeenCalledOnce()
        expect(cleanup).toHaveBeenCalledOnce()
    })

    it('closes the native source when a chunk consumer returns early', async () => {
        const { streamNativeBackupFile } = await import('./backuplocal')
        const chunks = streamNativeBackupFile('native.risudat', 3)

        await expect(chunks.next()).resolves.toEqual({
            done: false,
            value: new Uint8Array([7, 8]),
        })
        await chunks.return(undefined)

        expect(state.nativeFileClose).toHaveBeenCalledOnce()
    })
})
