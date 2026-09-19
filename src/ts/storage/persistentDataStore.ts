import type { Chat, Database, Message, botPreset, character, groupChat } from './database.svelte'

export type DataRevision = number

export interface Versioned<T> {
    revision: DataRevision
    value: T
}

export type CharacterDetail = Omit<character, 'chats'> | Omit<groupChat, 'chats'>

export interface PersistentConversationMetadata {
    characterId: string
    conversationId: string
    conversation: Omit<Chat, 'message'>
    totalMessages: number
}

export type PersistentRoot = Omit<
    Database,
    'characters' | 'botPresets' | 'pluginCustomStorage' | 'pluginStorageMeta'
>

export interface PluginStorageSummary {
    owner: string
    key: string
    byteSize: number
}

export interface PluginStorageValue {
    owner: string
    key: string
    value: unknown
}

/** What the plugin data screen lists. Values are fetched one at a time. */
export interface PluginStorageListItem {
    owner: string
    key: string
    space?: 'string' | 'json'
    valueType: 'string' | 'json'
    byteSize: number
    claimedFrom?: string
    importBatchId?: string
    assignedAt?: number
}

export interface PluginStorageCatalog {
    revision: DataRevision
    items: PluginStorageSummary[]
}

export type AssetAliasKind = 'asset' | 'inlay'
export type AssetAliasInlayType = 'image' | 'video' | 'audio' | 'signature'

export type AssetRepositoryAuthorityState =
    | { format: 'legacy' }
    | { format: 'preparing'; migrationId: string; sourceRevision: DataRevision }
    | { format: 'v2'; migrationId: string; compatibilityHash: string }

export interface AssetAliasIdentity {
    kind: AssetAliasKind
    key: string
}

export interface AssetAliasListQuery {
    kind?: AssetAliasKind
    limit: number
    cursor?: string
}

export interface AssetAliasPage {
    revision: DataRevision
    items: AssetAlias[]
    nextCursor?: string
}

interface AssetAliasBase {
    key: string
    objectHash: string | null
    size: number
    mime: string
    name: string
    ext: string
}

export type AssetAlias = AssetAliasBase & (
    | {
        kind: 'asset'
        inlayType?: never
        width?: never
        height?: never
    }
    | {
        kind: 'inlay'
        inlayType: AssetAliasInlayType
        width?: number
        height?: number
    }
)

export type AssetOwnerLocator =
    | { kind: 'character-additional-assets'; characterId: string }
    | { kind: 'root-module-assets'; index: number }
    | { kind: 'persona-embedded-module-assets'; index: number }

export type AssetOwnerHead = { owner: AssetOwnerLocator } & (
    | {
        present: false
        manifestHash: null
        entryCount: 0
    }
    | {
        present: true
        manifestHash: string
        entryCount: number
    }
)

export interface AssetRepositoryMigrationInput {
    sourceRevision: DataRevision
    migrationId: string
    compatibilityHash: string
    database: Database
    assetAliases: AssetAlias[]
    assetOwnerHeads: AssetOwnerHead[]
}

export function assetOwnerLocatorKey(owner: AssetOwnerLocator): string {
    validateAssetOwnerLocator(owner)
    return owner.kind === 'character-additional-assets'
        ? `${owner.kind}:${owner.characterId}`
        : `${owner.kind}:${owner.index}`
}

export function validateAssetOwnerLocator(owner: AssetOwnerLocator): void {
    if (owner.kind === 'character-additional-assets') {
        if (typeof owner.characterId !== 'string' || owner.characterId.length === 0) {
            throw new TypeError('Character asset owner requires a nonempty characterId')
        }
        return
    }
    if (
        owner.kind !== 'root-module-assets'
        && owner.kind !== 'persona-embedded-module-assets'
    ) {
        throw new TypeError('Asset owner kind is invalid')
    }
    if (!Number.isSafeInteger(owner.index) || owner.index < 0) {
        throw new TypeError('Asset owner occurrence index must be a nonnegative safe integer')
    }
}

export function validateAssetOwnerHead(head: AssetOwnerHead): void {
    validateAssetOwnerLocator(head.owner)
    if (typeof head.present !== 'boolean') {
        throw new TypeError('Asset owner head present must be a boolean')
    }
    if (!Number.isSafeInteger(head.entryCount) || head.entryCount < 0) {
        throw new TypeError('Asset owner head entryCount must be a nonnegative safe integer')
    }
    if (!head.present) {
        if (head.manifestHash !== null || head.entryCount !== 0) {
            throw new TypeError('Absent asset owner property cannot reference a manifest')
        }
        return
    }
    if (!/^[0-9a-f]{64}$/.test(head.manifestHash)) {
        throw new TypeError('Present asset owner property requires a lowercase SHA-256 manifestHash')
    }
}

export function validateAssetAlias(alias: AssetAlias): void {
    const inlayMetadata = alias as unknown as {
        inlayType?: unknown
        width?: unknown
        height?: unknown
    }
    if (typeof alias.key !== 'string') throw new TypeError('Asset alias key must be a string')
    if (alias.objectHash !== null && !/^[0-9a-f]{64}$/.test(alias.objectHash)) {
        throw new TypeError('Asset alias objectHash must be null or 64 lowercase hexadecimal characters')
    }
    if (alias.kind !== 'asset' && alias.kind !== 'inlay') {
        throw new TypeError('Asset alias kind must be asset or inlay')
    }
    if (!Number.isSafeInteger(alias.size) || alias.size < 0) {
        throw new TypeError('Asset alias size must be a nonnegative safe integer')
    }
    for (const [field, value] of [
        ['mime', alias.mime],
        ['name', alias.name],
        ['ext', alias.ext],
    ] as const) {
        if (typeof value !== 'string') throw new TypeError(`Asset alias ${field} must be a string`)
    }
    if (alias.kind === 'inlay') {
        if (
            typeof inlayMetadata.inlayType !== 'string'
            || !['image', 'video', 'audio', 'signature'].includes(inlayMetadata.inlayType)
        ) {
            throw new TypeError('Asset alias inlayType is required and must be valid')
        }
    } else if (
        inlayMetadata.inlayType !== undefined
        || inlayMetadata.width !== undefined
        || inlayMetadata.height !== undefined
    ) {
        throw new TypeError('Asset alias Inlay metadata is forbidden for ordinary assets')
    }
    for (const [field, value] of [
        ['width', inlayMetadata.width],
        ['height', inlayMetadata.height],
    ] as const) {
        if (
            value !== undefined
            && (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0)
        ) {
            throw new TypeError(`Asset alias ${field} must be a nonnegative safe integer`)
        }
    }
}

export function validateAssetAliasIdentity(identity: AssetAliasIdentity): void {
    if (identity.kind !== 'asset' && identity.kind !== 'inlay') {
        throw new TypeError('Asset alias identity kind must be asset or inlay')
    }
    if (typeof identity.key !== 'string') {
        throw new TypeError('Asset alias identity key must be a string')
    }
}

export function validateAssetAliasKeyBatch(kind: AssetAliasKind, keys: string[]): void {
    if (kind !== 'asset' && kind !== 'inlay') {
        throw new TypeError('Asset alias identity kind must be asset or inlay')
    }
    if (!Array.isArray(keys) || keys.length < 1 || keys.length > 512) {
        throw new TypeError('Asset alias key batch size must be between 1 and 512')
    }
    const uniqueKeys = new Set<string>()
    for (const key of keys) {
        if (typeof key !== 'string') {
            throw new TypeError('Asset alias identity key must be a string')
        }
        if (uniqueKeys.has(key)) {
            throw new TypeError('Asset alias key batch must contain unique keys')
        }
        uniqueKeys.add(key)
    }
}

export type PluginStorageMutation =
    | { type: 'set'; owner: string; key: string; value: unknown }
    | { type: 'delete'; owner: string; key: string }
    | { type: 'clear'; owner: string }

/// What the list needs about an archived character. The stored object and the
/// asset hashes it holds stay inside the store.
export interface ArchivedCharacterSummary {
    archivedAt: number
    conversationCount: number
    messageCount: number
}

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
    archived?: ArchivedCharacterSummary
}

export interface ArchivePreview {
    characterId: string
    name: string
    conversationCount: number
    messageCount: number
    archived: boolean
}

export class ArchivedCharacterError extends Error {
    constructor(readonly characterId: string) {
        super(`Character ${characterId} is archived`)
        this.name = 'ArchivedCharacterError'
    }
}

/// One coalesced change locator. `messages` and `conversations` share a
/// locator, so a message edit arrives as a change to its conversation.
export interface ContentChangeKey {
    kind: string
    key1: string
    key2: string
}

export interface ContentChangeWindow {
    revision: DataRevision
    /// Null asks for a full reprojection; the cursor is unusable for this window.
    afterRevision: DataRevision | null
}

export const CONTENT_CHANGE_PAGE_LIMIT = 1_024

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

export interface ConversationMessageMetadata {
    chatId?: string
    role?: Message['role']
    disabled?: Message['disabled']
    parserInert: boolean
}

export interface ConversationMessageMetadataWindow {
    characterId: string
    conversationId: string
    messages: ConversationMessageMetadata[]
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
    anchorOccurrence?: 'first' | 'last'
    before?: number
    after?: number
}

export const CONVERSATION_RANGE_MAX_LIMIT = 4_096

export function validateConversationWindowQuery(input: ConversationWindowQuery): void {
    if (
        input.anchorOccurrence !== undefined &&
        input.anchorOccurrence !== 'first' &&
        input.anchorOccurrence !== 'last'
    ) {
        throw new RangeError('Conversation anchor occurrence must be first or last')
    }
    if (input.anchorOccurrence !== undefined && input.anchorMessageId === undefined) {
        throw new RangeError('Conversation anchor occurrence requires anchorMessageId')
    }
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
        input.anchorOccurrence !== undefined ||
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

export type RootMutation =
    { type: 'set'; key: string; value: unknown } | { type: 'delete'; key: string }

export interface WorkingSetCommit {
    expectedRevision: DataRevision
    root?: PersistentRoot
    /** Mutually exclusive with a complete root replacement. */
    rootMutations?: RootMutation[]
    replacePresets?: botPreset[]
    character?: CharacterDetail
    characterDetails?: CharacterDetail[]
    replaceCharacter?: character | groupChat
    addCharacter?: character | groupChat
    conversations?: ConversationMutation[]
    deleteCharacterId?: string
    pluginStorage?: PluginStorageMutation[]
    assetAliases?: AssetAlias[]
    assetOwnerHeads?: AssetOwnerHead[]
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
    readCharacterSummary(id: string): Promise<CharacterSummary | null>
    readCharacter(id: string): Promise<Versioned<CharacterDetail> | null>
    queryConversations(input: ConversationQuery): Promise<ConversationPage>
    readConversation(characterId: string, conversationId: string): Promise<Versioned<Chat> | null>
    readConversationMetadata(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<PersistentConversationMetadata> | null>
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
    readConversationMessageMetadataWindow?(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationMessageMetadataWindow> | null>
    queryPluginStorage(): Promise<PluginStorageCatalog>
    readPluginStorage(owner: string, key: string): Promise<Versioned<unknown> | null>
    readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null>
    readAssetAliasesByKeys(kind: AssetAliasKind, keys: string[]): Promise<Versioned<AssetAlias[]>>
    listAssetAliases(query: AssetAliasListQuery): Promise<AssetAliasPage>
    readAssetRepositoryAuthority(): Promise<Versioned<AssetRepositoryAuthorityState>>
    readAssetOwnerHead(owner: AssetOwnerLocator): Promise<Versioned<AssetOwnerHead> | null>
    /// Present only where the store tracks changes. The window and every record
    /// reprojected for it must be read through this one lease.
    readWorkingSetChangeWindow?(): Promise<ContentChangeWindow>
    readWorkingSetChangePage?(
        afterRevision: DataRevision,
        afterKey: ContentChangeKey | null,
        limit: number,
    ): Promise<ContentChangeKey[]>
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
    readCharacterSummary(id: string): Promise<CharacterSummary | null>
    readCharacter(id: string): Promise<Versioned<CharacterDetail> | null>
    queryConversations(input: ConversationQuery): Promise<ConversationPage>
    readConversation(characterId: string, conversationId: string): Promise<Versioned<Chat> | null>
    readConversationMetadata(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<PersistentConversationMetadata> | null>
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
    readConversationMessageMetadataWindow?(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationMessageMetadataWindow> | null>
    queryPluginStorage(): Promise<PluginStorageCatalog>
    readPluginStorage(owner: string, key: string): Promise<Versioned<unknown> | null>
    /** Sizes and ownership only. Values stay in the store until one is opened. */
    listPluginStorage(): Promise<PluginStorageListItem[]>
    /// Advances only once the working set for `revision` has been installed.
    commitWorkingSetChangeCursor?(revision: DataRevision): Promise<void>
    readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null>
    readAssetAliasesByKeys(kind: AssetAliasKind, keys: string[]): Promise<Versioned<AssetAlias[]>>
    listAssetAliases(query: AssetAliasListQuery): Promise<AssetAliasPage>
    readAssetRepositoryAuthority(): Promise<Versioned<AssetRepositoryAuthorityState>>
    readAssetOwnerHead(owner: AssetOwnerLocator): Promise<Versioned<AssetOwnerHead> | null>
    commitAssetAlias(alias: AssetAlias, expectedRevision: DataRevision): Promise<{ revision: DataRevision }>
    deleteAssetAlias(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }>
    activateAssetRepositoryMigration(
        input: AssetRepositoryMigrationInput,
    ): Promise<{ revision: DataRevision }>
    commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }>
    archivePreview(characterId: string): Promise<ArchivePreview>
    archiveCharacter(
        characterId: string,
        expectedRevision: DataRevision,
        signal?: AbortSignal,
    ): Promise<{ revision: DataRevision }>
    restoreCharacter(
        characterId: string,
        expectedRevision: DataRevision,
        signal?: AbortSignal,
    ): Promise<{ revision: DataRevision }>
    replaceFromDatabase(
        database: Database,
        expectedRevision?: DataRevision,
        assetAliases?: AssetAlias[],
        pluginStorageValues?: PluginStorageValue[],
    ): Promise<{ revision: DataRevision }>
    materializeDatabase(revision?: DataRevision): Promise<Database>
    acquireRevision(revision: DataRevision): Promise<PersistentRevisionLease>
}
