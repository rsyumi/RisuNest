import type { Chat, Database, Message } from './database.svelte'
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
    Versioned,
    WorkingSetCommit,
} from './persistentDataStore'
import { RevisionConflictError, SnapshotReleasedError } from './persistentDataStore'

const DATABASE_VERSION = 3
const MESSAGE_PAGE_SIZE = 128
const SNAPSHOT_LEASE_TTL_MS = 24 * 60 * 60 * 1000
const MAX_INDEX_VALUE = Number.MAX_SAFE_INTEGER
const STORE_NAMES = ['meta', 'root', 'catalog', 'characters', 'conversations', 'messagePages'] as const
const DATA_STORE_NAMES = ['root', 'catalog', 'characters', 'conversations', 'messagePages'] as const
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

interface PreparedGenerationCounts {
    root: number
    catalog: number
    characters: number
    conversations: number
    messagePages: number
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
            this.backfillOrderKeys<CharacterSummary>(
                transaction.objectStore('catalog'),
                (record) => record.value,
            )
            this.backfillOrderKeys<StoredConversation>(
                transaction.objectStore('conversations'),
                (record) => record.value.summary,
            )
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

    async readRoot(): Promise<Versioned<Omit<Database, 'characters'>>> {
        const database = this.requireDatabase()
        const transaction = database.transaction(['meta', 'root'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        const record = (await requestResult(
            transaction.objectStore('root').get(generation),
        )) as StoredRecord<Omit<Database, 'characters'>> | undefined
        await transactionDone(transaction)
        return { revision, value: record?.value ?? ({} as Omit<Database, 'characters'>) }
    }

    async queryCharacters(input: CharacterQuery): Promise<CharacterPage> {
        const database = this.requireDatabase()
        const transaction = database.transaction(['meta', 'catalog'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        const store = transaction.objectStore('catalog')
        const index = store.index(
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

    async readCharacter(id: string): Promise<Versioned<CharacterDetail> | null> {
        const database = this.requireDatabase()
        const transaction = database.transaction(['meta', 'characters'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
        const record = (await requestResult(
            transaction.objectStore('characters').get(this.characterKey(generation, id)),
        )) as StoredRecord<CharacterDetail> | undefined
        await transactionDone(transaction)
        return record ? { revision, value: record.value } : null
    }

    async queryConversations(input: ConversationQuery): Promise<ConversationPage> {
        const database = this.requireDatabase()
        const transaction = database.transaction(['meta', 'conversations'], 'readonly')
        const { revision, generation } = await this.readActive(transaction)
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
            items: result.items.map((item) => item.summary),
            nextCursor: result.nextCursor,
        }
    }

    async readConversation(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<Chat> | null> {
        const database = this.requireDatabase()
        const transaction = database.transaction(
            ['meta', 'conversations', 'messagePages'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
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

    async readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null> {
        const database = this.requireDatabase()
        const transaction = database.transaction(
            ['meta', 'conversations', 'messagePages'],
            'readonly',
        )
        const { revision, generation } = await this.readActive(transaction)
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

        const result = {
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
        await transactionDone(transaction)
        return result
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

            const revision = active.revision + 1
            const generation = active.generation
            if (input.root) this.putRoot(transaction, generation, input.root)
            if (input.deleteCharacterId) {
                await this.deleteCharacter(transaction, generation, input.deleteCharacterId)
            }
            if (input.character) await this.putCharacter(transaction, generation, input.character)
            if (input.replaceCharacter) {
                await this.replaceCharacter(transaction, generation, input.replaceCharacter)
            }
            if (input.addCharacter) {
                await this.addCharacter(transaction, generation, input.addCharacter)
            }
            for (const mutation of input.conversations ?? []) {
                await this.applyConversationMutation(transaction, generation, mutation)
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
            for (const storeName of ['catalog', 'characters', 'conversations', 'messagePages']) {
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
            ['meta', 'root', 'catalog', 'characters', 'conversations', 'messagePages'],
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
        )) as StoredRecord<Omit<Database, 'characters'>> | undefined
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
        const result = { ...root.value, characters } as Database
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
            )) as StoredRecord<Omit<Database, 'characters'>> | undefined
            if (!root) throw new RevisionConflictError(revision, active.revision)
            transaction.objectStore('root').put({ ...root, key: generation, generation })
            transaction.objectStore('meta').put({
                key: this.snapshotLeaseKey(generation),
                value: generation,
                createdAt: Date.now(),
            })
            for (const storeName of ['catalog', 'characters', 'conversations', 'messagePages'] as const) {
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
                return this.readRootAt(revision, generation)
            },
            queryCharacters: async (input) => {
                assertActive()
                return this.queryCharactersAt(revision, generation, input)
            },
            readCharacter: async (id) => {
                assertActive()
                return this.readCharacterAt(revision, generation, id)
            },
            queryConversations: async (input) => {
                assertActive()
                return this.queryConversationsAt(revision, generation, input)
            },
            readConversation: async (characterId, conversationId) => {
                assertActive()
                return this.readConversationAt(revision, generation, characterId, conversationId)
            },
            readConversationWindow: async (input) => {
                assertActive()
                return this.readConversationWindowAt(revision, generation, input)
            },
            release: async () => {
                if (releasePromise) return releasePromise
                released = true
                releasePromise = this.releaseSnapshotLease(generation).finally(() => {
                    activeSnapshotGenerations.delete(generation)
                })
                return releasePromise
            },
        }
    }

    private async readRootAt(
        revision: DataRevision,
        generation: string,
    ): Promise<Versioned<Omit<Database, 'characters'>>> {
        const transaction = this.requireDatabase().transaction('root', 'readonly')
        const record = (await requestResult(
            transaction.objectStore('root').get(generation),
        )) as StoredRecord<Omit<Database, 'characters'>> | undefined
        await transactionDone(transaction)
        if (!record) throw new Error('Persistent snapshot root is missing')
        return { revision, value: record.value }
    }

    private async queryCharactersAt(
        revision: DataRevision,
        generation: string,
        input: CharacterQuery,
    ): Promise<CharacterPage> {
        const transaction = this.requireDatabase().transaction('catalog', 'readonly')
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

    private async readCharacterAt(
        revision: DataRevision,
        generation: string,
        id: string,
    ): Promise<Versioned<CharacterDetail> | null> {
        const transaction = this.requireDatabase().transaction('characters', 'readonly')
        const record = (await requestResult(
            transaction.objectStore('characters').get(this.characterKey(generation, id)),
        )) as StoredRecord<CharacterDetail> | undefined
        await transactionDone(transaction)
        return record ? { revision, value: record.value } : null
    }

    private async queryConversationsAt(
        revision: DataRevision,
        generation: string,
        input: ConversationQuery,
    ): Promise<ConversationPage> {
        const transaction = this.requireDatabase().transaction('conversations', 'readonly')
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
            items: result.items.map((item) => item.summary),
            nextCursor: result.nextCursor,
        }
    }

    private async readConversationAt(
        revision: DataRevision,
        generation: string,
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<Chat> | null> {
        const transaction = this.requireDatabase().transaction(
            ['conversations', 'messagePages'],
            'readonly',
        )
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

    private async readConversationWindowAt(
        revision: DataRevision,
        generation: string,
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null> {
        const transaction = this.requireDatabase().transaction(
            ['conversations', 'messagePages'],
            'readonly',
        )
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
    ): Promise<PreparedGenerationCounts> {
        const ids = new Set<string>()
        const conversationIds = new Set<string>()
        let conversationCount = 0
        let messagePageCount = 0
        const { characters, ...root } = databaseValue
        this.putRoot(transaction, generation, root)
        for (let index = 0; index < characters.length; index++) {
            const character = characters[index]
            if (!character.chaId || ids.has(character.chaId)) {
                throw new Error('Persistent data import requires unique character IDs')
            }
            ids.add(character.chaId)
            const { chats, ...detail } = character
            this.putCharacterRecords(transaction, generation, detail, index, chats.length)
            const characterConversationIds = new Set<string>()
            for (let conversationIndex = 0; conversationIndex < chats.length; conversationIndex++) {
                const conversation = chats[conversationIndex]
                if (!conversation.id || characterConversationIds.has(conversation.id)) {
                    throw new Error(`Character ${character.chaId} requires unique conversation IDs`)
                }
                characterConversationIds.add(conversation.id)
                conversationIds.add(`${character.chaId}:${conversation.id}`)
                conversationCount++
                messagePageCount += Math.ceil(conversation.message.length / MESSAGE_PAGE_SIZE)
                this.putConversation(
                    transaction,
                    generation,
                    character.chaId,
                    conversation,
                    conversationIndex,
                )
            }
        }

        const stagedCatalog = await this.generationRecords<CharacterSummary>(
            transaction.objectStore('catalog'),
            generation,
        )
        if (
            stagedCatalog.length !== characters.length ||
            stagedCatalog.some((item) => !ids.has(item.value.id))
        ) {
            throw new Error('Persistent data staging validation failed')
        }
        const stagedConversations = await this.generationRecords<StoredConversation>(
            transaction.objectStore('conversations'),
            generation,
        )
        if (
            stagedConversations.length !== conversationCount ||
            stagedConversations.some(
                (item) =>
                    !conversationIds.has(
                        `${item.value.summary.characterId}:${item.value.summary.id}`,
                    ),
            )
        ) {
            throw new Error('Persistent conversation staging validation failed')
        }
        const counts = {
            root: 1,
            catalog: characters.length,
            characters: characters.length,
            conversations: conversationCount,
            messagePages: messagePageCount,
        }
        await this.validateGeneration(transaction, generation, counts)
        return counts
    }

    private async validateGeneration(
        transaction: IDBTransaction,
        generation: string,
        expected: PreparedGenerationCounts,
    ): Promise<void> {
        const root = await requestResult(transaction.objectStore('root').get(generation))
        const counts = {
            root: root ? 1 : 0,
            catalog: await requestResult(
                transaction.objectStore('catalog').index('byGeneration').count(generation),
            ),
            characters: await requestResult(
                transaction.objectStore('characters').index('byGeneration').count(generation),
            ),
            conversations: await requestResult(
                transaction.objectStore('conversations').index('byGeneration').count(generation),
            ),
            messagePages: await requestResult(
                transaction.objectStore('messagePages').index('byGeneration').count(generation),
            ),
        }
        if (
            Object.keys(expected).some(
                (key) =>
                    counts[key as keyof typeof counts] !==
                    expected[key as keyof typeof expected],
            )
        ) {
            throw new Error('Persistent prepared replacement data validation failed')
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
        for (const storeName of ['catalog', 'characters', 'conversations', 'messagePages'] as const) {
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
        root: Omit<Database, 'characters'>,
    ): void {
        transaction.objectStore('root').put({ key: generation, generation, value: root })
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

    private generationFor(revision: DataRevision): string {
        return `revision-${revision}`
    }

    private characterKey(generation: string, characterId: string): string {
        return `${generation}:character:${characterId}`
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
}
