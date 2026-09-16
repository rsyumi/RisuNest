import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import {
    coldStorageHeader,
    listCharacterResources,
    listDatabaseRootResources,
} from '../../process/coldstorageData'
import type { AccountReadResult, AccountWriteOptions, AccountWriteResult } from '../accountStorage'
import type { BlobMetadata, BlobStore } from '../blobStore'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import type { Database } from '../database.svelte'
import { RevisionConflictError, type PersistentDataStore } from '../persistentDataStore'
import { decodeRisuSave, encodeRisuSaveLegacy } from '../risuSave'
import { streamRisuSaveFromStore } from '../risuSaveStoreAdapter'
import { risuSaveFixtureDatabase } from '../tests/risuSaveFixtures'
import {
    createOfficialAssetLedger,
    type LedgerStorage,
    type OfficialAssetLedger,
} from './officialAssetLedger'
import {
    OfficialAccountSnapshotAdapter,
    createOfficialAssociationMarkers,
    type OfficialAccountSnapshotDependencies,
    type OfficialAssociationMarkers,
    type OfficialNativeDatabasePublisher,
    type OfficialSyncConflictBackupInput,
    type OfficialSyncConflictContext,
    type OfficialSyncConflictHandler,
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
    const { characters, ...root } = database
    return [...new Set([
        ...listDatabaseRootResources(root),
        ...characters.flatMap((character) => listCharacterResources(character)),
    ])]
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
    accountId?: string
    blobs?: Map<string, Uint8Array>
    remoteAssets?: Map<string, Uint8Array>
    remoteCold?: Map<string, unknown>
    databaseRead?: AccountReadResult
    prepareCandidate?: (database: Database) => Promise<Database>
    ledger?: OfficialAssetLedger
    association?: OfficialAssociationMarkers
    conflict?: OfficialSyncConflictHandler
    now?: () => number
    nativeDatabasePublisher?: OfficialNativeDatabasePublisher
    flushPublicationMetadata?: () => Promise<void>
    events?: string[]
}

function memoryLedgerStorage(): LedgerStorage {
    const values = new Map<string, string>()
    return {
        getItem: (key) => values.get(key) ?? null,
        setItem: (key, value) => void values.set(key, value),
        removeItem: (key) => void values.delete(key),
    }
}

async function makeHarness(options: HarnessOptions = {}) {
    const database = options.database ?? makeDatabase()
    if (options.accountId) {
        database.account = { id: options.accountId, token: 'token', data: {} }
    }
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
    const remoteCold = options.remoteCold ?? new Map<string, unknown>()
    const events = options.events ?? []
    const writes: Array<{ key: string; bytes?: Uint8Array; value?: unknown }> = []
    const readItem = vi.fn(async (key: string): Promise<AccountReadResult> => {
        if (key === databaseKey) {
            return options.databaseRead ?? { kind: 'missing' }
        }
        const bytes = remoteAssets.get(key)
        return bytes ? { kind: 'value', bytes: bytes.slice() } : { kind: 'missing' }
    })
    const writeItem = vi.fn(async (
        key: string,
        bytes: Uint8Array,
        _options?: AccountWriteOptions,
    ): Promise<AccountWriteResult> => {
        events.push(`asset:${key}`)
        writes.push({ key, bytes: bytes.slice() })
        return { kind: 'written', replacementKey: `remote/${key}` }
    })
    const cold = {
        readRemote: vi.fn(async (key: string) => structuredClone(remoteCold.get(key) ?? null)),
    }
    const markPublished = vi.fn()
    const prepareCandidate = options.prepareCandidate ?? vi.fn(async (value: Database) => structuredClone(value))
    const resolveBlobs = vi.fn(async () => blobStore)
    const ledger = options.ledger ?? createOfficialAssetLedger(memoryLedgerStorage(), 'test-account')
    const dependencies: OfficialAccountSnapshotDependencies = {
        store,
        resolveBlobs,
        account: { readItem, writeItem },
        cold,
        prepareCandidate,
        markPublished,
        ledger,
        association: options.association,
        conflict: options.conflict,
        now: options.now,
        nativeDatabasePublisher: options.nativeDatabasePublisher,
        flushPublicationMetadata: options.flushPublicationMetadata,
    }
    const adapter = new OfficialAccountSnapshotAdapter(dependencies)
    return {
        adapter,
        restartAdapter: () => new OfficialAccountSnapshotAdapter(dependencies),
        blobStore,
        cold,
        database,
        events,
        imported,
        ledger,
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
    it('adopts a recovered receipt only for the active account and a non-future revision', async () => {
        const association: OfficialAssociationMarkers = {
            load: vi.fn(() => null),
            save: vi.fn(),
        }
        const harness = await makeHarness({
            accountId: 'account-1',
            association,
            now: () => 1234,
        })
        const fingerprint = 'c'.repeat(64)

        await harness.adapter.adoptPublishedRevision({
            accountId: 'account-1',
            revision: harness.imported.revision,
            databaseFingerprint: fingerprint,
        })

        expect(harness.markPublished).toHaveBeenCalledWith(harness.imported.revision)
        expect(association.save).toHaveBeenCalledWith('account-1', {
            revision: harness.imported.revision,
            databaseFingerprint: fingerprint,
            syncedAt: 1234,
        })

        await expect(harness.adapter.adoptPublishedRevision({
            accountId: 'account-2',
            revision: harness.imported.revision,
            databaseFingerprint: fingerprint,
        })).rejects.toThrow('account does not match')
        await expect(harness.adapter.adoptPublishedRevision({
            accountId: 'account-1',
            revision: harness.imported.revision + 1,
            databaseFingerprint: fingerprint,
        })).rejects.toThrow('newer than the local revision')
        expect(harness.markPublished).toHaveBeenCalledOnce()
        expect(association.save).toHaveBeenCalledOnce()
    })

    it('publishes the exact replacement projection natively and finalizes it durably in order', async () => {
        const events: string[] = []
        const fingerprint = 'a'.repeat(64)
        const association: OfficialAssociationMarkers = {
            load: vi.fn(() => null),
            save: vi.fn(() => events.push('association')),
        }
        const acknowledge = vi.fn(async () => { events.push('acknowledge') })
        const completeReload = vi.fn(async () => { events.push('reload') })
        const nativeDatabasePublisher = vi.fn<OfficialNativeDatabasePublisher>(async () => {
            events.push('native-database')
            return { databaseFingerprint: fingerprint, acknowledge, completeReload }
        })
        const harness = await makeHarness({
            accountId: 'account-1',
            association,
            nativeDatabasePublisher,
            flushPublicationMetadata: vi.fn(async () => { events.push('metadata-flush') }),
            events,
        })
        harness.markPublished.mockImplementation(async () => { events.push('mark-published') })
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const publication = await harness.adapter.pin(harness.imported.revision)
        const lease = await acquireRevision.mock.results[0].value
        const releaseLease = lease.release.bind(lease)
        vi.spyOn(lease, 'release').mockImplementation(async () => {
            events.push('lease-release')
            await releaseLease()
        })

        await publication.publish()

        expect(nativeDatabasePublisher).toHaveBeenCalledOnce()
        const input = nativeDatabasePublisher.mock.calls[0][0]
        expect(input).toMatchObject({
            revision: harness.imported.revision,
            accountId: 'account-1',
            lease,
        })
        expect(input).not.toHaveProperty('bytes')
        expect(input).not.toHaveProperty('path')
        expect(Object.keys(input.resourceReplacements)).toEqual(
            Object.keys(input.resourceReplacements).sort(),
        )
        for (const [local, remote] of Object.entries(input.resourceReplacements)) {
            expect(local).toMatch(/^assets\//)
            expect(remote).toBe(`remote/${local}`)
        }
        expect(harness.writeItem.mock.calls.some(([key]) => key === databaseKey)).toBe(false)
        expect(events.slice(-8)).toEqual([
            'metadata-flush',
            'native-database',
            'mark-published',
            'association',
            'metadata-flush',
            'acknowledge',
            'lease-release',
            'reload',
        ])
    })

    it('falls back before native job acceptance but never after a native publication failure', async () => {
        const unavailable = vi.fn<OfficialNativeDatabasePublisher>(async () => null)
        const fallback = await makeHarness({
            accountId: 'account-1',
            nativeDatabasePublisher: unavailable,
            flushPublicationMetadata: vi.fn(async () => undefined),
        })

        await (await fallback.adapter.pin(fallback.imported.revision)).publish()

        expect(unavailable).toHaveBeenCalledOnce()
        expect(fallback.writeItem.mock.calls.some(([key]) => key === databaseKey)).toBe(true)

        const failed = vi.fn<OfficialNativeDatabasePublisher>(async () => {
            throw new Error('native publication failed after acceptance')
        })
        const noFallback = await makeHarness({
            accountId: 'account-1',
            nativeDatabasePublisher: failed,
            flushPublicationMetadata: vi.fn(async () => undefined),
        })
        const publication = await noFallback.adapter.pin(noFallback.imported.revision)

        await expect(publication.publish()).rejects.toThrow(
            'native publication failed after acceptance',
        )
        expect(noFallback.writeItem.mock.calls.some(([key]) => key === databaseKey)).toBe(false)
        expect(noFallback.markPublished).not.toHaveBeenCalled()
    })

    it('reacquires the exact current revision before retrying a consumed native lease', async () => {
        const leases: Array<Parameters<OfficialNativeDatabasePublisher>[0]['lease']> = []
        let attempt = 0
        const nativeDatabasePublisher = vi.fn<OfficialNativeDatabasePublisher>(async (input) => {
            leases.push(input.lease)
            const root = await input.lease.readRoot()
            expect(root.revision).toBe(input.revision)
            if (attempt++ === 0) {
                await input.lease.release()
                throw new Error('native publication failed after consuming its lease')
            }
            return {
                databaseFingerprint: 'd'.repeat(64),
                acknowledge: vi.fn(async () => undefined),
                completeReload: vi.fn(async () => undefined),
            }
        })
        const harness = await makeHarness({
            accountId: 'account-1',
            nativeDatabasePublisher,
            flushPublicationMetadata: vi.fn(async () => undefined),
        })
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const publication = await harness.adapter.pin(harness.imported.revision)

        await expect(publication.publish()).rejects.toThrow(
            'native publication failed after consuming its lease',
        )
        await expect(publication.publish()).resolves.toBeUndefined()

        expect(acquireRevision).toHaveBeenCalledTimes(2)
        expect(acquireRevision).toHaveBeenNthCalledWith(1, harness.imported.revision)
        expect(acquireRevision).toHaveBeenNthCalledWith(2, harness.imported.revision)
        expect(leases).toHaveLength(2)
        expect(leases[1]).not.toBe(leases[0])
        expect(harness.markPublished).toHaveBeenCalledWith(harness.imported.revision)
    })

    it('retries only local finalization after a native remote commit', async () => {
        const events: string[] = []
        const acknowledge = vi.fn(async () => { events.push('acknowledge') })
        const nativeDatabasePublisher = vi.fn<OfficialNativeDatabasePublisher>(async () => {
            events.push('native-database')
            return {
                databaseFingerprint: 'b'.repeat(64),
                acknowledge,
                completeReload: vi.fn(async () => { events.push('reload') }),
            }
        })
        let flushAttempts = 0
        const harness = await makeHarness({
            accountId: 'account-1',
            nativeDatabasePublisher,
            flushPublicationMetadata: async () => {
                events.push('metadata-flush')
                if (flushAttempts++ === 1) throw new Error('metadata unavailable')
            },
        })
        const publication = await harness.adapter.pin(harness.imported.revision)

        await expect(publication.publish()).rejects.toThrow('metadata unavailable')
        expect(nativeDatabasePublisher).toHaveBeenCalledOnce()
        expect(acknowledge).not.toHaveBeenCalled()

        await expect(publication.publish()).resolves.toBeUndefined()

        expect(nativeDatabasePublisher).toHaveBeenCalledOnce()
        expect(harness.markPublished).toHaveBeenCalledOnce()
        expect(acknowledge).toHaveBeenCalledOnce()
        expect(events).toEqual([
            'metadata-flush',
            'native-database',
            'metadata-flush',
            'metadata-flush',
            'acknowledge',
            'reload',
        ])
    })

    it('does not transfer database ownership before asset and cold metadata is durable', async () => {
        const nativeDatabasePublisher = vi.fn<OfficialNativeDatabasePublisher>()
        const harness = await makeHarness({
            accountId: 'account-1',
            nativeDatabasePublisher,
            flushPublicationMetadata: vi.fn(async () => {
                throw new Error('metadata unavailable')
            }),
        })
        const publication = await harness.adapter.pin(harness.imported.revision)

        await expect(publication.publish()).rejects.toThrow('metadata unavailable')

        expect(nativeDatabasePublisher).not.toHaveBeenCalled()
        expect(harness.writeItem.mock.calls.some(([key]) => key === databaseKey)).toBe(false)
        await publication.dispose()
    })

    it('aborts an in-flight native publication and releases its lease only after the job settles', async () => {
        let finishCancellation: () => void = () => undefined
        const cancellationSettled = new Promise<void>((resolve) => {
            finishCancellation = resolve
        })
        const aborted = new DOMException('native job cancelled', 'AbortError')
        const nativeDatabasePublisher = vi.fn<OfficialNativeDatabasePublisher>(async ({ signal }) => {
            await new Promise<void>((resolve) => {
                if (signal?.aborted) return resolve()
                signal?.addEventListener('abort', () => resolve(), { once: true })
            })
            await cancellationSettled
            throw aborted
        })
        const harness = await makeHarness({
            accountId: 'account-1',
            nativeDatabasePublisher,
            flushPublicationMetadata: vi.fn(async () => undefined),
        })
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const publication = await harness.adapter.pin(harness.imported.revision)
        const lease = await acquireRevision.mock.results[0].value
        const release = vi.spyOn(lease, 'release')

        const publishing = publication.publish()
        await vi.waitFor(() => expect(nativeDatabasePublisher).toHaveBeenCalledOnce())
        const disposing = publication.dispose()
        await vi.waitFor(() => expect(
            nativeDatabasePublisher.mock.calls[0][0].signal?.aborted,
        ).toBe(true))
        expect(release).not.toHaveBeenCalled()

        finishCancellation()
        await expect(publishing).rejects.toBe(aborted)
        await expect(disposing).resolves.toBeUndefined()
        expect(release).toHaveBeenCalledOnce()
        expect(harness.markPublished).not.toHaveBeenCalled()
    })

    it('releases its lease when disposal aborts a pending asset account reauthentication', async () => {
        const harness = await makeHarness()
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const publication = await harness.adapter.pin(harness.imported.revision)
        const lease = await acquireRevision.mock.results[0].value
        const release = vi.spyOn(lease, 'release')
        harness.writeItem.mockImplementationOnce(async (_key, _bytes, options) => {
            await new Promise<never>((_resolve, reject) => {
                options?.signal?.addEventListener('abort', () => {
                    reject(options.signal?.reason)
                }, { once: true })
            })
            throw new Error('Asset upload must not retry after disposal')
        })

        const publishing = publication.publish()
        await vi.waitFor(() => expect(harness.writeItem).toHaveBeenCalledOnce())
        const disposing = publication.dispose()

        await expect(publishing).rejects.toMatchObject({ name: 'AbortError' })
        await expect(disposing).resolves.toBeUndefined()
        expect(release).toHaveBeenCalledOnce()
        expect(harness.writeItem).toHaveBeenCalledOnce()
        expect(harness.markPublished).not.toHaveBeenCalled()
    })

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

    it('retries lease cleanup after a transient release failure without republishing', async () => {
        const harness = await makeHarness()
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const publication = await harness.adapter.pin(harness.imported.revision)
        const lease = await acquireRevision.mock.results[0].value
        const releaseLease = lease.release.bind(lease)
        const release = vi.spyOn(lease, 'release')
            .mockRejectedValueOnce(new Error('release unavailable'))
            .mockImplementation(releaseLease)

        await expect(publication.publish()).resolves.toBeUndefined()
        const databaseWrites = () => harness.writeItem.mock.calls
            .filter(([key]) => key === databaseKey).length
        expect(databaseWrites()).toBe(1)

        await expect(publication.publish()).resolves.toBeUndefined()
        await expect(publication.dispose()).resolves.toBeUndefined()

        expect(release).toHaveBeenCalledTimes(2)
        expect(databaseWrites()).toBe(1)
        expect(harness.markPublished).toHaveBeenCalledTimes(1)
    })

    it('retries dispose after a transient release failure', async () => {
        const harness = await makeHarness()
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const publication = await harness.adapter.pin(harness.imported.revision)
        const lease = await acquireRevision.mock.results[0].value
        const releaseLease = lease.release.bind(lease)
        const release = vi.spyOn(lease, 'release')
            .mockRejectedValueOnce(new Error('release unavailable'))
            .mockImplementation(releaseLease)

        await expect(publication.dispose()).resolves.toBeUndefined()
        await expect(publication.dispose()).resolves.toBeUndefined()

        expect(release).toHaveBeenCalledTimes(2)
        await expect(publication.publish()).rejects.toThrow('disposed')
    })

    it('publishes sorted assets, then the projected database', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)

        await publication.publish()

        const assetEvents = harness.events.filter((event) => event.startsWith('asset:assets/'))
        expect(assetEvents).toEqual([...assetEvents].sort())
        expect(harness.events.slice(0, assetEvents.length)).toEqual(assetEvents)
        expect(harness.events.at(-1)).toBe(`asset:${databaseKey}`)

        const databaseCalls = harness.writeItem.mock.calls.filter(([key]) => key === databaseKey)
        expect(databaseCalls).toHaveLength(1)
        expect(databaseCalls[0][1]).toBeInstanceOf(Uint8Array)
        const databaseWrite = harness.writes.find((write) => write.key === databaseKey)
        const projected = await decodeRisuSave(databaseWrite!.bytes!) as Database
        for (const key of resources(projected)) {
            expect(key).toMatch(/^remote\/assets\//)
        }
    })

    it('pins a legacy backslash asset key and uploads its normalized local payload', async () => {
        const database = makeDatabase() as any
        const legacyKey = 'assets\\windows.gif'
        database.characters[0].additionalAssets = [['legacy', legacyKey, 'gif']]
        const localBlobs = new Map(
            resources(database)
                .filter((key) => key !== legacyKey)
                .map((key, index): [string, Uint8Array] => [key, Uint8Array.of(index + 1)]),
        )
        localBlobs.set('assets/windows.gif', Uint8Array.of(77))
        const harness = await makeHarness({ database, blobs: localBlobs })

        const publication = await harness.adapter.pin(harness.imported.revision)
        await publication.publish()

        const upload = harness.writes.find((write) => write.key === legacyKey)
        expect(upload?.bytes).toEqual(Uint8Array.of(77))
        const databaseWrite = harness.writes.find((write) => write.key === databaseKey)
        const projected = await decodeRisuSave(databaseWrite!.bytes!) as any
        expect(projected.characters[0].additionalAssets[0][1]).toBe(`remote/${legacyKey}`)
    })

    it('publishes without a legacy backslash asset whose payload no longer exists anywhere', async () => {
        const database = makeDatabase() as any
        const legacyKey = 'assets\\gone.gif'
        database.characters[0].additionalAssets = [['legacy', legacyKey, 'gif']]
        const localBlobs = new Map(
            resources(database)
                .filter((key) => key !== legacyKey)
                .map((key, index): [string, Uint8Array] => [key, Uint8Array.of(index + 1)]),
        )
        const harness = await makeHarness({ database, blobs: localBlobs })

        const publication = await harness.adapter.pin(harness.imported.revision)
        await publication.publish()

        expect(harness.writes.some((write) => write.key === legacyKey)).toBe(false)
        expect(harness.writes.some((write) => write.key === databaseKey)).toBe(true)
    })

    it('leaves sentinel, URL, data URL, and Tauri path resources outside official asset I/O', async () => {
        const database = makeDatabase() as any
        database.customBackground = '-'
        database.userIcon = 'https://example.invalid/user.png'
        database.modules[0].icon = 'data:image/png;base64,AA=='
        database.characters[0].image = 'C:\\app-data\\portrait.png'
        const harness = await makeHarness({ database })

        const publication = await harness.adapter.pin(harness.imported.revision)
        await publication.publish()

        const excluded = [
            '-',
            'https://example.invalid/user.png',
            'data:image/png;base64,AA==',
            'C:\\app-data\\portrait.png',
        ]
        for (const key of excluded) {
            expect(harness.blobStore.stat).not.toHaveBeenCalledWith(key)
            expect(harness.readItem).not.toHaveBeenCalledWith(key)
            expect(harness.writeItem.mock.calls.some(([written]) => written === key)).toBe(false)
        }
        const databaseWrite = harness.writes.find((write) => write.key === databaseKey)!
        const projected = await decodeRisuSave(databaseWrite.bytes!) as any
        expect(projected.customBackground).toBe('-')
        expect(projected.userIcon).toBe('https://example.invalid/user.png')
        expect(projected.modules[0].icon).toBe('data:image/png;base64,AA==')
        expect(projected.characters[0].image).toBe('C:\\app-data\\portrait.png')
    })

    it('publishes the pinned snapshot after a later commit without creating a publication revision', async () => {
        const harness = await makeHarness()
        const publication = await harness.adapter.pin(harness.imported.revision)
        const root = (await harness.store.readRoot()).value
        const later = await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Later local user' },
            pluginStorage: [
                { type: 'set', owner: 'test-plugin', key: 'fixture', value: { value: 'later' } },
            ],
        })
        const commit = vi.spyOn(harness.store, 'commit')
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        await publication.publish()

        const databaseWrite = harness.writes.find((write) => write.key === databaseKey)
        const published = await decodeRisuSave(databaseWrite!.bytes!)
        expect(published.username).toBe('Snapshot User')
        expect(published.pluginCustomStorage).toEqual({ fixture: { value: 'stored' } })
        expect((await harness.store.readRoot()).value.username).toBe('Later local user')
        expect((await harness.store.readPluginStorage('test-plugin', 'fixture'))?.value).toEqual({
            value: 'later',
        })
        expect(harness.markPublished).toHaveBeenCalledWith(harness.imported.revision)
        expect(later.revision).toBe(harness.imported.revision + 1)
        expect(commit).not.toHaveBeenCalled()
        expect(replace).not.toHaveBeenCalled()
    })

    it('publishes and projects exact plugin storage assets from the pinned revision', async () => {
        const database = makeDatabase()
        const pluginAssetKey = 'assets/plugin-official.bin'
        const laterAssetKey = 'assets/plugin-later.bin'
        const rejectedNestedKey = 'assets/plugins/plugin-nested.bin'
        database.pluginCustomStorage = {
            plugin: {
                nested: [{ asset: pluginAssetKey }],
                rejectedNested: rejectedNestedKey,
                prose: 'prefix assets/not-a-reference.bin',
            },
        }
        const blobs = new Map(
            resources(database).map((key, index): [string, Uint8Array] => [
                key,
                Uint8Array.of(index + 1),
            ]),
        )
        blobs.set(pluginAssetKey, Uint8Array.of(91, 92))
        blobs.set(laterAssetKey, Uint8Array.of(93))
        blobs.set(rejectedNestedKey, Uint8Array.of(94))
        const harness = await makeHarness({ database, blobs })

        const publication = await harness.adapter.pin(harness.imported.revision)
        const root = (await harness.store.readRoot()).value
        await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Later local user' },
            pluginStorage: [{
                type: 'set',
                owner: 'test-plugin',
                key: 'plugin',
                value: { nested: [{ asset: laterAssetKey }] },
            }],
        })

        await publication.publish()

        expect(harness.writeItem.mock.calls.some(([key]) => key === pluginAssetKey)).toBe(true)
        expect(harness.writeItem.mock.calls.some(([key]) => key === laterAssetKey)).toBe(false)
        expect(harness.writeItem.mock.calls.some(([key]) => key === rejectedNestedKey)).toBe(false)
        expect(harness.writeItem.mock.calls.some(
            ([key]) => key === 'assets/not-a-reference.bin',
        )).toBe(false)
        const databaseWrite = harness.writes.find((write) => write.key === databaseKey)
        const projected = await decodeRisuSave(databaseWrite!.bytes!)
        expect(projected.pluginCustomStorage.plugin).toEqual({
            nested: [{ asset: `remote/${pluginAssetKey}` }],
            rejectedNested: rejectedNestedKey,
            prose: 'prefix assets/not-a-reference.bin',
        })
        expect((await harness.store.readPluginStorage('test-plugin', 'plugin'))?.value).toEqual({
            nested: [{ asset: laterAssetKey }],
        })
    })

    it('uploads each local asset once across publications', async () => {
        const harness = await makeHarness()
        const assetCount = resources(harness.database).length

        await (await harness.adapter.pin(harness.imported.revision)).publish()
        const afterFirst = harness.events.filter((event) => event.startsWith('asset:assets/')).length
        await (await harness.adapter.pin(harness.imported.revision)).publish()

        expect(afterFirst).toBe(assetCount)
        expect(harness.events.filter((event) => event.startsWith('asset:assets/'))).toHaveLength(assetCount)
        expect(harness.writes.filter((write) => write.key === databaseKey)).toHaveLength(2)
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
        expect(harness.markPublished).toHaveBeenCalledTimes(1)
    })

    it('validates remote-only resources before publishing', async () => {
        const database = makeDatabase()
        const allResources = resources(database)
        const localKey = allResources[0]
        const remoteOnly = allResources.slice(1)
        const harness = await makeHarness({
            database,
            blobs: new Map([[localKey, Uint8Array.of(9)]]),
            remoteAssets: new Map(remoteOnly.map((key) => [key, Uint8Array.of(7)])),
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
    })

    it('publishes without assets that are unavailable everywhere and probes each missing key once', async () => {
        const database = makeDatabase()
        const consoleWarn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
        try {
            const harness = await makeHarness({ database, blobs: new Map(), remoteAssets: new Map() })

            await (await harness.adapter.pin(harness.imported.revision)).publish()

            expect(harness.writes.some((write) => write.key.startsWith('assets/'))).toBe(false)
            const databaseWrite = harness.writes.find((write) => write.key === databaseKey)
            const projected = await decodeRisuSave(databaseWrite!.bytes!) as Database
            expect(resources(projected)).toEqual(resources(database))

            const probesAfterFirst = harness.readItem.mock.calls.length
            await (await harness.adapter.pin(harness.imported.revision)).publish()
            expect(harness.readItem.mock.calls.length).toBe(probesAfterFirst)
        } finally {
            consoleWarn.mockRestore()
        }
    })

    it('releases the lease when resolving blobs fails during pin', async () => {
        const harness = await makeHarness()
        harness.resolveBlobs.mockRejectedValue(new Error('blobs offline'))
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')

        await expect(harness.adapter.pin(harness.imported.revision)).rejects.toThrow('blobs offline')
        const lease = await acquireRevision.mock.results[0].value
        await expect(lease.readRoot()).rejects.toThrow('released')
        expect(harness.writeItem).not.toHaveBeenCalled()
    })

    it('preserves the pin error when lease cleanup also fails', async () => {
        const harness = await makeHarness()
        harness.resolveBlobs.mockRejectedValue(new Error('blobs offline'))
        const acquireLease = harness.store.acquireRevision.bind(harness.store)
        const release = vi.fn(async () => {
            throw new Error('release unavailable')
        })
        vi.spyOn(harness.store, 'acquireRevision').mockImplementation(async (revision) => {
            const lease = await acquireLease(revision)
            lease.release = release
            return lease
        })

        await expect(harness.adapter.pin(harness.imported.revision)).rejects.toThrow(
            'blobs offline',
        )

        expect(release).toHaveBeenCalledTimes(2)
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
        const acquireRevision = vi.spyOn(harness.store, 'acquireRevision')
        const publication = await harness.adapter.pin(harness.imported.revision)
        const lease = await acquireRevision.mock.results[0].value
        const releaseLease = lease.release.bind(lease)
        const release = vi.spyOn(lease, 'release')
            .mockRejectedValueOnce(new Error('release unavailable'))
            .mockImplementation(releaseLease)
        const abort = new DOMException('cancelled', 'AbortError')
        harness.writeItem.mockRejectedValueOnce(abort)

        await expect(publication.publish()).rejects.toBe(abort)

        expect((await harness.store.readRoot()).revision).toBe(harness.imported.revision)
        expect(harness.markPublished).not.toHaveBeenCalled()
        await expect(publication.dispose()).resolves.toBeUndefined()
        expect(release).toHaveBeenCalledTimes(2)
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
        const activated = replace.mock.calls[0][0] as any
        expect(replace.mock.calls[0][1]).toBe(harness.imported.revision)
        expect(activated.username).toBe('Prepared remote')
        expect(activated.characters[0].coldStoragedChats).toBeUndefined()
        expect(activated.characters[0].chats.at(-1).message).toEqual([{ data: 'remote message' }])
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

    it('stages stale cached bytes when the revision matches but the fingerprint does not', async () => {
        const local = makeDatabase()
        const stale = makeDatabase()
        stale.username = 'Cached A'
        const fresh = makeDatabase()
        fresh.username = 'Fresh B'
        const staleBytes = encodeRisuSaveLegacy(stale, 'compression')
        const freshBytes = encodeRisuSaveLegacy(fresh, 'compression')
        const harness = await makeHarness({
            database: local,
            databaseRead: { kind: 'value', bytes: freshBytes },
            remoteAssets: new Map(resources(fresh).map((key) => [key, Uint8Array.of(1)])),
            remoteCold: new Map([
                ['cold-chat', { message: [] }],
                ['cold-message', { message: [] }],
            ]),
        })
        await harness.adapter.pull()
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'not-modified', bytes: staleBytes }
            return { kind: 'value', bytes: Uint8Array.of(1) }
        })

        const result = await harness.adapter.pull()

        expect(result.kind).toBe('activated')
        expect((await harness.store.readRoot()).value.username).toBe('Cached A')
    })

    it('rejects an abort that arrives while fingerprinting associated cached bytes', async () => {
        const database = makeDatabase()
        const bytes = encodeRisuSaveLegacy(database, 'compression')
        const harness = await makeHarness({
            database,
            databaseRead: { kind: 'value', bytes },
            remoteAssets: new Map(resources(database).map((key) => [key, Uint8Array.of(1)])),
            remoteCold: new Map([
                ['cold-chat', { message: [] }],
                ['cold-message', { message: [] }],
            ]),
        })
        await harness.adapter.pull()
        harness.readItem.mockResolvedValue({ kind: 'not-modified', bytes })
        const controller = new AbortController()
        const originalDigest = globalThis.crypto.subtle.digest.bind(globalThis.crypto.subtle)
        const digest = vi.spyOn(globalThis.crypto.subtle, 'digest').mockImplementation(
            async (...args) => {
                controller.abort()
                return originalDigest(...args)
            },
        )

        try {
            await expect(harness.adapter.pull(controller.signal)).rejects.toMatchObject({
                name: 'AbortError',
            })
        } finally {
            digest.mockRestore()
        }
    })

    it('activates without probing account assets and keeps a degraded cold record', async () => {
        const remote = makeDatabase() as any
        remote.characters[0].additionalAssets = [['legacy', 'assets\\windows.gif', 'gif']]
        const bytes = encodeRisuSaveLegacy(remote, 'compression')
        const harness = await makeHarness({
            databaseRead: { kind: 'value', bytes },
            remoteAssets: new Map(),
            remoteCold: new Map(),
        })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        const result = await harness.adapter.pull()

        expect(result.kind).toBe('activated')
        expect(replace).toHaveBeenCalledTimes(1)
        expect(harness.readItem.mock.calls.map(([key]) => key)).toEqual([databaseKey])
        expect(harness.cold.readRemote.mock.calls.map(([key]) => key)).toEqual(['cold-message'])
        const activated = replace.mock.calls[0][0] as any
        expect(activated.characters[0].coldStoragedChats).toBeUndefined()
        expect(activated.characters[0].chats.at(-1).message[0].data)
            .toBe(`${coldStorageHeader}cold-message`)
    })

    it('checks abort immediately before the single replacement', async () => {
        const remote = makeDatabase()
        const bytes = encodeRisuSaveLegacy(remote, 'compression')
        const controller = new AbortController()
        const harness = await makeHarness({
            databaseRead: { kind: 'value', bytes },
            prepareCandidate: vi.fn(async (value: Database) => {
                controller.abort()
                return structuredClone(value)
            }),
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
            cold: { readRemote: vi.fn(async () => null) },
            prepareCandidate: async (value) => {
                if (!raced) {
                    raced = true
                    await second.replaceFromDatabase(concurrent, imported.revision)
                }
                return structuredClone(value)
            },
            markPublished: vi.fn(),
            ledger: createOfficialAssetLedger(memoryLedgerStorage(), 'other-account'),
        })

        await expect(adapter.pull()).rejects.toBeInstanceOf(RevisionConflictError)
        expect(replace).toHaveBeenCalledTimes(1)
        expect((await first.readRoot()).value.username).toBe('Concurrent wins')
    })
})

describe('OfficialAccountSnapshotAdapter persisted association', () => {
    const accountId = 'assoc-account'

    it('reloads the keyed durable association when the active account changes', async () => {
        const association: OfficialAssociationMarkers = {
            load: vi.fn(() => null),
            save: vi.fn(),
        }
        const harness = await makeHarness({ accountId, association })

        harness.adapter.resetAccountAssociation(accountId)
        harness.adapter.resetAccountAssociation('other-account')

        expect(association.load).toHaveBeenNthCalledWith(1, accountId)
        expect(association.load).toHaveBeenNthCalledWith(2, 'other-account')
    })

    async function publishHarness(options: HarnessOptions = {}) {
        const association = options.association
            ?? createOfficialAssociationMarkers(memoryLedgerStorage())
        const harness = await makeHarness({ accountId, association, ...options })
        await (await harness.adapter.pin(harness.imported.revision)).publish()
        const publishedBytes = harness.writes.find((write) => write.key === databaseKey)!.bytes!
        return { ...harness, association, publishedBytes }
    }

    it('pulls a changed remote normally after a restart with a clean published state', async () => {
        const harness = await publishHarness({
            remoteCold: new Map([
                ['cold-chat', { message: [] }],
                ['cold-message', { message: [] }],
            ]),
        })
        const remote = makeDatabase()
        remote.account = { id: accountId, token: 'token', data: {} }
        remote.username = 'Remote after restart'
        const remoteBytes = encodeRisuSaveLegacy(remote, 'compression')
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'value', bytes: remoteBytes }
            return { kind: 'value', bytes: Uint8Array.of(1) }
        })
        const restarted = harness.restartAdapter()

        const result = await restarted.pull()

        expect(result.kind).toBe('activated')
        expect((await harness.store.readRoot()).value.username).toBe('Remote after restart')
    })

    it('treats an unchanged remote as a no-op pull after a restart', async () => {
        const harness = await publishHarness()
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'not-modified', bytes: harness.publishedBytes }
            return { kind: 'missing' }
        })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')
        const restarted = harness.restartAdapter()

        await expect(restarted.pull()).resolves.toEqual({ kind: 'unchanged' })
        expect(replace).not.toHaveBeenCalled()
    })

    it('keeps unpublished local commits across a restart instead of restoring the published snapshot', async () => {
        const harness = await publishHarness()
        const root = (await harness.store.readRoot()).value
        await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Offline edit' },
        })
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'value', bytes: harness.publishedBytes }
            return { kind: 'missing' }
        })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')
        const restarted = harness.restartAdapter()

        await expect(restarted.pull()).resolves.toEqual({ kind: 'kept-local', conflict: false })
        expect(replace).not.toHaveBeenCalled()
        expect(harness.prepareCandidate).not.toHaveBeenCalled()
        expect((await harness.store.readRoot()).value.username).toBe('Offline edit')
    })

    it('flags the conflict but still keeps unpublished local commits when the remote changed too', async () => {
        const harness = await publishHarness()
        const root = (await harness.store.readRoot()).value
        await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Offline edit' },
        })
        const remote = makeDatabase()
        remote.username = 'Other device'
        const remoteBytes = encodeRisuSaveLegacy(remote, 'compression')
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'value', bytes: remoteBytes }
            return { kind: 'value', bytes: Uint8Array.of(1) }
        })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')
        const restarted = harness.restartAdapter()

        await expect(restarted.pull()).resolves.toEqual({ kind: 'kept-local', conflict: true })
        expect(replace).not.toHaveBeenCalled()
        expect((await harness.store.readRoot()).value.username).toBe('Offline edit')
    })

    it('publishes after a skipped pull and re-associates the new revision', async () => {
        const harness = await publishHarness()
        const root = (await harness.store.readRoot()).value
        const later = await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Offline edit' },
        })
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'value', bytes: harness.publishedBytes }
            return { kind: 'missing' }
        })
        const restarted = harness.restartAdapter()
        await expect(restarted.pull()).resolves.toEqual({ kind: 'kept-local', conflict: false })

        await (await restarted.pin(later.revision)).publish()

        const republished = harness.writes.filter((write) => write.key === databaseKey).at(-1)!.bytes!
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'not-modified', bytes: republished }
            return { kind: 'missing' }
        })
        await expect(restarted.pull()).resolves.toEqual({ kind: 'unchanged' })
        await expect(harness.restartAdapter().pull()).resolves.toEqual({ kind: 'unchanged' })
    })
})

describe('OfficialAccountSnapshotAdapter conflict resolution', () => {
    const accountId = 'conflict-account'

    interface ConflictOptions {
        conflict?: OfficialSyncConflictHandler
        now?: () => number
    }

    async function divergedHarness(options: ConflictOptions = {}) {
        const association = createOfficialAssociationMarkers(memoryLedgerStorage())
        const harness = await makeHarness({
            accountId,
            association,
            conflict: options.conflict,
            now: options.now,
            remoteCold: new Map([
                ['cold-chat', { message: [] }],
                ['cold-message', { message: [] }],
            ]),
        })
        await (await harness.adapter.pin(harness.imported.revision)).publish()
        const root = (await harness.store.readRoot()).value
        await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Offline edit' },
        })
        const remote = makeDatabase()
        remote.account = { id: accountId, token: 'token', data: {} }
        remote.username = 'Other device'
        const remoteBytes = encodeRisuSaveLegacy(remote, 'compression')
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'value', bytes: remoteBytes }
            return { kind: 'value', bytes: Uint8Array.of(1) }
        })
        return { ...harness, association, remoteBytes }
    }

    it('keeps local data and backs up the remote snapshot when the user chooses keep-local', async () => {
        const resolve = vi.fn(async (_context: OfficialSyncConflictContext) => 'keep-local' as const)
        const backup = vi.fn(async (_input: OfficialSyncConflictBackupInput) => undefined)
        const harness = await divergedHarness({ conflict: { resolve, backup }, now: () => 111 })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        await expect(harness.restartAdapter().pull()).resolves.toEqual({
            kind: 'kept-local',
            conflict: true,
        })

        expect(replace).not.toHaveBeenCalled()
        expect((await harness.store.readRoot()).value.username).toBe('Offline edit')
        expect(resolve).toHaveBeenCalledTimes(1)
        const context = resolve.mock.calls[0][0]
        expect(context.remote.username).toBe('Other device')
        expect(context.syncedAt).toBe(111)
        expect(backup).toHaveBeenCalledTimes(1)
        const input = backup.mock.calls[0][0]
        expect(input.side).toBe('remote')
        expect(input.bytes).toEqual(harness.remoteBytes)
        expect(input.characterCount).toBe(context.remote.characters.length)
    })

    it('activates the remote and backs up the local snapshot when the user chooses load-remote', async () => {
        const resolve = vi.fn(async (_context: OfficialSyncConflictContext) => 'load-remote' as const)
        const backup = vi.fn(async (_input: OfficialSyncConflictBackupInput) => undefined)
        const harness = await divergedHarness({ conflict: { resolve, backup } })

        const result = await harness.restartAdapter().pull()

        expect(result.kind).toBe('activated')
        expect((await harness.store.readRoot()).value.username).toBe('Other device')
        expect(backup).toHaveBeenCalledTimes(1)
        const input = backup.mock.calls[0][0]
        expect(input.side).toBe('local')
        expect(input.characterCount).toBeGreaterThan(0)
        const backedUp = await decodeRisuSave(input.bytes) as Database
        expect(backedUp.username).toBe('Offline edit')
    })

    it('aborts the pull when the local backup cannot be written', async () => {
        const resolve = vi.fn(async (_context: OfficialSyncConflictContext) => 'load-remote' as const)
        const backup = vi.fn(async () => {
            throw new Error('backup failed')
        })
        const harness = await divergedHarness({ conflict: { resolve, backup } })
        const replace = vi.spyOn(harness.store, 'replaceFromDatabase')

        await expect(harness.restartAdapter().pull()).rejects.toThrow('backup failed')
        expect(replace).not.toHaveBeenCalled()
        expect((await harness.store.readRoot()).value.username).toBe('Offline edit')
    })

    it('propagates a failed remote backup instead of silently keeping local', async () => {
        const resolve = vi.fn(async (_context: OfficialSyncConflictContext) => 'keep-local' as const)
        const backup = vi.fn(async () => {
            throw new Error('backup failed')
        })
        const harness = await divergedHarness({ conflict: { resolve, backup } })

        await expect(harness.restartAdapter().pull()).rejects.toThrow('backup failed')
        expect((await harness.store.readRoot()).value.username).toBe('Offline edit')
    })

    it('does not consult the conflict handler when only local advanced', async () => {
        const resolve = vi.fn(async (_context: OfficialSyncConflictContext) => 'load-remote' as const)
        const backup = vi.fn(async () => undefined)
        const harness = await makeHarness({
            accountId,
            association: createOfficialAssociationMarkers(memoryLedgerStorage()),
            conflict: { resolve, backup },
        })
        await (await harness.adapter.pin(harness.imported.revision)).publish()
        const publishedBytes = harness.writes.find((write) => write.key === databaseKey)!.bytes!
        const root = (await harness.store.readRoot()).value
        await harness.store.commit({
            expectedRevision: harness.imported.revision,
            root: { ...root, username: 'Offline edit' },
        })
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'value', bytes: publishedBytes }
            return { kind: 'missing' }
        })

        await expect(harness.restartAdapter().pull()).resolves.toEqual({
            kind: 'kept-local',
            conflict: false,
        })
        expect(resolve).not.toHaveBeenCalled()
        expect(backup).not.toHaveBeenCalled()
    })

    it('stamps the sync time on publish and on pull activation', async () => {
        const association = createOfficialAssociationMarkers(memoryLedgerStorage())
        let time = 500
        const harness = await makeHarness({
            accountId,
            association,
            now: () => time,
            remoteCold: new Map([
                ['cold-chat', { message: [] }],
                ['cold-message', { message: [] }],
            ]),
        })

        await (await harness.adapter.pin(harness.imported.revision)).publish()
        expect(association.load(accountId)?.syncedAt).toBe(500)

        time = 900
        const remote = makeDatabase()
        remote.account = { id: accountId, token: 'token', data: {} }
        remote.username = 'Remote after publish'
        const remoteBytes = encodeRisuSaveLegacy(remote, 'compression')
        harness.readItem.mockImplementation(async (key: string): Promise<AccountReadResult> => {
            if (key === databaseKey) return { kind: 'value', bytes: remoteBytes }
            return { kind: 'value', bytes: Uint8Array.of(1) }
        })

        const result = await harness.restartAdapter().pull()
        expect(result.kind).toBe('activated')
        expect(association.load(accountId)?.syncedAt).toBe(900)
    })
})

describe('createOfficialAssociationMarkers', () => {
    it('loads legacy records without a sync time and round-trips records with one', () => {
        const storage = memoryLedgerStorage()
        storage.setItem(
            'officialAssociation:acc',
            JSON.stringify({ revision: 3, databaseFingerprint: 'abc' }),
        )
        const markers = createOfficialAssociationMarkers(storage)

        expect(markers.load('acc')).toEqual({ revision: 3, databaseFingerprint: 'abc' })

        markers.save('acc', { revision: 4, databaseFingerprint: 'def', syncedAt: 42 })
        expect(markers.load('acc')).toEqual({ revision: 4, databaseFingerprint: 'def', syncedAt: 42 })
    })
})

type DependencyContract = OfficialAccountSnapshotDependencies
const _requiresCandidatePreparation: DependencyContract['prepareCandidate'] = async (database) => database
void _requiresCandidatePreparation
