import { isLegacyBackupAssetKey } from '../../drive/backupAssets'
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
import {
    canonicalJson,
    type OfficialRevisionPublisher,
    type PinnedPublication,
} from '../saveCoordinator'
import type { OfficialAssetLedger } from './officialAssetLedger'
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
    ledger: OfficialAssetLedger
}

export type OfficialPullResult =
    | { kind: 'missing' }
    | { kind: 'unchanged' }
    | { kind: 'activated'; revision: DataRevision }

interface PinnedColdValue {
    value: unknown
}

interface PinnedAsset {
    key: string
    /** The local blob key holding this asset's payload. */
    localKey: string
    /** The key the account already holds this asset under, or null when it must be uploaded. */
    publishedAs: string | null
}

interface AssociatedProjection {
    revision: DataRevision
    databaseFingerprint: string
}

function isOfficialAssetKey(key: string): boolean {
    return isLegacyBackupAssetKey(key)
}

function normalizeLegacyAssetKey(key: string): string {
    return key.replace(/\\/g, '/')
}

function addOfficialAssets(target: Set<string>, values: readonly string[]): void {
    for (const key of values) {
        if (isOfficialAssetKey(key)) target.add(key)
    }
}

function addColdCharacterAssets(target: Set<string>, value: unknown): void {
    if (
        value
        && typeof value === 'object'
        && 'character' in value
        && value.character
        && typeof value.character === 'object'
    ) {
        addOfficialAssets(
            target,
            listCharacterResources(value.character as Database['characters'][number]),
        )
    }
}

async function fingerprintText(value: string): Promise<string> {
    return fingerprintDatabase(new TextEncoder().encode(value))
}

async function fingerprintDatabase(bytes: Uint8Array): Promise<string> {
    const digest = await globalThis.crypto.subtle.digest('SHA-256', bytes as BufferSource)
    return Array.from(new Uint8Array(digest), (value) => value.toString(16).padStart(2, '0')).join('')
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

function requireRemoteAsset(result: AccountReadResult, key: string): boolean {
    if (result.kind !== 'missing') return true
    if (normalizeLegacyAssetKey(key) !== key) {
        console.warn(`Skipping a legacy official asset without a payload: ${key}`)
        return false
    }
    throw new Error(`Missing official asset: ${key}`)
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
    const assets = new Set<string>()
    addOfficialAssets(assets, listDatabaseRootResources(root))
    const coldKeys = new Set<string>()
    for (const summary of await listCharacterSummaries(lease)) {
        const character = await readCompleteCharacter(lease, summary)
        addOfficialAssets(assets, listCharacterResources(character))
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
    private databaseFingerprint: string | null = null
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
        private readonly onPublished: (
            revision: DataRevision,
            databaseFingerprint: string,
        ) => void,
    ) {
        for (const asset of assets) {
            if (asset.publishedAs !== null) this.replacements.set(asset.key, asset.publishedAs)
        }
    }

    async publish(): Promise<void> {
        if (this.disposed) throw new Error('Official publication has been disposed')
        if (this.published) return

        for (const asset of this.assets) {
            if (this.replacements.has(asset.key)) continue
            const bytes = await this.blobs.read(asset.localKey)
            if (!bytes) throw new Error(`Missing pinned asset payload: ${asset.key}`)
            const result = await this.dependencies.account.writeItem(asset.key, bytes)
            const replacementKey = requireWriteSuccess(result, asset.key)
            this.replacements.set(asset.key, replacementKey)
            this.dependencies.ledger.record(asset.key, replacementKey)
        }

        const replacementRecord = Object.fromEntries(this.replacements)
        for (const [key, pinned] of this.coldValues) {
            if (this.completedColdKeys.has(key)) continue
            const projected = replaceColdStoragePayloadResources(pinned.value, replacementRecord)
            const digest = await fingerprintText(canonicalJson(projected))
            if (digest !== this.dependencies.ledger.coldDigest(key)) {
                await this.dependencies.cold.writeRemote(key, projected)
                this.dependencies.ledger.recordCold(key, digest)
            }
            this.completedColdKeys.add(key)
        }

        this.databaseBytes ??= await concatenate(streamRisuSaveFromLease(this.lease, {
            replaceResources: replacementRecord,
        }))
        this.databaseFingerprint ??= await fingerprintDatabase(this.databaseBytes)
        const result = await this.dependencies.account.writeItem(databaseKey, this.databaseBytes)
        requireWriteSuccess(result, databaseKey)
        await this.dependencies.markPublished(this.revision)
        this.onPublished(this.revision, this.databaseFingerprint)
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
    private associatedProjection: AssociatedProjection | null = null

    constructor(private readonly dependencies: OfficialAccountSnapshotDependencies) {}

    async pin(revision: DataRevision): Promise<PinnedPublication> {
        const lease = await this.dependencies.store.acquireRevision(revision)
        try {
            const blobs = await this.dependencies.resolveBlobs()
            const references = await collectPinnedReferences(lease)
            const assetKeys = new Set(references.assets)
            const coldValues = new Map<string, PinnedColdValue>()
            for (const key of references.coldKeys) {
                const local = await this.dependencies.cold.readLocal(key)
                if (local !== null) {
                    if (!isColdStorageBackupData(local)) {
                        throw new Error(`Invalid local cold payload: ${key}`)
                    }
                    coldValues.set(key, { value: safeStructuredClone(local) })
                    addColdCharacterAssets(assetKeys, local)
                    continue
                }
                const remote = await this.dependencies.cold.readRemote(key)
                if (remote === null) throw new Error(`Missing official cold payload: ${key}`)
                if (!isColdStorageBackupData(remote)) {
                    throw new Error(`Invalid official cold payload: ${key}`)
                }
                const pinnedRemote = safeStructuredClone(remote)
                coldValues.set(key, { value: pinnedRemote })
                this.dependencies.ledger.recordCold(key, await fingerprintText(canonicalJson(pinnedRemote)))
                addColdCharacterAssets(assetKeys, remote)
            }

            const assets: PinnedAsset[] = []
            for (const key of [...assetKeys].sort()) {
                const publishedAs = this.dependencies.ledger.publishedAs(key)
                if (publishedAs !== null) {
                    assets.push({ key, localKey: key, publishedAs })
                    continue
                }
                if (await blobs.stat(key)) {
                    assets.push({ key, localKey: key, publishedAs: null })
                    continue
                }
                const normalized = normalizeLegacyAssetKey(key)
                if (normalized !== key && await blobs.stat(normalized)) {
                    assets.push({ key, localKey: normalized, publishedAs: null })
                    continue
                }
                if (!requireRemoteAsset(await this.dependencies.account.readItem(key), key)) {
                    continue
                }
                this.dependencies.ledger.record(key, key)
                assets.push({ key, localKey: key, publishedAs: key })
            }

            return new OfficialPinnedPublication(
                revision,
                lease,
                blobs,
                assets,
                coldValues,
                this.dependencies,
                (publishedRevision, databaseFingerprint) => {
                    this.associatedProjection = {
                        revision: publishedRevision,
                        databaseFingerprint,
                    }
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
        const databaseFingerprint = await fingerprintDatabase(result.bytes)
        throwIfAborted(signal)
        if (
            result.kind === 'not-modified'
            && this.associatedProjection?.revision === expectedRevision
            && this.associatedProjection.databaseFingerprint === databaseFingerprint
        ) {
            return { kind: 'unchanged' }
        }

        throwIfAborted(signal)
        const decoded = await decodeRisuSave(result.bytes)
        validateCandidate(decoded)
        const candidate = await this.dependencies.prepareCandidate(decoded)
        validateCandidate(candidate)

        const assetKeys = new Set<string>()
        addOfficialAssets(assetKeys, listDatabaseRootResources(candidate))
        const coldKeys = new Set<string>()
        for (const character of candidate.characters) {
            addOfficialAssets(assetKeys, listCharacterResources(character))
            for (const key of listColdDataKeysFromCharacter(character)) coldKeys.add(key)
        }
        for (const key of [...coldKeys].sort()) {
            throwIfAborted(signal)
            const value = await this.dependencies.cold.readRemote(key, signal)
            if (value === null) throw new Error(`Missing official cold payload: ${key}`)
            if (!isColdStorageBackupData(value)) {
                throw new Error(`Invalid official cold payload: ${key}`)
            }
            addColdCharacterAssets(assetKeys, value)
        }
        for (const key of [...assetKeys].sort()) {
            throwIfAborted(signal)
            requireRemoteAsset(await this.dependencies.account.readItem(key, { signal }), key)
        }

        throwIfAborted(signal)
        const activated = await this.dependencies.store.replaceFromDatabase(
            candidate,
            expectedRevision,
        )
        this.associatedProjection = {
            revision: activated.revision,
            databaseFingerprint,
        }
        return { kind: 'activated', revision: activated.revision }
    }
}
