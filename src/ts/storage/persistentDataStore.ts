import type { Chat, Database, Message, character, groupChat } from './database.svelte'

export type DataRevision = number

export interface Versioned<T> {
    revision: DataRevision
    value: T
}

export type CharacterDetail = Omit<character, 'chats'> | Omit<groupChat, 'chats'>

export interface CharacterSummary {
    id: string
    name: string
    image?: string
    configuredIndex: number
    recentAt: number
    trashed: boolean
    conversationCount: number
}

export interface ConversationSummary {
    id: string
    characterId: string
    name: string
    configuredIndex: number
    recentAt: number
    messageCount: number
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
    items: ConversationSummary[]
    nextCursor?: string
}

export interface ConversationWindowQuery {
    characterId: string
    conversationId: string
    limit?: number
    anchorMessageId?: string
    before?: number
    after?: number
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
      }
    | {
          type: 'delete'
          characterId: string
          conversationId: string
      }

export interface WorkingSetCommit {
    expectedRevision: DataRevision
    root?: Omit<Database, 'characters'>
    character?: CharacterDetail
    conversations?: ConversationMutation[]
    deleteCharacterId?: string
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

export interface PersistentRevisionLease {
    readonly revision: DataRevision
    readRoot(): Promise<Versioned<Omit<Database, 'characters'>>>
    queryCharacters(input: CharacterQuery): Promise<CharacterPage>
    readCharacter(id: string): Promise<Versioned<CharacterDetail> | null>
    queryConversations(input: ConversationQuery): Promise<ConversationPage>
    readConversation(characterId: string, conversationId: string): Promise<Versioned<Chat> | null>
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
    release(): Promise<void>
}

export interface PersistentDataStore {
    open(): Promise<void>
    readRoot(): Promise<Versioned<Omit<Database, 'characters'>>>
    queryCharacters(input: CharacterQuery): Promise<CharacterPage>
    readCharacter(id: string): Promise<Versioned<CharacterDetail> | null>
    queryConversations(input: ConversationQuery): Promise<ConversationPage>
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
    commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }>
    replaceFromDatabase(database: Database): Promise<{ revision: DataRevision }>
    materializeDatabase(revision?: DataRevision): Promise<Database>
    acquireRevision(revision: DataRevision): Promise<PersistentRevisionLease>
}
