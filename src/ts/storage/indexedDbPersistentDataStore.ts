import isEqual from 'lodash/isEqual'
import { applyRootMutations } from './rootMutation'
import type { Chat, Database, Message, botPreset } from './database.svelte'
import type {
    ArchivePreview,
    AssetAlias,
    AssetAliasIdentity,
    AssetAliasKind,
    AssetAliasListQuery,
    AssetAliasPage,
    AssetOwnerHead,
    AssetOwnerLocator,
    CharacterDetail,
    CharacterPage,
    CharacterQuery,
    CharacterSummary,
    ConversationMutation,
    ConversationPage,
    ConversationQuery,
    ConversationSummary,
    ConversationWindow,
    ConversationWindowQuery,
    DataRevision,
    AssetRepositoryAuthorityState,
    AssetRepositoryMigrationInput,
    PersistentConversationMetadata,
    PersistentDataStore,
    PersistentRevisionLease,
    PersistentRoot,
    PluginStorageCatalog,
    PluginStorageListItem,
    PluginStorageMutation,
    PresetCatalog,
    PresetSummary,
    Versioned,
    WorkingSetCommit,
} from './persistentDataStore'
import {
    RevisionConflictError,
    SnapshotReleasedError,
    validateAssetAlias,
    validateAssetAliasKeyBatch,
    validateAssetAliasIdentity,
    assetOwnerLocatorKey,
    validateConversationWindowQuery,
    validateAssetOwnerHead,
} from './persistentDataStore'
import type { PluginStorageMeta } from '../plugins/pluginOwner'
import { readPluginStorageMetaOwner } from '../plugins/pluginOwner'
import { parseAssetRepositoryAuthorityState } from './assetRepositoryAuthority'

const DATABASE_VERSION = 1
const DATABASE_SCHEMA_ID = 'risunest-persistent-data-v1'
const MESSAGE_PAGE_SIZE = 128
const SNAPSHOT_LEASE_TTL_MS = 24 * 60 * 60 * 1000
const MAX_INDEX_VALUE = Number.MAX_SAFE_INTEGER
// Add every generation-scoped record family here so lease COW and cleanup cannot omit it.
const INDEXED_GENERATION_STORE_NAMES = [
    'presets',
    'catalog',
    'characters',
    'conversations',
    'messagePages',
    'messageOccurrences',
    'pluginStorage',
    'pluginStorageMetadata',
    'assetAliases',
    'assetOwnerHeads',
    'assetRepositoryAuthority',
] as const
const DATA_STORE_NAMES = ['root', ...INDEXED_GENERATION_STORE_NAMES] as const
const STORE_NAMES = ['meta', ...DATA_STORE_NAMES] as const
const activeSnapshotLeases = new Set<string>()

interface StoredRecord<T> {
    key: string
    generation: string
    value: T
}

interface StoredMessagePage extends StoredRecord<Message[]> {
    characterId: string
    conversationId: string
    pageIndex: number
}

interface StoredConversation {
    summary: ConversationSummary
    detail: Omit<Chat, 'message'>
}

interface StoredPreset {
    summary: PresetSummary
    preset: botPreset
}

interface StoredPluginStorage extends StoredRecord<unknown> {
    storageKey: string
    byteSize: number
    ordinal: number
}

interface StoredPluginStorageMetadata {
    key: string
    generation: string
    owner: string
    storageKey: string
    valueType: 'string' | 'json'
    byteSize: number
    ordinal: number
}

function ownArrayProperty(value: object, key: string): unknown[] | undefined {
    if (!Object.prototype.hasOwnProperty.call(value, key)) return undefined
    const property = (value as Record<string, unknown>)[key]
    if (!Array.isArray(property)) {
        throw new TypeError(`Asset owner ${key} property must be an array when present`)
    }
    return property
}

interface ReplacementOwnerTuple {
    present: boolean
    entries: unknown[]
}

// A malformed parent or non-array property yields no tuple, so the head is
// dropped instead of failing the whole replacement. Staged-head validation
// stays strict through ownArrayProperty above.
function replacementOwnerTupleFromParent(
    parent: object | null | undefined,
    property: string,
): ReplacementOwnerTuple | null {
    if (!parent || typeof parent !== 'object') return null
    if (!Object.prototype.hasOwnProperty.call(parent, property)) {
        return { present: false, entries: [] }
    }
    const entries = (parent as Record<string, unknown>)[property]
    return Array.isArray(entries)
        ? { present: true, entries }
        : null
}

function replacementOwnerTupleFromDatabase(
    database: Database,
    owner: AssetOwnerLocator,
): ReplacementOwnerTuple | null {
    if (owner.kind === 'character-additional-assets') {
        return replacementOwnerTupleFromParent(
            database.characters.find((character) => character.chaId === owner.characterId),
            'additionalAssets',
        )
    }
    if (owner.kind === 'root-module-assets') {
        return replacementOwnerTupleFromParent(database.modules?.[owner.index], 'assets')
    }
    return replacementOwnerTupleFromParent(
        database.personas?.[owner.index]?.embeddedModule,
        'assets',
    )
}

function replacementOwnerTuplesEqual(
    left: ReplacementOwnerTuple | null,
    right: ReplacementOwnerTuple | null,
): boolean {
    return left !== null
        && right !== null
        && left.present === right.present
        && isEqual(left.entries, right.entries)
}

function retainedModuleIndex(
    oldRoot: PersistentRoot,
    newRoot: PersistentRoot,
    property: 'modules' | 'personas',
    index: number,
    embedded: boolean,
): number | null {
    const oldValues = (oldRoot as Record<string, unknown>)[property]
    const newValues = (newRoot as Record<string, unknown>)[property]
    if (!Array.isArray(oldValues) || !Array.isArray(newValues)) return null
    const source = oldValues[index]
    if (source === undefined) return null
    const moduleFrom = (value: unknown): unknown => {
        if (!embedded) return value
        if (!value || typeof value !== 'object') return undefined
        return (value as Record<string, unknown>).embeddedModule
    }
    const sourceModule = moduleFrom(source)
    const id = sourceModule && typeof sourceModule === 'object'
        ? (sourceModule as Record<string, unknown>).id
        : undefined
    if (typeof id === 'string' && id.length > 0) {
        const matchesId = (value: unknown) => {
            const module = moduleFrom(value)
            return module && typeof module === 'object'
                && (module as Record<string, unknown>).id === id
        }
        if (oldValues.filter(matchesId).length === 1 && newValues.filter(matchesId).length === 1) {
            return newValues.findIndex(matchesId)
        }
    }
    if (isEqual(newValues[index], source)) return index
    const oldMatches = oldValues.filter((value) => isEqual(value, source))
    const newMatches = newValues
        .map((value, candidate) => ({ value, candidate }))
        .filter(({ value }) => isEqual(value, source))
    return oldMatches.length === 1 && newMatches.length === 1
        ? newMatches[0].candidate
        : null
}

interface StoredMessageOccurrencePage {
    key: string
    generation: string
    characterId: string
    conversationId: string
    pageIndex: number
    lookupKeys: string[]
}

function commitCharacterParents(input: WorkingSetCommit): Map<string, CharacterDetail> {
    const parents = new Map<string, CharacterDetail>()
    for (const character of [
        input.character,
        ...(input.characterDetails ?? []),
        input.replaceCharacter,
        input.addCharacter,
    ]) {
        if (!character) continue
        parents.set(character.chaId, character)
    }
    return parents
}

function validateOwnerHeadsForCommit(input: WorkingSetCommit): void {
    const heads = input.assetOwnerHeads ?? []
    const keys = new Set<string>()
    const characterParents = commitCharacterParents(input)
    for (const head of heads) {
        validateAssetOwnerHead(head)
        const key = assetOwnerLocatorKey(head.owner)
        if (keys.has(key)) throw new TypeError(`Duplicate asset owner head ${key}`)
        keys.add(key)

        let entries: unknown[] | undefined
        if (head.owner.kind === 'character-additional-assets') {
            const parent = characterParents.get(head.owner.characterId)
            if (!parent) {
                throw new TypeError('Character asset owner head requires its parent mutation')
            }
            entries = ownArrayProperty(parent, 'additionalAssets')
        } else {
            if (!input.root) throw new TypeError('Root asset owner head requires its parent root')
            if (head.owner.kind === 'root-module-assets') {
                const module = input.root.modules?.[head.owner.index]
                if (!module) throw new TypeError('Root module asset owner occurrence does not exist')
                entries = ownArrayProperty(module, 'assets')
            } else {
                const module = input.root.personas?.[head.owner.index]?.embeddedModule
                if (!module) throw new TypeError('Persona module asset owner occurrence does not exist')
                entries = ownArrayProperty(module, 'assets')
            }
        }
        if (head.present !== (entries !== undefined)) {
            throw new TypeError('Asset owner head property presence does not match its parent')
        }
        if (head.present && head.entryCount !== entries?.length) {
            throw new TypeError('Asset owner head entryCount does not match its parent')
        }
    }
}

const textEncoder = new TextEncoder()

function serializedByteSize(value: unknown): number {
    return textEncoder.encode(JSON.stringify(value) ?? 'null').byteLength
}

function arrayIndexKey(key: string): number | null {
    if (!/^(0|[1-9]\d*)$/.test(key)) return null
    const value = Number(key)
    return Number.isSafeInteger(value) && value >= 0 && value < 4_294_967_295
        ? value
        : null
}

function comparePluginStorageRecords(
    left: Pick<StoredPluginStorageMetadata, 'storageKey' | 'ordinal'>,
    right: Pick<StoredPluginStorageMetadata, 'storageKey' | 'ordinal'>,
): number {
    const leftIndex = arrayIndexKey(left.storageKey)
    const rightIndex = arrayIndexKey(right.storageKey)
    if (leftIndex !== null && rightIndex !== null) return leftIndex - rightIndex
    if (leftIndex !== null) return -1
    if (rightIndex !== null) return 1
    return left.ordinal - right.ordinal || left.storageKey.localeCompare(right.storageKey)
}

interface SnapshotLeaseTarget {
    generation: string
    revision: DataRevision
}

interface SnapshotLeaseRecord {
    key: string
    value: SnapshotLeaseTarget
    createdAt?: number
}

export type PersistentGenerationCleanupErrorHandler = (
    generation: string,
    error: unknown,
) => void

function reportGenerationCleanupError(generation: string, error: unknown): void {
    console.error(`Persistent data cleanup failed for generation ${generation}`, error)
}

function reportBlockedUpgrade(): void {
    console.warn('Persistent data upgrade is waiting for another open document to close')
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
    return new Promise((resolve, reject) => {
        request.onsuccess = () => resolve(request.result)
        request.onerror = () => reject(request.error)
    })
}

// Composite record keys join their components with ':'. Components carrying
// user-controlled ids are escaped so distinct (characterId, conversationId)
// tuples can never collide onto the same joined key; the escape alphabet is
// colon-free, so exact lookups and fixed-prefix scans keep working.
function encodeKeyComponent(component: string): string {
    return component.replace(/%/g, '%25').replace(/:/g, '%3A')
}

function transactionDone(transaction: IDBTransaction): Promise<void> {
    return new Promise((resolve, reject) => {
        transaction.oncomplete = () => resolve()
        transaction.onabort = () => reject(transaction.error ?? new Error('IndexedDB transaction aborted'))
        transaction.onerror = () => reject(transaction.error ?? new Error('IndexedDB transaction failed'))
    })
}

function cursorPage<T>(
    index: IDBIndex,
    range: IDBKeyRange,
    input: { limit: number; cursor?: string },
    predicate: (value: T) => boolean,
): Promise<{ items: T[]; nextCursor?: string }> {
    if (!(input.limit > 0)) {
        throw new RangeError('Query limit must be a positive number')
    }
    const parsedOffset = input.cursor === undefined ? 0 : Number.parseInt(input.cursor, 10)
    const offset = Number.isFinite(parsedOffset) && parsedOffset >= 0 ? parsedOffset : 0
    const limit = input.limit
    return new Promise((resolve, reject) => {
        const items: T[] = []
        let matched = 0
        let hasMore = false
        const request = index.openCursor(range)
        request.onerror = () => reject(request.error)
        request.onsuccess = () => {
            try {
                const cursor = request.result
                if (!cursor) {
                    resolve({
                        items,
                        nextCursor: hasMore ? String(offset + items.length) : undefined,
                    })
                    return
                }
                const value = (cursor.value as StoredRecord<T>).value
                if (predicate(value)) {
                    if (matched >= offset && items.length < limit) {
                        items.push(value)
                    } else if (matched >= offset + limit) {
                        hasMore = true
                        resolve({ items, nextCursor: String(offset + items.length) })
                        return
                    }
                    matched++
                }
                cursor.continue()
            } catch (error) {
                reject(error)
                try {
                    index.objectStore.transaction.abort()
                } catch {}
            }
        }
    })
}

export class IndexedDbPersistentDataStore implements PersistentDataStore {
    private database?: IDBDatabase
    private openPromise?: Promise<void>

    constructor(
        private readonly databaseName: string,
        private readonly indexedDbFactory: IDBFactory = indexedDB,
        private readonly keyRangeFactory: typeof IDBKeyRange = globalThis.IDBKeyRange,
        private readonly onCleanupError: PersistentGenerationCleanupErrorHandler =
            reportGenerationCleanupError,
        private readonly onBlockedUpgrade: () => void = reportBlockedUpgrade,
    ) {}

    async open(): Promise<void> {
        if (this.database) return
        this.openPromise ??= this.openDatabase().finally(() => {
            this.openPromise = undefined
        })
        return this.openPromise
    }

    private async openDatabase(): Promise<void> {
        const request = this.indexedDbFactory.open(this.databaseName, DATABASE_VERSION)
        // Another document holding the previous version would otherwise stall boot forever.
        request.onblocked = () => this.onBlockedUpgrade()
        request.onupgradeneeded = () => {
            const database = request.result
            for (const storeName of STORE_NAMES) {
                if (!database.objectStoreNames.contains(storeName)) {
                    database.createObjectStore(storeName, { keyPath: 'key' })
                }
            }
            const transaction = request.transaction!
            this.createIndex(
                transaction.objectStore('presets'),
                'byGenerationConfigured',
                ['generation', 'configuredIndex'],
            )
            this.createIndex(transaction.objectStore('presets'), 'byGeneration', 'generation')
            this.createIndex(
                transaction.objectStore('catalog'),
                'byGenerationConfigured',
                ['generation', 'configuredIndex'],
            )
            this.createIndex(
                transaction.objectStore('catalog'),
                'byGenerationRecent',
                ['generation', 'recentSortValue', 'configuredIndex'],
            )
            this.createIndex(transaction.objectStore('catalog'), 'byGeneration', 'generation')
            this.createIndex(transaction.objectStore('characters'), 'byGeneration', 'generation')
            this.createIndex(
                transaction.objectStore('conversations'),
                'byGenerationCharacterConfigured',
                ['generation', 'value.summary.characterId', 'configuredIndex'],
            )
            this.createIndex(
                transaction.objectStore('conversations'),
                'byGenerationCharacterRecent',
                ['generation', 'value.summary.characterId', 'recentSortValue', 'configuredIndex'],
            )
            this.createIndex(transaction.objectStore('conversations'), 'byGeneration', 'generation')
            this.createIndex(
                transaction.objectStore('messagePages'),
                'byConversationPage',
                ['generation', 'characterId', 'conversationId', 'pageIndex'],
            )
            this.createIndex(
                transaction.objectStore('messagePages'),
                'byGenerationCharacter',
                ['generation', 'characterId'],
            )
            this.createIndex(transaction.objectStore('messagePages'), 'byGeneration', 'generation')
            const messageOccurrences = transaction.objectStore('messageOccurrences')
            this.createIndex(
                messageOccurrences,
                'byLookupKey',
                'lookupKeys',
                { multiEntry: true },
            )
            this.createIndex(
                messageOccurrences,
                'byConversationPage',
                ['generation', 'characterId', 'conversationId', 'pageIndex'],
            )
            this.createIndex(
                messageOccurrences,
                'byGenerationCharacter',
                ['generation', 'characterId'],
            )
            this.createIndex(
                messageOccurrences,
                'byGeneration',
                'generation',
            )
            this.createIndex(
                transaction.objectStore('pluginStorage'),
                'byGenerationKey',
                ['generation', 'storageKey'],
            )
            this.createIndex(transaction.objectStore('pluginStorage'), 'byGeneration', 'generation')
            this.createIndex(
                transaction.objectStore('pluginStorageMetadata'),
                'byGenerationOrdinal',
                ['generation', 'ordinal'],
            )
            this.createIndex(
                transaction.objectStore('pluginStorageMetadata'),
                'byGeneration',
                'generation',
            )
            this.createIndex(transaction.objectStore('assetAliases'), 'byGeneration', 'generation')
            this.createIndex(
                transaction.objectStore('assetAliases'),
                'byGenerationKindKey',
                ['generation', 'value.kind', 'value.key'],
            )
            this.createIndex(
                transaction.objectStore('assetOwnerHeads'),
                'byGeneration',
                'generation',
            )
            this.createIndex(
                transaction.objectStore('assetRepositoryAuthority'),
                'byGeneration',
                'generation',
            )
            transaction.objectStore('meta').put({
                key: 'schemaIdentity',
                value: DATABASE_SCHEMA_ID,
            })
            transaction.objectStore('meta').put({
                key: 'schemaVersion',
                value: DATABASE_VERSION,
            })
        }
        const database = await requestResult(request)
        try {
            await this.validateDatabaseSchema(database)
        } catch (error) {
            database.close()
            throw error
        }
        this.database = database
        database.onversionchange = () => {
            this.database?.close()
            this.database = undefined
        }

        const transaction = this.database.transaction(
            ['meta', 'root', 'assetRepositoryAuthority'],
            'readwrite',
        )
        const meta = transaction.objectStore('meta')
        const currentRevision = await requestResult(meta.get('currentRevision'))
        if (!currentRevision) {
            const generation = this.generationFor(0)
            meta.put({ key: 'activeGeneration', value: generation })
            meta.put({ key: 'currentRevision', value: 0 })
            transaction.objectStore('root').put({ key: generation, generation, value: {} })
            this.putAssetRepositoryAuthority(transaction, generation, { format: 'legacy' })
        }
        await transactionDone(transaction)
        await this.sweepTemporaryGenerations()
    }

    private async validateDatabaseSchema(database: IDBDatabase): Promise<void> {
        if (STORE_NAMES.some((storeName) => !database.objectStoreNames.contains(storeName))) {
            throw new Error('Unsupported RisuNest IndexedDB schema')
        }
        const transaction = database.transaction('meta', 'readonly')
        const done = transactionDone(transaction)
        void done.catch(() => {})
        const meta = transaction.objectStore('meta')
        const [identity, version] = await Promise.all([
            requestResult(meta.get('schemaIdentity')),
            requestResult(meta.get('schemaVersion')),
        ]) as Array<{ value?: unknown } | undefined>
        await done
        if (
            identity?.value !== DATABASE_SCHEMA_ID
            || version?.value !== DATABASE_VERSION
        ) {
            throw new Error('Unsupported RisuNest IndexedDB schema')
        }
    }

    async readRoot(): Promise<Versioned<PersistentRoot>> {
        const transaction = this.requireDatabase().transaction(['meta', 'root'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        const record = await this.readRootRecordFromTransaction(transaction, generation)
        return { revision, value: record?.value ?? ({} as PersistentRoot) }
    }

    async queryPresets(): Promise<PresetCatalog> {
        const transaction = this.requireDatabase().transaction(['meta', 'presets'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.queryPresetsFromTransaction(transaction, revision, generation)
    }

    async readPreset(id: string): Promise<Versioned<botPreset> | null> {
        const transaction = this.requireDatabase().transaction(['meta', 'presets'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.readPresetFromTransaction(transaction, revision, generation, id)
    }

    async queryCharacters(input: CharacterQuery): Promise<CharacterPage> {
        const transaction = this.requireDatabase().transaction(['meta', 'catalog'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.queryCharactersFromTransaction(transaction, revision, generation, input)
    }

    async readCharacterSummary(id: string): Promise<CharacterSummary | null> {
        const transaction = this.requireDatabase().transaction(['meta', 'catalog'], 'readonly')
        const { generation } = await this.readActive(transaction)
        return this.readCharacterSummaryFromTransaction(transaction, generation, id)
    }

    async readCharacter(id: string): Promise<Versioned<CharacterDetail> | null> {
        const transaction = this.requireDatabase().transaction(['meta', 'characters'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.readCharacterFromTransaction(transaction, revision, generation, id)
    }

    async queryConversations(input: ConversationQuery): Promise<ConversationPage> {
        const transaction = this.requireDatabase().transaction(['meta', 'conversations'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.queryConversationsFromTransaction(transaction, revision, generation, input)
    }

    async readConversation(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<Chat> | null> {
        const transaction = this.requireDatabase().transaction(
            ['meta', 'conversations', 'messagePages'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
        return this.readConversationFromTransaction(
            transaction,
            revision,
            generation,
            characterId,
            conversationId,
        )
    }

    async readConversationMetadata(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<PersistentConversationMetadata> | null> {
        const transaction = this.requireDatabase().transaction(
            ['meta', 'conversations'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
        return this.readConversationMetadataFromTransaction(
            transaction,
            revision,
            generation,
            characterId,
            conversationId,
        )
    }

    async readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null> {
        validateConversationWindowQuery(input)
        const transaction = this.requireDatabase().transaction(
            ['meta', 'conversations', 'messagePages', 'messageOccurrences'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
        return this.readConversationWindowFromTransaction(transaction, revision, generation, input)
    }

    async queryPluginStorage(): Promise<PluginStorageCatalog> {
        const transaction = this.requireDatabase().transaction(
            ['meta', 'pluginStorageMetadata'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
        return this.queryPluginStorageFromTransaction(transaction, revision, generation)
    }

    async readPluginStorage(owner: string, key: string): Promise<Versioned<unknown> | null> {
        const transaction = this.requireDatabase().transaction(['meta', 'pluginStorage'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.readPluginStorageFromTransaction(transaction, revision, generation, owner, key)
    }

    async listPluginStorage(): Promise<PluginStorageListItem[]> {
        const transaction = this.requireDatabase().transaction(
            ['meta', 'pluginStorageMetadata'],
            'readonly',
        )
        const { generation } = await this.readActive(transaction)
        const records = (await requestResult(
            transaction.objectStore('pluginStorageMetadata').index('byGeneration').getAll(generation),
        )) as StoredPluginStorageMetadata[]
        await transactionDone(transaction)
        return records.sort(comparePluginStorageRecords).map((record) => ({
            owner: record.owner,
            key: record.storageKey,
            valueType: record.valueType,
            byteSize: record.byteSize,
        }))
    }

    async readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null> {
        validateAssetAliasIdentity(identity)
        const transaction = this.requireDatabase().transaction(['meta', 'assetAliases'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.readAssetAliasFromTransaction(transaction, revision, generation, identity)
    }

    async readAssetAliasesByKeys(
        kind: AssetAliasKind,
        keys: string[],
    ): Promise<Versioned<AssetAlias[]>> {
        validateAssetAliasKeyBatch(kind, keys)
        const transaction = this.requireDatabase().transaction(['meta', 'assetAliases'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.readAssetAliasesByKeysFromTransaction(
            transaction,
            revision,
            generation,
            kind,
            keys,
        )
    }

    async listAssetAliases(input: AssetAliasListQuery): Promise<AssetAliasPage> {
        const transaction = this.requireDatabase().transaction(['meta', 'assetAliases'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.listAssetAliasesFromTransaction(transaction, revision, generation, input)
    }

    async readAssetRepositoryAuthority(): Promise<Versioned<AssetRepositoryAuthorityState>> {
        const transaction = this.requireDatabase().transaction(
            ['meta', 'assetRepositoryAuthority'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
        return this.readAssetRepositoryAuthorityFromTransaction(
            transaction,
            revision,
            generation,
        )
    }

    async readAssetOwnerHead(
        owner: AssetOwnerLocator,
    ): Promise<Versioned<AssetOwnerHead> | null> {
        const transaction = this.requireDatabase().transaction(
            ['meta', 'assetOwnerHeads'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
        return this.readAssetOwnerHeadFromTransaction(transaction, revision, generation, owner)
    }

    async commitAssetAlias(
        alias: AssetAlias,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        const transaction = this.requireDatabase().transaction([...STORE_NAMES], 'readwrite')
        try {
            const active = await this.readActive(transaction)
            if (active.revision !== expectedRevision) {
                throw new RevisionConflictError(expectedRevision, active.revision)
            }
            validateAssetAlias(alias)
            const revision = active.revision + 1
            const generation = await this.ensureWritableGeneration(
                transaction,
                active.generation,
                revision,
            )
            transaction.objectStore('assetAliases').put({
                key: this.assetAliasKey(generation, alias.kind, alias.key),
                generation,
                value: structuredClone(alias),
            } satisfies StoredRecord<AssetAlias>)
            this.setActive(transaction, revision, generation)
            await transactionDone(transaction)
            return { revision }
        } catch (error) {
            try {
                transaction.abort()
            } catch {}
            throw error
        }
    }

    async deleteAssetAlias(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        validateAssetAliasIdentity(identity)
        const transaction = this.requireDatabase().transaction([...STORE_NAMES], 'readwrite')
        try {
            const active = await this.readActive(transaction)
            if (active.revision !== expectedRevision) {
                throw new RevisionConflictError(expectedRevision, active.revision)
            }
            const revision = active.revision + 1
            const generation = await this.ensureWritableGeneration(
                transaction,
                active.generation,
                revision,
            )
            transaction.objectStore('assetAliases').delete(
                this.assetAliasKey(generation, identity.kind, identity.key),
            )
            this.setActive(transaction, revision, generation)
            await transactionDone(transaction)
            return { revision }
        } catch (error) {
            try {
                transaction.abort()
            } catch {}
            throw error
        }
    }

    async activateAssetRepositoryMigration(
        input: AssetRepositoryMigrationInput,
    ): Promise<{ revision: DataRevision }> {
        const authority = parseAssetRepositoryAuthorityState({
            format: 'v2',
            migrationId: input.migrationId,
            compatibilityHash: input.compatibilityHash,
        })
        for (const alias of input.assetAliases) validateAssetAlias(alias)
        const {
            characters,
            botPresets: _botPresets,
            pluginCustomStorage: _pluginStorage,
            pluginStorageMeta: _pluginStorageMeta,
            ...root
        } = input.database
        const characterDetails = characters.map(({ chats: _chats, ...detail }) => detail)
        validateOwnerHeadsForCommit({
            expectedRevision: input.sourceRevision,
            root,
            characterDetails,
            assetOwnerHeads: input.assetOwnerHeads,
        })

        const transaction = this.requireDatabase().transaction([...STORE_NAMES], 'readwrite')
        try {
            const active = await this.readActive(transaction)
            if (active.revision !== input.sourceRevision) {
                throw new RevisionConflictError(input.sourceRevision, active.revision)
            }
            const revision = active.revision + 1
            const generation = this.generationFor(revision)
            await this.stageDatabase(transaction, input.database, generation, input.assetAliases)
            this.putAssetRepositoryAuthority(transaction, generation, {
                format: 'preparing',
                migrationId: input.migrationId,
                sourceRevision: input.sourceRevision,
            })
            for (const head of input.assetOwnerHeads) {
                this.putAssetOwnerHead(transaction, generation, head)
            }
            this.putAssetRepositoryAuthority(transaction, generation, authority)
            if (!(await this.generationIsLeased(transaction, active.generation))) {
                await this.deleteGenerationFromTransaction(transaction, active.generation)
            }
            this.setActive(transaction, revision, generation)
            await transactionDone(transaction)
            return { revision }
        } catch (error) {
            try {
                transaction.abort()
            } catch {}
            throw error
        }
    }

    archivePreview(_characterId: string): Promise<ArchivePreview> {
        return Promise.reject(new Error('Archiving characters requires the native store'))
    }

    archiveCharacter(
        _characterId: string,
        _expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        return Promise.reject(new Error('Archiving characters requires the native store'))
    }

    restoreCharacter(
        _characterId: string,
        _expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }> {
        return Promise.reject(new Error('Archiving characters requires the native store'))
    }

    async commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }> {
        const database = this.requireDatabase()
        const transaction = database.transaction([...STORE_NAMES], 'readwrite')
        try {
            const active = await this.readActive(transaction)
            if (active.revision !== input.expectedRevision) {
                throw new RevisionConflictError(input.expectedRevision, active.revision)
            }
            if (input.rootMutations !== undefined) {
                if (input.root !== undefined)
                    throw new TypeError('Root and rootMutations are mutually exclusive')
                const record = await requestResult<StoredRecord<PersistentRoot> | undefined>(
                    transaction.objectStore('root').get(active.generation),
                )
                if (!record) throw new TypeError('Missing persistent root')
                const { rootMutations, ...rest } = input
                input = { ...rest, root: applyRootMutations(record.value, rootMutations) }
            }
            if (input.replaceCharacter) {
                this.validateCharacterInput(input.replaceCharacter, 'Selected character replacement')
            }
            if (input.addCharacter) {
                this.validateCharacterInput(input.addCharacter, 'Character addition')
            }
            if (input.characterDetails) {
                await this.validateCharacterDetails(
                    transaction,
                    active.generation,
                    input.characterDetails,
                    input.deleteCharacterId,
                )
            }
            const aliasKeys = new Set<string>()
            for (const alias of input.assetAliases ?? []) {
                validateAssetAlias(alias)
                const key = `${alias.kind}\0${alias.key}`
                if (aliasKeys.has(key)) {
                    throw new TypeError(`Duplicate asset alias ${alias.kind}:${alias.key}`)
                }
                aliasKeys.add(key)
            }
            validateOwnerHeadsForCommit(input)
            const retainedOwnerHeads = await this.retainedCommitOwnerHeads(
                transaction,
                active.generation,
                input,
            )

            const revision = active.revision + 1
            const generation = await this.ensureWritableGeneration(
                transaction,
                active.generation,
                revision,
            )
            if (input.root) this.putRoot(transaction, generation, input.root)
            if (input.replacePresets) await this.putPresets(transaction, generation, input.replacePresets)
            if (input.deleteCharacterId) {
                await this.deleteCharacter(transaction, generation, input.deleteCharacterId)
            }
            if (input.character) await this.putCharacter(transaction, generation, input.character)
            for (const detail of input.characterDetails ?? []) {
                await this.putCharacter(transaction, generation, detail)
            }
            if (input.replaceCharacter) {
                await this.replaceCharacter(transaction, generation, input.replaceCharacter)
            }
            if (input.addCharacter) {
                await this.addCharacter(transaction, generation, input.addCharacter)
            }
            for (const mutation of input.conversations ?? []) {
                await this.applyConversationMutation(transaction, generation, mutation)
            }
            for (const mutation of input.pluginStorage ?? []) {
                await this.applyPluginStorageMutation(transaction, generation, mutation)
            }
            for (const alias of input.assetAliases ?? []) {
                transaction.objectStore('assetAliases').put({
                    key: this.assetAliasKey(generation, alias.kind, alias.key),
                    generation,
                    value: structuredClone(alias),
                } satisfies StoredRecord<AssetAlias>)
            }
            await this.replaceChangedOwnerHeads(
                transaction,
                generation,
                input,
                retainedOwnerHeads,
            )
            this.setActive(transaction, revision, generation)
            await transactionDone(transaction)
            return { revision }
        } catch (error) {
            try {
                transaction.abort()
            } catch {}
            throw error
        }
    }

    async replaceFromDatabase(
        databaseValue: Database,
        expectedRevision?: DataRevision,
        assetAliases: AssetAlias[] = [],
    ): Promise<{ revision: DataRevision }> {
        const database = this.requireDatabase()
        const transaction = database.transaction([...STORE_NAMES], 'readwrite')
        try {
            const active = await this.readActive(transaction)
            if (expectedRevision !== undefined && active.revision !== expectedRevision) {
                throw new RevisionConflictError(expectedRevision, active.revision)
            }
            const revision = active.revision + 1
            const generation = this.generationFor(revision)
            await this.stageDatabase(transaction, databaseValue, generation, [])
            await this.preserveRepositoriesForReplacement(
                transaction,
                active.generation,
                generation,
                databaseValue,
            )
            for (const alias of assetAliases) {
                validateAssetAlias(alias)
                transaction.objectStore('assetAliases').put({
                    key: this.assetAliasKey(generation, alias.kind, alias.key),
                    generation,
                    value: structuredClone(alias),
                } satisfies StoredRecord<AssetAlias>)
            }
            if (!(await this.generationIsLeased(transaction, active.generation))) {
                await this.deleteGenerationFromTransaction(transaction, active.generation)
            }
            this.setActive(transaction, revision, generation)
            await transactionDone(transaction)
            return { revision }
        } catch (error) {
            try {
                transaction.abort()
            } catch {}
            throw error
        }
    }

    async materializeDatabase(revision?: DataRevision): Promise<Database> {
        const database = this.requireDatabase()
        const transaction = database.transaction(
            ['meta', 'root', 'presets', 'catalog', 'characters', 'conversations', 'messagePages', 'pluginStorage'],
            'readonly',
        )
        const active = await this.readActive(transaction)
        const targetRevision = revision ?? active.revision
        if (targetRevision !== active.revision) {
            await transactionDone(transaction)
            throw new RevisionConflictError(targetRevision, active.revision)
        }
        const generation = active.generation
        const root = (await requestResult(
            transaction.objectStore('root').get(generation),
        )) as StoredRecord<PersistentRoot> | undefined
        if (!root) throw new RevisionConflictError(targetRevision, active.revision)

        const catalog = (
            await this.generationRecords<CharacterSummary>(
                transaction.objectStore('catalog'),
                generation,
            )
        )
            .map((record) => record.value)
            .sort((left, right) => left.configuredIndex - right.configuredIndex)
        const characterRecords = await this.generationRecords<CharacterDetail>(
            transaction.objectStore('characters'),
            generation,
        )
        const conversationRecords = await this.generationRecords<StoredConversation>(
            transaction.objectStore('conversations'),
            generation,
        )
        const presetRecords = await this.generationRecords<StoredPreset>(
            transaction.objectStore('presets'),
            generation,
        )
        const pluginStorageRecords = await this.generationRecords<unknown>(
            transaction.objectStore('pluginStorage'),
            generation,
        ) as StoredPluginStorage[]
        const characters = [] as Database['characters']
        for (const summary of catalog) {
            const detail = characterRecords.find(
                (record) => record.key === this.characterKey(generation, summary.id),
            )
            if (!detail) throw new Error(`Missing character detail for ${summary.id}`)
            const conversations = conversationRecords
                .map((record) => record.value)
                .filter((conversation) => conversation.summary.characterId === summary.id)
                .sort(
                    (left, right) =>
                        left.summary.configuredIndex - right.summary.configuredIndex,
                )
            const chats: Chat[] = []
            for (const conversation of conversations) {
                chats.push({
                    ...conversation.detail,
                    message: await this.readMessagesFromTransaction(
                        transaction,
                        generation,
                        summary.id,
                        conversation.summary.id,
                    ),
                })
            }
            characters.push({ ...detail.value, chats } as Database['characters'][number])
        }
        const botPresets = presetRecords
            .map((record) => record.value)
            .sort((left, right) => left.summary.configuredIndex - right.summary.configuredIndex)
            .map((record) => record.preset)
        const pluginCustomStorage = Object.fromEntries(
            pluginStorageRecords
                .sort(comparePluginStorageRecords)
                .map((record) => [record.storageKey, record.value]),
        )
        const result = {
            ...root.value,
            characters,
            botPresets,
            pluginCustomStorage,
        } as Database
        await transactionDone(transaction)
        return result
    }

    async acquireRevision(revision: DataRevision): Promise<PersistentRevisionLease> {
        const database = this.requireDatabase()
        const lease = `snapshot-${revision}-${globalThis.crypto.randomUUID()}`
        const transaction = database.transaction(['meta', 'root'], 'readwrite')
        activeSnapshotLeases.add(lease)
        let generation = ''
        try {
            const active = await this.readActive(transaction)
            if (active.revision !== revision) {
                throw new RevisionConflictError(revision, active.revision)
            }
            const root = (await requestResult(
                transaction.objectStore('root').get(active.generation),
            )) as StoredRecord<PersistentRoot> | undefined
            if (!root) throw new RevisionConflictError(revision, active.revision)
            generation = active.generation
            transaction.objectStore('meta').put({
                key: this.snapshotLeaseKey(lease),
                value: { generation, revision } satisfies SnapshotLeaseTarget,
                createdAt: Date.now(),
            })
            await transactionDone(transaction)
        } catch (error) {
            activeSnapshotLeases.delete(lease)
            try {
                transaction.abort()
            } catch {}
            throw error
        }

        let released = false
        let releasePromise: Promise<void> | undefined
        const assertActive = () => {
            if (released) throw new SnapshotReleasedError()
        }
        return {
            revision,
            readRoot: async () => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'root'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                const record = await this.readRootRecordFromTransaction(
                    transaction,
                    generation,
                )
                if (!record) throw new Error('Persistent snapshot root is missing')
                return { revision, value: record.value }
            },
            queryPresets: async () => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'presets'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.queryPresetsFromTransaction(
                    transaction,
                    revision,
                    generation,
                )
            },
            readPreset: async (id) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'presets'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readPresetFromTransaction(
                    transaction,
                    revision,
                    generation,
                    id,
                )
            },
            queryCharacters: async (input) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'catalog'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.queryCharactersFromTransaction(
                    transaction,
                    revision,
                    generation,
                    input,
                )
            },
            readCharacterSummary: async (id) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'catalog'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readCharacterSummaryFromTransaction(transaction, generation, id)
            },
            readCharacter: async (id) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'characters'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readCharacterFromTransaction(
                    transaction,
                    revision,
                    generation,
                    id,
                )
            },
            queryConversations: async (input) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'conversations'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.queryConversationsFromTransaction(
                    transaction,
                    revision,
                    generation,
                    input,
                )
            },
            readConversation: async (characterId, conversationId) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'conversations', 'messagePages'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readConversationFromTransaction(
                    transaction,
                    revision,
                    generation,
                    characterId,
                    conversationId,
                )
            },
            readConversationMetadata: async (characterId, conversationId) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'conversations'],
                    'readonly',
                )
                await this.validateSnapshotLease(
                    transaction,
                    lease,
                    generation,
                    revision,
                )
                return this.readConversationMetadataFromTransaction(
                    transaction,
                    revision,
                    generation,
                    characterId,
                    conversationId,
                )
            },
            readConversationWindow: async (input) => {
                assertActive()
                validateConversationWindowQuery(input)
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'conversations', 'messagePages', 'messageOccurrences'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readConversationWindowFromTransaction(
                    transaction,
                    revision,
                    generation,
                    input,
                )
            },
            queryPluginStorage: async () => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'pluginStorageMetadata'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.queryPluginStorageFromTransaction(
                    transaction,
                    revision,
                    generation,
                )
            },
            readPluginStorage: async (owner, key) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'pluginStorage'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readPluginStorageFromTransaction(
                    transaction,
                    revision,
                    generation,
                    owner,
                    key,
                )
            },
            readAssetAlias: async (identity) => {
                assertActive()
                validateAssetAliasIdentity(identity)
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'assetAliases'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readAssetAliasFromTransaction(
                    transaction,
                    revision,
                    generation,
                    identity,
                )
            },
            readAssetAliasesByKeys: async (kind, keys) => {
                assertActive()
                validateAssetAliasKeyBatch(kind, keys)
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'assetAliases'],
                    'readonly',
                )
                return this.readAssetAliasesByKeysFromTransaction(
                    transaction,
                    revision,
                    generation,
                    kind,
                    keys,
                    lease,
                )
            },
            listAssetAliases: async (input) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'assetAliases'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.listAssetAliasesFromTransaction(
                    transaction,
                    revision,
                    generation,
                    input,
                )
            },
            readAssetRepositoryAuthority: async () => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'assetRepositoryAuthority'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readAssetRepositoryAuthorityFromTransaction(
                    transaction,
                    revision,
                    generation,
                )
            },
            readAssetOwnerHead: async (owner) => {
                assertActive()
                const transaction = this.requireDatabase().transaction(
                    ['meta', 'assetOwnerHeads'],
                    'readonly',
                )
                await this.validateSnapshotLease(transaction, lease, generation, revision)
                return this.readAssetOwnerHeadFromTransaction(
                    transaction,
                    revision,
                    generation,
                    owner,
                )
            },
            release: async () => {
                if (releasePromise) return releasePromise
                releasePromise = this.releaseSnapshotLease(lease).then(
                    () => {
                        released = true
                        activeSnapshotLeases.delete(lease)
                    },
                    (error) => {
                        releasePromise = undefined
                        throw error
                    },
                )
                return releasePromise
            },
        }
    }

    private async validateSnapshotLease(
        transaction: IDBTransaction,
        lease: string,
        generation: string,
        revision: DataRevision,
    ): Promise<void> {
        const record = (await requestResult(
            transaction.objectStore('meta').get(this.snapshotLeaseKey(lease)),
        )) as SnapshotLeaseRecord | undefined
        const target = record ? this.snapshotLeaseTarget(record) : undefined
        if (!target || target.generation !== generation || target.revision !== revision) {
            await transactionDone(transaction)
            throw new SnapshotReleasedError()
        }
    }

    private async readRootRecordFromTransaction(
        transaction: IDBTransaction,
        generation: string,
    ): Promise<StoredRecord<PersistentRoot> | undefined> {
        const record = (await requestResult(
            transaction.objectStore('root').get(generation),
        )) as StoredRecord<PersistentRoot> | undefined
        await transactionDone(transaction)
        return record
    }

    private async readAssetAliasFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        identity: AssetAliasIdentity,
    ): Promise<Versioned<AssetAlias> | null> {
        const record = (await requestResult(
            transaction.objectStore('assetAliases').get(
                this.assetAliasKey(generation, identity.kind, identity.key),
            ),
        )) as StoredRecord<AssetAlias> | undefined
        await transactionDone(transaction)
        if (!record) return null
        if (record.generation !== generation) {
            throw new TypeError('Asset alias stored generation does not match its lookup key')
        }
        if (record.value.kind !== identity.kind) {
            throw new TypeError('Asset alias stored kind does not match its lookup key')
        }
        if (record.value.key !== identity.key) {
            throw new TypeError('Asset alias stored logical key does not match its lookup key')
        }
        validateAssetAlias(record.value)
        return { revision, value: structuredClone(record.value) }
    }

    private async readAssetAliasesByKeysFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        kind: AssetAliasKind,
        keys: string[],
        lease?: string,
    ): Promise<Versioned<AssetAlias[]>> {
        const leaseRequest = lease === undefined
            ? undefined
            : transaction.objectStore('meta').get(this.snapshotLeaseKey(lease))
        const objectStore = transaction.objectStore('assetAliases')
        const requests = keys.map((key) => objectStore.get(this.assetAliasKey(generation, kind, key)))
        const done = transactionDone(transaction)
        void done.catch(() => {})
        const leaseRecord = leaseRequest === undefined
            ? undefined
            : await requestResult(leaseRequest) as SnapshotLeaseRecord | undefined
        const records = await Promise.all(requests.map(async (request) =>
            await requestResult(request) as StoredRecord<AssetAlias> | undefined))
        await done
        if (lease !== undefined) {
            const target = leaseRecord ? this.snapshotLeaseTarget(leaseRecord) : undefined
            if (!target || target.generation !== generation || target.revision !== revision) {
                throw new SnapshotReleasedError()
            }
        }
        const values: AssetAlias[] = []
        for (let index = 0; index < records.length; index++) {
            const record = records[index]
            if (!record) continue
            if (record.generation !== generation) {
                throw new TypeError('Asset alias stored generation does not match its lookup key')
            }
            if (record.value.kind !== kind) {
                throw new TypeError('Asset alias stored kind does not match its lookup key')
            }
            if (record.value.key !== keys[index]) {
                throw new TypeError('Asset alias stored logical key does not match its lookup key')
            }
            validateAssetAlias(record.value)
            values.push(structuredClone(record.value))
        }
        return { revision, value: values }
    }

    private async listAssetAliasesFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        input: AssetAliasListQuery,
    ): Promise<AssetAliasPage> {
        if (!Number.isSafeInteger(input.limit) || input.limit < 1 || input.limit > 512) {
            throw new TypeError('Asset alias page limit must be between 1 and 512')
        }
        if (input.kind !== undefined && input.kind !== 'asset' && input.kind !== 'inlay') {
            throw new TypeError('Asset alias kind is invalid')
        }
        let cursorIdentity: AssetAliasIdentity | undefined
        if (input.cursor !== undefined) {
            try {
                const decoded = JSON.parse(input.cursor) as unknown
                if (!Array.isArray(decoded) || decoded.length !== 2) throw new TypeError()
                cursorIdentity = { kind: decoded[0], key: decoded[1] } as AssetAliasIdentity
                validateAssetAliasIdentity(cursorIdentity)
            } catch {
                throw new TypeError('Asset alias cursor is invalid')
            }
            if (input.kind !== undefined && cursorIdentity.kind !== input.kind) {
                throw new TypeError('Asset alias cursor kind does not match the query')
            }
        }
        const startKind = cursorIdentity?.kind ?? input.kind ?? 'asset'
        const startKey = cursorIdentity?.key ?? ''
        const range = this.keyRangeFactory.lowerBound(
            [generation, startKind, startKey],
            cursorIdentity !== undefined,
        )
        const items = await new Promise<AssetAlias[]>((resolve, reject) => {
            const collected: AssetAlias[] = []
            const request = transaction
                .objectStore('assetAliases')
                .index('byGenerationKindKey')
                .openCursor(range)
            request.onerror = () => reject(request.error)
            request.onsuccess = () => {
                const cursor = request.result
                if (!cursor) {
                    resolve(collected)
                    return
                }
                const [recordGeneration, recordKind] = cursor.key as [string, string, string]
                if (
                    recordGeneration !== generation
                    || (input.kind !== undefined && recordKind !== input.kind)
                ) {
                    resolve(collected)
                    return
                }
                collected.push((cursor.value as StoredRecord<AssetAlias>).value)
                if (collected.length > input.limit) {
                    resolve(collected)
                    return
                }
                cursor.continue()
            }
        })
        await transactionDone(transaction)
        for (const alias of items) validateAssetAlias(alias)
        const clonedItems = items.map((alias) => structuredClone(alias))
        if (clonedItems.length <= input.limit) return { revision, items: clonedItems }
        const pageItems = clonedItems.slice(0, input.limit)
        const last = pageItems[pageItems.length - 1]
        return {
            revision,
            items: pageItems,
            nextCursor: JSON.stringify([last.kind, last.key]),
        }
    }

    private async readAssetRepositoryAuthorityFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
    ): Promise<Versioned<AssetRepositoryAuthorityState>> {
        const record = (await requestResult(
            transaction.objectStore('assetRepositoryAuthority').get(generation),
        )) as StoredRecord<AssetRepositoryAuthorityState> | undefined
        await transactionDone(transaction)
        if (!record) return { revision, value: { format: 'legacy' } }
        if (record.generation !== generation) {
            throw new Error('Persistent asset repository authority marker generation is invalid')
        }
        return {
            revision,
            value: parseAssetRepositoryAuthorityState(record.value),
        }
    }

    private async readAssetOwnerHeadFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        owner: AssetOwnerLocator,
    ): Promise<Versioned<AssetOwnerHead> | null> {
        const ownerKey = assetOwnerLocatorKey(owner)
        const record = (await requestResult(
            transaction.objectStore('assetOwnerHeads').get(
                this.assetOwnerHeadKey(generation, ownerKey),
            ),
        )) as StoredRecord<AssetOwnerHead> | undefined
        await transactionDone(transaction)
        if (!record) return null
        if (record.generation !== generation) {
            throw new TypeError('Asset owner head stored generation does not match its lookup key')
        }
        if (assetOwnerLocatorKey(record.value.owner) !== ownerKey) {
            throw new TypeError('Asset owner head stored locator does not match its lookup key')
        }
        validateAssetOwnerHead(record.value)
        return { revision, value: structuredClone(record.value) }
    }

    private async replaceChangedOwnerHeads(
        transaction: IDBTransaction,
        generation: string,
        input: WorkingSetCommit,
        retained: AssetOwnerHead[],
    ): Promise<void> {
        const changedCharacters = new Set(commitCharacterParents(input).keys())
        if (input.deleteCharacterId) changedCharacters.add(input.deleteCharacterId)
        const store = transaction.objectStore('assetOwnerHeads')
        if (input.root) {
            this.deleteAssetOwnerHeadKind(store, generation, 'root-module-assets')
            this.deleteAssetOwnerHeadKind(
                store,
                generation,
                'persona-embedded-module-assets',
            )
        }
        for (const characterId of changedCharacters) {
            store.delete(
                this.assetOwnerHeadKey(
                    generation,
                    `character-additional-assets:${characterId}`,
                ),
            )
        }
        for (const head of [...retained, ...(input.assetOwnerHeads ?? [])]) {
            this.putAssetOwnerHead(transaction, generation, head)
        }
    }

    private async retainedCommitOwnerHeads(
        transaction: IDBTransaction,
        generation: string,
        input: WorkingSetCommit,
    ): Promise<AssetOwnerHead[]> {
        const characterParents = commitCharacterParents(input)
        if (!input.root && characterParents.size === 0) return []
        const store = transaction.objectStore('assetOwnerHeads')
        const records: StoredRecord<AssetOwnerHead>[] = []
        if (input.root) {
            for (const kind of [
                'root-module-assets',
                'persona-embedded-module-assets',
            ] as const) {
                const prefix = `${generation}:asset-owner-head:${kind}:`
                records.push(...await requestResult(
                    store.getAll(this.keyRangeFactory.bound(prefix, `${prefix}\uffff`)),
                ) as StoredRecord<AssetOwnerHead>[])
            }
        }
        for (const characterId of characterParents.keys()) {
            const ownerKey = `character-additional-assets:${characterId}`
            const record = await requestResult(
                store.get(this.assetOwnerHeadKey(generation, ownerKey)),
            ) as StoredRecord<AssetOwnerHead> | undefined
            if (record) records.push(record)
        }

        let oldRoot: PersistentRoot | undefined
        if (input.root) {
            const record = await requestResult(
                transaction.objectStore('root').get(generation),
            ) as StoredRecord<PersistentRoot> | undefined
            if (!record) throw new Error('Persistent active generation root is missing')
            if (record.generation !== generation) {
                throw new TypeError('Persistent root generation does not match its lookup key')
            }
            oldRoot = record.value
        }

        const retained: AssetOwnerHead[] = []
        for (const record of records) {
            if (record.generation !== generation) {
                throw new TypeError('Asset owner head stored generation does not match its lookup key')
            }
            validateAssetOwnerHead(record.value)
            const originalOwner = record.value.owner
            if (record.key !== this.assetOwnerHeadKey(
                generation,
                assetOwnerLocatorKey(originalOwner),
            )) {
                throw new TypeError('Asset owner head stored locator does not match its lookup key')
            }
            let owner = originalOwner
            let replacementTuple: ReplacementOwnerTuple | null
            if (owner.kind === 'character-additional-assets') {
                if (input.deleteCharacterId === owner.characterId) continue
                replacementTuple = replacementOwnerTupleFromParent(
                    characterParents.get(owner.characterId),
                    'additionalAssets',
                )
            } else {
                const property = owner.kind === 'root-module-assets' ? 'modules' : 'personas'
                const index = retainedModuleIndex(
                    oldRoot!,
                    input.root!,
                    property,
                    owner.index,
                    owner.kind === 'persona-embedded-module-assets',
                )
                if (index === null) continue
                owner = { ...owner, index }
                replacementTuple = owner.kind === 'root-module-assets'
                    ? replacementOwnerTupleFromParent(input.root!.modules?.[index], 'assets')
                    : replacementOwnerTupleFromParent(
                        input.root!.personas?.[index]?.embeddedModule,
                        'assets',
                    )
            }
            const sourceTuple = await this.readReplacementOwnerTuple(
                transaction,
                generation,
                oldRoot ?? ({} as PersistentRoot),
                originalOwner,
            )
            if (replacementOwnerTuplesEqual(sourceTuple, replacementTuple)) {
                retained.push(structuredClone({ ...record.value, owner }))
            }
        }
        return retained
    }

    private putAssetOwnerHead(
        transaction: IDBTransaction,
        generation: string,
        head: AssetOwnerHead,
    ): void {
        const ownerKey = assetOwnerLocatorKey(head.owner)
        transaction.objectStore('assetOwnerHeads').put({
            key: this.assetOwnerHeadKey(generation, ownerKey),
            generation,
            value: structuredClone(head),
        } satisfies StoredRecord<AssetOwnerHead>)
    }

    private putAssetRepositoryAuthority(
        transaction: IDBTransaction,
        generation: string,
        authority: AssetRepositoryAuthorityState,
    ): void {
        transaction.objectStore('assetRepositoryAuthority').put({
            key: generation,
            generation,
            value: structuredClone(authority),
        } satisfies StoredRecord<AssetRepositoryAuthorityState>)
    }

    private deleteAssetOwnerHeadKind(
        store: IDBObjectStore,
        generation: string,
        kind: 'root-module-assets' | 'persona-embedded-module-assets',
    ): void {
        const prefix = `${generation}:asset-owner-head:${kind}:`
        store.delete(this.keyRangeFactory.bound(prefix, `${prefix}\uffff`))
    }

    private async queryPluginStorageFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
    ): Promise<PluginStorageCatalog> {
        const records = (await requestResult(
            transaction.objectStore('pluginStorageMetadata').index('byGeneration').getAll(generation),
        )) as StoredPluginStorageMetadata[]
        await transactionDone(transaction)
        return {
            revision,
            items: records
                .sort(comparePluginStorageRecords)
                .map(({ owner, storageKey: key, byteSize }) => ({ owner, key, byteSize })),
        }
    }

    private async readPluginStorageFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        owner: string,
        key: string,
    ): Promise<Versioned<unknown> | null> {
        const record = (await requestResult(
            transaction
                .objectStore('pluginStorage')
                .get(this.pluginStorageKey(generation, owner, key)),
        )) as StoredPluginStorage | undefined
        await transactionDone(transaction)
        return record ? { revision, value: record.value } : null
    }

    private async queryPresetsFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
    ): Promise<PresetCatalog> {
        const records = (await requestResult(
            transaction.objectStore('presets').index('byGenerationConfigured').getAll(
                this.keyRangeFactory.bound(
                    [generation, 0],
                    [generation, MAX_INDEX_VALUE],
                ),
            ),
        )) as StoredRecord<StoredPreset>[]
        await transactionDone(transaction)
        return { revision, items: records.map((record) => record.value.summary) }
    }

    private async readPresetFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        id: string,
    ): Promise<Versioned<botPreset> | null> {
        const record = (await requestResult(
            transaction.objectStore('presets').get(this.presetKey(generation, id)),
        )) as StoredRecord<StoredPreset> | undefined
        await transactionDone(transaction)
        return record ? { revision, value: record.value.preset } : null
    }

    private async queryCharactersFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        input: CharacterQuery,
    ): Promise<CharacterPage> {
        const index = transaction.objectStore('catalog').index(
            input.order === 'configured' ? 'byGenerationConfigured' : 'byGenerationRecent',
        )
        const search = input.search?.trim().toLocaleLowerCase()
        const range =
            input.order === 'configured'
                ? this.keyRangeFactory.bound([generation, 0], [generation, MAX_INDEX_VALUE])
                : this.keyRangeFactory.bound(
                      [generation, -MAX_INDEX_VALUE, 0],
                      [generation, 0, MAX_INDEX_VALUE],
                  )
        const result = await cursorPage<CharacterSummary>(
            index,
            range,
            input,
            (item) =>
                item.trashed === input.trash &&
                (!search || item.name.toLocaleLowerCase().includes(search)),
        )
        await transactionDone(transaction)
        return { revision, ...result }
    }

    private async readCharacterSummaryFromTransaction(
        transaction: IDBTransaction,
        generation: string,
        id: string,
    ): Promise<CharacterSummary | null> {
        const record = (await requestResult(
            transaction.objectStore('catalog').get(this.characterKey(generation, id)),
        )) as StoredRecord<CharacterSummary> | undefined
        await transactionDone(transaction)
        return record ? record.value : null
    }

    private async readCharacterFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        id: string,
    ): Promise<Versioned<CharacterDetail> | null> {
        const record = (await requestResult(
            transaction.objectStore('characters').get(this.characterKey(generation, id)),
        )) as StoredRecord<CharacterDetail> | undefined
        await transactionDone(transaction)
        return record ? { revision, value: record.value } : null
    }

    private async queryConversationsFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        input: ConversationQuery,
    ): Promise<ConversationPage> {
        const index = transaction.objectStore('conversations').index(
            input.order === 'configured'
                ? 'byGenerationCharacterConfigured'
                : 'byGenerationCharacterRecent',
        )
        const prefix = [generation, input.characterId]
        const range =
            input.order === 'configured'
                ? this.keyRangeFactory.bound([...prefix, 0], [...prefix, MAX_INDEX_VALUE])
                : this.keyRangeFactory.bound(
                      [...prefix, -MAX_INDEX_VALUE, 0],
                      [...prefix, 0, MAX_INDEX_VALUE],
                  )
        const result = await cursorPage<StoredConversation>(index, range, input, () => true)
        await transactionDone(transaction)
        return {
            revision,
            items: result.items.map((item) => ({
                ...item.summary,
                folderId: item.detail.folderId,
                bindedPersona: item.detail.bindedPersona,
                ...(item.detail.fmIndex === undefined ? {} : { fmIndex: item.detail.fmIndex }),
            })),
            nextCursor: result.nextCursor,
        }
    }

    private async readConversationFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<Chat> | null> {
        const record = (await requestResult(
            transaction
                .objectStore('conversations')
                .get(this.conversationKey(generation, characterId, conversationId)),
        )) as StoredRecord<StoredConversation> | undefined
        if (!record) {
            await transactionDone(transaction)
            return null
        }
        const message = await this.readMessagesFromTransaction(
            transaction,
            generation,
            characterId,
            conversationId,
        )
        await transactionDone(transaction)
        return { revision, value: { ...record.value.detail, message } }
    }

    private async readConversationMetadataFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<PersistentConversationMetadata> | null> {
        const record = (await requestResult(
            transaction
                .objectStore('conversations')
                .get(
                    this.conversationKey(
                        generation,
                        characterId,
                        conversationId,
                    ),
                ),
        )) as StoredRecord<StoredConversation> | undefined
        await transactionDone(transaction)
        if (!record) return null
        const totalMessages = record.value.summary.messageCount
        if (!Number.isSafeInteger(totalMessages) || totalMessages < 0) {
            throw new TypeError(
                'Persistent conversation metadata message count must be a nonnegative safe integer',
            )
        }
        const detail = record.value.detail as Omit<Chat, 'message'> & {
            message?: unknown
        }
        const { message: _message, ...conversation } = detail
        return {
            revision,
            value: {
                characterId,
                conversationId,
                conversation,
                totalMessages,
            },
        }
    }

    private async readConversationWindowFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null> {
        const conversation = (await requestResult(
            transaction
                .objectStore('conversations')
                .get(this.conversationKey(generation, input.characterId, input.conversationId)),
        )) as StoredRecord<StoredConversation> | undefined
        if (!conversation) {
            await transactionDone(transaction)
            return null
        }
        const totalMessages = conversation.value.summary.messageCount
        let startIndex: number
        let endIndex: number
        let anchorPage: StoredMessagePage | undefined
        if (input.startIndex !== undefined) {
            startIndex = Math.min(totalMessages, input.startIndex)
            endIndex = Math.min(totalMessages, startIndex + input.limit!)
        } else if (input.anchorMessageId !== undefined) {
            const anchor = await this.findMessage(
                transaction,
                generation,
                input.characterId,
                input.conversationId,
                input.anchorMessageId,
                totalMessages,
                input.anchorOccurrence ?? 'first',
            )
            if (!anchor) {
                await transactionDone(transaction)
                return null
            }
            anchorPage = anchor.page
            startIndex = Math.max(0, anchor.index - Math.max(0, input.before ?? 0))
            endIndex = Math.min(totalMessages, anchor.index + Math.max(0, input.after ?? 0) + 1)
        } else {
            endIndex = totalMessages
            startIndex = Math.max(0, endIndex - Math.max(0, input.limit ?? MESSAGE_PAGE_SIZE))
        }
        const messages = await this.readMessageRange(
            transaction,
            generation,
            input.characterId,
            input.conversationId,
            startIndex,
            endIndex,
            anchorPage,
        )
        await transactionDone(transaction)
        return {
            revision,
            value: {
                characterId: input.characterId,
                conversationId: input.conversationId,
                messages,
                startIndex,
                endIndex,
                totalMessages,
                hasMoreBefore: startIndex > 0,
                hasMoreAfter: endIndex < totalMessages,
            },
        }
    }

    private copyGeneration(
        store: IDBObjectStore,
        sourceGeneration: string,
        targetGeneration: string,
    ): Promise<void> {
        return new Promise((resolve, reject) => {
            const request = store.index('byGeneration').openCursor(this.keyRangeFactory.only(sourceGeneration))
            request.onerror = () => reject(request.error)
            request.onsuccess = () => {
                try {
                    const cursor = request.result
                    if (!cursor) {
                        resolve()
                        return
                    }
                    const record = cursor.value as StoredRecord<unknown>
                    const copiedRecord: StoredRecord<unknown> & Record<string, unknown> = {
                        ...record,
                        key: `${targetGeneration}${record.key.slice(sourceGeneration.length)}`,
                        generation: targetGeneration,
                    }
                    if (store.name === 'messageOccurrences') {
                        copiedRecord.lookupKeys = this.retargetMessageOccurrenceLookupKeys(
                            record as unknown as StoredMessageOccurrencePage,
                            sourceGeneration,
                            targetGeneration,
                        )
                    }
                    store.put(copiedRecord)
                    cursor.continue()
                } catch (error) {
                    reject(error)
                    try {
                        store.transaction.abort()
                    } catch {
                        // Preserve the original cursor validation or write error.
                    }
                    return
                }
            }
        })
    }

    private async ensureWritableGeneration(
        transaction: IDBTransaction,
        sourceGeneration: string,
        revision: DataRevision,
    ): Promise<string> {
        if (!(await this.generationIsLeased(transaction, sourceGeneration))) {
            return sourceGeneration
        }
        const targetGeneration = this.generationFor(revision)
        const root = (await requestResult(
            transaction.objectStore('root').get(sourceGeneration),
        )) as StoredRecord<PersistentRoot> | undefined
        if (!root) throw new Error('Persistent active generation root is missing')
        transaction.objectStore('root').put({
            ...root,
            key: targetGeneration,
            generation: targetGeneration,
        })
        for (const storeName of INDEXED_GENERATION_STORE_NAMES) {
            await this.copyGeneration(
                transaction.objectStore(storeName),
                sourceGeneration,
                targetGeneration,
            )
        }
        return targetGeneration
    }

    private async preserveRepositoriesForReplacement(
        transaction: IDBTransaction,
        sourceGeneration: string,
        targetGeneration: string,
        replacementDatabase: Database,
    ): Promise<void> {
        const sourceRootRecord = (await requestResult(
            transaction.objectStore('root').get(sourceGeneration),
        )) as StoredRecord<PersistentRoot> | undefined
        if (!sourceRootRecord) throw new Error('Persistent active generation root is missing')
        if (sourceRootRecord.generation !== sourceGeneration) {
            throw new TypeError('Persistent root generation does not match its lookup key')
        }

        const assetAuthorityRecord = (await requestResult(
            transaction.objectStore('assetRepositoryAuthority').get(sourceGeneration),
        )) as StoredRecord<AssetRepositoryAuthorityState> | undefined
        if (assetAuthorityRecord && assetAuthorityRecord.generation !== sourceGeneration) {
            throw new TypeError(
                'Persistent asset repository authority marker generation is invalid',
            )
        }
        const assetAuthority = assetAuthorityRecord
            ? parseAssetRepositoryAuthorityState(assetAuthorityRecord.value)
            : { format: 'legacy' as const }
        if (assetAuthority.format === 'preparing') {
            throw new Error('Active asset repository generation cannot be preparing')
        }
        await this.copyGeneration(
            transaction.objectStore('assetAliases'),
            sourceGeneration,
            targetGeneration,
        )
        this.putAssetRepositoryAuthority(transaction, targetGeneration, assetAuthority)

        const ownerHeadRecords = (await requestResult(
            transaction.objectStore('assetOwnerHeads').index('byGeneration').getAll(sourceGeneration),
        )) as StoredRecord<AssetOwnerHead>[]
        for (const record of ownerHeadRecords) {
            if (record.generation !== sourceGeneration) {
                throw new TypeError('Asset owner head stored generation does not match its index')
            }
            validateAssetOwnerHead(record.value)
            const sourceTuple = await this.readReplacementOwnerTuple(
                transaction,
                sourceGeneration,
                sourceRootRecord.value,
                record.value.owner,
            )
            const replacementTuple = replacementOwnerTupleFromDatabase(
                replacementDatabase,
                record.value.owner,
            )
            if (replacementOwnerTuplesEqual(sourceTuple, replacementTuple)) {
                this.putAssetOwnerHead(transaction, targetGeneration, record.value)
            }
        }
    }

    private async readReplacementOwnerTuple(
        transaction: IDBTransaction,
        generation: string,
        root: PersistentRoot,
        owner: AssetOwnerLocator,
    ): Promise<ReplacementOwnerTuple | null> {
        if (owner.kind === 'character-additional-assets') {
            const record = (await requestResult(
                transaction.objectStore('characters').get(
                    this.characterKey(generation, owner.characterId),
                ),
            )) as StoredRecord<CharacterDetail> | undefined
            if (record && record.generation !== generation) {
                throw new TypeError('Character stored generation does not match its lookup key')
            }
            return replacementOwnerTupleFromParent(record?.value, 'additionalAssets')
        }
        if (owner.kind === 'root-module-assets') {
            return replacementOwnerTupleFromParent(root.modules?.[owner.index], 'assets')
        }
        return replacementOwnerTupleFromParent(
            root.personas?.[owner.index]?.embeddedModule,
            'assets',
        )
    }

    private async generationIsLeased(
        transaction: IDBTransaction,
        generation: string,
    ): Promise<boolean> {
        const records = await this.readMetaRecordsByPrefix<SnapshotLeaseTarget>(
            transaction.objectStore('meta'),
            'snapshotLease:',
        )
        return records.some((record) => this.snapshotLeaseTarget(record).generation === generation)
    }

    private async stageDatabase(
        transaction: IDBTransaction,
        databaseValue: Database,
        generation: string,
        assetAliases: AssetAlias[],
    ): Promise<void> {
        for (const alias of assetAliases) validateAssetAlias(alias)
        const ids = new Set<string>()
        const conversationIds = new Set<string>()
        const {
            characters,
            botPresets,
            pluginCustomStorage,
            pluginStorageMeta,
            ...root
        } = databaseValue as Database & { pluginStorageMeta?: PluginStorageMeta }
        this.putRoot(transaction, generation, root)
        this.putAssetRepositoryAuthority(transaction, generation, { format: 'legacy' })
        this.writePresetRows(transaction, generation, botPresets ?? [])
        this.writePluginStorageRows(
            transaction,
            generation,
            pluginCustomStorage ?? {},
            pluginStorageMeta,
        )
        for (const alias of assetAliases) {
            transaction.objectStore('assetAliases').put({
                key: this.assetAliasKey(generation, alias.kind, alias.key),
                generation,
                value: structuredClone(alias),
            } satisfies StoredRecord<AssetAlias>)
        }
        for (let index = 0; index < characters.length; index++) {
            const character = characters[index]
            if (!character.chaId || ids.has(character.chaId)) {
                throw new Error('Persistent data import requires unique character IDs')
            }
            ids.add(character.chaId)
            const { chats, ...detail } = character
            this.putCharacterRecords(transaction, generation, detail, index, chats.length)
            for (let conversationIndex = 0; conversationIndex < chats.length; conversationIndex++) {
                const conversation = chats[conversationIndex]
                // Stored keys join ids with ':', so the composite must stay unique across characters.
                const compositeId = `${character.chaId}:${conversation.id}`
                if (!conversation.id || conversationIds.has(compositeId)) {
                    throw new Error(`Character ${character.chaId} requires unique conversation IDs`)
                }
                conversationIds.add(compositeId)
                this.putConversation(
                    transaction,
                    generation,
                    character.chaId,
                    conversation,
                    conversationIndex,
                )
            }
        }
    }

    private async deleteGenerationFromTransaction(
        transaction: IDBTransaction,
        generation: string,
    ): Promise<void> {
        transaction.objectStore('root').delete(generation)
        for (const storeName of INDEXED_GENERATION_STORE_NAMES) {
            await this.deleteIndexRange(
                transaction.objectStore(storeName).index('byGeneration'),
                this.keyRangeFactory.only(generation),
            )
        }
    }

    private reportCleanupError(generation: string, error: unknown): void {
        try {
            this.onCleanupError(generation, error)
        } catch (reportError) {
            reportGenerationCleanupError(generation, reportError)
        }
    }

    private snapshotLeaseKey(lease: string): string {
        return `snapshotLease:${lease}`
    }

    private async releaseSnapshotLease(lease: string): Promise<void> {
        const transaction = this.requireDatabase().transaction([...STORE_NAMES], 'readwrite')
        const meta = transaction.objectStore('meta')
        const record = (await requestResult(
            meta.get(this.snapshotLeaseKey(lease)),
        )) as SnapshotLeaseRecord | undefined
        if (!record) {
            await transactionDone(transaction)
            return
        }
        const target = this.snapshotLeaseTarget(record)
        await requestResult(meta.delete(record.key))
        const active = await this.readActive(transaction)
        if (
            target.generation !== active.generation &&
            !(await this.generationIsLeased(transaction, target.generation))
        ) {
            await this.deleteGenerationFromTransaction(transaction, target.generation)
        }
        await transactionDone(transaction)
    }

    /** Removes inactive generations and generations retained only by expired leases. */
    private async sweepTemporaryGenerations(): Promise<void> {
        const database = this.requireDatabase()
        const cutoff = Date.now() - SNAPSHOT_LEASE_TTL_MS
        const transaction = database.transaction([...STORE_NAMES], 'readwrite')
        const done = transactionDone(transaction)
        void done.catch(() => {})
        try {
            const meta = transaction.objectStore('meta')
            const leaseRecords = await this.readMetaRecordsByPrefix<SnapshotLeaseTarget>(
                meta,
                'snapshotLease:',
            )
            const leased = new Set<string>()
            const reclaimCandidates = new Set<string>()
            for (const record of leaseRecords) {
                const target = this.snapshotLeaseTarget(record)
                if (typeof record.createdAt !== 'number' || !Number.isFinite(record.createdAt)) {
                    throw new TypeError('Snapshot lease createdAt is invalid')
                }
                const lease = record.key.slice('snapshotLease:'.length)
                const live = activeSnapshotLeases.has(lease) || record.createdAt >= cutoff
                if (live) leased.add(target.generation)
                else {
                    reclaimCandidates.add(target.generation)
                    meta.delete(record.key)
                }
            }
            const active = await this.readActive(transaction)
            const rootKeys = await requestResult(transaction.objectStore('root').getAllKeys())
            for (const key of rootKeys) {
                if (typeof key === 'string' && /^revision-(0|[1-9]\d*)$/.test(key)) {
                    reclaimCandidates.add(key)
                }
            }

            for (const generation of reclaimCandidates) {
                if (generation === active.generation || leased.has(generation)) continue
                await this.deleteGenerationFromTransaction(transaction, generation)
            }
            await done
        } catch (error) {
            try {
                transaction.abort()
            } catch {}
            try {
                await done
            } catch {}
            throw error
        }
    }

    private snapshotLeaseTarget(record: SnapshotLeaseRecord): SnapshotLeaseTarget {
        const target = record.value as unknown
        if (
            !target
            || typeof target !== 'object'
            || typeof (target as { generation?: unknown }).generation !== 'string'
            || !Number.isSafeInteger((target as { revision?: unknown }).revision)
            || ((target as { revision: number }).revision < 0)
        ) {
            throw new TypeError('Snapshot lease target is invalid')
        }
        return target as SnapshotLeaseTarget
    }

    private readMetaRecordsByPrefix<T>(
        store: IDBObjectStore,
        prefix: string,
    ): Promise<Array<{ key: string; value: T; createdAt?: number }>> {
        return new Promise((resolve, reject) => {
            const records: Array<{ key: string; value: T; createdAt?: number }> = []
            const request = store.openCursor(
                this.keyRangeFactory.bound(prefix, `${prefix}\uffff`),
            )
            request.onerror = () => reject(request.error)
            request.onsuccess = () => {
                const cursor = request.result
                if (!cursor) {
                    resolve(records)
                    return
                }
                records.push(cursor.value as { key: string; value: T; createdAt?: number })
                cursor.continue()
            }
        })
    }

    private async applyConversationMutation(
        transaction: IDBTransaction,
        generation: string,
        mutation: ConversationMutation,
    ): Promise<void> {
        const key = this.conversationKey(generation, mutation.characterId, mutation.conversationId)
        if (mutation.type === 'delete') {
            transaction.objectStore('conversations').delete(key)
            await this.deleteConversationPages(
                transaction,
                generation,
                mutation.characterId,
                mutation.conversationId,
                0,
            )
            await this.refreshCharacterSummary(transaction, generation, mutation.characterId)
            return
        }

        const existing = (await requestResult(
            transaction.objectStore('conversations').get(key),
        )) as StoredRecord<StoredConversation> | undefined
        if (existing && mutation.configuredIndex !== undefined) {
            throw new Error(`Conversation ${mutation.conversationId} already exists`)
        }
        if (!existing && !mutation.conversation) {
            throw new Error(`Conversation ${mutation.conversationId} does not exist`)
        }
        if (!existing) {
            const conversationCount = await this.conversationCount(
                transaction,
                generation,
                mutation.characterId,
            )
            const position = Math.min(
                conversationCount,
                Math.max(0, mutation.configuredIndex ?? conversationCount),
            )
            const index = transaction.objectStore('conversations').index(
                'byGenerationCharacterConfigured',
            )
            const appending = position === conversationCount
            const cursorRequest = index.openCursor(this.keyRangeFactory.bound(
                [generation, mutation.characterId, 0],
                [generation, mutation.characterId, MAX_INDEX_VALUE],
            ), appending ? 'prev' : 'next')
            let cursor = await requestResult(cursorRequest)
            if (cursor && !appending && position > 0) {
                cursor.advance(position)
                cursor = await requestResult(cursorRequest)
            }
            // Deletion leaves order-key gaps. The requested position is an ordinal,
            // while appending must follow the highest surviving order key.
            const configuredIndex = cursor
                ? (cursor.value as StoredRecord<StoredConversation>).value.summary.configuredIndex
                    + (appending ? 1 : 0)
                : 0
            if (!appending) {
                const records = await requestResult(index.getAll(this.keyRangeFactory.bound(
                    [generation, mutation.characterId, configuredIndex],
                    [generation, mutation.characterId, MAX_INDEX_VALUE],
                ))) as Array<StoredRecord<StoredConversation>>
                for (const record of records) {
                    const nextConfiguredIndex = record.value.summary.configuredIndex + 1
                    transaction.objectStore('conversations').put({
                        ...record,
                        configuredIndex: nextConfiguredIndex,
                        value: {
                            ...record.value,
                            summary: {
                                ...record.value.summary,
                                configuredIndex: nextConfiguredIndex,
                            },
                        },
                    })
                }
            }
            this.putConversation(
                transaction,
                generation,
                mutation.characterId,
                {
                    ...mutation.conversation!,
                    id: mutation.conversationId,
                    message: mutation.messages,
                },
                configuredIndex,
            )
            await this.refreshCharacterSummary(transaction, generation, mutation.characterId)
            return
        }

        const oldMessageCount = existing.value.summary.messageCount
        const start = Math.max(0, Math.min(oldMessageCount, mutation.start))
        const deleteCount = Math.min(Math.max(0, mutation.deleteCount), oldMessageCount - start)
        const detail = mutation.conversation ?? existing.value.detail
        const delta = mutation.messages.length - deleteCount

        if (deleteCount > 0 || mutation.messages.length > 0) {
            const startPage = Math.floor(start / MESSAGE_PAGE_SIZE)
            const lastPage = Math.max(startPage, Math.ceil(oldMessageCount / MESSAGE_PAGE_SIZE) - 1)
            const endPage =
                delta === 0
                    ? Math.floor((start + Math.max(deleteCount, 1) - 1) / MESSAGE_PAGE_SIZE)
                    : lastPage
            const pages = await this.readMessagePages(
                transaction,
                generation,
                mutation.characterId,
                mutation.conversationId,
                startPage,
                endPage,
            )
            const firstPageStart = startPage * MESSAGE_PAGE_SIZE
            const messages = pages.flatMap((page) => page.value)
            messages.splice(start - firstPageStart, deleteCount, ...mutation.messages)

            if (delta !== 0) {
                await this.deleteConversationPages(
                    transaction,
                    generation,
                    mutation.characterId,
                    mutation.conversationId,
                    startPage,
                )
            }
            for (let offset = 0; offset < messages.length; offset += MESSAGE_PAGE_SIZE) {
                this.putMessagePage(
                    transaction,
                    generation,
                    mutation.characterId,
                    mutation.conversationId,
                    startPage + offset / MESSAGE_PAGE_SIZE,
                    messages.slice(offset, offset + MESSAGE_PAGE_SIZE),
                )
            }
        }

        const summary: ConversationSummary = {
            ...existing.value.summary,
            name: detail.name,
            recentAt: detail.lastDate ?? existing.value.summary.recentAt,
            messageCount: oldMessageCount + delta,
        }
        this.putConversationRecord(transaction, generation, summary, detail)
        await this.refreshCharacterSummary(transaction, generation, mutation.characterId)
    }

    private async putCharacter(
        transaction: IDBTransaction,
        generation: string,
        detail: CharacterDetail,
    ): Promise<void> {
        const existing = (await requestResult(
            transaction.objectStore('catalog').get(this.characterKey(generation, detail.chaId)),
        )) as StoredRecord<CharacterSummary> | undefined
        const configuredIndex =
            existing?.value.configuredIndex ??
            (await requestResult(
                transaction.objectStore('catalog').index('byGeneration').count(generation),
            ))
        const conversationCount = await this.conversationCount(transaction, generation, detail.chaId)
        this.putCharacterRecords(transaction, generation, detail, configuredIndex, conversationCount)
    }

    private validateCharacterInput(
        character: Database['characters'][number],
        context: string,
    ): void {
        if (!character.chaId) {
            throw new Error(`${context} requires a nonempty character ID`)
        }
        const conversationIds = new Set<string>()
        for (const conversation of character.chats) {
            if (!conversation.id || conversationIds.has(conversation.id)) {
                throw new Error(`${context} requires unique, nonempty chat IDs`)
            }
            conversationIds.add(conversation.id)
        }
    }

    private async validateCharacterDetails(
        transaction: IDBTransaction,
        generation: string,
        details: readonly CharacterDetail[],
        deleteCharacterId?: string,
    ): Promise<void> {
        const ids = new Set<string>()
        for (const detail of details) {
            if (!detail.chaId) {
                throw new Error('Batch character detail mutation requires nonempty character IDs')
            }
            if (detail.chaId === deleteCharacterId || ids.has(detail.chaId)) {
                throw new Error('Batch character detail mutation requires unique retained character IDs')
            }
            ids.add(detail.chaId)
            const existing = await requestResult(
                transaction.objectStore('catalog').get(
                    this.characterKey(generation, detail.chaId),
                ),
            )
            if (!existing) throw new Error(`Character ${detail.chaId} does not exist`)
        }
    }

    private async addCharacter(
        transaction: IDBTransaction,
        generation: string,
        character: Database['characters'][number],
    ): Promise<void> {
        const existing = await requestResult(
            transaction.objectStore('catalog').get(
                this.characterKey(generation, character.chaId),
            ),
        )
        if (existing) throw new Error(`Character ${character.chaId} already exists`)
        await this.replaceCharacter(transaction, generation, character)
    }

    private async replaceCharacter(
        transaction: IDBTransaction,
        generation: string,
        character: Database['characters'][number],
    ): Promise<void> {
        const key = this.characterKey(generation, character.chaId)
        const existing = (await requestResult(
            transaction.objectStore('catalog').get(key),
        )) as StoredRecord<CharacterSummary> | undefined
        const configuredIndex =
            existing?.value.configuredIndex ??
            (await this.nextCharacterConfiguredIndex(transaction, generation))

        await this.deleteIndexRange(
            transaction.objectStore('conversations').index('byGenerationCharacterConfigured'),
            this.keyRangeFactory.bound(
                [generation, character.chaId, 0],
                [generation, character.chaId, MAX_INDEX_VALUE],
            ),
        )
        await this.deleteIndexRange(
            transaction.objectStore('messagePages').index('byGenerationCharacter'),
            this.keyRangeFactory.only([generation, character.chaId]),
        )
        await this.deleteIndexRange(
            transaction.objectStore('messageOccurrences').index('byGenerationCharacter'),
            this.keyRangeFactory.only([generation, character.chaId]),
        )

        const { chats, ...detail } = character
        this.putCharacterRecords(
            transaction,
            generation,
            detail,
            configuredIndex,
            chats.length,
        )
        for (let index = 0; index < chats.length; index++) {
            this.putConversation(transaction, generation, character.chaId, chats[index], index)
        }
    }

    private async nextCharacterConfiguredIndex(
        transaction: IDBTransaction,
        generation: string,
    ): Promise<number> {
        const cursor = await requestResult(
            transaction
                .objectStore('catalog')
                .index('byGenerationConfigured')
                .openCursor(
                    this.keyRangeFactory.bound(
                        [generation, 0],
                        [generation, MAX_INDEX_VALUE],
                    ),
                    'prev',
                ),
        )
        if (!cursor) return 0
        return (cursor.value as StoredRecord<CharacterSummary>).value.configuredIndex + 1
    }

    private putCharacterRecords(
        transaction: IDBTransaction,
        generation: string,
        detail: CharacterDetail,
        configuredIndex: number,
        conversationCount: number,
    ): void {
        const summary: CharacterSummary = {
            id: detail.chaId,
            name: detail.name,
            image: detail.image,
            configuredIndex,
            recentAt: detail.lastInteraction ?? 0,
            trashed: detail.trashTime !== undefined,
            conversationCount,
            type: detail.type,
            creatorNotes: detail.creatorNotes,
            trashTime: detail.trashTime,
        }
        transaction.objectStore('catalog').put({
            key: this.characterKey(generation, detail.chaId),
            generation,
            configuredIndex,
            recentSortValue: -summary.recentAt,
            value: summary,
        })
        transaction.objectStore('characters').put({
            key: this.characterKey(generation, detail.chaId),
            generation,
            value: detail,
        })
    }

    private putConversation(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversation: Chat,
        configuredIndex: number,
    ): void {
        const { message, ...detail } = conversation
        const id = conversation.id!
        const summary: ConversationSummary = {
            id,
            characterId,
            name: conversation.name,
            configuredIndex,
            recentAt: conversation.lastDate ?? message.at(-1)?.time ?? 0,
            messageCount: message.length,
        }
        this.putConversationRecord(transaction, generation, summary, detail)
        for (let offset = 0; offset < message.length; offset += MESSAGE_PAGE_SIZE) {
            this.putMessagePage(
                transaction,
                generation,
                characterId,
                id,
                offset / MESSAGE_PAGE_SIZE,
                message.slice(offset, offset + MESSAGE_PAGE_SIZE),
            )
        }
    }

    private putConversationRecord(
        transaction: IDBTransaction,
        generation: string,
        summary: ConversationSummary,
        detail: Omit<Chat, 'message'>,
    ): void {
        transaction.objectStore('conversations').put({
            key: this.conversationKey(generation, summary.characterId, summary.id),
            generation,
            configuredIndex: summary.configuredIndex,
            recentSortValue: -summary.recentAt,
            value: { summary, detail },
        })
    }

    private putMessagePage(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        pageIndex: number,
        messages: Message[],
    ): void {
        transaction.objectStore('messagePages').put({
            key: this.messagePageKey(generation, characterId, conversationId, pageIndex),
            generation,
            characterId,
            conversationId,
            pageIndex,
            value: messages,
        })
        this.putMessageOccurrencePage(
            transaction,
            generation,
            characterId,
            conversationId,
            pageIndex,
            messages,
        )
    }

    private async refreshCharacterSummary(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
    ): Promise<void> {
        const key = this.characterKey(generation, characterId)
        const record = (await requestResult(
            transaction.objectStore('catalog').get(key),
        )) as StoredRecord<CharacterSummary> | undefined
        if (!record) return
        record.value.conversationCount = await this.conversationCount(transaction, generation, characterId)
        transaction.objectStore('catalog').put(record)
    }

    private async deleteCharacter(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
    ): Promise<void> {
        const key = this.characterKey(generation, characterId)
        transaction.objectStore('catalog').delete(key)
        transaction.objectStore('characters').delete(key)
        await this.deleteIndexRange(
            transaction.objectStore('conversations').index('byGenerationCharacterConfigured'),
            this.keyRangeFactory.bound(
                [generation, characterId, 0],
                [generation, characterId, MAX_INDEX_VALUE],
            ),
        )
        await this.deleteIndexRange(
            transaction.objectStore('messagePages').index('byGenerationCharacter'),
            this.keyRangeFactory.only([generation, characterId]),
        )
        await this.deleteIndexRange(
            transaction.objectStore('messageOccurrences').index('byGenerationCharacter'),
            this.keyRangeFactory.only([generation, characterId]),
        )
    }

    private async conversationCount(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
    ): Promise<number> {
        return requestResult(
            transaction
                .objectStore('conversations')
                .index('byGenerationCharacterConfigured')
                .count(
                    this.keyRangeFactory.bound(
                        [generation, characterId, 0],
                        [generation, characterId, MAX_INDEX_VALUE],
                    ),
                ),
        )
    }

    private async readMessagesFromTransaction(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
    ): Promise<Message[]> {
        return (
            await this.readMessagePages(
                transaction,
                generation,
                characterId,
                conversationId,
                0,
                MAX_INDEX_VALUE,
            )
        ).flatMap((record) => record.value)
    }

    private readMessageRange(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        startIndex: number,
        endIndex: number,
        cachedPage?: StoredMessagePage,
    ): Promise<Message[]> {
        if (startIndex >= endIndex) return Promise.resolve([])
        const startPage = Math.floor(startIndex / MESSAGE_PAGE_SIZE)
        const endPage = Math.floor((endIndex - 1) / MESSAGE_PAGE_SIZE)
        const pagesPromise = cachedPage
            ? this.readMessagePagesByKey(
                  transaction,
                  generation,
                  characterId,
                  conversationId,
                  startPage,
                  endPage,
                  cachedPage,
              )
            : this.readMessagePages(
                  transaction,
                  generation,
                  characterId,
                  conversationId,
                  startPage,
                  endPage,
              )
        return pagesPromise.then((pages) => {
            const firstPageStart = startPage * MESSAGE_PAGE_SIZE
            return pages
                .flatMap((record) => record.value)
                .slice(startIndex - firstPageStart, endIndex - firstPageStart)
        })
    }

    private async readMessagePagesByKey(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        startPage: number,
        endPage: number,
        cachedPage: StoredMessagePage,
    ): Promise<StoredMessagePage[]> {
        const pages: StoredMessagePage[] = []
        for (let pageIndex = startPage; pageIndex <= endPage; pageIndex++) {
            if (pageIndex === cachedPage.pageIndex) {
                pages.push(cachedPage)
                continue
            }
            const page = (await requestResult(
                transaction
                    .objectStore('messagePages')
                    .get(this.messagePageKey(generation, characterId, conversationId, pageIndex)),
            )) as StoredMessagePage | undefined
            if (page) pages.push(page)
        }
        return pages
    }

    private readMessagePages(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        startPage: number,
        endPage: number,
    ): Promise<StoredMessagePage[]> {
        if (startPage > endPage) return Promise.resolve([])
        const range = this.keyRangeFactory.bound(
            [generation, characterId, conversationId, startPage],
            [generation, characterId, conversationId, endPage],
        )
        return new Promise((resolve, reject) => {
            const pages: StoredMessagePage[] = []
            const request = transaction
                .objectStore('messagePages')
                .index('byConversationPage')
                .openCursor(range)
            request.onerror = () => reject(request.error)
            request.onsuccess = () => {
                const cursor = request.result
                if (!cursor) {
                    resolve(pages)
                    return
                }
                pages.push(cursor.value as StoredMessagePage)
                cursor.continue()
            }
        })
    }

    private findMessage(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        messageId: string,
        totalMessages: number,
        occurrence: 'first' | 'last',
    ): Promise<{ index: number; page: StoredMessagePage } | null> {
        if (totalMessages === 0) return Promise.resolve(null)
        const lookupKey = this.messageOccurrenceLookupKey(
            generation,
            characterId,
            conversationId,
            messageId,
        )
        return new Promise((resolve, reject) => {
            const request = transaction
                .objectStore('messageOccurrences')
                .index('byLookupKey')
                .openCursor(lookupKey, occurrence === 'last' ? 'prev' : 'next')
            request.onerror = () => reject(request.error)
            request.onsuccess = () => {
                const cursor = request.result
                if (!cursor) {
                    resolve(null)
                    return
                }
                const record = cursor.value as StoredMessageOccurrencePage
                const pageStart = record.pageIndex * MESSAGE_PAGE_SIZE
                if (pageStart >= totalMessages) {
                    reject(new Error('Persistent message occurrence page exceeds conversation bounds'))
                    return
                }
                const pageRequest = transaction.objectStore('messagePages').get(
                    this.messagePageKey(
                        generation,
                        characterId,
                        conversationId,
                        record.pageIndex,
                    ),
                )
                pageRequest.onerror = () => reject(pageRequest.error)
                pageRequest.onsuccess = () => {
                    const page = pageRequest.result as StoredMessagePage | undefined
                    const indexInPage = occurrence === 'last'
                        ? page?.value.findLastIndex((message) => message.chatId === messageId) ?? -1
                        : page?.value.findIndex((message) => message.chatId === messageId) ?? -1
                    if (!page || indexInPage < 0) {
                        reject(new Error('Persistent message occurrence index is inconsistent'))
                        return
                    }
                    resolve({ index: pageStart + indexInPage, page })
                }
            }
        })
    }

    private async deleteConversationPages(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        startPage: number,
    ): Promise<void> {
        await this.deleteIndexRange(
            transaction.objectStore('messagePages').index('byConversationPage'),
            this.keyRangeFactory.bound(
                [generation, characterId, conversationId, startPage],
                [generation, characterId, conversationId, MAX_INDEX_VALUE],
            ),
        )
        await this.deleteIndexRange(
            transaction.objectStore('messageOccurrences').index('byConversationPage'),
            this.keyRangeFactory.bound(
                [generation, characterId, conversationId, startPage],
                [generation, characterId, conversationId, MAX_INDEX_VALUE],
            ),
        )
    }

    private deleteIndexRange(index: IDBIndex, range: IDBKeyRange): Promise<void> {
        return new Promise((resolve, reject) => {
            const request = index.openCursor(range)
            request.onerror = () => reject(request.error)
            request.onsuccess = () => {
                const cursor = request.result
                if (!cursor) {
                    resolve()
                    return
                }
                cursor.delete()
                cursor.continue()
            }
        })
    }

    private async readActive(
        transaction: IDBTransaction,
    ): Promise<{ revision: DataRevision; generation: string }> {
        const store = transaction.objectStore('meta')
        const [revisionRecord, generationRecord] = await Promise.all([
            requestResult(store.get('currentRevision')),
            requestResult(store.get('activeGeneration')),
        ])
        return {
            revision: (revisionRecord as { value: DataRevision }).value,
            generation: (generationRecord as { value: string }).value,
        }
    }

    private setActive(transaction: IDBTransaction, revision: DataRevision, generation: string): void {
        const meta = transaction.objectStore('meta')
        meta.put({ key: 'activeGeneration', value: generation })
        meta.put({ key: 'currentRevision', value: revision })
    }

    private putRoot(
        transaction: IDBTransaction,
        generation: string,
        root: PersistentRoot,
    ): void {
        const {
            characters: _characters,
            botPresets: _botPresets,
            pluginCustomStorage: _pluginCustomStorage,
            ...value
        } = root as Database
        transaction.objectStore('root').put({ key: generation, generation, value })
    }

    private writePluginStorageRows(
        transaction: IDBTransaction,
        generation: string,
        values: Record<string, unknown>,
        meta?: PluginStorageMeta,
    ): void {
        const valueStore = transaction.objectStore('pluginStorage')
        const metadataStore = transaction.objectStore('pluginStorageMetadata')
        for (const [ordinal, storageKey] of Object.keys(values).entries()) {
            const value = values[storageKey]
            const owner = readPluginStorageMetaOwner(meta, storageKey)
            const metadata = {
                key: this.pluginStorageKey(generation, owner, storageKey),
                generation,
                owner,
                storageKey,
                valueType: typeof value === 'string' ? 'string' : 'json',
                byteSize: serializedByteSize(value),
                ordinal,
            } satisfies StoredPluginStorageMetadata
            valueStore.put({
                ...metadata,
                value,
            } satisfies StoredPluginStorage)
            metadataStore.put(metadata)
        }
    }

    private async applyPluginStorageMutation(
        transaction: IDBTransaction,
        generation: string,
        mutation: PluginStorageMutation,
    ): Promise<void> {
        const valueStore = transaction.objectStore('pluginStorage')
        const metadataStore = transaction.objectStore('pluginStorageMetadata')
        if (mutation.type === 'clear') {
            const owned = (await requestResult(
                metadataStore.index('byGeneration').getAll(generation),
            )) as StoredPluginStorageMetadata[]
            for (const record of owned) {
                if (record.owner !== mutation.owner) continue
                valueStore.delete(record.key)
                metadataStore.delete(record.key)
            }
            return
        }
        if (mutation.type === 'delete') {
            const key = this.pluginStorageKey(generation, mutation.owner, mutation.key)
            valueStore.delete(key)
            metadataStore.delete(key)
            return
        }
        const existing = (await requestResult(
            metadataStore.get(this.pluginStorageKey(generation, mutation.owner, mutation.key)),
        )) as StoredPluginStorageMetadata | undefined
        const ordinal = existing?.ordinal ?? await this.nextPluginStorageOrdinal(
            metadataStore,
            generation,
        )
        const metadata = {
            key: this.pluginStorageKey(generation, mutation.owner, mutation.key),
            generation,
            owner: mutation.owner,
            storageKey: mutation.key,
            valueType: typeof mutation.value === 'string' ? 'string' : 'json',
            byteSize: serializedByteSize(mutation.value),
            ordinal,
        } satisfies StoredPluginStorageMetadata
        valueStore.put({
            ...metadata,
            value: mutation.value,
        } satisfies StoredPluginStorage)
        metadataStore.put(metadata)
    }

    private async nextPluginStorageOrdinal(
        metadataStore: IDBObjectStore,
        generation: string,
    ): Promise<number> {
        const cursor = await requestResult(
            metadataStore.index('byGenerationOrdinal').openKeyCursor(
                this.keyRangeFactory.bound(
                    [generation, 0],
                    [generation, MAX_INDEX_VALUE],
                ),
                'prev',
            ),
        )
        const ordinal = cursor
            ? (cursor.key as [string, number])[1]
            : -1
        return ordinal + 1
    }

    private async putPresets(
        transaction: IDBTransaction,
        generation: string,
        presets: botPreset[],
    ): Promise<void> {
        await this.deleteIndexRange(
            transaction.objectStore('presets').index('byGeneration'),
            this.keyRangeFactory.only(generation),
        )
        this.writePresetRows(transaction, generation, presets)
    }

    private writePresetRows(
        transaction: IDBTransaction,
        generation: string,
        presets: botPreset[],
    ): void {
        for (let configuredIndex = 0; configuredIndex < presets.length; configuredIndex++) {
            const id = String(configuredIndex)
            const preset = presets[configuredIndex]
            const summary: PresetSummary = {
                id,
                name: preset.name ?? '',
                image: preset.image,
                configuredIndex,
            }
            transaction.objectStore('presets').put({
                key: this.presetKey(generation, id),
                generation,
                configuredIndex,
                value: { summary, preset },
            })
        }
    }

    private async generationRecords<T>(
        store: IDBObjectStore,
        generation: string,
    ): Promise<StoredRecord<T>[]> {
        return (await requestResult(store.index('byGeneration').getAll(generation))) as StoredRecord<T>[]
    }

    private requireDatabase(): IDBDatabase {
        if (!this.database) throw new Error('Persistent data store is not open')
        return this.database
    }

    private createIndex(
        store: IDBObjectStore,
        name: string,
        keyPath: string | string[],
        options?: IDBIndexParameters,
    ): void {
        if (!store.indexNames.contains(name)) store.createIndex(name, keyPath, options)
    }

    private generationFor(revision: DataRevision): string {
        return `revision-${revision}`
    }

    private characterKey(generation: string, characterId: string): string {
        return `${generation}:character:${characterId}`
    }

    private presetKey(generation: string, id: string): string {
        return `${generation}:preset:${id}`
    }

    private conversationKey(generation: string, characterId: string, conversationId: string): string {
        return `${generation}:conversation:${encodeKeyComponent(characterId)}:${encodeKeyComponent(conversationId)}`
    }

    private messagePageKey(
        generation: string,
        characterId: string,
        conversationId: string,
        pageIndex: number,
    ): string {
        return `${generation}:message-page:${encodeKeyComponent(characterId)}:${encodeKeyComponent(conversationId)}:${pageIndex}`
    }

    private putMessageOccurrencePage(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        pageIndex: number,
        messages: readonly Message[],
    ): void {
        const messageIds = new Set<string>()
        for (const message of messages) {
            if (message.chatId !== undefined) messageIds.add(message.chatId)
        }
        transaction.objectStore('messageOccurrences').put({
            key: `${generation}:message-occurrence-page:${encodeKeyComponent(characterId)}:${encodeKeyComponent(conversationId)}:${String(pageIndex).padStart(16, '0')}`,
            generation,
            characterId,
            conversationId,
            pageIndex,
            lookupKeys: [...messageIds].map((messageId) => this.messageOccurrenceLookupKey(
                generation,
                characterId,
                conversationId,
                messageId,
            )),
        } satisfies StoredMessageOccurrencePage)
    }

    private messageOccurrenceLookupKey(
        generation: string,
        characterId: string,
        conversationId: string,
        messageId: string,
    ): string {
        return JSON.stringify([generation, characterId, conversationId, messageId])
    }

    private retargetMessageOccurrenceLookupKeys(
        record: StoredMessageOccurrencePage,
        sourceGeneration: string,
        targetGeneration: string,
    ): string[] {
        return record.lookupKeys.map((lookupKey) => {
            let locator: unknown
            try {
                locator = JSON.parse(lookupKey)
            } catch {
                throw new TypeError('Persistent message occurrence lookup key is invalid')
            }
            if (
                !Array.isArray(locator)
                || locator.length !== 4
                || locator[0] !== sourceGeneration
                || locator[1] !== record.characterId
                || locator[2] !== record.conversationId
                || typeof locator[3] !== 'string'
            ) {
                throw new TypeError('Persistent message occurrence lookup key does not match its row')
            }
            return this.messageOccurrenceLookupKey(
                targetGeneration,
                record.characterId,
                record.conversationId,
                locator[3],
            )
        })
    }

    private pluginStorageKey(generation: string, owner: string, key: string): string {
        return `${generation}:plugin-storage:${JSON.stringify([owner, key])}`
    }

    private assetAliasKey(generation: string, kind: AssetAlias['kind'], key: string): string {
        return `${generation}:asset-alias:${kind}:${key}`
    }

    private assetOwnerHeadKey(generation: string, ownerKey: string): string {
        return `${generation}:asset-owner-head:${ownerKey}`
    }

}
