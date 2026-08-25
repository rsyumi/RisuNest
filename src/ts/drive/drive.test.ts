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
    alertError: vi.fn(),
    alertInput: vi.fn(async () => 'drive-token'),
    alertSelect: vi.fn(async () => '0'),
    blobStore: null as BlobStore | null,
    currentDatabase: null as Database | null,
    forageInit: vi.fn(async () => undefined),
    localCold: new Map<string, unknown>(),
    officialCold: new Map<string, unknown>(),
    publishCurrentOfficialRevision: vi.fn<() => Promise<void>>(),
    replacePersistentDatabase: vi.fn<(database: Database, reason: string) => Promise<void>>(),
    runtime: null as PersistentDataRuntime | null,
    snapshotSeenByColdStorage: null as Database | null,
    coldStoragePayloads: [] as Array<{
        key: string
        backupName: string
        value: unknown
        encoded: Uint8Array
    }>,
    getUncleanables: vi.fn(async () => ['assets/second-read.png']),
}))

vi.mock('../alert', () => ({
    alertError: state.alertError,
    alertInput: state.alertInput,
    alertNormal: vi.fn(),
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
    getUncleanablesSync: vi.fn((_database: Database, _mode: string, options: {
        chars: Array<{ image?: string }>
    }) => options.chars.flatMap((character) => (
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
}))

vi.mock('../../lang', () => ({
    language: { pasteAuthCode: 'Paste code' },
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
    collectColdStorageBackupPayloads: vi.fn(async (database: Database) => {
        state.snapshotSeenByColdStorage = database
        return { payloads: state.coldStoragePayloads, missingKeys: [], invalidKeys: [] }
    }),
    confirmIncompleteColdStorageOperation: vi.fn(async () => true),
    getColdStorageBackupName: (key: string) => `coldstorage_${key}.json`,
    getColdStorageItem: async (key: string, options?: { accountFallback?: boolean }) => (
        options?.accountFallback ? structuredClone(state.localCold.get(key) ?? null) : null
    ),
    isColdStorageBackupData: (value: unknown) => Boolean(
        value
        && typeof value === 'object'
        && ('character' in value || 'message' in value),
    ),
    listColdDataKeys: async (database: Database) => database.characters
        .map((character) => character.coldstorage)
        .filter((key): key is string => Boolean(key)),
    setLocalColdStorageItem: vi.fn(async (key: string, value: unknown) => {
        state.localCold.set(key, structuredClone(value))
        return true
    }),
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => state.runtime,
    publishCurrentOfficialRevision: () => state.publishCurrentOfficialRevision(),
    replacePersistentDatabase: (database: Database, reason: string) => (
        state.replacePersistentDatabase(database, reason)
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

describe('Drive restore cold snapshot assets', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        state.localCold.clear()
        state.officialCold.clear()
        state.blobStore = null
        state.currentDatabase = driveDatabase('current-cold')
        state.runtime = null
        state.snapshotSeenByColdStorage = null
        state.coldStoragePayloads = []
    })

    it('materializes the selected Drive cold asset before publishing the accepted revision', async () => {
        const coldKey = '85cc96bc-d6c5-4cee-9a7f-48ae292e58ac'
        const database = driveDatabase(coldKey)
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
                readLocal: async (key) => structuredClone(state.localCold.get(key) ?? null),
                readRemote: async (key) => structuredClone(state.officialCold.get(key) ?? null),
                writeRemote: async () => undefined,
            },
            prepareCandidate: async (candidate) => structuredClone(candidate),
            markPublished: vi.fn(),
            ledger: createUnrecordedOfficialAssetLedger(),
        })
        state.replacePersistentDatabase.mockImplementation(async (candidate, reason) => {
            expect(reason).toBe('drive-restore')
            revision = (await store.replaceFromDatabase(structuredClone(candidate))).revision
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
        ]
        const fileBytes = new Map<string, Uint8Array>([
            ['database', databaseBytes],
            ['cold', new TextEncoder().encode(JSON.stringify(driveCold))],
            ['asset', driveAsset],
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

        const { checkDriver } = await import('./drive')
        await checkDriver('loadtauri')

        expect(localAssets.get('assets/drive-only.png')).toEqual(driveAsset)
        expect(accountWrites).toContain('assets/drive-only.png')
        expect(accountWrites).not.toContain('assets/account-only.png')
        expect(accountWrites.at(-1)).toBe('database/database.bin')
        expect(state.forageInit).not.toHaveBeenCalled()
        expect(state.alertError).not.toHaveBeenCalled()
    })

    it('uploads database and pinned cold assets without rereading mutable cold storage', async () => {
        const persisted = structuredClone(risuSaveFixtureDatabase) as Database
        const store = new IndexedDbPersistentDataStore(
            `drive-backup-${crypto.randomUUID()}`,
            new IDBFactory(),
            IDBKeyRange,
        )
        await store.open()
        const imported = await store.replaceFromDatabase(persisted)
        const live = structuredClone(persisted)
        live.characters[0].chats[0].message = []
        state.currentDatabase = live
        state.blobStore = makeBlobStore(new Map([
            ['assets/cold-pinned.png', Uint8Array.of(3)],
            ['assets/second-read.png', Uint8Array.of(4)],
        ]))
        state.coldStoragePayloads = [{
            key: 'cold-char',
            backupName: 'coldstorage_cold-char.json',
            value: {
                character: {
                    ...persisted.characters[0],
                    image: 'assets/cold-pinned.png',
                },
            },
            encoded: Uint8Array.of(1),
        }]
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

        const { checkDriver } = await import('./drive')
        await checkDriver('savetauri')

        const databaseEntry = [...uploads.entries()].find(([name]) =>
            name.endsWith('-database.risudat'),
        )
        expect(databaseEntry).toBeDefined()
        const backedUp = await decodeRisuSave(databaseEntry![1])
        expect(backedUp.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(state.snapshotSeenByColdStorage?.characters[0].chats[0].message).toEqual(
            persisted.characters[0].chats[0].message,
        )
        expect(uploads.has('cold-pinned.png.bin')).toBe(true)
        expect(uploads.has('second-read.png.bin')).toBe(false)
        expect(state.getUncleanables).not.toHaveBeenCalled()
        expect(state.runtime.capturePersistentMutationToken).toHaveBeenCalledWith('drive-backup')
    })
})
