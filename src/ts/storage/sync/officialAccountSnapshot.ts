import {
    collectExactPluginStorageAssetReferences,
    isLegacyBackupAssetKey,
} from '../../drive/backupAssets'
import {
    isColdStorageBackupData,
    listCharacterResources,
    listColdDataKeysFromCharacter,
    listDatabaseRootResources,
    replaceColdStoragePayloadResources,
} from '../../process/coldstorageData'
import type {
    AccountStorage,
    AccountWriteResult,
} from '../accountStorage'
import type { BlobStore } from '../blobStore'
import type { Database } from '../database.svelte'
import type {
    DataRevision,
    PersistentDataStore,
    PersistentRevisionLease,
    PersistentRevisionReader,
} from '../persistentDataStore'
import {
    assertPinnedRevision,
    iteratePinnedCharacters,
    iteratePinnedConversations,
    releasePersistentRevisionLease,
} from '../persistentRecordIterator'
import { decodeRisuSave } from '../risuSave'
import {
    streamRisuSaveFromLease,
    withPinnedRisuSaveExport,
} from '../risuSaveStoreAdapter'
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

export interface OfficialAssociationRecord {
    revision: DataRevision
    databaseFingerprint: string
    syncedAt?: number
}

export interface OfficialRecoveredPublication extends OfficialAssociationRecord {
    accountId: string
}

export interface OfficialAssociationMarkers {
    load(accountId: string): OfficialAssociationRecord | null
    save(accountId: string, record: OfficialAssociationRecord): void
}

export type OfficialSyncConflictChoice = 'keep-local' | 'load-remote'

export interface OfficialSyncConflictContext {
    remote: Database
    syncedAt: number | null
}

export interface OfficialSyncConflictBackupInput {
    side: 'local' | 'remote'
    bytes: Uint8Array
    characterCount: number
}

export interface OfficialSyncConflictHandler {
    resolve(context: OfficialSyncConflictContext): Promise<OfficialSyncConflictChoice>
    backup(input: OfficialSyncConflictBackupInput): Promise<void>
}

export interface OfficialAccountSnapshotDependencies {
    store: PersistentDataStore
    resolveBlobs(): Promise<BlobStore>
    account: Pick<AccountStorage, 'readItem' | 'writeItem'>
    cold: OfficialColdStorageTransport
    prepareCandidate(database: Database): Promise<Database>
    markPublished(revision: DataRevision): Promise<void> | void
    ledger: OfficialAssetLedger
    /** Persists the published association so a restart can tell local from remote progress. */
    association?: OfficialAssociationMarkers
    /** Decides diverged pulls and archives the overwritten side; absent means keep local. */
    conflict?: OfficialSyncConflictHandler
    now?(): number
    nativeDatabasePublisher?: OfficialNativeDatabasePublisher
    flushPublicationMetadata?(): Promise<void>
}

export interface OfficialNativeDatabasePublicationInput {
    revision: DataRevision
    accountId: string
    lease: PersistentRevisionLease
    resourceReplacements: Readonly<Record<string, string>>
    signal?: AbortSignal
}

export interface OfficialNativeDatabasePublicationReceipt {
    databaseFingerprint: string
    acknowledge(): Promise<void>
    completeReload(): Promise<void>
}

export type OfficialNativeDatabasePublisher = (
    input: OfficialNativeDatabasePublicationInput,
) => Promise<OfficialNativeDatabasePublicationReceipt | null>

export type OfficialPullResult =
    | { kind: 'missing' }
    | { kind: 'unchanged' }
    | { kind: 'kept-local'; conflict: boolean }
    | { kind: 'activated'; revision: DataRevision }

interface PinnedColdReference {
    key: string
    source: 'local' | 'remote'
    fingerprint: string
}

interface PinnedAsset {
    key: string
    /** The local blob key holding this asset's payload. */
    localKey: string
    /** The key the account already holds this asset under, or null when it must be uploaded. */
    publishedAs: string | null
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

function linkAbortSignals(signals: readonly (AbortSignal | undefined)[]): {
    signal: AbortSignal
    dispose(): void
} {
    const controller = new AbortController()
    const listeners: Array<{ signal: AbortSignal; listener: () => void }> = []
    for (const signal of signals) {
        if (!signal) continue
        if (signal.aborted) {
            controller.abort(signal.reason)
            break
        }
        const listener = () => controller.abort(signal.reason)
        signal.addEventListener('abort', listener, { once: true })
        listeners.push({ signal, listener })
    }
    return {
        signal: controller.signal,
        dispose() {
            for (const { signal, listener } of listeners) {
                signal.removeEventListener('abort', listener)
            }
        },
    }
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

export function createOfficialAssociationMarkers(storage: {
    getItem(key: string): string | null
    setItem(key: string, value: string): void
}): OfficialAssociationMarkers {
    const storageKey = (accountId: string) => `officialAssociation:${accountId}`
    return {
        load(accountId) {
            const raw = storage.getItem(storageKey(accountId))
            if (!raw) return null
            try {
                const parsed = JSON.parse(raw) as Partial<OfficialAssociationRecord>
                if (
                    typeof parsed?.revision !== 'number'
                    || typeof parsed?.databaseFingerprint !== 'string'
                ) {
                    return null
                }
                return {
                    revision: parsed.revision,
                    databaseFingerprint: parsed.databaseFingerprint,
                    syncedAt: typeof parsed.syncedAt === 'number' ? parsed.syncedAt : undefined,
                }
            } catch {
                return null
            }
        },
        save(accountId, record) {
            try {
                storage.setItem(storageKey(accountId), JSON.stringify(record))
            } catch (error) {
                console.error('Failed to persist the official sync association', error)
            }
        },
    }
}

async function collectPinnedReferences(reader: PersistentRevisionReader): Promise<{
    accountId: string | undefined
    assets: string[]
    coldKeys: string[]
}> {
    const rootRecord = await reader.readRoot()
    assertPinnedRevision(reader.revision, rootRecord.revision, 'Root')
    const root = rootRecord.value
    const assets = new Set<string>()
    addOfficialAssets(assets, listDatabaseRootResources(root))
    const pluginStorage = await reader.queryPluginStorage()
    assertPinnedRevision(reader.revision, pluginStorage.revision, 'Plugin storage catalog')
    for (const summary of pluginStorage.items) {
        const value = await reader.readPluginStorage(summary.key)
        if (!value) throw new Error(`Missing plugin storage value for ${summary.key}`)
        assertPinnedRevision(
            reader.revision,
            value.revision,
            `Plugin storage value ${summary.key}`,
        )
        addOfficialAssets(
            assets,
            collectExactPluginStorageAssetReferences(value.value),
        )
    }
    const coldKeys = new Set<string>()
    for await (const character of iteratePinnedCharacters(reader)) {
        const detail = {
            ...character.detail,
            chats: [],
        } as Database['characters'][number]
        addOfficialAssets(assets, listCharacterResources(detail))
        for (const key of listColdDataKeysFromCharacter(detail)) coldKeys.add(key)
        for await (const conversation of iteratePinnedConversations(reader, character.summary.id)) {
            for (const key of listColdDataKeysFromCharacter({
                ...detail,
                coldstorage: undefined,
                coldStoragedChats: [],
                chats: [conversation.value],
            })) coldKeys.add(key)
        }
    }
    return {
        accountId: root.account?.id,
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
    private publicationCommitted = false
    private nativeReceipt: OfficialNativeDatabasePublicationReceipt | null = null
    private markedPublished = false
    private associationApplied = false
    private metadataFlushed = false
    private acknowledged = false
    private reloadCompleted = false
    private nativeLeaseNeedsRefresh = false
    private inFlight: Promise<void> | null = null
    private readonly disposalController = new AbortController()

    constructor(
        private readonly revision: DataRevision,
        private lease: PersistentRevisionLease,
        private readonly blobs: BlobStore,
        private readonly assets: readonly PinnedAsset[],
        private readonly coldReferences: readonly PinnedColdReference[],
        private readonly accountId: string | undefined,
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

    async publish(signal?: AbortSignal): Promise<void> {
        if (this.disposed) throw new Error('Official publication has been disposed')
        if (this.published) return
        if (this.inFlight) return this.inFlight
        const linked = linkAbortSignals([this.disposalController.signal, signal])
        this.inFlight = this.publishOnce(linked.signal).finally(() => {
            linked.dispose()
            this.inFlight = null
        })
        return this.inFlight
    }

    private async publishOnce(signal: AbortSignal): Promise<void> {
        if (this.publicationCommitted) return this.finalizePublication()
        for (const asset of this.assets) {
            if (this.replacements.has(asset.key)) continue
            throwIfAborted(signal)
            const bytes = await this.blobs.read(asset.localKey)
            throwIfAborted(signal)
            if (!bytes) throw new Error(`Missing pinned asset payload: ${asset.key}`)
            const result = await this.dependencies.account.writeItem(asset.key, bytes, { signal })
            const replacementKey = requireWriteSuccess(result, asset.key)
            this.replacements.set(asset.key, replacementKey)
            this.dependencies.ledger.record(asset.key, replacementKey)
            throwIfAborted(signal)
        }

        const replacementRecord = Object.fromEntries(this.replacements)
        for (const pinned of this.coldReferences) {
            const key = pinned.key
            if (this.completedColdKeys.has(key)) continue
            throwIfAborted(signal)
            const value = pinned.source === 'local'
                ? await this.dependencies.cold.readLocal(key)
                : await this.dependencies.cold.readRemote(key, signal)
            throwIfAborted(signal)
            if (value === null || !isColdStorageBackupData(value)) {
                throw new Error(`Pinned cold payload became unavailable before publication: ${key}`)
            }
            const fingerprint = await fingerprintText(canonicalJson(value))
            if (fingerprint !== pinned.fingerprint) {
                throw new Error(`Pinned cold payload changed before publication: ${key}`)
            }
            const projected = replaceColdStoragePayloadResources(value, replacementRecord)
            const digest = await fingerprintText(canonicalJson(projected))
            if (digest !== this.dependencies.ledger.coldDigest(key)) {
                await this.dependencies.cold.writeRemote(key, projected, signal)
                this.dependencies.ledger.recordCold(key, digest)
            }
            this.completedColdKeys.add(key)
            throwIfAborted(signal)
        }

        throwIfAborted(signal)
        if (this.dependencies.nativeDatabasePublisher && this.accountId) {
            if (!this.dependencies.flushPublicationMetadata) {
                throw new Error('Native official publication metadata flush is not configured')
            }
            await this.dependencies.flushPublicationMetadata()
            throwIfAborted(signal)
            await this.refreshNativeLeaseIfNeeded()
            let receipt: OfficialNativeDatabasePublicationReceipt | null
            try {
                receipt = await this.dependencies.nativeDatabasePublisher({
                    revision: this.revision,
                    accountId: this.accountId,
                    lease: this.lease,
                    resourceReplacements: replacementRecord,
                    signal,
                })
            } catch (error) {
                this.nativeLeaseNeedsRefresh = true
                throw error
            }
            if (receipt) {
                this.nativeReceipt = receipt
                this.databaseFingerprint = receipt.databaseFingerprint
                this.publicationCommitted = true
                return this.finalizePublication()
            }
        }

        this.databaseBytes ??= await concatenate(streamRisuSaveFromLease(this.lease, {
            replaceResources: replacementRecord,
        }))
        throwIfAborted(signal)
        this.databaseFingerprint ??= await fingerprintDatabase(this.databaseBytes)
        const result = await this.dependencies.account.writeItem(
            databaseKey,
            this.databaseBytes,
            { signal },
        )
        requireWriteSuccess(result, databaseKey)
        this.publicationCommitted = true
        return this.finalizePublication()
    }

    private async refreshNativeLeaseIfNeeded(): Promise<void> {
        if (!this.nativeLeaseNeedsRefresh) return
        const previous = this.lease
        const fresh = await this.dependencies.store.acquireRevision(this.revision)
        try {
            await releasePersistentRevisionLease(previous)
        } catch (error) {
            try {
                await releasePersistentRevisionLease(fresh)
            } catch {}
            throw error
        }
        this.lease = fresh
        this.nativeLeaseNeedsRefresh = false
    }

    private async finalizePublication(): Promise<void> {
        if (!this.databaseFingerprint) {
            throw new Error('Official publication completed without a database fingerprint')
        }
        if (!this.markedPublished) {
            await this.dependencies.markPublished(this.revision)
            this.markedPublished = true
        }
        if (!this.associationApplied) {
            this.onPublished(this.revision, this.databaseFingerprint)
            this.associationApplied = true
        }
        if (this.nativeReceipt) {
            if (!this.dependencies.flushPublicationMetadata) {
                throw new Error('Native official publication metadata flush is not configured')
            }
            if (!this.metadataFlushed) {
                await this.dependencies.flushPublicationMetadata()
                this.metadataFlushed = true
            }
            if (!this.acknowledged) {
                await this.nativeReceipt.acknowledge()
                this.acknowledged = true
            }
        }
        await this.release()
        if (this.nativeReceipt && !this.reloadCompleted) {
            await this.nativeReceipt.completeReload()
            this.reloadCompleted = true
        }
        this.published = true
    }

    async dispose(): Promise<void> {
        if (this.disposed) return
        this.disposalController.abort(new DOMException('Publication disposed', 'AbortError'))
        if (this.inFlight) await this.inFlight.catch(() => undefined)
        await this.release()
        this.disposed = true
    }

    private async release(): Promise<void> {
        if (this.released) return
        await releasePersistentRevisionLease(this.lease)
        this.released = true
    }
}

export class OfficialAccountSnapshotAdapter implements OfficialRevisionPublisher {
    readonly capability = officialAccountSnapshotCapability
    private associatedAccountId: string | null = null
    private associatedProjection: OfficialAssociationRecord | null = null
    /** Keys already known to be unavailable everywhere; skipping them keeps publishes from re-probing. */
    private readonly unavailableRemoteAssets = new Set<string>()

    constructor(private readonly dependencies: OfficialAccountSnapshotDependencies) {}

    resetAccountAssociation(accountId: string | null): void {
        this.associatedAccountId = accountId
        this.associatedProjection = accountId
            ? this.dependencies.association?.load(accountId) ?? null
            : null
        this.unavailableRemoteAssets.clear()
    }

    rememberNativeActivation(
        accountId: string,
        revision: DataRevision,
        databaseFingerprint: string,
    ): void {
        this.rememberAssociation(accountId, {
            revision,
            databaseFingerprint,
            syncedAt: this.stampTime(),
        })
    }

    async adoptPublishedRevision(publication: OfficialRecoveredPublication): Promise<void> {
        const root = await this.dependencies.store.readRoot()
        const activeAccountId = root.value.account?.id
        if (activeAccountId !== publication.accountId) {
            throw new Error('Recovered official publication account does not match the active account')
        }
        if (root.revision < publication.revision) {
            throw new Error('Recovered official publication is newer than the local revision')
        }
        await this.dependencies.markPublished(publication.revision)
        this.rememberAssociation(publication.accountId, {
            revision: publication.revision,
            databaseFingerprint: publication.databaseFingerprint,
            syncedAt: this.stampTime(),
        })
    }

    private resolveAssociation(accountId: string | undefined): OfficialAssociationRecord | null {
        const resolvedAccountId = accountId ?? null
        if (resolvedAccountId !== this.associatedAccountId) {
            this.resetAccountAssociation(resolvedAccountId)
        }
        return this.associatedProjection
    }

    private rememberAssociation(
        accountId: string | undefined,
        record: OfficialAssociationRecord,
    ): void {
        this.associatedAccountId = accountId ?? null
        this.associatedProjection = record
        if (accountId) this.dependencies.association?.save(accountId, record)
    }

    private stampTime(): number {
        return this.dependencies.now?.() ?? Date.now()
    }

    async pin(revision: DataRevision): Promise<PinnedPublication> {
        const lease = await this.dependencies.store.acquireRevision(revision)
        try {
            const blobs = await this.dependencies.resolveBlobs()
            const references = await collectPinnedReferences(lease)
            const assetKeys = new Set(references.assets)
            const coldReferences: PinnedColdReference[] = []
            for (const key of references.coldKeys) {
                const local = await this.dependencies.cold.readLocal(key)
                if (local !== null && isColdStorageBackupData(local)) {
                    coldReferences.push({
                        key,
                        source: 'local',
                        fingerprint: await fingerprintText(canonicalJson(local)),
                    })
                    addColdCharacterAssets(assetKeys, local)
                    continue
                }
                if (local !== null) {
                    console.warn(`Ignoring an invalid local cold payload: ${key}`)
                }
                const remote = await this.dependencies.cold.readRemote(key)
                if (remote === null || !isColdStorageBackupData(remote)) {
                    console.warn(`Skipping the official publish of an unavailable cold payload: ${key}`)
                    continue
                }
                const fingerprint = await fingerprintText(canonicalJson(remote))
                coldReferences.push({ key, source: 'remote', fingerprint })
                this.dependencies.ledger.recordCold(key, fingerprint)
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
                if (this.unavailableRemoteAssets.has(key)) continue
                const remote = await this.dependencies.account.readItem(key)
                if (remote.kind === 'missing') {
                    this.unavailableRemoteAssets.add(key)
                    console.warn(`Skipping an official asset that is missing locally and in the account: ${key}`)
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
                coldReferences,
                references.accountId,
                this.dependencies,
                (publishedRevision, databaseFingerprint) => {
                    this.rememberAssociation(references.accountId, {
                        revision: publishedRevision,
                        databaseFingerprint,
                        syncedAt: this.stampTime(),
                    })
                },
            )
        } catch (error) {
            try {
                await releasePersistentRevisionLease(lease)
            } catch {}
            throw error
        }
    }

    async pull(signal?: AbortSignal): Promise<OfficialPullResult> {
        throwIfAborted(signal)
        const localRoot = await this.dependencies.store.readRoot()
        const expectedRevision = localRoot.revision
        const association = this.resolveAssociation(localRoot.value.account?.id)
        const result = await this.dependencies.account.readItem(databaseKey, { signal })
        throwIfAborted(signal)
        if (result.kind === 'missing') return { kind: 'missing' }
        const databaseFingerprint = await fingerprintDatabase(result.bytes)
        throwIfAborted(signal)
        let decoded: Database | null = null
        if (association) {
            const remoteChanged = association.databaseFingerprint !== databaseFingerprint
            if (!remoteChanged && association.revision === expectedRevision) {
                return { kind: 'unchanged' }
            }
            if (expectedRevision > association.revision) {
                const conflict = remoteChanged ? this.dependencies.conflict : undefined
                if (!conflict) return { kind: 'kept-local', conflict: remoteChanged }
                throwIfAborted(signal)
                const parsed = await decodeRisuSave(result.bytes)
                validateCandidate(parsed)
                decoded = parsed
                const choice = await conflict.resolve({
                    remote: parsed,
                    syncedAt: association.syncedAt ?? null,
                })
                throwIfAborted(signal)
                if (choice === 'keep-local') {
                    await conflict.backup({
                        side: 'remote',
                        bytes: result.bytes,
                        characterCount: parsed.characters.length,
                    })
                    return { kind: 'kept-local', conflict: true }
                }
                await this.backupLocalRevision(conflict, expectedRevision)
            }
        }

        throwIfAborted(signal)
        if (!decoded) {
            const parsed = await decodeRisuSave(result.bytes)
            validateCandidate(parsed)
            decoded = parsed
        }
        const candidate = await this.dependencies.prepareCandidate(decoded)
        validateCandidate(candidate)

        // Referenced assets and cold payloads load lazily on use, matching the upstream client;
        // a missing one degrades that item instead of failing the whole pull.
        throwIfAborted(signal)
        const activated = await this.dependencies.store.replaceFromDatabase(
            candidate,
            expectedRevision,
        )
        this.rememberAssociation(candidate.account?.id ?? localRoot.value.account?.id, {
            revision: activated.revision,
            databaseFingerprint,
            syncedAt: this.stampTime(),
        })
        return { kind: 'activated', revision: activated.revision }
    }

    private async backupLocalRevision(
        conflict: OfficialSyncConflictHandler,
        revision: DataRevision,
    ): Promise<void> {
        await withPinnedRisuSaveExport(
            this.dependencies.store,
            { revision, mutationGeneration: 0 },
            async (pinned) => {
                const bytes = await pinned.collectBytes()
                const characterCount = await pinned.countCharacters()
                await conflict.backup({ side: 'local', bytes, characterCount })
            },
        )
    }
}
