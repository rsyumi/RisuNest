import 'fake-indexeddb/auto'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { BlobStore, BlobMetadata, BlobWriteMetadata } from 'src/ts/storage/blobStore'
import type { Database } from 'src/ts/storage/database.svelte'
import type { CommittedApplyOutcome } from 'src/ts/storage/persistentDataRuntime'
import { encodeRisuSaveLegacy } from 'src/ts/storage/risuSave'
import { risuSaveFixtureDatabase } from 'src/ts/storage/tests/risuSaveFixtures'
import { encodeBackupInlayEntry, getBackupInlayName } from 'src/ts/drive/backupAssets'

const state = vi.hoisted(() => ({
    blobs: new Map<string, Uint8Array>(),
    metadata: new Map<string, BlobMetadata>(),
    afterPut: vi.fn(),
    replace: vi.fn(async (_database: Database): Promise<CommittedApplyOutcome> => ({
        kind: 'committed', revision: 2, projection: 'applied',
    } as const)),
}))
vi.mock('src/ts/alert', () => ({
    alertConfirm: vi.fn(async () => true), alertError: vi.fn(), alertMd: vi.fn(),
    alertNormal: vi.fn(), alertStore: { set: vi.fn() }, alertWait: vi.fn(),
}))
vi.mock('src/ts/nativeLog', () => ({ recordNativeLogError: vi.fn(async () => undefined) }))
vi.mock('src/ts/globalApi.svelte', () => ({
    forageStorage: { Init: vi.fn(async () => undefined), isAccount: false },
    getUncleanables: vi.fn(async () => []),
    LocalWriter: class {},
}))
vi.mock('src/ts/storage/platformBlobStore', () => ({
    resolveBlobStore: async () => ({
        put: async (key: string, bytes: Uint8Array, input: BlobWriteMetadata) => {
            const metadata = { ...input, key, size: bytes.length } as BlobMetadata
            state.blobs.set(key, bytes.slice())
            state.metadata.set(key, metadata)
            state.afterPut(key)
            return metadata
        },
        read: async (key: string) => state.blobs.get(key) ?? null,
        stat: async (key: string) => state.metadata.get(key) ?? null,
        list: async () => [...state.metadata.values()],
        remove: async (key: string) => { state.blobs.delete(key); state.metadata.delete(key) },
        resolveUrl: async () => null,
    } as BlobStore),
}))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => ({}), presetTemplate: {} }))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => ({}),
    publishCurrentOfficialRevision: vi.fn(async () => undefined),
    replacePersistentDatabase: (database: Database) => state.replace(database),
}))
vi.mock('src/ts/process/coldstorage.svelte', () => ({
    confirmIncompleteColdStorageRestore: vi.fn(async () => true),
    getColdStorageBackupKey: () => null,
    isColdStorageBackupData: () => false,
    listColdDataKeys: async () => [],
}))
vi.mock('src/ts/platform', () => ({ isTauri: false, isTauriDesktop: false }))
vi.mock('@tauri-apps/plugin-fs', () => ({ BaseDirectory: {}, open: vi.fn(), readFile: vi.fn(), writeFile: vi.fn() }))
vi.mock('@tauri-apps/plugin-process', () => ({ relaunch: vi.fn() }))
vi.mock('src/ts/drive/legacyLocalBackupFileRouteProduction.svelte', () => ({
    exportLegacyLocalBackupFromSystemPicker: vi.fn(),
    importLegacyLocalBackupFromSystemPicker: vi.fn(),
}))
vi.mock('src/ts/util', () => ({
    decryptBuffer: vi.fn(), encryptBuffer: vi.fn(), sleep: vi.fn(async () => undefined),
}))
vi.mock('src/ts/characterCards', () => ({ hubURL: 'https://synthetic.invalid' }))
vi.mock('src/lang', () => ({ language: {} }))

function entry(name: string, data: Uint8Array): Uint8Array {
    const nameBytes = new TextEncoder().encode(name)
    const bytes = new Uint8Array(8 + nameBytes.length + data.length)
    const view = new DataView(bytes.buffer)
    view.setUint32(0, nameBytes.length, true)
    bytes.set(nameBytes, 4)
    view.setUint32(4 + nameBytes.length, data.length, true)
    bytes.set(data, 8 + nameBytes.length)
    return bytes
}

function archive(...entries: Uint8Array[]): Uint8Array {
    const bytes = new Uint8Array(entries.reduce((size, item) => size + item.length, 0))
    let offset = 0
    for (const item of entries) { bytes.set(item, offset); offset += item.length }
    return bytes
}

function databaseEntry() {
    return entry('database.risudat', encodeRisuSaveLegacy(structuredClone(risuSaveFixtureDatabase), 'compression'))
}

async function restore(bytes: Uint8Array, controller = new AbortController()) {
    const { importLegacyBackupWithWebView } = await import('src/ts/drive/backuplocal')
    const input = {
        type: '', accept: '',
        files: [{
            name: 'synthetic.bin', size: bytes.length,
            stream: () => new ReadableStream<Uint8Array>({
                start(controller) { controller.enqueue(bytes); controller.close() },
            }),
        }],
        onchange: null as null | (() => void),
        click: vi.fn(), remove: vi.fn(),
    }
    vi.spyOn(document, 'createElement').mockReturnValueOnce(input as unknown as HTMLInputElement)
    const context = {
        signal: controller.signal,
        onStatus: vi.fn(), setSource: vi.fn(), setPartialWritesPossible: vi.fn(),
    }
    const pending = importLegacyBackupWithWebView(context)
    input.onchange!()
    return pending
}

afterEach(() => {
    vi.restoreAllMocks()
    state.blobs.clear()
    state.metadata.clear()
    state.afterPut.mockReset()
    state.replace.mockReset().mockResolvedValue({ kind: 'committed', revision: 2, projection: 'applied' })
})

describe('legacy WebView restore integrity', () => {
    it.each([1, 3, 6, 12, 15, 17])('rejects a trailing frame truncated at byte %s', async (length) => {
        const damaged = entry('file.png', new Uint8Array([1, 2, 3]))
        await expect(restore(archive(databaseEntry(), damaged.subarray(0, length))))
            .rejects.toThrow('truncated entry')
        expect(state.replace).not.toHaveBeenCalled()
        expect(state.blobs.size).toBe(0)
    })

    it('rejects a truncated database after a complete attachment', async () => {
        await expect(restore(archive(entry('file.png', new Uint8Array([1])), databaseEntry().subarray(0, -1))))
            .rejects.toThrow('truncated entry')
        expect(state.replace).not.toHaveBeenCalled()
        expect(state.blobs.size).toBe(0)
    })

    it.each(['database.risudat', 'encryption.risudat'])('rejects duplicate %s records', async (name) => {
        const data = name === 'database.risudat'
            ? databaseEntry()
            : entry(name, new TextEncoder().encode('{"type":"account","time":1}'))
        await expect(restore(archive(data, data))).rejects.toThrow('Duplicate')
        expect(state.replace).not.toHaveBeenCalled()
    })

    it.each(['asset', 'inlay', 'pocket'] as const)('preserves existing %s bytes and metadata on every pre-commit failure', async (kind) => {
        const key = kind === 'asset' ? 'assets/existing.png' : 'existing'
        const metadata = {
            key, kind: kind === 'asset' ? 'asset' : 'inlay', inlayType: 'image',
            mime: 'image/png', name: 'original.png', ext: 'png', size: 3, width: 2, height: 2,
        } as BlobMetadata
        const oldBytes = new Uint8Array([9, 8, 7])
        const newBytes = new Uint8Array([1, 2, 3])
        const attachment = kind === 'asset' ? entry('existing.png', newBytes)
            : kind === 'pocket' ? entry('inlay/existing.png', newBytes)
            : entry(getBackupInlayName(key), encodeBackupInlayEntry({ ...metadata, kind: 'inlay', inlayType: 'image' }, newBytes))
        for (const failure of ['missing', 'invalid', 'truncated', 'cancel', 'activation', 'write'] as const) {
            state.blobs.set(key, oldBytes.slice())
            state.metadata.set(key, structuredClone(metadata))
            state.replace.mockClear()
            const controller = new AbortController()
            let bytes = archive(attachment, databaseEntry())
            if (failure === 'missing') bytes = attachment
            if (failure === 'invalid') bytes = archive(attachment, entry('database.risudat', new Uint8Array([0xff, 0xff, 0xff, 0xff])))
            if (failure === 'truncated') bytes = bytes.subarray(0, -1)
            if (failure === 'activation') state.replace.mockRejectedValueOnce(new Error('activation rejected'))
            if (failure === 'cancel') state.afterPut.mockImplementationOnce(() => controller.abort())
            if (failure === 'write') state.afterPut.mockImplementationOnce(() => { throw new Error('write failed after payload') })
            const error = await restore(bytes, controller).then(() => null, (error: unknown) => error)
            expect(error, failure).not.toBeNull()
            expect(state.blobs.get(key), failure).toEqual(oldBytes)
            expect(state.metadata.get(key), failure).toEqual(metadata)
            if (failure !== 'activation') expect(state.replace, failure).not.toHaveBeenCalled()
        }
        await restore(archive(attachment, databaseEntry()))
        expect(state.blobs.get(key)).toEqual(newBytes)
        expect(state.replace).toHaveBeenCalled()
    })

    it('removes newly published attachments when activation is rejected', async () => {
        state.replace.mockRejectedValueOnce(new Error('activation rejected'))
        await expect(restore(archive(entry('new.png', new Uint8Array([1])), databaseEntry()))).rejects.toThrow('activation rejected')
        expect(state.blobs.size).toBe(0)
        expect(state.metadata.size).toBe(0)
    })

    it('keeps attachments after a committed replacement that still needs projection', async () => {
        state.replace.mockResolvedValueOnce({ kind: 'committed', revision: 2, projection: 'refresh-required' })
        await restore(archive(entry('new.png', new Uint8Array([1])), databaseEntry()))
        expect(state.blobs.get('assets/new.png')).toEqual(new Uint8Array([1]))
    })

    it('rolls back every published attachment if a later write fails', async () => {
        state.afterPut.mockImplementationOnce(() => undefined)
            .mockImplementationOnce(() => { throw new Error('second write failed') })
        await expect(restore(archive(
            entry('first.png', new Uint8Array([1])),
            entry('second.png', new Uint8Array([2])),
            databaseEntry(),
        ))).rejects.toThrow('second write failed')
        expect(state.blobs.size).toBe(0)
        expect(state.metadata.size).toBe(0)
        expect(state.replace).not.toHaveBeenCalled()
    })
    it('rejects a truncated trailing asset instead of activating the earlier database', async () => {
        const db = entry('database.risudat', encodeRisuSaveLegacy(structuredClone(risuSaveFixtureDatabase), 'compression'))
        const asset = entry('synthetic-image.png', new Uint8Array([1, 2, 3, 4, 5]))
        const truncated = new Uint8Array(db.length + asset.length - 2)
        truncated.set(db)
        truncated.set(asset.subarray(0, asset.length - 2), db.length)
        let failed = false
        try { await restore(truncated) } catch { failed = true }
        expect({ failed, activated: state.replace.mock.calls.length }).toEqual({ failed: true, activated: 0 })
    })

    it('does not overwrite an existing inlay when the archive has no database', async () => {
        const original = new Uint8Array([9, 9, 9])
        state.blobs.set('synthetic-existing-inlay', original.slice())
        const metadata = {
            key: 'synthetic-existing-inlay', kind: 'inlay', inlayType: 'image',
            mime: 'image/png', name: 'synthetic.png', ext: 'png', size: 3,
        } as const
        const replacement = new Uint8Array([1, 2, 3])
        const bytes = entry(getBackupInlayName(metadata.key), encodeBackupInlayEntry(metadata, replacement))
        await expect(restore(bytes)).rejects.toThrow('no database entry')
        expect(state.replace).not.toHaveBeenCalled()
        expect(state.blobs.get(metadata.key)).toEqual(original)
    })
})
