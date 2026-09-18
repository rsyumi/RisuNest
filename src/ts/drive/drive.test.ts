import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { createUnrecordedOfficialAssetLedger } from '../storage/sync/officialAssetLedger'
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { BlobMetadata, BlobStore } from '../storage/blobStore'
import type { Database } from '../storage/database.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'
import { decodeRisuSave, encodeRisuSaveLegacy } from '../storage/risuSave'
import { risuSaveFixtureDatabase } from '../storage/tests/risuSaveFixtures'
import { OfficialAccountSnapshotAdapter } from '../storage/sync/officialAccountSnapshot'

const state = vi.hoisted(() => ({
    ios: false,
    restartNativeApp: vi.fn(async () => undefined),
    alertError: vi.fn(),
    alertInput: vi.fn(async () => 'drive-token'),
    alertNormal: vi.fn(),
    alertSelect: vi.fn(async () => '0'),
    blobStore: null as BlobStore | null,
    currentDatabase: null as Database | null,
    forageInit: vi.fn(async () => undefined),
    officialCold: new Map<string, unknown>(),
    publishCurrentOfficialRevision: vi.fn<() => Promise<void>>(),
    replacePersistentDatabase: vi.fn<PersistentDataRuntime['replacePersistentDatabase']>(),
    runtime: null as PersistentDataRuntime | null,
    restoreEvents: [] as string[],
    getUncleanables: vi.fn(async () => ['assets/second-read.png']),
    externalGetState: vi.fn(async () => ({
        supported: true,
        selection: { kind: 'none', selectionEpoch: 'selection', paused: false, decisionRequired: false },
        connections: [],
        jobs: [],
    })),
    externalListHistory: vi.fn(async () => ({ items: [] })),
    requestExternalStorageNow: vi.fn(async () => undefined),
    requestExternalStorageRestore: vi.fn(async () => undefined),
    settingsOpenSet: vi.fn(),
    settingsMenuSet: vi.fn(),
}))

vi.mock('../alert', () => ({
    alertError: state.alertError,
    alertInput: state.alertInput,
    alertNormal: state.alertNormal,
    alertSelect: state.alertSelect,
    alertStore: { set: vi.fn() },
}))

vi.mock('../storage/database.svelte', () => ({
    getDatabase: () => state.currentDatabase,
    presetTemplate: {},
}))

vi.mock('../globalApi.svelte', () => ({
    forageStorage: {
        Init: state.forageInit,
        isAccount: true,
    },
    getUncleanablesSync: vi.fn((database: Database, _mode: string, options?: {
        chars: Array<{ image?: string }>
    }) => (options?.chars ?? database.characters).flatMap((character) => (
        character.image?.split('/').at(-1) ? [character.image.split('/').at(-1)!] : []
    ))),
    getUncleanables: state.getUncleanables,
    openURL: vi.fn(),
}))

vi.mock('../storage/platformBlobStore', () => ({
    resolveBlobStore: async () => state.blobStore,
}))

vi.mock('src/ts/platform', () => ({
    isNodeServer: false,
    isTauri: true,
    get isTauriIOS() { return state.ios },
}))

vi.mock('../storage/nativePersistentMaintenance', () => ({
    restartNativeApp: state.restartNativeApp,
}))

vi.mock('../../lang', () => ({
    language: {
        pasteAuthCode: 'Paste code',
        cancel: 'Cancel',
        risuNest: {
            storage: { emptyList: 'Nothing saved yet.' },
            backup: { actionFailed: 'Backup action failed.' },
        },
    },
}))

vi.mock('../storage/sync/external/bridge', () => ({
    getExternalStorageBridge: () => ({
        getState: state.externalGetState,
        listHistory: state.externalListHistory,
    }),
}))

vi.mock('../storage/sync/external/production', () => ({
    requestExternalStorageNow: state.requestExternalStorageNow,
    requestExternalStorageRestore: state.requestExternalStorageRestore,
}))

vi.mock('../stores.svelte', () => ({
    settingsOpen: { set: state.settingsOpenSet },
    SettingsMenuIndex: { set: state.settingsMenuSet },
}))

vi.mock('@tauri-apps/plugin-process', () => ({
    relaunch: vi.fn(async () => undefined),
}))

vi.mock('../util', () => ({
    sleep: vi.fn(async () => undefined),
}))

vi.mock('../characterCards', () => ({
    hubURL: 'https://hub.invalid',
}))

vi.mock('../process/coldstorage.svelte', () => ({
    confirmIncompleteColdStorageRestore: vi.fn(async () => true),
    getColdStorageBackupName: (key: string) => `coldstorage_${key}.json`,
    isColdStorageBackupData: (value: unknown) => Boolean(
        value
        && typeof value === 'object'
        && ('character' in value || 'message' in value),
    ),
    listColdDataKeys: async (database: Database) => database.characters
        .map((character) => character.coldstorage)
        .filter((key): key is string => Boolean(key)),
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => state.runtime,
    publishCurrentOfficialRevision: () => state.publishCurrentOfficialRevision(),
    replacePersistentDatabase: (...args: Parameters<PersistentDataRuntime['replacePersistentDatabase']>) => (
        state.replacePersistentDatabase(...args)
    ),
}))

function metadata(key: string, size: number): BlobMetadata {
    return {
        key,
        kind: 'asset',
        size,
        mime: 'application/octet-stream',
        name: key.split('/').at(-1) ?? key,
        ext: key.split('.').at(-1) ?? '',
    }
}

function makeBlobStore(values: Map<string, Uint8Array>): BlobStore {
    return {
        put: vi.fn(async (key, bytes) => {
            state.restoreEvents.push(`asset:${key}`)
            values.set(key, bytes.slice())
            return metadata(key, bytes.byteLength)
        }),
        read: vi.fn(async (key) => values.get(key)?.slice() ?? null),
        stat: vi.fn(async (key) => {
            const value = values.get(key)
            return value ? metadata(key, value.byteLength) : null
        }),
        list: vi.fn(async () => []),
        remove: vi.fn(async () => undefined),
        resolveUrl: vi.fn(async () => null),
    }
}

function driveDatabase(coldKey: string): Database {
    const database = structuredClone(risuSaveFixtureDatabase) as Database
    database.account = { useSync: true } as Database['account']
    database.characters = [{
        ...database.characters[0],
        chaId: 'cold-character',
        image: '',
        chats: [],
        coldstorage: coldKey,
    }]
    return database
}

describe('native Drive settings routing', () => {
    const googleConnection = {
        id: 'google-primary',
        providerId: 'google_drive',
        displayName: 'Google backup',
        status: 'ready',
        scope: {
            library: true,
            referencedAssets: true,
            deviceSettings: true,
            devicePlugins: false,
        },
    }

    beforeEach(() => {
        vi.clearAllMocks()
        state.externalGetState.mockResolvedValue({
            supported: true,
            selection: { kind: 'none', selectionEpoch: 'selection', paused: false, decisionRequired: false },
            connections: [googleConnection],
            jobs: [],
        })
        state.externalListHistory.mockResolvedValue({ items: [] })
        state.alertSelect.mockResolvedValue('0')
    })

    it('uses the common external backup orchestration for the native save button', async () => {
        const { checkDriver } = await import('./drive')
        await checkDriver('savetauri')

        expect(state.requestExternalStorageNow).toHaveBeenCalledWith('google-primary', 'backup')
        expect(state.alertInput).not.toHaveBeenCalled()
    })

    it('restores a verified external snapshot with what the bundle covers', async () => {
        state.externalListHistory.mockResolvedValue({
            items: [{
                id: 'snapshot-7',
                kind: 'backup-point',
                createdAtMs: '1000',
                logicalRevision: '7',
                pinned: false,
                complete: true,
                verified: true,
                includedSections: ['hypa', 'local-plugins', 'local-settings'],
                sameDevice: false,
            }],
        })
        const { checkDriver } = await import('./drive')
        await checkDriver('loadtauri')

        expect(state.requestExternalStorageRestore).toHaveBeenCalledWith(
            'google-primary',
            'snapshot-7',
            ['library', 'referencedAssets', 'hypa', 'local-plugins'],
        )
    })

    it('opens the existing external storage setup when Google is not connected', async () => {
        state.externalGetState.mockResolvedValue({
            supported: true,
            selection: { kind: 'none', selectionEpoch: 'selection', paused: false, decisionRequired: false },
            connections: [],
            jobs: [],
        })
        const { checkDriver } = await import('./drive')
        await checkDriver('savetauri')

        expect(state.settingsOpenSet).toHaveBeenCalledWith(true)
        expect(state.settingsMenuSet).toHaveBeenCalledWith(17)
        expect(state.requestExternalStorageNow).not.toHaveBeenCalled()
    })

    it('preserves the official-account refresh-token authorization URL', async () => {
        const { checkDriver } = await import('./drive')
        const url = await checkDriver('reftoken')

        expect(url).toContain('redirect_uri=https%3A%2F%2Fsv.risuai.xyz%2Fdrive')
        expect(url).toContain('state=accesstauri')
        expect(url).toContain('access_type=offline')
    })
})

describe('Drive restore cold snapshot assets', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        state.ios = false
        state.officialCold.clear()
        state.blobStore = null
        state.currentDatabase = driveDatabase('current-cold')
        state.runtime = null
        state.restoreEvents = []
    })

    it.each([false, true])('materializes the selected Drive cold asset before publishing the accepted revision (iOS: %s)', async (ios) => {
        state.ios = ios
        const coldKey = '85cc96bc-d6c5-4cee-9a7f-48ae292e58ac'
        const database = driveDatabase(coldKey)
        const pluginAssetKey = 'assets/plugin-drive-restore.bin'
        const missingPluginAssetKey = 'assets/plugin-drive-missing.bin'
        const pluginAsset = Uint8Array.of(4, 5, 6)
        const collisionAsset = Uint8Array.of(7, 7, 7)
        database.pluginCustomStorage = {
            nested: [{ asset: pluginAssetKey }, { missing: missingPluginAssetKey }],
            rejectedBasenameCollision: [
                'assets/first/shared.bin',
                'assets/second/shared.bin',
            ],
        }
        const driveCold = {
            character: {
                ...database.characters[0],
                image: 'assets/drive-only.png',
                coldstorage: undefined,
            },
        }
        const officialCold = {
            character: {
                ...database.characters[0],
                image: 'assets/account-only.png',
                coldstorage: undefined,
            },
        }
        const driveAsset = Uint8Array.of(7, 8, 9)
        const localAssets = new Map<string, Uint8Array>()
        const blobStore = makeBlobStore(localAssets)
        state.blobStore = blobStore
        state.officialCold.set(coldKey, officialCold)

        const store = new IndexedDbPersistentDataStore(
            `drive-restore-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        let revision = 0
        const accountWrites: string[] = []
        const adapter = new OfficialAccountSnapshotAdapter({
            store,
            resolveBlobs: async () => blobStore,
            account: {
                readItem: async (key) => key === 'assets/account-only.png'
                    ? { kind: 'value', bytes: Uint8Array.of(1) }
                    : { kind: 'missing' },
                writeItem: async (key) => {
                    accountWrites.push(key)
                    return { kind: 'written', replacementKey: key }
                },
            },
            cold: {
                readRemote: async (key) => structuredClone(state.officialCold.get(key) ?? null),
            },
            prepareCandidate: async (candidate) => structuredClone(candidate),
            markPublished: vi.fn(),
            ledger: createUnrecordedOfficialAssetLedger(),
        })
        state.replacePersistentDatabase.mockImplementation(async (candidate, reason, options) => {
            expect(reason).toBe('drive-restore')
            expect(options?.publishOfficial).toBe(true)
            state.restoreEvents.push('database')
            revision = (await store.replaceFromDatabase(structuredClone(candidate))).revision
            return { kind: 'committed', revision, projection: 'applied' }
        })
        state.publishCurrentOfficialRevision.mockImplementation(async () => {
            const publication = await adapter.pin(revision)
            await publication.publish()
        })

        const databaseBytes = encodeRisuSaveLegacy(database, 'compression')
        const files = [
            { id: 'database', name: '100-database.risudat', mimeType: 'application/octet-stream' },
            { id: 'cold', name: `coldstorage_${coldKey}.json`, mimeType: 'application/json' },
            { id: 'asset', name: 'drive-only.png.bin', mimeType: 'application/octet-stream' },
            { id: 'plugin-asset', name: 'plugin-drive-restore.bin.bin', mimeType: 'application/octet-stream' },
            { id: 'collision', name: 'shared.bin.bin', mimeType: 'application/octet-stream' },
        ]
        const fileBytes = new Map<string, Uint8Array>([
            ['database', databaseBytes],
            ['cold', new TextEncoder().encode(JSON.stringify(driveCold))],
            ['asset', driveAsset],
            ['plugin-asset', pluginAsset],
            ['collision', collisionAsset],
        ])
        vi.stubGlobal('fetch', vi.fn(async (input: string | URL) => {
            const url = String(input)
            if (url.includes('/drive/v3/files?spaces=')) {
                return new Response(JSON.stringify({ files }), {
                    status: 200,
                    headers: { 'content-type': 'application/json' },
                })
            }
            const id = url.match(/\/drive\/v3\/files\/([^?]+)/)?.[1]
            const bytes = id ? fileBytes.get(id) : undefined
            return bytes
                ? new Response(bytes.slice(), { status: 200 })
                : new Response(null, { status: 404 })
        }))

        const { loadDrive } = await import('./drive')
        await loadDrive('drive-token', 'backup')

        expect(state.restartNativeApp).toHaveBeenCalledTimes(ios ? 1 : 0)
        const { relaunch } = await import('@tauri-apps/plugin-process')
        expect(relaunch).toHaveBeenCalledTimes(ios ? 0 : 1)

        expect(localAssets.get('assets/drive-only.png')).toEqual(driveAsset)
        expect(localAssets.get(pluginAssetKey)).toEqual(pluginAsset)
        expect(localAssets.has(missingPluginAssetKey)).toBe(false)
        expect(localAssets.has('assets/shared.bin')).toBe(false)
        expect(accountWrites).toContain('assets/drive-only.png')
        expect(accountWrites).not.toContain('assets/account-only.png')
        expect(accountWrites.at(-1)).toBe('database/database.bin')
        expect(state.restoreEvents).toEqual([
            'asset:assets/drive-only.png',
            `asset:${pluginAssetKey}`,
            'database',
        ])
        expect(state.forageInit).not.toHaveBeenCalled()
        expect(state.alertError).not.toHaveBeenCalled()
    })

    it('uploads the pinned database and its referenced assets', async () => {
        const persisted = structuredClone(risuSaveFixtureDatabase) as Database
        const pluginAssetKey = 'assets/plugin-drive-upload.bin'
        const pluginAssetBytes = Uint8Array.of(8, 9)
        persisted.pluginCustomStorage = {
            nested: { asset: pluginAssetKey },
            rejectedBasenameCollision: [
                'assets/first/shared.bin',
                'assets/second/shared.bin',
            ],
            prose: 'prefix assets/not-a-reference.bin',
        }
        persisted.characters[0].image = 'assets/cold-pinned.png'
        const store = new IndexedDbPersistentDataStore(
            `drive-backup-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(persisted)
        const materializeDatabase = vi.spyOn(store, 'materializeDatabase').mockRejectedValue(
            new Error('Drive backup must not materialize the full database'),
        )
        const live = structuredClone(persisted)
        live.characters[0].chats[0].message = []
        state.currentDatabase = live
        state.blobStore = makeBlobStore(new Map([
            ['assets/cold-pinned.png', Uint8Array.of(3)],
            ['assets/second-read.png', Uint8Array.of(4)],
            [pluginAssetKey, pluginAssetBytes],
            ['assets/first/shared.bin', Uint8Array.of(1)],
            ['assets/second/shared.bin', Uint8Array.of(2)],
        ]))
        state.runtime = {
            store,
            revision: imported.revision,
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: imported.revision,
                mutationGeneration: 0,
            })),
        } as unknown as PersistentDataRuntime
        const uploads = new Map<string, Uint8Array>()
        vi.stubGlobal('fetch', vi.fn(async (_input: string | URL, init?: RequestInit) => {
            if (init?.method === 'GET') {
                return new Response(JSON.stringify({ files: [] }), {
                    status: 200,
                    headers: { 'content-type': 'application/json' },
                })
            }
            const body = init?.body as FormData
            const metadata = JSON.parse(await (body.get('metadata') as Blob).text()) as {
                name: string
            }
            const file = body.get('file') as Blob
            uploads.set(metadata.name, new Uint8Array(await file.arrayBuffer()))
            return new Response('{}', {
                status: 200,
                headers: { 'content-type': 'application/json' },
            })
        }))

        const { backupDrive } = await import('./drive')
        await backupDrive('drive-token')

        const databaseEntry = [...uploads.entries()].find(([name]) =>
            name.endsWith('-database.risudat'),
        )
        expect(databaseEntry).toBeDefined()
        const backedUp = await decodeRisuSave(databaseEntry![1])
        expect(backedUp.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(backedUp.pluginCustomStorage).toEqual(persisted.pluginCustomStorage)
        expect(materializeDatabase).not.toHaveBeenCalled()
        expect(uploads.has('cold-pinned.png.bin')).toBe(true)
        expect(uploads.has('second-read.png.bin')).toBe(false)
        expect(uploads.get('plugin-drive-upload.bin.bin')).toEqual(pluginAssetBytes)
        expect(uploads.has('shared.bin.bin')).toBe(false)
        expect(uploads.has('not-a-reference.bin.bin')).toBe(false)
        expect([...uploads.keys()].some((name) => name.startsWith('coldstorage_'))).toBe(false)
        expect(state.getUncleanables).not.toHaveBeenCalled()
        expect(state.runtime.capturePersistentMutationToken).toHaveBeenCalledWith('drive-backup')
    })
})
