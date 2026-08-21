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
    Versioned,
    WorkingSetCommit,
} from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'

const DATABASE_VERSION = 1
const MESSAGE_PAGE_SIZE = 128
const STORE_NAMES = ['meta', 'root', 'catalog', 'characters', 'conversations', 'messagePages'] as const

interface StoredRecord<T> {
    key: string
    generation: string
    value: T
}

interface StoredConversation {
    summary: ConversationSummary
    detail: Omit<Chat, 'message'>
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

function page<T>(items: T[], limit: number, cursor?: string): { items: T[]; nextCursor?: string } {
    const offset = cursor === undefined ? 0 : Number.parseInt(cursor, 10)
    const safeOffset = Number.isFinite(offset) && offset >= 0 ? offset : 0
    const safeLimit = Math.max(0, limit)
    const result = items.slice(safeOffset, safeOffset + safeLimit)
    const nextOffset = safeOffset + result.length
    return {
        items: result,
        nextCursor: nextOffset < items.length ? String(nextOffset) : undefined,
    }
}

export class IndexedDbPersistentDataStore implements PersistentDataStore {
    private database?: IDBDatabase

    constructor(
        private readonly databaseName: string,
        private readonly indexedDbFactory: IDBFactory = indexedDB,
    ) {}

    async open(): Promise<void> {
        if (this.database) return

        const request = this.indexedDbFactory.open(this.databaseName, DATABASE_VERSION)
        request.onupgradeneeded = () => {
            const database = request.result
            for (const storeName of STORE_NAMES) {
                if (!database.objectStoreNames.contains(storeName)) {
                    database.createObjectStore(storeName, { keyPath: 'key' })
                }
            }
        }
        this.database = await requestResult(request)

        const transaction = this.database.transaction(['meta', 'root'], 'readwrite')
        const meta = transaction.objectStore('meta')
        const currentRevision = await requestResult(meta.get('currentRevision'))
        if (!currentRevision) {
            const generation = this.generationFor(0)
            meta.put({ key: 'schemaVersion', value: DATABASE_VERSION })
            meta.put({ key: 'activeGeneration', value: generation })
            meta.put({ key: 'currentRevision', value: 0 })
            transaction.objectStore('root').put({ key: generation, generation, value: {} })
        }
        await transactionDone(transaction)
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
        const { generation } = await this.readActive(transaction)
        const records = await this.generationRecords<CharacterSummary>(
            transaction.objectStore('catalog'),
            generation,
        )
        const search = input.search?.trim().toLocaleLowerCase()
        const items = records
            .map((record) => record.value)
            .filter((item) => item.trashed === input.trash)
            .filter((item) => !search || item.name.toLocaleLowerCase().includes(search))
            .sort((left, right) =>
                input.order === 'configured'
                    ? left.configuredIndex - right.configuredIndex
                    : right.recentAt - left.recentAt || left.configuredIndex - right.configuredIndex,
            )
        await transactionDone(transaction)
        return page(items, input.limit, input.cursor)
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
        const { generation } = await this.readActive(transaction)
        const items = (
            await this.generationRecords<StoredConversation>(
                transaction.objectStore('conversations'),
                generation,
            )
        )
            .map((record) => record.value.summary)
            .filter((summary) => summary.characterId === input.characterId)
            .sort((left, right) =>
                input.order === 'configured'
                    ? left.configuredIndex - right.configuredIndex
                    : right.recentAt - left.recentAt || left.configuredIndex - right.configuredIndex,
            )
        await transactionDone(transaction)
        return page(items, input.limit, input.cursor)
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

        const messages = await this.readMessagesFromTransaction(
            transaction,
            generation,
            input.characterId,
            input.conversationId,
        )
        let startIndex: number
        let endIndex: number
        if (input.anchorMessageId !== undefined) {
            const anchorIndex = messages.findIndex((message) => message.chatId === input.anchorMessageId)
            if (anchorIndex === -1) {
                await transactionDone(transaction)
                return null
            }
            startIndex = Math.max(0, anchorIndex - Math.max(0, input.before ?? 0))
            endIndex = Math.min(messages.length, anchorIndex + Math.max(0, input.after ?? 0) + 1)
        } else {
            endIndex = messages.length
            startIndex = Math.max(0, endIndex - Math.max(0, input.limit ?? MESSAGE_PAGE_SIZE))
        }

        const result = {
            revision,
            value: {
                characterId: input.characterId,
                conversationId: input.conversationId,
                messages: messages.slice(startIndex, endIndex),
                startIndex,
                endIndex,
                totalMessages: messages.length,
                hasMoreBefore: startIndex > 0,
                hasMoreAfter: endIndex < messages.length,
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

            const revision = active.revision + 1
            const generation = active.generation
            if (input.root) this.putRoot(transaction, generation, input.root)
            if (input.deleteCharacterId) {
                await this.deleteCharacter(transaction, generation, input.deleteCharacterId)
            }
            if (input.character) await this.putCharacter(transaction, generation, input.character)
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

    async replaceFromDatabase(databaseValue: Database): Promise<{ revision: DataRevision }> {
        const database = this.requireDatabase()
        const transaction = database.transaction([...STORE_NAMES], 'readwrite')
        try {
            const active = await this.readActive(transaction)
            const revision = active.revision + 1
            const generation = this.generationFor(revision)
            const ids = new Set<string>()
            const conversationIds = new Set<string>()
            let expectedConversationCount = 0

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
                    expectedConversationCount++
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
            if (stagedCatalog.length !== characters.length || stagedCatalog.some((item) => !ids.has(item.value.id))) {
                throw new Error('Persistent data staging validation failed')
            }
            const stagedConversations = await this.generationRecords<StoredConversation>(
                transaction.objectStore('conversations'),
                generation,
            )
            if (
                stagedConversations.length !== expectedConversationCount ||
                stagedConversations.some(
                    (item) =>
                        !conversationIds.has(`${item.value.summary.characterId}:${item.value.summary.id}`),
                )
            ) {
                throw new Error('Persistent conversation staging validation failed')
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

    private async applyConversationMutation(
        transaction: IDBTransaction,
        generation: string,
        mutation: ConversationMutation,
    ): Promise<void> {
        const key = this.conversationKey(generation, mutation.characterId, mutation.conversationId)
        if (mutation.type === 'delete') {
            transaction.objectStore('conversations').delete(key)
            await this.deleteMatching(
                transaction.objectStore('messagePages'),
                (record) =>
                    record.generation === generation &&
                    record.characterId === mutation.characterId &&
                    record.conversationId === mutation.conversationId,
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
        const messages = existing
            ? await this.readMessagesFromTransaction(
                  transaction,
                  generation,
                  mutation.characterId,
                  mutation.conversationId,
              )
            : []
        const start = Math.max(0, Math.min(messages.length, mutation.start))
        messages.splice(start, Math.max(0, mutation.deleteCount), ...mutation.messages)
        const detail = mutation.conversation ?? existing!.value.detail
        const configuredIndex = existing?.value.summary.configuredIndex ?? (await this.conversationCount(transaction, generation, mutation.characterId))
        await this.deleteMatching(
            transaction.objectStore('messagePages'),
            (record) =>
                record.generation === generation &&
                record.characterId === mutation.characterId &&
                record.conversationId === mutation.conversationId,
        )
        this.putConversation(
            transaction,
            generation,
            mutation.characterId,
            { ...detail, id: mutation.conversationId, message: messages },
            configuredIndex,
        )
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
        const configuredIndex = existing?.value.configuredIndex ?? (await this.generationRecords(transaction.objectStore('catalog'), generation)).length
        const conversationCount = await this.conversationCount(transaction, generation, detail.chaId)
        this.putCharacterRecords(transaction, generation, detail, configuredIndex, conversationCount)
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
        transaction.objectStore('conversations').put({
            key: this.conversationKey(generation, characterId, id),
            generation,
            value: { summary, detail },
        })
        for (let offset = 0; offset < message.length; offset += MESSAGE_PAGE_SIZE) {
            const pageIndex = offset / MESSAGE_PAGE_SIZE
            transaction.objectStore('messagePages').put({
                key: this.messagePageKey(generation, characterId, id, pageIndex),
                generation,
                characterId,
                conversationId: id,
                pageIndex,
                value: message.slice(offset, offset + MESSAGE_PAGE_SIZE),
            })
        }
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
        await this.deleteMatching(
            transaction.objectStore('conversations'),
            (record) => record.generation === generation && record.value.summary.characterId === characterId,
        )
        await this.deleteMatching(
            transaction.objectStore('messagePages'),
            (record) => record.generation === generation && record.characterId === characterId,
        )
    }

    private async conversationCount(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
    ): Promise<number> {
        return (
            await this.generationRecords<StoredConversation>(
                transaction.objectStore('conversations'),
                generation,
            )
        ).filter((record) => record.value.summary.characterId === characterId).length
    }

    private async readMessagesFromTransaction(
        transaction: IDBTransaction,
        generation: string,
        characterId: string,
        conversationId: string,
    ): Promise<Message[]> {
        const records = (
            await this.generationRecords<Message[]>(transaction.objectStore('messagePages'), generation)
        )
            .filter(
                (record: any) =>
                    record.characterId === characterId && record.conversationId === conversationId,
            )
            .sort((left: any, right: any) => left.pageIndex - right.pageIndex)
        return records.flatMap((record) => record.value)
    }

    private async deleteMatching(
        store: IDBObjectStore,
        predicate: (record: any) => boolean,
    ): Promise<void> {
        const records = (await requestResult(store.getAll())) as any[]
        for (const record of records) {
            if (predicate(record)) store.delete(record.key)
        }
    }

    private async readActive(
        transaction: IDBTransaction,
    ): Promise<{ revision: DataRevision; generation: string }> {
        const store = transaction.objectStore('meta')
        const revisionRecord = await requestResult(store.get('currentRevision'))
        const generationRecord = await requestResult(store.get('activeGeneration'))
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
        const records = (await requestResult(store.getAll())) as StoredRecord<T>[]
        return records.filter((record) => record.generation === generation)
    }

    private requireDatabase(): IDBDatabase {
        if (!this.database) throw new Error('Persistent data store is not open')
        return this.database
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
