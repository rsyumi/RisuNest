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

export type PersistentRoot = Omit<Database, 'characters' | 'botPresets' | 'pluginCustomStorage'>

export interface PluginStorageSummary {
    key: string
    byteSize: number
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

export type ColdPayloadAuthorityState =
    | { format: 'legacy' }
    | { format: 'preparing'; migrationId: string; sourceRevision: DataRevision }
    | { format: 'v2'; migrationId: string; compatibilityHash: string }

export interface ColdAlias {
    key: string
    objectHash: string | null
    size: number
    metadata: Record<string, unknown>
}

export interface ColdPayloadMigrationInput {
    sourceRevision: DataRevision
    migrationId: string
    compatibilityHash: string
    coldAliases: ColdAlias[]
}

function validateJsonValue(value: unknown, ancestors: Set<object>): void {
    if (value === null || typeof value === 'string' || typeof value === 'boolean') return
    if (typeof value === 'number') {
        if (Number.isFinite(value)) return
        throw new TypeError('Cold alias metadata numbers must be finite')
    }
    if (typeof value !== 'object') {
        throw new TypeError('Cold alias metadata must contain only JSON values')
    }
    const prototype = Object.getPrototypeOf(value)
    if (!Array.isArray(value) && prototype !== Object.prototype && prototype !== null) {
        throw new TypeError('Cold alias metadata objects must be plain JSON objects')
    }
    if (ancestors.has(value)) {
        throw new TypeError('Cold alias metadata must not contain cycles')
    }
    ancestors.add(value)
    for (const child of Array.isArray(value) ? value : Object.values(value)) {
        validateJsonValue(child, ancestors)
    }
    ancestors.delete(value)
}

export function validateColdAlias(alias: ColdAlias): void {
    if (typeof alias.key !== 'string' || alias.key.length === 0 || alias.key.includes('\0')) {
        throw new TypeError('Cold alias key must be nonempty and contain no NUL characters')
    }
    if (alias.objectHash !== null && !/^[0-9a-f]{64}$/.test(alias.objectHash)) {
        throw new TypeError('Cold alias objectHash must be null or lowercase SHA-256')
    }
    if (!Number.isSafeInteger(alias.size) || alias.size < 0) {
        throw new TypeError('Cold alias size must be a nonnegative safe integer')
    }
    if (alias.metadata === null || typeof alias.metadata !== 'object' || Array.isArray(alias.metadata)) {
        throw new TypeError('Cold alias metadata must be an object')
    }
    validateJsonValue(alias.metadata, new Set())
}

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
    queryPluginStorage(): Promise<PluginStorageCatalog>
    readPluginStorage(key: string): Promise<Versioned<unknown> | null>
    readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null>
    readAssetAliasesByKeys(kind: AssetAliasKind, keys: string[]): Promise<Versioned<AssetAlias[]>>
    listAssetAliases(query: AssetAliasListQuery): Promise<AssetAliasPage>
    readAssetRepositoryAuthority(): Promise<Versioned<AssetRepositoryAuthorityState>>
    readAssetOwnerHead(owner: AssetOwnerLocator): Promise<Versioned<AssetOwnerHead> | null>
    readColdPayloadAuthority(): Promise<Versioned<ColdPayloadAuthorityState>>
    readColdAlias(key: string): Promise<Versioned<ColdAlias> | null>
    listColdAliases(): Promise<Versioned<ColdAlias[]>>
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
    readConversationMetadata(
        characterId: string,
        conversationId: string,
    ): Promise<Versioned<PersistentConversationMetadata> | null>
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
    queryPluginStorage(): Promise<PluginStorageCatalog>
    readPluginStorage(key: string): Promise<Versioned<unknown> | null>
    readAssetAlias(identity: AssetAliasIdentity): Promise<Versioned<AssetAlias> | null>
    readAssetAliasesByKeys(kind: AssetAliasKind, keys: string[]): Promise<Versioned<AssetAlias[]>>
    listAssetAliases(query: AssetAliasListQuery): Promise<AssetAliasPage>
    readAssetRepositoryAuthority(): Promise<Versioned<AssetRepositoryAuthorityState>>
    readAssetOwnerHead(owner: AssetOwnerLocator): Promise<Versioned<AssetOwnerHead> | null>
    readColdPayloadAuthority(): Promise<Versioned<ColdPayloadAuthorityState>>
    readColdAlias(key: string): Promise<Versioned<ColdAlias> | null>
    listColdAliases(): Promise<Versioned<ColdAlias[]>>
    commitAssetAlias(alias: AssetAlias, expectedRevision: DataRevision): Promise<{ revision: DataRevision }>
    deleteAssetAlias(
        identity: AssetAliasIdentity,
        expectedRevision: DataRevision,
    ): Promise<{ revision: DataRevision }>
    activateAssetRepositoryMigration(
        input: AssetRepositoryMigrationInput,
    ): Promise<{ revision: DataRevision }>
    commitColdAlias(alias: ColdAlias, expectedRevision: DataRevision): Promise<{ revision: DataRevision }>
    deleteColdAlias(key: string, expectedRevision: DataRevision): Promise<{ revision: DataRevision }>
    activateColdPayloadMigration(
        input: ColdPayloadMigrationInput,
    ): Promise<{ revision: DataRevision }>
    commit(input: WorkingSetCommit): Promise<{ revision: DataRevision }>
    replaceFromDatabase(
        database: Database,
        expectedRevision?: DataRevision,
        assetAliases?: AssetAlias[],
    ): Promise<{ revision: DataRevision }>
    materializeDatabase(revision?: DataRevision): Promise<Database>
    acquireRevision(revision: DataRevision): Promise<PersistentRevisionLease>
}
