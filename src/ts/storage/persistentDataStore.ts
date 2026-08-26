import type { Chat, Database, Message, botPreset, character, groupChat } from './database.svelte'

export type DataRevision = number

export interface Versioned<T> {
    revision: DataRevision
    value: T
}

export type CharacterDetail = Omit<character, 'chats'> | Omit<groupChat, 'chats'>

export type PersistentRoot = Omit<Database, 'characters' | 'botPresets' | 'pluginCustomStorage'>

export interface PluginStorageSummary {
    key: string
    byteSize: number
}

export interface PluginStorageCatalog {
    revision: DataRevision
    items: PluginStorageSummary[]
}

export type PluginStorageMutation =
    | { type: 'set'; key: string; value: unknown }
    | { type: 'delete'; key: string }
    | { type: 'clear' }

export interface CharacterSummary {
    id: string
    name: string
    image?: string
    configuredIndex: number
    recentAt: number
    trashed: boolean
    conversationCount: number
    type: CharacterDetail['type']
    creatorNotes?: string
    trashTime?: number
}

export interface PresetSummary {
    id: string
    name: string
    image?: string
    configuredIndex: number
}

export interface PresetCatalog {
    revision: DataRevision
    items: PresetSummary[]
}

export interface ConversationSummary {
    id: string
    characterId: string
    name: string
    folderId?: string
    bindedPersona?: string
    configuredIndex: number
    recentAt: number
    messageCount: number
    fmIndex?: number
}

export interface ConversationWindow {
    characterId: string
    conversationId: string
    messages: Message[]
    startIndex: number
    endIndex: number
    totalMessages: number
    hasMoreBefore: boolean
    hasMoreAfter: boolean
}

export interface CharacterQuery {
    search?: string
    order: 'configured' | 'recent'
    trash: boolean
    limit: number
    cursor?: string
}

export interface CharacterPage {
    revision: DataRevision
    items: CharacterSummary[]
    nextCursor?: string
}

export interface ConversationQuery {
    characterId: string
    order: 'configured' | 'recent'
    limit: number
    cursor?: string
}

export interface ConversationPage {
    revision: DataRevision
    items: ConversationSummary[]
    nextCursor?: string
}

export interface ConversationWindowQuery {
    characterId: string
    conversationId: string
    startIndex?: number
    limit?: number
    anchorMessageId?: string
    before?: number
    after?: number
}

export const CONVERSATION_RANGE_MAX_LIMIT = 4_096

export function validateConversationWindowQuery(input: ConversationWindowQuery): void {
    if (input.startIndex === undefined) return
    if (!Number.isSafeInteger(input.startIndex) || input.startIndex < 0) {
        throw new RangeError('Conversation range startIndex must be a nonnegative safe integer')
    }
    if (!Number.isSafeInteger(input.limit) || input.limit === undefined || input.limit <= 0) {
        throw new RangeError('Conversation range limit must be a positive safe integer')
    }
    if (input.limit > CONVERSATION_RANGE_MAX_LIMIT) {
        throw new RangeError(
            `Conversation range limit cannot exceed ${CONVERSATION_RANGE_MAX_LIMIT}`,
        )
    }
    if (
        input.anchorMessageId !== undefined ||
        input.before !== undefined ||
        input.after !== undefined
    ) {
        throw new RangeError('Conversation absolute range cannot include anchor options')
    }
}

export type ConversationMutation =
    | {
          type: 'replace-range'
          characterId: string
          conversationId: string
          start: number
          deleteCount: number
          messages: Message[]
          conversation?: Omit<Chat, 'message'>
          configuredIndex?: number
      }
    | {
          type: 'delete'
          characterId: string
          conversationId: string
      }

export interface WorkingSetCommit {
    expectedRevision: DataRevision
    root?: PersistentRoot
    replacePresets?: botPreset[]
    character?: CharacterDetail
    characterDetails?: CharacterDetail[]
    replaceCharacter?: character | groupChat
    addCharacter?: character | groupChat
    conversations?: ConversationMutation[]
    deleteCharacterId?: string
    pluginStorage?: PluginStorageMutation[]
}

export class RevisionConflictError extends Error {
    readonly expectedRevision: DataRevision
    readonly actualRevision: DataRevision

    constructor(expectedRevision: DataRevision, actualRevision: DataRevision) {
        super(`Expected data revision ${expectedRevision}, but current revision is ${actualRevision}`)
        this.name = 'RevisionConflictError'
        this.expectedRevision = expectedRevision
        this.actualRevision = actualRevision
    }
}

export class SnapshotReleasedError extends Error {
    constructor() {
        super('Persistent revision snapshot has been released')
        this.name = 'SnapshotReleasedError'
    }
}

export interface PersistentRevisionReader {
    readonly revision: DataRevision
    readRoot(): Promise<Versioned<PersistentRoot>>
    queryPresets(): Promise<PresetCatalog>
    readPreset(id: string): Promise<Versioned<botPreset> | null>
    queryCharacters(input: CharacterQuery): Promise<CharacterPage>
    readCharacter(id: string): Promise<Versioned<CharacterDetail> | null>
    queryConversations(input: ConversationQuery): Promise<ConversationPage>
    readConversation(characterId: string, conversationId: string): Promise<Versioned<Chat> | null>
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
    queryPluginStorage(): Promise<PluginStorageCatalog>
    readPluginStorage(key: string): Promise<Versioned<unknown> | null>
}

export interface PersistentRevisionLease extends PersistentRevisionReader {
    release(): Promise<void>
}

export interface PersistentDataStore {
    open(): Promise<void>
    readRoot(): Promise<Versioned<PersistentRoot>>
    queryPresets(): Promise<PresetCatalog>
    readPreset(id: string): Promise<Versioned<botPreset> | null>
    queryCharacters(input: CharacterQuery): Promise<CharacterPage>
    readCharacter(id: string): Promise<Versioned<CharacterDetail> | null>
    queryConversations(input: ConversationQuery): Promise<ConversationPage>
    readConversation(characterId: string, conversationId: string): Promise<Versioned<Chat> | null>
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
    queryPluginStorage(): Promise<PluginStorageCatalog>
    readPluginStorage(key: string): Promise<Versioned<unknown> | null>
    commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }>
    replaceFromDatabase(
        database: Database,
        expectedRevision?: DataRevision,
    ): Promise<{ revision: DataRevision }>
    materializeDatabase(revision?: DataRevision): Promise<Database>
    acquireRevision(revision: DataRevision): Promise<PersistentRevisionLease>
}
