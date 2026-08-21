import {
    isColdStorageBackupData,
    listCharacterResources,
    listColdDataKeysFromCharacter,
    listDatabaseRootResources,
    replaceColdStoragePayloadResources,
} from '../../process/coldstorageData'
import { safeStructuredClone } from '../../polyfill'
import type {
    AccountReadResult,
    AccountStorage,
    AccountWriteResult,
} from '../accountStorage'
import type { BlobStore } from '../blobStore'
import type { Database } from '../database.svelte'
import type {
    CharacterSummary,
    DataRevision,
    PersistentDataStore,
    PersistentRevisionLease,
} from '../persistentDataStore'
import { decodeRisuSave } from '../risuSave'
import { streamRisuSaveFromLease } from '../risuSaveStoreAdapter'
import type { OfficialRevisionPublisher, PinnedPublication } from '../saveCoordinator'
import { officialAccountSnapshotCapability } from './types'

const databaseKey = 'database/database.bin'

export interface OfficialColdStorageTransport {
    readRemote(key: string, signal?: AbortSignal): Promise<unknown | null>
    writeRemote(key: string, value: unknown, signal?: AbortSignal): Promise<void>
    readLocal(key: string): Promise<unknown | null>
}

export interface OfficialAccountSnapshotDependencies {
    store: PersistentDataStore
    resolveBlobs(): Promise<BlobStore>
    account: Pick<AccountStorage, 'readItem' | 'writeItem'>
    cold: OfficialColdStorageTransport
    prepareCandidate(database: Database): Promise<Database>
    markPublished(revision: DataRevision): Promise<void> | void
}

export type OfficialPullResult =
    | { kind: 'missing' }
    | { kind: 'unchanged' }
    | { kind: 'activated'; revision: DataRevision }

interface PinnedColdValue {
    value: unknown
    remoteOnly: boolean
}

interface PinnedAsset {
    key: string
    remoteOnly: boolean
}

function throwIfAborted(signal?: AbortSignal): void {
    if (!signal?.aborted) return
    throw signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
}

function requireWriteSuccess(result: AccountWriteResult, key: string): string {
    if (result.kind === 'auth-warning') {
        throw new Error(`Official account authorization warning while writing ${key}`)
    }
    if (!result.replacementKey) {
        throw new Error(`Official account write returned no key for ${key}`)
    }
    return result.replacementKey
}

function requireRemoteValue(result: AccountReadResult, key: string): Uint8Array {
    if (result.kind === 'missing') {
        throw new Error(`Missing official asset: ${key}`)
    }
    return result.bytes
}

function validateCandidate(value: unknown): asserts value is Database {
    if (!value || typeof value !== 'object' || !Array.isArray((value as Database).characters)) {
        throw new Error('Invalid official database snapshot')
    }
    for (const character of (value as Database).characters) {
        if (
            !character
            || typeof character !== 'object'
            || typeof character.chaId !== 'string'
            || !character.chaId
            || !Array.isArray(character.chats)
        ) {
            throw new Error('Invalid character in official database snapshot')
        }
    }
}

function canonical(value: unknown): string {
    if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`
    if (value && typeof value === 'object') {
        return `{${Object.keys(value as object)
            .sort()
            .filter((key) => (value as Record<string, unknown>)[key] !== undefined)
            .map((key) => `${JSON.stringify(key)}:${canonical((value as Record<string, unknown>)[key])}`)
            .join(',')}}`
    }
    return JSON.stringify(value)
}

async function listCharacterSummaries(
    lease: PersistentRevisionLease,
): Promise<CharacterSummary[]> {
    const values: CharacterSummary[] = []
    for (const trash of [false, true]) {
        let cursor: string | undefined
        do {
            const page = await lease.queryCharacters({
                order: 'configured',
                trash,
                limit: 128,
                cursor,
            })
            values.push(...page.items)
            cursor = page.nextCursor
        } while (cursor !== undefined)
    }
    return values.sort((left, right) => left.configuredIndex - right.configuredIndex)
}

async function readCompleteCharacter(
    lease: PersistentRevisionLease,
    summary: CharacterSummary,
): Promise<Database['characters'][number]> {
    const detail = await lease.readCharacter(summary.id)
    if (!detail) throw new Error(`Missing character detail for ${summary.id}`)
    const chats: Database['characters'][number]['chats'] = []
    let cursor: string | undefined
    do {
        const page = await lease.queryConversations({
            characterId: summary.id,
            order: 'configured',
            limit: 128,
            cursor,
        })
        for (const conversation of page.items) {
            const value = await lease.readConversation(summary.id, conversation.id)
            if (!value) throw new Error(`Missing conversation ${conversation.id}`)
            chats.push(value.value)
        }
        cursor = page.nextCursor
    } while (cursor !== undefined)
    return { ...detail.value, chats } as Database['characters'][number]
}

async function collectPinnedReferences(lease: PersistentRevisionLease): Promise<{
    assets: string[]
    coldKeys: string[]
}> {
    const root = (await lease.readRoot()).value
    const assets = new Set(listDatabaseRootResources(root))
    const coldKeys = new Set<string>()
    for (const summary of await listCharacterSummaries(lease)) {
        const character = await readCompleteCharacter(lease, summary)
        for (const key of listCharacterResources(character)) assets.add(key)
        for (const key of listColdDataKeysFromCharacter(character)) coldKeys.add(key)
    }
    return {
        assets: [...assets].sort(),
        coldKeys: [...coldKeys].sort(),
    }
}

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

class OfficialPinnedPublication implements PinnedPublication {
    private readonly replacements = new Map<string, string>()
    private readonly completedColdKeys = new Set<string>()
    private databaseBytes: Uint8Array | null = null
    private released = false
    private disposed = false
    private published = false

    constructor(
        private readonly revision: DataRevision,
        private readonly lease: PersistentRevisionLease,
        private readonly blobs: BlobStore,
        private readonly assets: readonly PinnedAsset[],
        private readonly coldValues: ReadonlyMap<string, PinnedColdValue>,
        private readonly dependencies: OfficialAccountSnapshotDependencies,
        private readonly onPublished: (revision: DataRevision) => void,
    ) {
        for (const asset of assets) {
            if (asset.remoteOnly) this.replacements.set(asset.key, asset.key)
        }
    }

    async publish(): Promise<void> {
        if (this.disposed) throw new Error('Official publication has been disposed')
        if (this.published) return

        for (const asset of this.assets) {
            if (this.replacements.has(asset.key)) continue
            const bytes = await this.blobs.read(asset.key)
            if (!bytes) throw new Error(`Missing pinned asset payload: ${asset.key}`)
            const result = await this.dependencies.account.writeItem(asset.key, bytes)
            this.replacements.set(asset.key, requireWriteSuccess(result, asset.key))
        }

        const replacementRecord = Object.fromEntries(this.replacements)
        for (const [key, pinned] of this.coldValues) {
            if (this.completedColdKeys.has(key)) continue
            const projected = replaceColdStoragePayloadResources(pinned.value, replacementRecord)
            if (!pinned.remoteOnly || canonical(projected) !== canonical(pinned.value)) {
                await this.dependencies.cold.writeRemote(key, projected)
            }
            this.completedColdKeys.add(key)
        }

        this.databaseBytes ??= await concatenate(streamRisuSaveFromLease(this.lease, {
            replaceResources: replacementRecord,
        }))
        const result = await this.dependencies.account.writeItem(databaseKey, this.databaseBytes)
        requireWriteSuccess(result, databaseKey)
        await this.dependencies.markPublished(this.revision)
        this.onPublished(this.revision)
        this.published = true
        await this.release()
    }

    async dispose(): Promise<void> {
        if (this.disposed) return
        this.disposed = true
        await this.release()
    }

    private async release(): Promise<void> {
        if (this.released) return
        this.released = true
        await this.lease.release()
    }
}

export class OfficialAccountSnapshotAdapter implements OfficialRevisionPublisher {
    readonly capability = officialAccountSnapshotCapability
    private associatedRevision: DataRevision | null = null

    constructor(private readonly dependencies: OfficialAccountSnapshotDependencies) {}

    async pin(revision: DataRevision): Promise<PinnedPublication> {
        const lease = await this.dependencies.store.acquireRevision(revision)
        try {
            const blobs = await this.dependencies.resolveBlobs()
            const references = await collectPinnedReferences(lease)
            const assets: PinnedAsset[] = []
            for (const key of references.assets) {
                const local = await blobs.stat(key)
                if (local) {
                    assets.push({ key, remoteOnly: false })
                    continue
                }
                requireRemoteValue(await this.dependencies.account.readItem(key), key)
                assets.push({ key, remoteOnly: true })
            }

            const coldValues = new Map<string, PinnedColdValue>()
            for (const key of references.coldKeys) {
                const local = await this.dependencies.cold.readLocal(key)
                if (local !== null) {
                    if (!isColdStorageBackupData(local)) {
                        throw new Error(`Invalid local cold payload: ${key}`)
                    }
                    coldValues.set(key, {
                        value: safeStructuredClone(local),
                        remoteOnly: false,
                    })
                    continue
                }
                const remote = await this.dependencies.cold.readRemote(key)
                if (remote === null) throw new Error(`Missing official cold payload: ${key}`)
                if (!isColdStorageBackupData(remote)) {
                    throw new Error(`Invalid official cold payload: ${key}`)
                }
                coldValues.set(key, {
                    value: safeStructuredClone(remote),
                    remoteOnly: true,
                })
            }

            return new OfficialPinnedPublication(
                revision,
                lease,
                blobs,
                assets,
                coldValues,
                this.dependencies,
                (publishedRevision) => {
                    this.associatedRevision = publishedRevision
                },
            )
        } catch (error) {
            await lease.release()
            throw error
        }
    }

    async pull(signal?: AbortSignal): Promise<OfficialPullResult> {
        throwIfAborted(signal)
        const expectedRevision = (await this.dependencies.store.readRoot()).revision
        const result = await this.dependencies.account.readItem(databaseKey, { signal })
        throwIfAborted(signal)
        if (result.kind === 'missing') return { kind: 'missing' }
        if (result.kind === 'not-modified' && this.associatedRevision === expectedRevision) {
            return { kind: 'unchanged' }
        }

        throwIfAborted(signal)
        const decoded = await decodeRisuSave(result.bytes)
        validateCandidate(decoded)
        const candidate = await this.dependencies.prepareCandidate(decoded)
        validateCandidate(candidate)

        const assetKeys = new Set(listDatabaseRootResources(candidate))
        const coldKeys = new Set<string>()
        for (const character of candidate.characters) {
            for (const key of listCharacterResources(character)) assetKeys.add(key)
            for (const key of listColdDataKeysFromCharacter(character)) coldKeys.add(key)
        }
        for (const key of [...assetKeys].sort()) {
            throwIfAborted(signal)
            requireRemoteValue(await this.dependencies.account.readItem(key, { signal }), key)
        }
        for (const key of [...coldKeys].sort()) {
            throwIfAborted(signal)
            const value = await this.dependencies.cold.readRemote(key, signal)
            if (value === null) throw new Error(`Missing official cold payload: ${key}`)
            if (!isColdStorageBackupData(value)) {
                throw new Error(`Invalid official cold payload: ${key}`)
            }
        }

        throwIfAborted(signal)
        const activated = await this.dependencies.store.replaceFromDatabase(
            candidate,
            expectedRevision,
        )
        this.associatedRevision = activated.revision
        return { kind: 'activated', revision: activated.revision }
    }
}
