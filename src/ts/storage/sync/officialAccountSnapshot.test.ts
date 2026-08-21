import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { coldStorageHeader, listCharacterResources } from '../../process/coldstorageData'
import type { AccountReadResult, AccountWriteResult } from '../accountStorage'
import type { BlobMetadata, BlobStore } from '../blobStore'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import type { Database } from '../database.svelte'
import { RevisionConflictError, type PersistentDataStore } from '../persistentDataStore'
import { decodeRisuSave, encodeRisuSaveLegacy } from '../risuSave'
import { streamRisuSaveFromStore } from '../risuSaveStoreAdapter'
import { risuSaveFixtureDatabase } from '../tests/risuSaveFixtures'
import {
    OfficialAccountSnapshotAdapter,
    type OfficialAccountSnapshotDependencies,
} from './officialAccountSnapshot'

vi.mock('../database.svelte', () => ({
    getDatabase: () => {
        throw new Error('Official snapshot adapter must not read DBState')
    },
    presetTemplate: {},
}))
vi.mock('../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

const databaseKey = 'database/database.bin'

async function concatenate(chunks: AsyncIterable<Uint8Array>): Promise<Uint8Array> {
    const values: Uint8Array[] = []
    let size = 0
    for await (const chunk of chunks) {
        values.push(chunk)
        size += chunk.byteLength
    }
    const result = new Uint8Array(size)
    let offset = 0
    for (const value of values) {
        result.set(value, offset)
        offset += value.byteLength
    }
    return result
}

function makeDatabase(): Database {
    const database = structuredClone(risuSaveFixtureDatabase) as any
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
        coldStoragedChats: ['cold-chat'],
    })
    database.characters[0].chats.push({
        id: 'cold-stub',
        name: 'Cold stub',
        message: [{
            role: 'char',
            data: `${coldStorageHeader}cold-message`,
            chatId: 'cold-stub-message',
        }],
    })
    return database
}

function resources(database: Database): string[] {
    return [
        database.customBackground,
        database.userIcon,
        database.modules[0].assets[0][1],
        database.modules[0].icon,
        database.personas[0].icon,
        database.personas[0].embeddedModule.assets[0][1],
        database.personas[0].embeddedModule.icon,
        (database.characterOrder[0] as any).imgFile,
        database.characters[0].image,
        database.characters[0].emotionImages[0][1],
        (database.characters[0] as any).additionalAssets[0][1],
        (database.characters[0] as any).vits.files.model,
        (database.characters[0] as any).ccAssets[0].uri,
    ].filter((value): value is string => !!value)
}

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

function makeBlobStore(values: ReadonlyMap<string, Uint8Array>): BlobStore {
    return {
        put: vi.fn(),
        read: vi.fn(async (key) => values.get(key)?.slice() ?? null),
        stat: vi.fn(async (key) => {
            const value = values.get(key)
            return value ? metadata(key, value.byteLength) : null
        }),
        list: vi.fn(),
        remove: vi.fn(),
        resolveUrl: vi.fn(),
    }
}

interface HarnessOptions {
    database?: Database
    blobs?: Map<string, Uint8Array>
    remoteAssets?: Map<string, Uint8Array>
    localCold?: Map<string, unknown>
    remoteCold?: Map<string, unknown>
    databaseRead?: AccountReadResult
    prepareCandidate?: (database: Database) => Promise<Database>
}

async function makeHarness(options: HarnessOptions = {}) {
    const database = options.database ?? makeDatabase()
    const store = new IndexedDbPersistentDataStore(
        `official-adapter-${crypto.randomUUID()}`,
        new IDBFactory(),
        IDBKeyRange,
    )
    await store.open()
    const imported = await store.replaceFromDatabase(structuredClone(database))
    const localBlobs = options.blobs ?? new Map(
        resources(database).map((key, index) => [key, Uint8Array.of(index + 1)]),
    )
    const blobStore = makeBlobStore(localBlobs)
    const remoteAssets = options.remoteAssets ?? new Map<string, Uint8Array>()
    const localCold = options.localCold ?? new Map<string, unknown>([
        ['cold-chat', { message: [{ data: 'local chat' }] }],
        ['cold-message', { message: [{ data: 'local message' }] }],
    ])
    const remoteCold = options.remoteCold ?? new Map<string, unknown>()
    const events: string[] = []
    const writes: Array<{ key: string; bytes?: Uint8Array; value?: unknown }> = []
    const readItem = vi.fn(async (key: string): Promise<AccountReadResult> => {
        if (key === databaseKey) {
            return options.databaseRead ?? { kind: 'missing' }
        }
        const bytes = remoteAssets.get(key)
        return bytes ? { kind: 'value', bytes: bytes.slice() } : { kind: 'missing' }
    })
    const writeItem = vi.fn(async (key: string, bytes: Uint8Array): Promise<AccountWriteResult> => {
        events.push(`asset:${key}`)
        writes.push({ key, bytes: bytes.slice() })
        return { kind: 'written', replacementKey: `remote/${key}` }
    })
    const cold = {
        readRemote: vi.fn(async (key: string) => structuredClone(remoteCold.get(key) ?? null)),
        writeRemote: vi.fn(async (key: string, value: unknown) => {
            events.push(`cold:${key}`)
            writes.push({ key, value: structuredClone(value) })
        }),
        readLocal: vi.fn(async (key: string) => structuredClone(localCold.get(key) ?? null)),
    }
    const markPublished = vi.fn()
    const prepareCandidate = options.prepareCandidate ?? vi.fn(async (value: Database) => structuredClone(value))
    const resolveBlobs = vi.fn(async () => blobStore)
    const adapter = new OfficialAccountSnapshotAdapter({
        store,
        resolveBlobs,
        account: { readItem, writeItem },
        cold,
        prepareCandidate,
        markPublished,
    })
    return {
        adapter,
        blobStore,
        cold,
        database,
        events,
        imported,
        markPublished,
        prepareCandidate,
        readItem,
        resolveBlobs,
        store,
        writeItem,
        writes,
    }
}

describe('OfficialAccountSnapshotAdapter publication', () => {
    it('owns one exact lease, resolves one concrete BlobStore per pin, and releases after DB-last success', async () => {
        const harness = await makeHarness()
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const materialize = vi.spyOn(harness.store, 'materializeDatabase')

        const publication = await harness.adapter.pin(harness.imported.revision)
        const lease = await acquireRevision.mock.results[0].value
        const release = vi.spyOn(lease, 'release')
        await publication.publish()
        await publication.dispose()

        expect(harness.resolveBlobs).toHaveBeenCalledTimes(1)
        expect(acquireRevision).toHaveBeenCalledWith(harness.imported.revision)
        expect(release).toHaveBeenCalledTimes(1)
        expect(materialize).not.toHaveBeenCalled()
        expect(harness.events.at(-1)).toBe(`asset:${databaseKey}`)
        expect(harness.markPublished).toHaveBeenCalledWith(harness.imported.revision)
    })

    it('publishes sorted assets, then sorted cold payloads, then the projected database', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)

        await publication.publish()

        const assetEvents = harness.events.filter((event) => event.startsWith('asset:assets/'))
        expect(assetEvents).toEqual([...assetEvents].sort())
        expect(harness.events.slice(0, assetEvents.length)).toEqual(assetEvents)
        expect(harness.events.slice(assetEvents.length, -1)).toEqual([
            'cold:cold-chat',
            'cold:cold-message',
        ])
        expect(harness.events.at(-1)).toBe(`asset:${databaseKey}`)

        const databaseWrite = harness.writes.find((write) => write.key === databaseKey)
        const projected = await decodeRisuSave(databaseWrite!.bytes!) as Database
        for (const key of resources(projected)) {
            expect(key).toMatch(/^remote\/assets\//)
        }
        for (const coldWrite of harness.writes.filter((write) => write.value)) {
            expect(coldWrite.value).not.toBe(harness.cold.readLocal)
        }
    })

    it('projects every character resource in a cloned pinned cold payload', async () => {
        const database = makeDatabase()
        const coldCharacter = structuredClone(database.characters[0])
        const coldPayload = { character: coldCharacter }
        const localCold = new Map<string, unknown>([
            ['cold-chat', coldPayload],
            ['cold-message', { message: [{ data: 'unchanged' }] }],
        ])
        const harness = await makeHarness({ database, localCold })
        const publication = await harness.adapter.pin(harness.imported.revision)
        coldCharacter.image = 'assets/mutated-after-pin.png'

        await publication.publish()

        const written = harness.writes.find((write) => write.key === 'cold-chat')!.value as {
            character: Database['characters'][number]
        }
        for (const key of listCharacterResources(written.character)) {
            expect(key).toMatch(/^remote\/assets\//)
        }
        expect(written.character.image).toBe('remote/assets/character.png')
        expect((localCold.get('cold-chat') as any).character.image).toBe('assets/mutated-after-pin.png')
    })

    it('publishes the pinned snapshot after a later commit without creating a publication revision', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)
        const root = (await harness.store.readRoot()).value
        const later = await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Later local user' },
        })
        const commit = vi.spyOn(harness.store, 'commit')
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        await publication.publish()

        const databaseWrite = harness.writes.find((write) => write.key === databaseKey)
        expect((await decodeRisuSave(databaseWrite!.bytes!)).username).toBe('Snapshot User')
        expect((await harness.store.readRoot()).value.username).toBe('Later local user')
        expect(harness.markPublished).toHaveBeenCalledWith(harness.imported.revision)
        expect(later.revision).toBe(harness.imported.revision + 1)
        expect(commit).not.toHaveBeenCalled()
        expect(replace).not.toHaveBeenCalled()
    })

    it('retries the same handle with completed state and exact cached database bytes', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)
        let databaseAttempts = 0
        harness.writeItem.mockImplementation(async (key: string, bytes: Uint8Array) => {
            harness.events.push(`asset:${key}`)
            harness.writes.push({ key, bytes: bytes.slice() })
            if (key === databaseKey && databaseAttempts++ === 0) throw new Error('offline')
            return { kind: 'written', replacementKey: `remote/${key}` }
        })

        await expect(publication.publish()).rejects.toThrow('offline')
        const firstDatabase = harness.writes.filter((write) => write.key === databaseKey)[0].bytes
        await publication.publish()
        const databaseWrites = harness.writes.filter((write) => write.key === databaseKey)

        expect(databaseWrites).toHaveLength(2)
        expect(databaseWrites[1].bytes).toEqual(firstDatabase)
        expect(harness.writeItem.mock.calls.filter(([key]) => key === databaseKey)[1][1]).toBe(
            harness.writeItem.mock.calls.filter(([key]) => key === databaseKey)[0][1],
        )
        expect(harness.events.filter((event) => event.startsWith('asset:assets/'))).toHaveLength(resources(harness.database).length)
        expect(harness.cold.writeRemote).toHaveBeenCalledTimes(2)
        expect(harness.markPublished).toHaveBeenCalledTimes(1)
    })

    it('validates remote-only resources and rewrites only changed remote cold projections', async () => {
        const database = makeDatabase()
        const allResources = resources(database)
        const localKey = allResources[0]
        const remoteOnly = allResources.slice(1)
        const harness = await makeHarness({
            database,
            blobs: new Map([[localKey, Uint8Array.of(9)]]),
            remoteAssets: new Map(remoteOnly.map((key) => [key, Uint8Array.of(7)])),
            localCold: new Map(),
            remoteCold: new Map([
                ['cold-chat', { character: { ...database.characters[0], image: localKey } }],
                ['cold-message', { message: [{ data: 'unchanged' }] }],
            ]),
        })

        const publication = await harness.adapter.pin(harness.imported.revision)
        await publication.publish()

        expect(harness.readItem).toHaveBeenCalledTimes(remoteOnly.length)
        expect(harness.writeItem.mock.calls.filter(([key]) => key.startsWith('assets/'))).toHaveLength(1)
        expect(harness.writeItem.mock.calls[0][0]).toBe(localKey)
        expect(harness.cold.writeRemote).toHaveBeenCalledTimes(1)
        expect(harness.cold.writeRemote.mock.calls[0][0]).toBe('cold-chat')
    })

    it('releases a lease when pin validation fails locally and remotely', async () => {
        const database = makeDatabase()
        const harness = await makeHarness({ database, blobs: new Map(), remoteAssets: new Map() })
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')

        await expect(harness.adapter.pin(harness.imported.revision)).rejects.toThrow('Missing official asset')
        const lease = await acquireRevision.mock.results[0].value
        await expect(lease.readRoot()).rejects.toThrow('released')
        expect(harness.writeItem).not.toHaveBeenCalled()
    })

    it('keeps auth warnings as failed publications and dispose is idempotent', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)
        harness.writeItem.mockResolvedValue({ kind: 'auth-warning' })

        await expect(publication.publish()).rejects.toThrow('authorization warning')
        expect(harness.markPublished).not.toHaveBeenCalled()
        await publication.dispose()
        await publication.dispose()
        await expect(publication.publish()).rejects.toThrow('disposed')
    })

    it('propagates publication abort without changing local authority or marking success', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)
        const abort = new DOMException('cancelled', 'AbortError')
        harness.writeItem.mockRejectedValueOnce(abort)

        await expect(publication.publish()).rejects.toBe(abort)

        expect((await harness.store.readRoot()).revision).toBe(harness.imported.revision)
        expect(harness.markPublished).not.toHaveBeenCalled()
        await publication.dispose()
    })

    it('associates a successful push with its active revision for cache hits', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)
        await publication.publish()
        const databaseBytes = harness.writeItem.mock.calls.find(([key]) => key === databaseKey)![1]
        harness.readItem.mockResolvedValue({ kind: 'not-modified', bytes: databaseBytes })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        await expect(harness.adapter.pull()).resolves.toEqual({ kind: 'unchanged' })
        expect(harness.prepareCandidate).not.toHaveBeenCalled()
        expect(replace).not.toHaveBeenCalled()
    })
})

describe('OfficialAccountSnapshotAdapter pull', () => {
    it.each([
        ['legacy compressed', async (database: Database) => encodeRisuSaveLegacy(database, 'compression')],
        ['current block', async (database: Database) => {
            const source = await makeHarness({ database })
            return concatenate(streamRisuSaveFromStore(source.store, source.imported.revision))
        }],
    ])('prepares, validates, and activates a %s snapshot once', async (_name, encode) => {
        const remote = makeDatabase()
        remote.username = 'Remote prepared source'
        const bytes = await encode(remote)
        const prepared = structuredClone(remote)
        prepared.username = 'Prepared remote'
        const harness = await makeHarness({
            databaseRead: { kind: 'value', bytes },
            remoteAssets: new Map(resources(remote).map((key) => [key, Uint8Array.of(1)])),
            remoteCold: new Map([
                ['cold-chat', { message: [{ data: 'remote chat' }] }],
                ['cold-message', { message: [{ data: 'remote message' }] }],
            ]),
            prepareCandidate: vi.fn(async () => structuredClone(prepared)),
        })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        const result = await harness.adapter.pull()

        expect(result.kind).toBe('activated')
        expect(harness.prepareCandidate).toHaveBeenCalledTimes(1)
        expect(replace).toHaveBeenCalledTimes(1)
        expect(replace).toHaveBeenCalledWith(prepared, harness.imported.revision)
        expect((await harness.store.readRoot()).value.username).toBe('Prepared remote')
    })

    it('returns missing without preparing or replacing', async () => {
        const harness = await makeHarness({ databaseRead: { kind: 'missing' } })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        await expect(harness.adapter.pull()).resolves.toEqual({ kind: 'missing' })
        expect(harness.prepareCandidate).not.toHaveBeenCalled()
        expect(replace).not.toHaveBeenCalled()
    })

    it('treats not-modified as unchanged only for the associated active revision', async () => {
        const database = makeDatabase()
        const bytes = encodeRisuSaveLegacy(database, 'compression')
        const harness = await makeHarness({
            database,
            databaseRead: { kind: 'not-modified', bytes },
            remoteAssets: new Map(resources(database).map((key) => [key, Uint8Array.of(1)])),
            remoteCold: new Map([
                ['cold-chat', { message: [{ data: 'remote chat' }] }],
                ['cold-message', { message: [{ data: 'remote message' }] }],
            ]),
        })
        const first = await harness.adapter.pull()
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')
        vi.mocked(harness.prepareCandidate).mockClear()

        await expect(harness.adapter.pull()).resolves.toEqual({ kind: 'unchanged' })

        expect(first.kind).toBe('activated')
        expect(harness.prepareCandidate).not.toHaveBeenCalled()
        expect(replace).not.toHaveBeenCalled()
    })

    it('rejects missing assets and invalid cold payloads before activation', async () => {
        const remote = makeDatabase()
        const bytes = encodeRisuSaveLegacy(remote, 'compression')
        const missingAsset = await makeHarness({
            databaseRead: { kind: 'value', bytes },
            remoteAssets: new Map(),
        })
        const missingReplace = vi.spyOn(missingAsset.store, 'replaceFromDatabase')
        await expect(missingAsset.adapter.pull()).rejects.toThrow('Missing official asset')
        expect(missingReplace).not.toHaveBeenCalled()

        const invalidCold = await makeHarness({
            databaseRead: { kind: 'value', bytes },
            remoteAssets: new Map(resources(remote).map((key) => [key, Uint8Array.of(1)])),
            remoteCold: new Map<string, unknown>([
                ['cold-chat', 'invalid'],
                ['cold-message', { message: [] }],
            ]),
        })
        const invalidReplace = vi.spyOn(invalidCold.store, 'replaceFromDatabase')
        await expect(invalidCold.adapter.pull()).rejects.toThrow('Invalid official cold payload')
        expect(invalidReplace).not.toHaveBeenCalled()
    })

    it('checks abort immediately before the single replacement', async () => {
        const remote = makeDatabase()
        const bytes = encodeRisuSaveLegacy(remote, 'compression')
        const controller = new AbortController()
        const harness = await makeHarness({
            databaseRead: { kind: 'value', bytes },
            remoteAssets: new Map(resources(remote).map((key) => [key, Uint8Array.of(1)])),
            remoteCold: new Map([
                ['cold-chat', { message: [] }],
                ['cold-message', { message: [] }],
            ]),
        })
        harness.cold.readRemote.mockImplementation(async (key: string) => {
            if (key === 'cold-message') controller.abort()
            return { message: [] }
        })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        await expect(harness.adapter.pull(controller.signal)).rejects.toMatchObject({ name: 'AbortError' })
        expect(replace).not.toHaveBeenCalled()
    })

    it('propagates a two-connection CAS race without retrying', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `official-race-${crypto.randomUUID()}`
        const first = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const second = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await first.open()
        await second.open()
        const local = makeDatabase()
        const imported = await first.replaceFromDatabase(local)
        const remote = makeDatabase()
        remote.username = 'Remote loses race'
        const concurrent = makeDatabase()
        concurrent.username = 'Concurrent wins'
        let raced = false
        const remoteCold = new Map<string, unknown>([
            ['cold-chat', { message: [] }],
            ['cold-message', { message: [] }],
        ])
        const replace = vi.spyOn(first, 'replaceFromDatabase')
        const adapter = new OfficialAccountSnapshotAdapter({
            store: first,
            resolveBlobs: async () => makeBlobStore(new Map()),
            account: {
                readItem: vi.fn(async (key: string): Promise<AccountReadResult> => key === databaseKey
                    ? { kind: 'value', bytes: encodeRisuSaveLegacy(remote, 'compression') }
                    : { kind: 'value', bytes: Uint8Array.of(1) }),
                writeItem: vi.fn(),
            },
            cold: {
                readLocal: vi.fn(async () => null),
                readRemote: vi.fn(async (key: string) => {
                    if (!raced) {
                        raced = true
                        await second.replaceFromDatabase(concurrent, imported.revision)
                    }
                    return remoteCold.get(key) ?? null
                }),
                writeRemote: vi.fn(),
            },
            prepareCandidate: async (value) => structuredClone(value),
            markPublished: vi.fn(),
        })

        await expect(adapter.pull()).rejects.toBeInstanceOf(RevisionConflictError)
        expect(replace).toHaveBeenCalledTimes(1)
        expect((await first.readRoot()).value.username).toBe('Concurrent wins')
    })
})

type DependencyContract = OfficialAccountSnapshotDependencies
const _requiresCandidatePreparation: DependencyContract['prepareCandidate'] = async (database) => database
void _requiresCandidatePreparation
