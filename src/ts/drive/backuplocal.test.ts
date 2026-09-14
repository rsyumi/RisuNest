import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { BlobStore } from '../storage/blobStore'
import type { Database } from '../storage/database.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'
import { decodeRisuSave, encodeRisuSaveLegacy } from '../storage/risuSave'
import { risuSaveFixtureDatabase } from '../storage/tests/risuSaveFixtures'
import { sleep } from '../util'
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
    restoredCold: new Map<string, unknown>(),
    restoreEvents: [] as string[],
    replacePersistentDatabase: vi.fn(async (_database: Database, _reason: string) => undefined),
    confirmColdStorage: vi.fn(async () => true),
    getUncleanables: vi.fn(async () => ['assets/second-read.png']),
    fallbackContext: {
        signal: new AbortController().signal,
        onStatus: vi.fn(),
        setSource: vi.fn(),
        setPartialWritesPossible: vi.fn(),
    },
}))

vi.mock('../alert', () => ({
    alertConfirm: vi.fn(async () => true),
    alertError: vi.fn(),
    alertMd: vi.fn(),
    alertNormal: vi.fn(),
    alertStore: { set: vi.fn() },
    alertWait: vi.fn(),
}))

vi.mock('../nativeLog', () => ({ recordNativeLogError: vi.fn(async () => undefined) }))

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
    replacePersistentDatabase: (database: Database, reason: string) => (
        state.replacePersistentDatabase(database, reason)
    ),
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
    getColdStorageBackupKey: (name: string) => {
        const match = /^coldstorage_(.+)\.json$/.exec(name)
        return match?.[1] ?? null
    },
    getColdStorageItem: async (key: string) => structuredClone(state.restoredCold.get(key) ?? null),
    isColdStorageBackupData: (value: unknown) => Boolean(
        value
        && typeof value === 'object'
        && ('character' in value || 'message' in value),
    ),
    listColdDataKeys: async (database: Database) => database.characters
        .map((character) => character.coldstorage)
        .filter((key): key is string => Boolean(key)),
    setLocalColdStorageItem: vi.fn(async (key: string, value: unknown) => {
        state.restoreEvents.push(`cold:${key}`)
        state.restoredCold.set(key, structuredClone(value))
        return true
    }),
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
vi.mock('./legacyLocalBackupFileRouteProduction.svelte', () => ({
    exportLegacyLocalBackupFromSystemPicker: vi.fn(async () => {
        throw { code: 'capability-unavailable' }
    }),
    // The real route falls back inside the shared operation; the mock hands the
    // WebView importer the same context it would receive there.
    importLegacyLocalBackupFromSystemPicker: vi.fn(async (
        options?: { onNativeFallback?(context: unknown): Promise<unknown> },
    ) => await options?.onNativeFallback?.(state.fallbackContext) ?? null),
}))
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
        state.restoredCold.clear()
        state.restoreEvents = []
        state.replacePersistentDatabase.mockReset().mockImplementation(async (_database, reason) => {
            expect(reason).toBe('local-backup')
            state.restoreEvents.push('database')
        })
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

    it('restores cold payloads before replacing the local backup database', async () => {
        const coldKey = 'local-restore-cold'
        const database = structuredClone(risuSaveFixtureDatabase) as Database
        database.characters[0].coldstorage = coldKey
        const cold = { message: [{ role: 'user', data: 'restored cold payload' }] }
        const encodeEntry = (name: string, data: Uint8Array) => {
            const encodedName = new TextEncoder().encode(name)
            const record = new Uint8Array(8 + encodedName.byteLength + data.byteLength)
            const view = new DataView(record.buffer)
            view.setUint32(0, encodedName.byteLength, true)
            record.set(encodedName, 4)
            view.setUint32(4 + encodedName.byteLength, data.byteLength, true)
            record.set(data, 8 + encodedName.byteLength)
            return record
        }
        const entries = [
            encodeEntry(`coldstorage_${coldKey}.json`, new TextEncoder().encode(JSON.stringify(cold))),
            encodeEntry('database.risudat', encodeRisuSaveLegacy(database, 'compression')),
        ]
        const archive = new Uint8Array(entries.reduce((sum, entry) => sum + entry.byteLength, 0))
        let offset = 0
        for (const entry of entries) {
            archive.set(entry, offset)
            offset += entry.byteLength
        }
        const file = {
            name: 'backup.bin',
            size: archive.byteLength,
            stream: () => new ReadableStream<Uint8Array>({
                start(controller) {
                    controller.enqueue(archive)
                    controller.close()
                },
            }),
        } as File
        const input = {
            type: '',
            accept: '',
            files: [file],
            onchange: null as null | (() => Promise<void>),
            click: vi.fn(),
            remove: vi.fn(),
        }
        const createElement = vi.spyOn(document, 'createElement')
            .mockReturnValueOnce(input as unknown as HTMLInputElement)
        const { LoadLocalBackup } = await import('./backuplocal')
        const { alertWait } = await import('../alert')

        const pending = LoadLocalBackup()
        await vi.waitFor(() => expect(input.onchange).not.toBeNull())
        await input.onchange?.()
        await pending
        createElement.mockRestore()

        expect(state.restoreEvents).toEqual([`cold:${coldKey}`, 'database'])
        expect(state.restoredCold.get(coldKey)).toEqual(cold)
        expect(state.replacePersistentDatabase).toHaveBeenCalledOnce()
        expect(alertWait).not.toHaveBeenCalled()

        const context = state.fallbackContext
        expect(context.setSource).toHaveBeenCalledExactlyOnceWith({ name: 'backup.bin', bytes: archive.byteLength })
        expect(context.setPartialWritesPossible).toHaveBeenCalledExactlyOnceWith(true)
        const stages = context.onStatus.mock.calls.map(([status]) => status.detail?.stage)
        expect(stages[0]).toBe('reading-archive')
        expect(stages.filter((stage, index) => stage !== stages[index - 1])).toEqual([
            'reading-archive', 'decoding-database', 'activating', 'restarting-app',
        ])
        const last = context.onStatus.mock.calls.at(-1)?.[0]
        expect(last?.kind).toBe('restore-legacy-local-backup')
        expect(last?.progress).toEqual({
            completedBytes: archive.byteLength,
            totalBytes: archive.byteLength,
            completedItems: 2,
            totalItems: 2,
        })
        expect(last?.detail?.counts).toMatchObject({
            entriesRead: 2,
            entriesTotal: 2,
            coldStorage: 1,
            assets: 0,
            skipped: 0,
            characters: database.characters.length,
            charactersTotal: database.characters.length,
            presets: database.botPresets.length,
        })
        expect(JSON.stringify(context.onStatus.mock.calls)).toContain(`coldstorage_${coldKey}.json`)
    })

    it('skips a damaged RisuNest inlay envelope instead of restoring it as an asset', async () => {
        const database = structuredClone(risuSaveFixtureDatabase) as Database
        const encodeEntry = (name: string, data: Uint8Array) => {
            const encodedName = new TextEncoder().encode(name)
            const record = new Uint8Array(8 + encodedName.byteLength + data.byteLength)
            const view = new DataView(record.buffer)
            view.setUint32(0, encodedName.byteLength, true)
            record.set(encodedName, 4)
            view.setUint32(4 + encodedName.byteLength, data.byteLength, true)
            record.set(data, 8 + encodedName.byteLength)
            return record
        }
        const damagedInlayName = getBackupInlayName('damaged-inlay')
        const entries = [
            encodeEntry(damagedInlayName, Uint8Array.of(9, 0, 0, 0, 1, 2)),
            encodeEntry('database.risudat', encodeRisuSaveLegacy(database, 'compression')),
        ]
        const archive = new Uint8Array(entries.reduce((sum, entry) => sum + entry.byteLength, 0))
        let archiveOffset = 0
        for (const entry of entries) {
            archive.set(entry, archiveOffset)
            archiveOffset += entry.byteLength
        }
        const file = {
            name: 'damaged-inlay.bin',
            size: archive.byteLength,
            stream: () => new ReadableStream<Uint8Array>({
                start(controller) {
                    controller.enqueue(archive)
                    controller.close()
                },
            }),
        } as File
        const input = {
            type: '',
            accept: '',
            files: [file],
            onchange: null as null | (() => Promise<void>),
            click: vi.fn(),
            remove: vi.fn(),
        }
        const createElement = vi.spyOn(document, 'createElement')
            .mockReturnValueOnce(input as unknown as HTMLInputElement)
        const { importLegacyBackupWithWebView } = await import('./backuplocal')
        const context = {
            signal: new AbortController().signal,
            onStatus: vi.fn(),
            setSource: vi.fn(),
            setPartialWritesPossible: vi.fn(),
        }

        const pending = importLegacyBackupWithWebView(context)
        await vi.waitFor(() => expect(input.onchange).not.toBeNull())
        await input.onchange?.()
        const result = await pending
        createElement.mockRestore()

        expect(result).toEqual({ warningCodes: ['invalid-inlay-entry'] })
        expect(state.blobStore?.put).not.toHaveBeenCalled()
        const last = context.onStatus.mock.calls.at(-1)?.[0]
        expect(last?.detail?.counts).toMatchObject({
            entriesRead: 2,
            inlays: 0,
            assets: 0,
            skipped: 1,
        })
    })

    it('stops a WebView import at the next chunk after cancellation and flags partial writes', async () => {
        const coldKey = 'partial-cold'
        const cold = { message: [{ role: 'user', data: 'already written' }] }
        const encodedName = new TextEncoder().encode(`coldstorage_${coldKey}.json`)
        const data = new TextEncoder().encode(JSON.stringify(cold))
        const entry = new Uint8Array(8 + encodedName.byteLength + data.byteLength)
        const view = new DataView(entry.buffer)
        view.setUint32(0, encodedName.byteLength, true)
        entry.set(encodedName, 4)
        view.setUint32(4 + encodedName.byteLength, data.byteLength, true)
        entry.set(data, 8 + encodedName.byteLength)
        const controller = new AbortController()
        // The importer yields between entries; cancelling there leaves the cold payload written.
        vi.mocked(sleep).mockImplementationOnce(async () => controller.abort())
        const file = {
            name: 'partial.bin',
            size: entry.byteLength,
            stream: () => new ReadableStream<Uint8Array>({
                start(stream) {
                    stream.enqueue(entry)
                    stream.close()
                },
            }),
        } as File
        const input = {
            type: '',
            accept: '',
            files: [file],
            onchange: null as null | (() => Promise<void>),
            click: vi.fn(),
            remove: vi.fn(),
        }
        const createElement = vi.spyOn(document, 'createElement')
            .mockReturnValueOnce(input as unknown as HTMLInputElement)
        const { importLegacyBackupWithWebView } = await import('./backuplocal')
        const context = {
            signal: controller.signal,
            onStatus: vi.fn(),
            setSource: vi.fn(),
            setPartialWritesPossible: vi.fn(),
        }

        const pending = importLegacyBackupWithWebView(context)
        await vi.waitFor(() => expect(input.onchange).not.toBeNull())
        await input.onchange?.()
        await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        createElement.mockRestore()

        expect(state.restoredCold.get(coldKey)).toEqual(cold)
        expect(context.setPartialWritesPossible).toHaveBeenCalledWith(true)
        expect(state.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('resolves to nothing when the WebView picker is closed or the operation is cancelled first', async () => {
        const controller = new AbortController()
        controller.abort()
        const { importLegacyBackupWithWebView } = await import('./backuplocal')
        const context = {
            signal: controller.signal,
            onStatus: vi.fn(),
            setSource: vi.fn(),
            setPartialWritesPossible: vi.fn(),
        }

        await expect(importLegacyBackupWithWebView(context)).resolves.toBeNull()
        expect(context.setSource).not.toHaveBeenCalled()
    })

    it('records the native failure code and reason for diagnostics without a second alert', async () => {
        const { importLegacyLocalBackupFromSystemPicker } =
            await import('./legacyLocalBackupFileRouteProduction.svelte')
        const { NativeFileJobError } = await import('../storage/nativeFileJobs')
        const { alertError } = await import('../alert')
        const { recordNativeLogError } = await import('../nativeLog')
        vi.mocked(recordNativeLogError).mockClear()
        vi.mocked(alertError).mockClear()
        vi.mocked(importLegacyLocalBackupFromSystemPicker).mockRejectedValueOnce(
            new NativeFileJobError('revision-conflict', 'The database changed during import'),
        )
        const { LoadLocalBackup } = await import('./backuplocal')
        await LoadLocalBackup()
        expect(recordNativeLogError).toHaveBeenCalledWith(
            'Backup import failed [revision-conflict]: The database changed during import',
        )
        expect(alertError).not.toHaveBeenCalled()
    })

    it('distinguishes an imported database from a failed screen refresh in diagnostics', async () => {
        const { importLegacyLocalBackupFromSystemPicker } =
            await import('./legacyLocalBackupFileRouteProduction.svelte')
        const { NativeFileJobActivationCommittedError } = await import('../storage/nativeFileJobs')
        const { alertError } = await import('../alert')
        const { recordNativeLogError } = await import('../nativeLog')
        vi.mocked(recordNativeLogError).mockClear()
        vi.mocked(alertError).mockClear()
        vi.mocked(importLegacyLocalBackupFromSystemPicker).mockRejectedValueOnce(
            new NativeFileJobActivationCommittedError(2, new Error('Synthetic refresh failed')),
        )
        const { LoadLocalBackup } = await import('./backuplocal')
        await LoadLocalBackup()
        expect(recordNativeLogError).toHaveBeenCalledWith(
            expect.stringContaining('Backup data was imported, but the app could not refresh'),
        )
        expect(recordNativeLogError).toHaveBeenCalledWith(
            expect.stringContaining('activation-committed-refresh-failed'),
        )
        expect(recordNativeLogError).toHaveBeenCalledWith(expect.stringContaining('Synthetic refresh failed'))
        expect(alertError).not.toHaveBeenCalled()
    })

    it('stays silent for cancelled imports because the dialog already reported them', async () => {
        const { importLegacyLocalBackupFromSystemPicker } =
            await import('./legacyLocalBackupFileRouteProduction.svelte')
        const { alertError } = await import('../alert')
        const { recordNativeLogError } = await import('../nativeLog')
        vi.mocked(recordNativeLogError).mockClear()
        vi.mocked(alertError).mockClear()
        vi.mocked(importLegacyLocalBackupFromSystemPicker).mockRejectedValueOnce(
            new DOMException('cancelled', 'AbortError'),
        )
        const { LoadLocalBackup } = await import('./backuplocal')
        await LoadLocalBackup()
        expect(recordNativeLogError).not.toHaveBeenCalled()
        expect(alertError).not.toHaveBeenCalled()
    })

    it('writes database and cold enumeration from the flushed store revision', async () => {
        const store = new IndexedDbPersistentDataStore(
            `local-backup-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const persisted = structuredClone(risuSaveFixtureDatabase) as Database
        const pluginAssetKey = 'assets/plugin-storage.bin'
        const pluginAssetBytes = Uint8Array.of(11, 22, 33)
        persisted.pluginCustomStorage = {
            nested: { asset: pluginAssetKey },
        }
        const imported = await store.replaceFromDatabase(persisted)
        const materializeDatabase = vi.spyOn(store, 'materializeDatabase').mockRejectedValue(
            new Error('local backup must not materialize the database'),
        )
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
        state.blobStore = {
            ...emptyBlobStore(),
            read: vi.fn(async (key: string) => key === pluginAssetKey
                ? pluginAssetBytes.slice()
                : null),
        }

        const { SaveLocalBackup } = await import('./backuplocal')
        await SaveLocalBackup()

        const databaseBytes = state.written.get('database.risudat')
        expect(databaseBytes).toBeDefined()
        const backedUp = await decodeRisuSave(databaseBytes!)
        expect(backedUp.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(backedUp.pluginCustomStorage).toEqual(persisted.pluginCustomStorage)
        expect(state.written.get('plugin-storage.bin')).toEqual(pluginAssetBytes)
        expect(state.snapshotSeenByColdStorage).toEqual({ characters: [] })
        expect(materializeDatabase).not.toHaveBeenCalled()
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
        persisted.pluginCustomStorage = {
            zero: 0,
            nested: {
                asset: 'assets/plugin-partial.bin',
                inlay: '{{inlay::plugin-partial-inlay}}',
            },
        }
        const imported = await store.replaceFromDatabase(persisted)
        const materializeDatabase = vi.spyOn(store, 'materializeDatabase').mockRejectedValue(
            new Error('partial backup must not materialize the database'),
        )
        const acquireRevision = store.acquireRevision.bind(store)
        let readPluginStorage: ReturnType<typeof vi.fn> | undefined
        vi.spyOn(store, 'acquireRevision').mockImplementation(async (revision) => {
            const lease = await acquireRevision(revision)
            const readPinnedPluginStorage = lease.readPluginStorage.bind(lease)
            readPluginStorage = vi.fn(readPinnedPluginStorage)
            lease.readPluginStorage = readPluginStorage
            return lease
        })
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
        state.blobStore = {
            ...emptyBlobStore(),
            read: vi.fn(async (key: string) => [
                'assets/plugin-partial.bin',
                'plugin-partial-inlay',
            ].includes(key) ? Uint8Array.of(7, 7) : null),
            list: vi.fn(async () => [{
                key: 'plugin-partial-inlay',
                kind: 'inlay' as const,
                size: 2,
                mime: 'application/octet-stream',
                name: 'partial.bin',
                ext: 'bin',
                inlayType: 'signature' as const,
            }]),
        }

        const { SavePartialLocalBackup } = await import('./backuplocal')
        await SavePartialLocalBackup()

        const databaseBytes = state.written.get('database.risudat')
        expect(databaseBytes).toBeDefined()
        const backedUp = await decodeRisuSave(databaseBytes!)
        expect(backedUp.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(backedUp.pluginCustomStorage).toEqual(persisted.pluginCustomStorage)
        expect(backedUp.pluginCustomStorage.zero).toBe(0)
        expect(backedUp.pluginCustomStorage.nested).toEqual({
            asset: 'assets/plugin-partial.bin',
            inlay: '{{inlay::plugin-partial-inlay}}',
        })
        expect(state.written.has('plugin-partial.bin')).toBe(false)
        expect(state.written.has(getBackupInlayName('plugin-partial-inlay'))).toBe(false)
        expect(readPluginStorage).toHaveBeenCalledTimes(2)
        expect(readPluginStorage).toHaveBeenNthCalledWith(1, 'zero')
        expect(readPluginStorage).toHaveBeenNthCalledWith(2, 'nested')
        expect(state.snapshotSeenByColdStorage).toEqual({ characters: [] })
        expect(materializeDatabase).not.toHaveBeenCalled()
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

    it('reads a partial backup asset from the normalized key after the raw key misses', async () => {
        const store = new IndexedDbPersistentDataStore(
            `partial-local-backslash-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const persisted = structuredClone(risuSaveFixtureDatabase) as Database
        persisted.characters[0].image = 'assets\\partial-profile.png'
        const imported = await store.replaceFromDatabase(persisted)
        state.currentDatabase = structuredClone(persisted)
        state.runtime = {
            store,
            revision: imported.revision,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        } as unknown as PersistentDataRuntime
        const profileBytes = Uint8Array.of(4, 2)
        const read = vi.fn(async (key: string) => key === 'assets/partial-profile.png'
            ? profileBytes.slice()
            : null)
        state.blobStore = {
            ...emptyBlobStore(),
            read,
        }

        const { SavePartialLocalBackup } = await import('./backuplocal')
        await SavePartialLocalBackup()

        expect(read).toHaveBeenCalledWith('assets\\partial-profile.png')
        expect(read).toHaveBeenCalledWith('assets/partial-profile.png')
        expect(state.written.get('assets\\partial-profile.png')).toEqual(profileBytes)
        const { alertMd, alertNormal } = await import('../alert')
        expect(alertMd).not.toHaveBeenCalled()
        expect(alertNormal).toHaveBeenCalledWith('Success')
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
            reader: { revision: 3 } as never,
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
            reader: { revision: 3 } as never,
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
            reader: { revision: 3 } as never,
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
