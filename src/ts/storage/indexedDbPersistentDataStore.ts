import type { Chat, Database, Message, botPreset } from './database.svelte'
import type {
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
    PersistentDataStore,
    PersistentRevisionLease,
    PersistentRoot,
    PluginStorageCatalog,
    PluginStorageMutation,
    PresetCatalog,
    PresetSummary,
    Versioned,
    WorkingSetCommit,
} from './persistentDataStore'
import { RevisionConflictError, SnapshotReleasedError } from './persistentDataStore'

const DATABASE_VERSION = 6
const MESSAGE_PAGE_SIZE = 128
const SNAPSHOT_LEASE_TTL_MS = 24 * 60 * 60 * 1000
const MAX_INDEX_VALUE = Number.MAX_SAFE_INTEGER
const STORE_NAMES = ['meta', 'root', 'presets', 'catalog', 'characters', 'conversations', 'messagePages', 'pluginStorage', 'pluginStorageMetadata'] as const
const DATA_STORE_NAMES = ['root', 'presets', 'catalog', 'characters', 'conversations', 'messagePages', 'pluginStorage', 'pluginStorageMetadata'] as const
const activeSnapshotGenerations = new Set<string>()

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
    storageKey: string
    byteSize: number
    ordinal: number
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
            this.backfillCharacterSummaries(transaction)
            this.backfillOrderKeys<StoredConversation>(
                transaction.objectStore('conversations'),
                (record) => record.value.summary,
            )
            this.migrateRootRows(transaction)
            this.backfillPluginStorageMetadata(transaction)
        }
        this.database = await requestResult(request)
        this.database.onversionchange = () => {
            this.database?.close()
            this.database = undefined
        }

        const transaction = this.database.transaction(['meta', 'root'], 'readwrite')
        const meta = transaction.objectStore('meta')
        const currentRevision = await requestResult(meta.get('currentRevision'))
        meta.put({ key: 'schemaVersion', value: DATABASE_VERSION })
        if (!currentRevision) {
            const generation = this.generationFor(0)
            meta.put({ key: 'activeGeneration', value: generation })
            meta.put({ key: 'currentRevision', value: 0 })
            transaction.objectStore('root').put({ key: generation, generation, value: {} })
        }
        await transactionDone(transaction)
        await this.sweepTemporaryGenerations()
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

    async readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null> {
        const transaction = this.requireDatabase().transaction(
            ['meta', 'conversations', 'messagePages'],
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

    async readPluginStorage(key: string): Promise<Versioned<unknown> | null> {
        const transaction = this.requireDatabase().transaction(['meta', 'pluginStorage'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        return this.readPluginStorageFromTransaction(transaction, revision, generation, key)
    }

    async commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }> {
        const database = this.requireDatabase()
        const transaction = database.transaction([...STORE_NAMES], 'readwrite')
        try {
            const active = await this.readActive(transaction)
            if (active.revision !== input.expectedRevision) {
                throw new RevisionConflictError(input.expectedRevision, active.revision)
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

            const revision = active.revision + 1
            const generation = active.generation
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
            await this.stageDatabase(transaction, databaseValue, generation)
            transaction.objectStore('root').delete(active.generation)
            for (const storeName of ['presets', 'catalog', 'characters', 'conversations', 'messagePages', 'pluginStorage', 'pluginStorageMetadata'] as const) {
                await this.deleteIndexRange(
                    transaction.objectStore(storeName).index('byGeneration'),
                    this.keyRangeFactory.only(active.generation),
                )
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
        const generation = `snapshot-${revision}-${globalThis.crypto.randomUUID()}`
        const transaction = database.transaction([...STORE_NAMES], 'readwrite')
        activeSnapshotGenerations.add(generation)
        try {
            const active = await this.readActive(transaction)
            if (active.revision !== revision) {
                throw new RevisionConflictError(revision, active.revision)
            }
            const root = (await requestResult(
                transaction.objectStore('root').get(active.generation),
            )) as StoredRecord<PersistentRoot> | undefined
            if (!root) throw new RevisionConflictError(revision, active.revision)
            transaction.objectStore('root').put({ ...root, key: generation, generation })
            transaction.objectStore('meta').put({
                key: this.snapshotLeaseKey(generation),
                value: generation,
                createdAt: Date.now(),
            })
            for (const storeName of ['presets', 'catalog', 'characters', 'conversations', 'messagePages', 'pluginStorage', 'pluginStorageMetadata'] as const) {
                await this.copyGeneration(
                    transaction.objectStore(storeName),
                    active.generation,
                    generation,
                )
            }
            await transactionDone(transaction)
        } catch (error) {
            activeSnapshotGenerations.delete(generation)
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
                const record = await this.readRootRecordFromTransaction(
                    this.requireDatabase().transaction('root', 'readonly'),
                    generation,
                )
                if (!record) throw new Error('Persistent snapshot root is missing')
                return { revision, value: record.value }
            },
            queryPresets: async () => {
                assertActive()
                return this.queryPresetsFromTransaction(
                    this.requireDatabase().transaction('presets', 'readonly'),
                    revision,
                    generation,
                )
            },
            readPreset: async (id) => {
                assertActive()
                return this.readPresetFromTransaction(
                    this.requireDatabase().transaction('presets', 'readonly'),
                    revision,
                    generation,
                    id,
                )
            },
            queryCharacters: async (input) => {
                assertActive()
                return this.queryCharactersFromTransaction(
                    this.requireDatabase().transaction('catalog', 'readonly'),
                    revision,
                    generation,
                    input,
                )
            },
            readCharacter: async (id) => {
                assertActive()
                return this.readCharacterFromTransaction(
                    this.requireDatabase().transaction('characters', 'readonly'),
                    revision,
                    generation,
                    id,
                )
            },
            queryConversations: async (input) => {
                assertActive()
                return this.queryConversationsFromTransaction(
                    this.requireDatabase().transaction('conversations', 'readonly'),
                    revision,
                    generation,
                    input,
                )
            },
            readConversation: async (characterId, conversationId) => {
                assertActive()
                return this.readConversationFromTransaction(
                    this.requireDatabase().transaction(['conversations', 'messagePages'], 'readonly'),
                    revision,
                    generation,
                    characterId,
                    conversationId,
                )
            },
            readConversationWindow: async (input) => {
                assertActive()
                return this.readConversationWindowFromTransaction(
                    this.requireDatabase().transaction(['conversations', 'messagePages'], 'readonly'),
                    revision,
                    generation,
                    input,
                )
            },
            queryPluginStorage: async () => {
                assertActive()
                return this.queryPluginStorageFromTransaction(
                    this.requireDatabase().transaction('pluginStorageMetadata', 'readonly'),
                    revision,
                    generation,
                )
            },
            readPluginStorage: async (key) => {
                assertActive()
                return this.readPluginStorageFromTransaction(
                    this.requireDatabase().transaction('pluginStorage', 'readonly'),
                    revision,
                    generation,
                    key,
                )
            },
            release: async () => {
                if (releasePromise) return releasePromise
                releasePromise = this.releaseSnapshotLease(generation).then(
                    () => {
                        released = true
                        activeSnapshotGenerations.delete(generation)
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
                .map(({ storageKey: key, byteSize }) => ({ key, byteSize })),
        }
    }

    private async readPluginStorageFromTransaction(
        transaction: IDBTransaction,
        revision: DataRevision,
        generation: string,
        key: string,
    ): Promise<Versioned<unknown> | null> {
        const record = (await requestResult(
            transaction.objectStore('pluginStorage').get(this.pluginStorageKey(generation, key)),
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
        if (input.anchorMessageId !== undefined) {
            const anchor = await this.findMessage(
                transaction,
                generation,
                input.characterId,
                input.conversationId,
                input.anchorMessageId,
                totalMessages,
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
                const cursor = request.result
                if (!cursor) {
                    resolve()
                    return
                }
                const record = cursor.value as StoredRecord<unknown>
                store.put({
                    ...record,
                    key: `${targetGeneration}${record.key.slice(sourceGeneration.length)}`,
                    generation: targetGeneration,
                })
                cursor.continue()
            }
        })
    }

    private async stageDatabase(
        transaction: IDBTransaction,
        databaseValue: Database,
        generation: string,
    ): Promise<void> {
        const ids = new Set<string>()
        const conversationIds = new Set<string>()
        const { characters, botPresets, pluginCustomStorage, ...root } = databaseValue
        this.putRoot(transaction, generation, root)
        this.writePresetRows(transaction, generation, botPresets ?? [])
        this.writePluginStorageRows(transaction, generation, pluginCustomStorage ?? {})
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

    private async deleteGeneration(generation: string): Promise<void> {
        const transaction = this.requireDatabase().transaction([...DATA_STORE_NAMES], 'readwrite')
        await this.deleteGenerationFromTransaction(transaction, generation)
        await transactionDone(transaction)
    }

    private async deleteGenerationFromTransaction(
        transaction: IDBTransaction,
        generation: string,
    ): Promise<void> {
        transaction.objectStore('root').delete(generation)
        for (const storeName of ['presets', 'catalog', 'characters', 'conversations', 'messagePages', 'pluginStorage', 'pluginStorageMetadata'] as const) {
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

    private snapshotLeaseKey(generation: string): string {
        return `snapshotLease:${generation}`
    }

    private async releaseSnapshotLease(generation: string): Promise<void> {
        await this.deleteGeneration(generation)
        const transaction = this.requireDatabase().transaction('meta', 'readwrite')
        transaction.objectStore('meta').delete(this.snapshotLeaseKey(generation))
        await transactionDone(transaction)
    }

    /**
     * Removes snapshot copies abandoned by an interrupted export. A lease record keeps snapshots
     * that another open document is still reading, which a module-local set cannot see; leases
     * older than the TTL are treated as crash leftovers and reclaimed with their generations.
     */
    private async sweepTemporaryGenerations(): Promise<void> {
        const database = this.requireDatabase()
        const cutoff = Date.now() - SNAPSHOT_LEASE_TTL_MS
        const leaseTransaction = database.transaction('meta', 'readwrite')
        const meta = leaseTransaction.objectStore('meta')
        const leaseRecords = await this.readMetaRecordsByPrefix<string>(meta, 'snapshotLease:')
        const leased = new Set<string>()
        for (const record of leaseRecords) {
            const live =
                activeSnapshotGenerations.has(record.value) || (record.createdAt ?? 0) >= cutoff
            if (live) leased.add(record.value)
            else meta.delete(record.key)
        }
        await transactionDone(leaseTransaction)

        const snapshots = this.keyRangeFactory.bound('snapshot-', 'snapshot-￿')
        const abandoned = (generation: string) =>
            !leased.has(generation) && !activeSnapshotGenerations.has(generation)
        const transaction = database.transaction([...DATA_STORE_NAMES], 'readwrite')
        for (const storeName of DATA_STORE_NAMES) {
            const store = transaction.objectStore(storeName)
            // Only the root store is keyed by generation; the rest carry it on an index.
            const request = storeName === 'root'
                ? store.openKeyCursor(snapshots)
                : store.index('byGeneration').openKeyCursor(snapshots)
            await new Promise<void>((resolve, reject) => {
                request.onerror = () => reject(request.error)
                request.onsuccess = () => {
                    const cursor = request.result
                    if (!cursor) {
                        resolve()
                        return
                    }
                    if (abandoned(String(cursor.key))) store.delete(cursor.primaryKey)
                    cursor.continue()
                }
            })
        }
        await transactionDone(transaction)
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
        if (!existing && !mutation.conversation) {
            throw new Error(`Conversation ${mutation.conversationId} does not exist`)
        }
        if (!existing) {
            this.putConversation(
                transaction,
                generation,
                mutation.characterId,
                {
                    ...mutation.conversation!,
                    id: mutation.conversationId,
                    message: mutation.messages,
                },
                await this.conversationCount(transaction, generation, mutation.characterId),
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
    ): Promise<{ index: number; page: StoredMessagePage } | null> {
        if (totalMessages === 0) return Promise.resolve(null)
        const lastPage = Math.floor((totalMessages - 1) / MESSAGE_PAGE_SIZE)
        const range = this.keyRangeFactory.bound(
            [generation, characterId, conversationId, 0],
            [generation, characterId, conversationId, lastPage],
        )
        return new Promise((resolve, reject) => {
            const request = transaction
                .objectStore('messagePages')
                .index('byConversationPage')
                .openCursor(range)
            request.onerror = () => reject(request.error)
            request.onsuccess = () => {
                const cursor = request.result
                if (!cursor) {
                    resolve(null)
                    return
                }
                const page = cursor.value as StoredMessagePage
                const indexInPage = page.value.findIndex((message) => message.chatId === messageId)
                if (indexInPage !== -1) {
                    resolve({ index: page.pageIndex * MESSAGE_PAGE_SIZE + indexInPage, page })
                    return
                }
                cursor.continue()
            }
        })
    }

    private deleteConversationPages(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
        startPage: number,
    ): Promise<void> {
        return this.deleteIndexRange(
            transaction.objectStore('messagePages').index('byConversationPage'),
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
    ): void {
        const valueStore = transaction.objectStore('pluginStorage')
        const metadataStore = transaction.objectStore('pluginStorageMetadata')
        for (const [ordinal, storageKey] of Object.keys(values).entries()) {
            const value = values[storageKey]
            const metadata = {
                key: this.pluginStorageKey(generation, storageKey),
                generation,
                storageKey,
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
            await Promise.all([
                this.deleteIndexRange(
                    valueStore.index('byGeneration'),
                    this.keyRangeFactory.only(generation),
                ),
                this.deleteIndexRange(
                    metadataStore.index('byGeneration'),
                    this.keyRangeFactory.only(generation),
                ),
            ])
            return
        }
        if (mutation.type === 'delete') {
            const key = this.pluginStorageKey(generation, mutation.key)
            valueStore.delete(key)
            metadataStore.delete(key)
            return
        }
        const existing = (await requestResult(
            metadataStore.get(this.pluginStorageKey(generation, mutation.key)),
        )) as StoredPluginStorageMetadata | undefined
        const ordinal = existing?.ordinal ?? await this.nextPluginStorageOrdinal(
            metadataStore,
            generation,
        )
        const metadata = {
            key: this.pluginStorageKey(generation, mutation.key),
            generation,
            storageKey: mutation.key,
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

    private createIndex(store: IDBObjectStore, name: string, keyPath: string | string[]): void {
        if (!store.indexNames.contains(name)) store.createIndex(name, keyPath)
    }

    private backfillOrderKeys<T>(
        store: IDBObjectStore,
        summaryFrom: (record: StoredRecord<T>) => { configuredIndex: number; recentAt: number },
    ): void {
        const request = store.openCursor()
        request.onsuccess = () => {
            const cursor = request.result
            if (!cursor) return
            const record = cursor.value as StoredRecord<T> & {
                configuredIndex?: number
                recentSortValue?: number
            }
            const summary = summaryFrom(record)
            record.configuredIndex = summary.configuredIndex
            record.recentSortValue = -summary.recentAt
            cursor.update(record)
            cursor.continue()
        }
    }

    private backfillCharacterSummaries(transaction: IDBTransaction): void {
        const catalog = transaction.objectStore('catalog')
        const characters = transaction.objectStore('characters')
        const request = catalog.openCursor()
        request.onsuccess = () => {
            const cursor = request.result
            if (!cursor) return
            const record = cursor.value as StoredRecord<CharacterSummary> & {
                configuredIndex?: number
                recentSortValue?: number
            }
            const detailRequest = characters.get(record.key)
            detailRequest.onsuccess = () => {
                const detail = (detailRequest.result as StoredRecord<CharacterDetail> | undefined)?.value
                record.configuredIndex = record.value.configuredIndex
                record.recentSortValue = -record.value.recentAt
                record.value.type = detail?.type ?? 'character'
                record.value.creatorNotes = detail?.creatorNotes
                record.value.trashTime = detail?.trashTime
                cursor.update(record)
                cursor.continue()
            }
        }
    }

    private migrateRootRows(transaction: IDBTransaction): void {
        const root = transaction.objectStore('root')
        const presets = transaction.objectStore('presets')
        const request = root.openCursor()
        request.onsuccess = () => {
            const cursor = request.result
            if (!cursor) return
            const record = cursor.value as StoredRecord<Record<string, unknown>>
            const hasLegacyPresets = Object.prototype.hasOwnProperty.call(
                record.value,
                'botPresets',
            )
            if (hasLegacyPresets && !Array.isArray(record.value.botPresets)) {
                transaction.abort()
                return
            }
            const legacyPresets = hasLegacyPresets
                ? record.value.botPresets as botPreset[]
                : []
            for (let configuredIndex = 0; configuredIndex < legacyPresets.length; configuredIndex++) {
                const id = String(configuredIndex)
                const preset = legacyPresets[configuredIndex]
                presets.put({
                    key: this.presetKey(record.generation, id),
                    generation: record.generation,
                    configuredIndex,
                    value: {
                        summary: { id, name: preset.name ?? '', image: preset.image, configuredIndex },
                        preset,
                    },
                })
            }
            const legacy = record.value.pluginCustomStorage
            if (
                legacy !== undefined &&
                (!legacy || typeof legacy !== 'object' || Array.isArray(legacy))
            ) {
                transaction.abort()
                return
            }
            this.writePluginStorageRows(
                transaction,
                record.generation,
                (legacy ?? {}) as Record<string, unknown>,
            )
            const {
                characters: _characters,
                botPresets: _botPresets,
                pluginCustomStorage: _pluginCustomStorage,
                ...value
            } = record.value
            cursor.update({ ...record, value })
            cursor.continue()
        }
    }

    private backfillPluginStorageMetadata(transaction: IDBTransaction): void {
        const valueStore = transaction.objectStore('pluginStorage')
        const metadataStore = transaction.objectStore('pluginStorageMetadata')
        const request = valueStore.openCursor()
        request.onsuccess = () => {
            const cursor = request.result
            if (!cursor) return
            const record = cursor.value as StoredPluginStorage
            metadataStore.put({
                key: record.key,
                generation: record.generation,
                storageKey: record.storageKey,
                byteSize: record.byteSize ?? serializedByteSize(record.value),
                ordinal: record.ordinal ?? 0,
            } satisfies StoredPluginStorageMetadata)
            cursor.continue()
        }
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
        return `${generation}:conversation:${characterId}:${conversationId}`
    }

    private messagePageKey(
        generation: string,
        characterId: string,
        conversationId: string,
        pageIndex: number,
    ): string {
        return `${generation}:message-page:${characterId}:${conversationId}:${pageIndex}`
    }

    private pluginStorageKey(generation: string, key: string): string {
        return `${generation}:plugin-storage:${key}`
    }
}
