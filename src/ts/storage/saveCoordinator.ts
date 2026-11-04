import type { PluginStorageWriteCheck } from '../plugins/pluginStorageProtection'
import { captureMaterializedCharacter, diffMaterializedCharacter, diffRecordCollection, recordCollections, diffFields } from './persistentUnitCapture'
import { isTauri } from '../platform'
import { Mutex } from '../mutex'
import { diffRootMutations } from './rootMutation'
import type { PersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'
import type { Chat, Database, Message, botPreset, character } from './database.svelte'
import type {
    AssetAlias,
    AssetOwnerHead,
    CharacterDetail,
    ContentChangeKey,
    ConversationMutation,
    DataRevision,
    PersistentDataStore,
    PersistentRevisionReader,
    PluginStorageMutation,
    PluginStorageValue,
    PluginStorageValueCursor,
    PersistentRoot,
    WorkingSetCommit,
    PersistentUnitMutation,
    WholeMessageIntent,
} from './persistentDataStore'
import type { RisuModule } from '../process/modules'
import type { CommittedApplyOutcome } from './persistentDataRuntime'
import { CONTENT_CHANGE_PAGE_LIMIT, RevisionConflictError } from './persistentDataStore'
import { replaceArrayRange } from '../arrayRange'
import { appendCharacterIdToOrder, removeCharacterIdFromOrder } from './characterOrderMutation'
import {
    createConversationSummaryStubFromChat,
    isConversationSummaryStub,
} from './conversationResidency'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import { isMetadataOnlySelectedConversation } from './selectedConversationLifecycle'
import { applyConversationBindingPatch, conversationBindingHolds, type ConversationBindingPatch } from './conversationBinding'
import { withPersistentRevisionLease } from './persistentRecordIterator'
import type {
    ActiveConversationMutationEvent,
    ConversationSessionToken,
} from './activeConversationSession'
import { safeStructuredClone } from '../safeStructuredClone'
import {
    applyPluginStorageMutations,
    orderPluginStorageKeys,
    PluginStorageBaseline,
    PluginStorageCaptureCache,
    type PluginStorageCapture,
    canonicalClone,
    clonePersistentRootFields,
    canonicalDatabaseClone,
    canonicalJson,
    diffPluginStorage,
    messageReplaceRange,
    pluginStorageJson,
    rebaseConcurrentLiveDelta,
    rebaseConcurrentPluginStorage,
    rebaseRootMutation,
    splitDatabase,
} from './saveCoordinatorHelpers'

import {
    capturePluginMutationScope,
    rebasePluginMutationPublication,
} from './pluginMutationPublication'
import { PENDING_SAVE_BYTE_LIMIT as PENDING_BYTE_LIMIT } from './pendingDataSize'
import { jsonByteLength } from './nativePersistenceValue'
import { collectOwnedAssetKeys } from './deletedOwnerAssetCleanup'
import { captureMessagePages, cloneConversationByMessage, createsConversation, planConversationInsertPages, type ConversationInsertPlan } from './conversationInsertPages'

export { canonicalJson }

const SAVE_DEBOUNCE_MS = 500
/** Official publishes upload the full database snapshot, so they are spaced like upstream's save loop. */
const OFFICIAL_PUBLISH_MIN_INTERVAL_MS = 3_000
const CHARACTER_MUTATION_PAGE_SIZE = 100

type CompleteCharacter = character
type RootDatabase = PersistentRoot

export interface PinnedPublication {
    publish(signal?: AbortSignal): Promise<void>
    dispose(): Promise<void>
}

export interface OfficialRevisionPublisher {
    pin(revision: DataRevision): Promise<PinnedPublication>
}

export interface SaveCoordinatorClock {
    setTimeout(callback: () => void, delay: number): unknown
    clearTimeout(handle: unknown): void
}

export interface SaveCoordinatorDependencies {
    canonicalCapture?: PersistenceCanonicalCapture
    store: PersistentDataStore
    captureRoot(): RootDatabase
    capturePluginStorage?(): Database['pluginCustomStorage'] | null
    publishPluginStorageWorkingSet?(storage: Database['pluginCustomStorage']): void
    publishPluginStorageMutations?(
        mutations: readonly PluginStorageMutation[],
        keys: readonly string[],
    ): void
    capturePresetRecords?(): readonly botPreset[]
    capturePresets?(): botPreset[] | null
    captureCharacters?(): readonly CompleteCharacter[]
    beforeCapture?(): void
    captureSelectedCharacter(): CompleteCharacter | null
    captureSelectedConversationAuthority?(): WindowedConversationPersistenceAuthority | null
    captureCharacter(id: string): CompleteCharacter | null
    /** Installs the working copy synchronously and must not throw. */
    replaceDatabase(database: Database): void
    /** Publishes the committed preset state synchronously and must not throw. */
    publishPresetWorkingSet?(state: PersistentPresetMutationResult): void
    publishRootWorkingSet?(root: RootDatabase): void
    /** Publishes one committed character mutation synchronously and must not throw. */
    publishCharacterMutation?(state: PersistentCharacterMutationResult): void
    /** Publishes one committed conversation replacement synchronously and must not throw. */
    publishConversationReplacement?(result: PersistentConversationReplacementResult): void
    isIncompleteWorkingSet?(database: Database): boolean
    getNavigationGeneration?(): number
    officialPublisher?: OfficialRevisionPublisher
    clock?: SaveCoordinatorClock
    now?(): number
    /** Message bytes of created conversations in one commit of a split save; defaults to CONVERSATION_INSERT_PAGE_BYTES. */
    conversationInsertPageBytes?: number
    onRoutineUnitsCommitted?(revision: DataRevision, keys: readonly string[]): Promise<void>
    onLocalRevision?(revision: DataRevision): void
    /** Advances revision-only working-set state synchronously and must not throw. */
    onStorageOnlyRevision?(revision: DataRevision): void
    /** Advances an adopted windowed selected-conversation authority synchronously, with its new message count when that changed. */
    onWindowedSelectedConversationRevision?(revision: DataRevision, totalMessages?: number, preserveRows?: boolean): void
    /**
     * Replaces the selected windowed conversation with the given complete messages synchronously,
     * keeping its live metadata, and returns the published character. Returns null without
     * publishing when the selection no longer matches the authority.
     */
    completeWindowedSelectedConversation?(
        authority: WindowedConversationPersistenceAuthority,
        messages: Message[],
    ): CompleteCharacter | null
    onConversationMutationPersistenceStarted?(
        event: ActiveConversationMutationEvent,
    ): ConversationMutationPersistenceHandle | null | undefined
    onConversationMutationPersisted?(event: PersistedConversationMutationEvent): void
    onConversationMutationFallbackPersisted?(event: PersistedConversationMutationEvent): void
    onPersistenceIdle?(): void
    onFlushPromise?(promise: Promise<void> | null): void
    onBackgroundError?(error: unknown): void
    onLocalSaveFailure?(error: unknown | null): void
    onWorkingSetRefreshRequired?(revision: DataRevision | null): void
    onDestructiveReplacementFenceChanged?(active: boolean): void
    isConversationOperationActive?(): boolean
    deletedCharacterAssets?: {
        /** The asset keys the conversations of characters about to be deleted name; never rejects. */
        collectConversationKeys(characterIds: string[]): Promise<string[]>
        /** Deletes, in the background, the assets only permanently deleted characters used. */
        cleanup(keys: string[], startedAtMs: number): void
    }
}

export interface PersistedConversationMutationEvent {
    characterId: string
    conversationId: string
    sessionToken: ConversationSessionToken
    sessionVersion: number
    revision: DataRevision
}

export interface WindowedConversationPersistenceAuthority {
    kind: 'windowed'
    characterId: string
    conversationId: string
    sessionToken: ConversationSessionToken
    storeRevision: DataRevision
    persistedSessionVersion: number
    sessionVersion: number
    totalMessages: number
}

export interface WindowedConversationActivationChange {
    character?: {
        before: CharacterDetail
        after: CharacterDetail
    }
    conversation?: {
        before: Omit<Chat, 'message'>
        after: Omit<Chat, 'message'>
    }
}

export class WindowedConversationRequiresCompatibilityError extends Error {
    constructor(reason: string) {
        super(`Windowed conversation requires complete compatibility: ${reason}`)
        this.name = 'WindowedConversationRequiresCompatibilityError'
    }
}

export class WindowedConversationSaveError extends Error {
    constructor(reason: string, cause?: unknown) {
        super(`The selected conversation could not be completed for saving: ${reason}`, { cause })
        this.name = 'WindowedConversationSaveError'
    }
}

/** A conflict whose other writer changed records the commit replaces; `keys` name those changes as units. */
class ConcurrentRecordChangeError extends RevisionConflictError {
    constructor(expectedRevision: DataRevision, actualRevision: DataRevision, readonly keys: readonly string[], readonly characterIds: readonly string[]) {
        super(expectedRevision, actualRevision)
    }
}

export class SelectedConversationTransitionInProgressError extends Error {
    constructor() {
        super('Selected conversation authority transition is in progress')
        this.name = 'SelectedConversationTransitionInProgressError'
    }
}

type WindowedCharacterShell = Omit<CompleteCharacter, 'chats'> & {
    chats: Array<Omit<Chat, 'message'>>
}

interface WindowedSelectedCharacterCapture {
    shell: WindowedCharacterShell
    shellCanonical: string
    authority: WindowedConversationPersistenceAuthority
}

interface CapturedState {
    characterId?: string | null
    root: RootDatabase
    rootCanonical: string
    pluginStorage: Database['pluginCustomStorage'] | null
    pluginStorageCanonical: string | null
    pluginStorageCapture?: PluginStorageCapture | null
    presets: botPreset[] | null
    presetsCanonical: string | null
    character: CompleteCharacter | null
    characterCanonical: string | null
    characterSnapshot?: CompleteCharacter | null
    conversationStubIds: ReadonlySet<string>
    windowedCharacter: WindowedSelectedCharacterCapture | null
}

export interface CharacterAdditionRequest {
    characterId: string
    estimatedBytes: number
    install(): void
}

interface ReservedCharacterAddition {
    request: CharacterAdditionRequest | null
    token: object
}

interface PendingCharacterAddition {
    characterId: string
    token: object
    locallyAdded: boolean
    baseline: string | CompleteCharacter | null
}

interface PendingConversationMutation {
    event: ActiveConversationMutationEvent
    recordedAt: number
    /** The session version of every recorded event this entry carries, in order. */
    versions: number[]
    /** Running flushes whose captured list holds this entry; nothing folds into a captured entry. */
    captures: number
}

/**
 * Folds a later event into `target` when it only rewrites messages that the last range of
 * `target` inserted and keeps the message count, so a streamed reply keeps one entry with its
 * latest content and metadata. Returns false, leaving `target` unchanged, when it does not apply.
 */
function foldConversationMutation(
    target: PendingConversationMutation,
    next: ActiveConversationMutationEvent,
    versions: readonly number[],
): boolean {
    const event = target.event
    const last = event.mutations.at(-1)
    const range = next.mutations.length === 1 ? next.mutations[0] : undefined
    if (
        target.captures !== 0 ||
        !last ||
        !range ||
        event.characterId !== next.characterId ||
        event.conversationId !== next.conversationId ||
        event.sessionToken !== next.sessionToken ||
        next.previousVersion !== event.sessionVersion ||
        range.completeOwner ||
        event.mutations.some((mutation) => mutation.completeOwner) ||
        range.deleteCount !== range.messages.length ||
        range.start < last.start ||
        range.start + range.deleteCount > last.start + last.messages.length ||
        // A replacement without lastDate keeps the stored recency, so an earlier lastDate must still be written.
        (event.conversation.lastDate != null && !Number.isSafeInteger(next.conversation.lastDate))
    )
        return false
    const messages = last.messages.slice()
    replaceArrayRange(messages, range.start - last.start, range.deleteCount, range.messages)
    const { displayVariableUpdate, variableUpdate, ...rest } = event
    target.event = {
        ...rest,
        ...(displayVariableUpdate && next.displayVariableUpdate ? { displayVariableUpdate } : {}),
        ...(variableUpdate && next.variableUpdate
            ? { variableUpdate: {
                ...(variableUpdate.origin === next.variableUpdate.origin && variableUpdate.origin ? { origin: variableUpdate.origin } : {}),
                keys: [...new Set([...variableUpdate.keys, ...next.variableUpdate.keys])],
            } }
            : {}),
        sessionVersion: next.sessionVersion,
        commands: [...new Set([...event.commands, ...next.commands])],
        mutations: [...event.mutations.slice(0, -1), { ...last, messages, sessionVersion: next.sessionVersion }],
        conversation: next.conversation,
    }
    for (const version of versions) target.versions.push(version)
    return true
}

function releaseConversationMutationCaptures(held: PendingConversationMutation[]): void {
    for (const pending of held.splice(0)) pending.captures -= 1
}

interface PendingWindowedActivationChange {
    authority: WindowedConversationPersistenceAuthority
    change: WindowedConversationActivationChange
}

interface PendingCharacterRecency {
    characterId: string
    conversationId: string
    sessionToken: ConversationSessionToken
    before: number | undefined
    after: number
}

interface PendingWindowedChatListChange {
    characterId: string
    conversationId: string
    sessionToken: ConversationSessionToken
    beforeCanonical: string
    before: WindowedCharacterShell
    after: WindowedCharacterShell
    insertedMessages: Map<string, Message[]>
}

export interface ConversationMutationPersistenceHandle {
    release(): void
}

interface ConversationMutationProjection {
    exactMutations: ConversationMutation[] | null
    coveredPending: PendingConversationMutation[]
    character?: CharacterDetail
    coveredActivation?: PendingWindowedActivationChange
    coveredChatList?: PendingWindowedChatListChange
    unitMutations?: PersistentUnitMutation[]
    coveredRecency?: PendingCharacterRecency
}

function cloneOwnPropertiesExcept(
    value: Record<string, unknown>,
    excludedKey: string,
): Record<string, unknown> {
    const result: Record<string, unknown> = {}
    for (const key of Object.keys(value)) {
        if (key === excludedKey) continue
        defineOwnEnumerableProperty(result, key, safeStructuredClone(value[key]))
    }
    return result
}

function captureWindowedCharacterShell(character: CompleteCharacter): WindowedCharacterShell {
    const detail = cloneOwnPropertiesExcept(
        character as unknown as Record<string, unknown>,
        'chats',
    )
    const chats = character.chats.map((conversation) =>
        cloneOwnPropertiesExcept(
            conversation as unknown as Record<string, unknown>,
            'message',
        ) as Omit<Chat, 'message'>
    )
    return { ...detail, chats } as WindowedCharacterShell
}

function prepareWindowedActivationChange(
    currentShell: WindowedCharacterShell,
    authority: WindowedConversationPersistenceAuthority,
    change: WindowedConversationActivationChange,
): {
    persistedShell: WindowedCharacterShell
    change: WindowedConversationActivationChange
} | null {
    if (!change.character && !change.conversation) return null
    let persistedShell = safeStructuredClone(currentShell)
    const detached: WindowedConversationActivationChange = {}

    if (change.character) {
        const before = change.character.before as CharacterDetail & {
            chats?: unknown
        }
        const after = change.character.after as CharacterDetail & {
            chats?: unknown
        }
        const currentDetail = cloneOwnPropertiesExcept(
            currentShell as unknown as Record<string, unknown>,
            'chats',
        ) as CharacterDetail
        if (
            Object.hasOwn(before, 'chats') ||
            Object.hasOwn(after, 'chats') ||
            before.chaId !== authority.characterId ||
            after.chaId !== authority.characterId ||
            canonicalJson(after) !== canonicalJson(currentDetail) ||
            canonicalJson(before) === canonicalJson(after)
        )
            return null
        persistedShell = {
            ...safeStructuredClone(before),
            chats: persistedShell.chats,
        } as WindowedCharacterShell
        detached.character = {
            before: safeStructuredClone(before),
            after: safeStructuredClone(after),
        }
    }

    if (change.conversation) {
        const { before, after } = change.conversation
        const beforeRecord = before as Omit<Chat, 'message'> & {
            message?: unknown
        }
        const afterRecord = after as Omit<Chat, 'message'> & {
            message?: unknown
        }
        const currentMatches = currentShell.chats.filter(
            (conversation) => conversation.id === authority.conversationId,
        )
        const persistedMatches = persistedShell.chats.filter(
            (conversation) => conversation.id === authority.conversationId,
        )
        if (
            Object.hasOwn(beforeRecord, 'message') ||
            Object.hasOwn(afterRecord, 'message') ||
            before.id !== authority.conversationId ||
            after.id !== authority.conversationId ||
            currentMatches.length !== 1 ||
            persistedMatches.length !== 1 ||
            canonicalJson(after) !== canonicalJson(currentMatches[0]) ||
            canonicalJson(before) === canonicalJson(after)
        )
            return null
        persistedShell.chats[
            persistedShell.chats.indexOf(persistedMatches[0])
        ] = safeStructuredClone(before)
        detached.conversation = {
            before: safeStructuredClone(before),
            after: safeStructuredClone(after),
        }
    }

    return { persistedShell, change: detached }
}

function validWindowedAuthority(
    authority: WindowedConversationPersistenceAuthority,
): boolean {
    return authority.kind === 'windowed'
        && typeof authority.characterId === 'string'
        && authority.characterId.length > 0
        && typeof authority.conversationId === 'string'
        && authority.conversationId.length > 0
        && typeof authority.sessionToken === 'string'
        && authority.sessionToken.length > 0
        && Number.isSafeInteger(authority.storeRevision)
        && authority.storeRevision >= 0
        && Number.isSafeInteger(authority.persistedSessionVersion)
        && authority.persistedSessionVersion >= 0
        && Number.isSafeInteger(authority.sessionVersion)
        && authority.sessionVersion >= authority.persistedSessionVersion
        && Number.isSafeInteger(authority.totalMessages)
        && authority.totalMessages >= 0
}

/**
 * Matches stored changes to the records a commit replaces with captured values
 * rather than per-unit edits, or returns null when it replaces none.
 */
function replacedRecordChange(commit: WorkingSetCommit): ((key: ContentChangeKey) => boolean) | null {
    const checks: ((key: ContentChangeKey) => boolean)[] = []
    if (commit.root) checks.push((key) => key.kind === 'root')
    if (commit.replacePresets) checks.push((key) => key.kind === 'preset')
    const characters = new Set([commit.replaceCharacter, commit.addCharacter].flatMap((value) => value ? [value.chaId] : []))
    if (characters.size) checks.push((key) => (key.kind === 'character' || key.kind === 'conversation') && characters.has(key.key1))
    const details = new Set([commit.character, ...(commit.characterDetails ?? [])].flatMap((value) => value ? [value.chaId] : []))
    if (details.size) checks.push((key) => key.kind === 'character' && details.has(key.key1))
    const ranges = new Set((commit.conversations ?? []).flatMap((mutation) =>
        mutation.type === 'replace-range' ? [JSON.stringify([mutation.characterId, mutation.conversationId])] : []))
    if (ranges.size) checks.push((key) => key.kind === 'conversation' && ranges.has(JSON.stringify([key.key1, key.key2])))
    return checks.length ? (key) => checks.some((check) => check(key)) : null
}

function sameWindowedAuthority(
    left: WindowedConversationPersistenceAuthority,
    right: WindowedConversationPersistenceAuthority,
): boolean {
    return left.characterId === right.characterId
        && left.conversationId === right.conversationId
        && left.sessionToken === right.sessionToken
        && left.storeRevision === right.storeRevision
        && left.persistedSessionVersion === right.persistedSessionVersion
        && left.sessionVersion === right.sessionVersion
        && left.totalMessages === right.totalMessages
}

function messageMatchesAfterIdNormalization(
    projected: Message,
    captured: Message,
): boolean {
    if (projected === captured || canonicalJson(projected) === canonicalJson(captured)) return true
    if (projected.chatId !== undefined || typeof captured.chatId !== 'string' || !captured.chatId) {
        return false
    }
    const capturedWithoutId = safeStructuredClone(captured)
    delete capturedWithoutId.chatId
    return canonicalJson(projected) === canonicalJson(capturedWithoutId)
}

function conversationMatchesAfterIdNormalization(
    projected: Chat,
    captured: Chat,
): boolean {
    const { message: projectedMessages, ...projectedMetadata } = projected
    const { message: capturedMessages, ...capturedMetadata } = captured
    return (
        canonicalJson(projectedMetadata) === canonicalJson(capturedMetadata) &&
        (projectedMessages === capturedMessages || (
            projectedMessages.length === capturedMessages.length &&
            projectedMessages.every((message, index) =>
                messageMatchesAfterIdNormalization(message, capturedMessages[index]),
            )
        ))
    )
}

/** `canonicalJson(left) === canonicalJson(right)`, serializing only metadata and the messages the two do not share. */
function conversationJsonEquals(left: Chat, right: Chat): boolean {
    if (left.message !== right.message && !(Array.isArray(left.message) && Array.isArray(right.message)))
        return canonicalJson(left) === canonicalJson(right)
    const { message: leftMessages, ...leftMetadata } = left
    const { message: rightMessages, ...rightMetadata } = right
    if (canonicalJson(leftMetadata) !== canonicalJson(rightMetadata)) return false
    if (leftMessages === rightMessages) return true
    if (leftMessages.length !== rightMessages.length) return false
    for (let index = 0; index < leftMessages.length; index++) {
        const message = leftMessages[index]
        if (message !== rightMessages[index] && canonicalJson([message]) !== canonicalJson([rightMessages[index]])) return false
    }
    return true
}

/** A copy whose metadata is owned and whose message array is shared. */
function cloneConversationSharingMessages(conversation: Chat): Chat {
    const { message, ...metadata } = conversation
    return { ...safeStructuredClone(metadata), message } as Chat
}

/**
 * The conversation after the given recorded events, rebuilt from the baseline for comparison.
 * Its array is its own; its metadata is the last event's recorded value, which it only reads.
 */
function replayConversationPrefix(
    baseline: Chat,
    recorded: readonly (readonly (ConversationMutation & { type: 'replace-range' })[])[],
    sharesMessages: boolean,
): Chat {
    const message = sharesMessages ? baseline.message : baseline.message.slice()
    if (!sharesMessages) {
        for (const mutations of recorded) {
            for (const mutation of mutations) {
                replaceArrayRange(message, mutation.start, mutation.deleteCount, mutation.messages)
            }
        }
    }
    const { message: _message, ...metadata } = recorded.at(-1)!.at(-1)!.conversation as Chat
    return { ...metadata, message } as Chat
}

interface ReplacementAdmission extends PersistentMutationToken {
    authorityEpoch: number
    navigationGeneration: number | undefined
    baseline: CapturedState
    hadPendingDebounce: boolean
    supersededAdditionToken: object | null
}

export interface PreparedUnitIntent {
    unitMutations: readonly PersistentUnitMutation[]
    conversations: readonly ConversationMutation[]
}

export interface PersistentReplacementOptions {
    signal?: AbortSignal
    upstreamImport?: boolean
    upstreamImportWarnings?: string[]
    firstRun?: boolean
    onPluginsRestarted?(): void
    publishOfficial?: boolean
    authoritative?: boolean
    expectedRevision?: DataRevision
    expectedMutationGeneration?: number
    pluginStorageValues?: PluginStorageValue[]
}

export interface PersistentPresetMutationState {
    root: Omit<RootDatabase, 'botPresetsId' | 'selectedPersona'> & Pick<Database, 'botPresetsId' | 'selectedPersona'>
    presets: botPreset[]
}

export interface PersistentPresetMutationResult {
    root: RootDatabase
    presets: botPreset[]
    revision: DataRevision
}

export type PersistentPresetMutation = (
    state: PersistentPresetMutationState,
) => void | Promise<void>

export interface PersistentCharacterMutationState {
    root: RootDatabase
    character: CharacterDetail
}

export interface PersistentCharacterMutationResult {
    revision: DataRevision
    root: RootDatabase
    characterId: string
    kind: 'detail' | 'replace' | 'add' | 'delete'
    character: CharacterDetail | CompleteCharacter | null
}

export interface PersistentMutationToken {
    revision: DataRevision
    mutationGeneration: number
}

export class PersistentMutationFencedError extends Error {
    constructor() {
        super('A destructive persistent replacement is active')
        this.name = 'PersistentMutationFencedError'
    }
}

export interface PersistentDatabaseSnapshot extends PersistentMutationToken {
    database: Database
    pluginStorageValues?: PluginStorageValue[]
}

export interface PersistentDatabaseMaterializationOptions {
    includePluginStorageValues?: boolean
}

export interface PersistentSelectedConversation {
    character: CharacterDetail
    conversation: Chat | null
}

export type PersistentCharacterDetailMutation = (
    state: PersistentCharacterMutationState,
) => void | { delete: true } | Promise<void | { delete: true }>

export type PersistentCompleteCharacterMutation = (
    character: CompleteCharacter,
) => CompleteCharacter | Promise<CompleteCharacter>

export interface PersistentScopedReplacementOptions {
    expectedRevision?: DataRevision
}

export interface PersistentConversationReplacementResult {
    revision: DataRevision
    characterId: string
    conversationId: string
    conversation: Chat
}

export type PersistentCompleteCharacterUpsert = (
    character: CompleteCharacter | null,
) => CompleteCharacter | Promise<CompleteCharacter>

export type PersistentCharacterAssetAlias = Extract<AssetAlias, { kind: 'asset' }>
export type PersistentCharacterAssetOwnerHead = AssetOwnerHead & {
    owner: {
        kind: 'character-additional-assets'
        characterId: string
    }
}

export interface PersistentCompleteCharacterUpsertOptions {
    includeInCharacterOrder?: boolean
    assetAliases?: readonly PersistentCharacterAssetAlias[]
    assetOwnerHeads?: readonly PersistentCharacterAssetOwnerHead[]
}

export interface PersistentRootModuleAppend {
    module: RisuModule
    assetAliases: readonly Extract<AssetAlias, { kind: 'asset' }>[]
    ownerHead: Omit<AssetOwnerHead, 'owner'>
}

export class PersistentRootModuleAppendRejectedError extends Error {
    constructor(message: string) {
        super(message)
        this.name = 'PersistentRootModuleAppendRejectedError'
    }
}

function persistentRootModuleAppendRejected(error: unknown): PersistentRootModuleAppendRejectedError {
    if (error instanceof PersistentRootModuleAppendRejectedError) return error
    return new PersistentRootModuleAppendRejectedError(
        error instanceof Error ? error.message : String(error),
    )
}

function defaultClock(): SaveCoordinatorClock {
    return {
        setTimeout: (callback, delay) => globalThis.setTimeout(callback, delay),
        clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
    }
}

export class SaveCoordinator {
    private readonly clock: SaveCoordinatorClock
    private currentRevision: DataRevision | null = null
    private authorityEpoch = 0
    private committedRefreshRevision: DataRevision | null = null
    private rootBaseline: string | null = null
    private pluginStorageBaselineEntries: PluginStorageBaseline | null = null
    private readonly pluginStorageCaptureCache = new PluginStorageCaptureCache()
    private get pluginStorageBaseline(): string | null {
        return this.pluginStorageBaselineEntries?.json ?? null
    }
    private set pluginStorageBaseline(value: string | null) {
        this.pluginStorageBaselineEntries = value === null ? null : new PluginStorageBaseline(value)
    }
    private readonly presetRecordBaselines = new Map<string, botPreset>()
    private presetsBaseline: string | null = null
    private readonly materializedCanonicalBaselines = new Map<string, string>()
    private readonly materializedBaselines = new Map<string, CompleteCharacter>()
    private characterBaseline: string | null = null
    private characterBaselineId: string | null = null
    private windowedCharacterBaseline: WindowedSelectedCharacterCapture | null = null
    private pendingWindowedActivationChange: PendingWindowedActivationChange | null = null
    private pendingWindowedChatListChange: PendingWindowedChatListChange | null = null
    private pendingCharacterRecency: PendingCharacterRecency | null = null
    private dirtyGeneration = 0
    private persistedDirtyGeneration = 0
    private backgroundRetryDelay = 2_000
    private flushAfterFenceRelease = false
    private publishAfterFenceRelease = false
    private pendingByteCount = 0
    private debounceHandle: unknown
    private readonly operationMutex = new Mutex()
    private flushPromise: Promise<void> | null = null
    private localFlushPromise: Promise<void> | null = null
    private localFlushDuringPublicationPromise: Promise<void> | null = null
    private queuedOperationCount = 0
    private queuedStorageOnlyOperationCount = 0
    private publicationInProgress = false
    private readonly operationStateWaiters = new Set<() => void>()
    private additionPromise: Promise<void> | null = null
    private lastReportedFlushPromise: Promise<void> | null = null
    private pendingPublication: PinnedPublication | null = null
    private pendingPublicationRevision: DataRevision | null = null
    private deferredPublicationRevision: DataRevision | null = null
    private readonly pendingPublicationCleanup = new Set<PinnedPublication>()
    private lastOfficialPublishAttemptAt: number | null = null
    private officialPublishRetryHandle: unknown
    private pendingCharacterAddition: PendingCharacterAddition | null = null
    private reservedCharacterAddition: ReservedCharacterAddition | null = null
    private pendingConversationMutations: PendingConversationMutation[] = []
    private lastBackgroundErrorMessage: string | null = null
    private localSaveFailure: unknown | null = null
    private destructiveReplacementFenceState: {
        owner: symbol
        state: 'acquiring' | 'held'
        blockedPrePublicationDirty: boolean
        refreshBaseline?: CapturedState
    } | null = null
    private get destructiveReplacementFence() {
        return this.destructiveReplacementFenceState
    }
    private set destructiveReplacementFence(value: SaveCoordinator['destructiveReplacementFenceState']) {
        this.destructiveReplacementFenceState = value
        try {
            this.dependencies.onDestructiveReplacementFenceChanged?.(value !== null)
        } catch (error) {
            this.reportBackgroundError(error)
        }
    }
    private activePausedWriteToken: PersistentMutationToken | null = null
    private activatedLibraryGuardOwner: symbol | null = null
    private selectedConversationTransitionActive = false
    private persistenceWasBusy = false

    constructor(private readonly dependencies: SaveCoordinatorDependencies) {
        this.clock = dependencies.clock ?? defaultClock()
    }

    get revision(): DataRevision {
        if (this.currentRevision === null) throw new Error('Save coordinator is not initialized')
        return this.currentRevision
    }

    get storageAuthorityEpoch(): number {
        return this.authorityEpoch
    }

    get pendingWorkingSetRefreshRevision(): DataRevision | null {
        return this.committedRefreshRevision
    }

    markCommittedWorkingSetRefreshRequired(revision: DataRevision, error: unknown): void {
        if (revision > this.revision) {
            this.currentRevision = revision
            this.authorityEpoch++
        }
        this.committedRefreshRevision = Math.max(revision, this.revision)
        this.cancelDebounce()
        try {
            this.dependencies.onWorkingSetRefreshRequired?.(this.committedRefreshRevision)
        } catch (notificationError) {
            this.reportBackgroundError(notificationError)
        }
        this.reportBackgroundError(error)
    }

    get pendingBytes(): number {
        return this.pendingByteCount
    }

    get mutationGeneration(): number {
        return this.dirtyGeneration
    }

    get hasDestructiveReplacementFence(): boolean {
        return this.destructiveReplacementFence !== null
    }

    get hasPendingPersistenceWork(): boolean {
        return this.queuedStorageOnlyOperationCount > 0 || this.hasPendingWorkingSetPersistence
    }

    /** Pending work apart from queued storage-only mutations, which carry no working-set edit. */
    private get hasPendingWorkingSetPersistence(): boolean {
        return (
            this.dirtyGeneration !== this.persistedDirtyGeneration ||
            this.pendingByteCount > 0 ||
            this.debounceHandle !== undefined ||
            this.flushPromise !== null ||
            this.localFlushPromise !== null ||
            this.localFlushDuringPublicationPromise !== null ||
            this.queuedOperationCount > this.queuedStorageOnlyOperationCount ||
            this.additionPromise !== null ||
            this.pendingCharacterAddition !== null ||
            this.reservedCharacterAddition !== null ||
            this.pendingConversationMutations.length > 0 ||
            this.pendingWindowedActivationChange !== null ||
            this.pendingWindowedChatListChange !== null
        )
    }

    get isSelectedConversationTransitionActive(): boolean {
        return this.selectedConversationTransitionActive
    }

    runSelectedConversationTransition<T>(transition: () => T): T {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        if (this.hasPendingPersistenceWork) {
            throw new Error('Selected conversation authority transition has pending persistence')
        }
        const saved = {
            characterBaseline: this.characterBaseline,
            characterBaselineId: this.characterBaselineId,
            windowedCharacterBaseline: this.windowedCharacterBaseline
                ? safeStructuredClone(this.windowedCharacterBaseline)
                : null,
            pendingWindowedActivationChange: this.pendingWindowedActivationChange
                ? safeStructuredClone(this.pendingWindowedActivationChange)
                : null,
        }
        this.selectedConversationTransitionActive = true
        try {
            return transition()
        } catch (error) {
            this.characterBaseline = saved.characterBaseline
            this.characterBaselineId = saved.characterBaselineId
            this.windowedCharacterBaseline = saved.windowedCharacterBaseline
            this.pendingWindowedActivationChange = saved.pendingWindowedActivationChange
            throw error
        } finally {
            this.selectedConversationTransitionActive = false
        }
    }

    initialize(revision: DataRevision, database?: Database): void {
        if (this.committedRefreshRevision !== null && revision < this.committedRefreshRevision) {
            throw new RevisionConflictError(this.committedRefreshRevision, revision)
        }
        this.cancelDebounce()
        this.cancelOfficialPublishRetry()
        this.pluginStorageCaptureCache.clear()
        const captured = database ? this.captureDatabase(database) : this.capture()
        if (captured.pluginStorageCapture) {
            this.dependencies.canonicalCapture?.seedPluginStorage?.(captured.pluginStorageCapture)
        }
        const recoveringSameRevision = this.committedRefreshRevision === revision
        const retainPublication = recoveringSameRevision && this.pendingPublicationRevision === revision
        const retainedDeferredRevision = recoveringSameRevision && this.deferredPublicationRevision === revision
            ? revision
            : null
        this.authorityEpoch++
        this.currentRevision = revision
        this.rootBaseline = captured.rootCanonical
        this.pluginStorageBaselineEntries = captured.pluginStorageCapture
            ? new PluginStorageBaseline(captured.pluginStorageCapture)
            : captured.pluginStorageCanonical === null
              ? null
              : new PluginStorageBaseline(captured.pluginStorageCanonical)
        this.presetsBaseline = captured.presetsCanonical
        this.presetRecordBaselines.clear()
        for (const value of this.dependencies.capturePresetRecords?.() ?? []) if (typeof value['id'] === 'string') this.presetRecordBaselines.set(value['id'], canonicalClone(value))
        this.setCharacterBaseline(captured)
        this.materializedCanonicalBaselines.clear()
        for (const [id, json] of (this.dependencies.canonicalCapture?.materializedCharacters ? [] : this.dependencies.canonicalCapture?.characters?.()) ?? []) this.materializedCanonicalBaselines.set(id, json)
        this.materializedBaselines.clear()
        for (const value of database?.characters ?? this.dependencies.captureCharacters?.() ?? []) {
            if (this.dependencies.captureCharacter(value.chaId)) this.materializedBaselines.set(value.chaId, this.dependencies.canonicalCapture?.materializedCharacters?.().get(value.chaId) ?? captureMaterializedCharacter(value))
        }
        if (captured.windowedCharacter) {
            this.setWindowedCharacterBaseline(captured.windowedCharacter, revision,
                captured.windowedCharacter.authority.persistedSessionVersion)
        }
        this.dirtyGeneration = 0
        this.persistedDirtyGeneration = 0
        this.setLocalSaveFailure(null)
        this.backgroundRetryDelay = 2_000
        this.pendingByteCount = 0
        if (!retainPublication) {
            if (this.pendingPublication) {
                this.pendingPublicationCleanup.add(this.pendingPublication)
            }
            this.pendingPublication = null
            this.pendingPublicationRevision = null
        }
        this.deferredPublicationRevision = retainedDeferredRevision
        if (!recoveringSameRevision) this.lastOfficialPublishAttemptAt = null
        this.pendingCharacterAddition = null
        this.reservedCharacterAddition = null
        this.pendingConversationMutations = []
        this.pendingWindowedActivationChange = null
        this.pendingWindowedChatListChange = null
        this.pendingCharacterRecency = null
        this.persistenceWasBusy = false
        this.lastBackgroundErrorMessage = null
        // A destructive fence compares against the baselines reset above, so a
        // later refresh under the same fence sees the installed state as clean.
        if (this.destructiveReplacementFence?.state === 'held' && this.destructiveReplacementFence.refreshBaseline) {
            this.destructiveReplacementFence.refreshBaseline = captured
        }
        if (this.committedRefreshRevision !== null && !this.activatedLibraryGuardOwner) {
            this.committedRefreshRevision = null
            try {
                this.dependencies.onWorkingSetRefreshRequired?.(null)
            } catch (error) {
                this.reportBackgroundError(error)
            }
        }
        if (!this.destructiveReplacementFence && this.hasPendingOfficialPublication) {
            this.armOfficialPublishRetry(this.officialPublishDelayMs())
        }
        this.armPublicationCleanupRetryIfNeeded()
    }

    runStorageOnlyMutation(
        operation: (expectedRevision: DataRevision) => Promise<DataRevision>,
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        return this.enqueue(async () => {
            const expectedRevision = this.revision
            const revision = await operation(expectedRevision)
            if (!Number.isSafeInteger(revision) || revision < 0) {
                throw new RangeError('Storage-only mutation returned an invalid revision')
            }
            if (revision < expectedRevision) {
                throw new RevisionConflictError(expectedRevision, revision)
            }
            if (revision === expectedRevision) return
            this.currentRevision = revision
            this.dependencies.onStorageOnlyRevision?.(revision)
            this.dependencies.onLocalRevision?.(revision)
        }, this.authorityEpoch, true)
    }

    adoptHydratedCharacter(
        revision: DataRevision,
        mutationGeneration: number,
        character: CompleteCharacter,
    ): boolean {
        if (
            this.destructiveReplacementFence !== null ||
            this.currentRevision !== revision ||
            this.dirtyGeneration !== mutationGeneration ||
            (this.dependencies.captureSelectedConversationAuthority?.() ?? null) !== null
        )
            return false
        // The capture describes the resident character; adopting it for a separate copy would
        // take in live edits the copy lacks, so a copy is still copied.
        const captured = this.dependencies.captureCharacter(character.chaId) === character
            ? this.dependencies.canonicalCapture?.materializedCharacters?.().get(character.chaId)
            : undefined
        this.materializedBaselines.set(character.chaId, captured ?? captureMaterializedCharacter(character))
        this.characterBaseline = this.dependencies.canonicalCapture?.materializedCharacters ? null : canonicalJson(character)
        this.characterBaselineId = character.chaId
        this.windowedCharacterBaseline = null
        this.pendingWindowedActivationChange = null
        this.pendingWindowedChatListChange = null
        this.pendingCharacterRecency = null
        return true
    }

    adoptWindowedSelectedConversation(
        revision: DataRevision,
        mutationGeneration: number,
        character: CompleteCharacter,
        authority: WindowedConversationPersistenceAuthority,
        activationChange?: WindowedConversationActivationChange,
    ): boolean {
        if (
            this.destructiveReplacementFence !== null ||
            this.currentRevision !== revision ||
            this.dirtyGeneration !== mutationGeneration ||
            !validWindowedAuthority(authority) ||
            authority.storeRevision !== revision ||
            authority.sessionVersion !== authority.persistedSessionVersion ||
            character.chaId !== authority.characterId ||
            this.pendingByteCount > 0 ||
            this.pendingConversationMutations.length > 0 ||
            this.pendingCharacterAddition !== null
        )
            return false
        const currentAuthority = this.dependencies.captureSelectedConversationAuthority?.() ?? null
        const currentCharacter = this.dependencies.captureSelectedCharacter()
        if (
            !currentAuthority ||
            !validWindowedAuthority(currentAuthority) ||
            !sameWindowedAuthority(currentAuthority, authority) ||
            currentCharacter?.chaId !== authority.characterId
        )
            return false
        if (this.dirtyGeneration !== this.persistedDirtyGeneration) return false
        const ownedShell = this.dependencies.canonicalCapture?.characterShell?.()
        const shell = character === currentCharacter && ownedShell ? ownedShell.value : captureWindowedCharacterShell(character)
        const currentShell = ownedShell?.value ?? captureWindowedCharacterShell(currentCharacter)
        const matchingConversations = shell.chats.filter(
            (conversation) => conversation.id === authority.conversationId,
        )
        if (
            matchingConversations.length !== 1 ||
            canonicalJson(shell) !== canonicalJson(currentShell)
        )
            return false
        let persistedShell = shell
        let pendingActivation: PendingWindowedActivationChange | null = null
        if (activationChange) {
            const prepared = prepareWindowedActivationChange(shell, authority, activationChange)
            if (!prepared) return false
            persistedShell = prepared.persistedShell
            pendingActivation = {
                authority: safeStructuredClone(authority),
                change: prepared.change,
            }
        }
        this.windowedCharacterBaseline = {
            shell: persistedShell,
            shellCanonical: canonicalJson(persistedShell),
            authority: safeStructuredClone(authority),
        }
        this.characterBaseline = null
        this.characterBaselineId = null
        this.pendingWindowedActivationChange = pendingActivation
        return true
    }

    retireWindowedSelectedConversation(): void {
        this.assertPersistentMutationAllowed()
        if (this.hasPendingWorkingSetPersistence || !this.captureMatchesBaseline()) {
            throw new Error('Selected conversation ownership has pending persistence')
        }
        this.windowedCharacterBaseline = null
        this.characterBaseline = null
        this.characterBaselineId = null
        this.pendingWindowedActivationChange = null
        this.pendingWindowedChatListChange = null
        this.pendingCharacterRecency = null
    }

    /** Applied units removed the selected conversation, or its character, so the ownership adopted for it has nothing left to persist. */
    releaseRemovedSelectedConversation(characterRemoved: boolean): void {
        this.windowedCharacterBaseline = null
        this.pendingWindowedActivationChange = null
        this.pendingWindowedChatListChange = null
        this.pendingCharacterRecency = null
        if (!characterRemoved) return
        this.characterBaseline = null
        this.characterBaselineId = null
    }

    advanceWindowedSelectedConversationRevision(
        revision: DataRevision,
        authority: WindowedConversationPersistenceAuthority,
    ): boolean {
        const baseline = this.windowedCharacterBaseline
        const current = this.dependencies.captureSelectedConversationAuthority?.() ?? null
        const pendingActivation = this.pendingWindowedActivationChange
        if (
            !baseline ||
            !current ||
            !validWindowedAuthority(authority) ||
            !validWindowedAuthority(current) ||
            this.currentRevision !== revision ||
            authority.storeRevision !== revision ||
            !sameWindowedAuthority(current, authority) ||
            baseline.authority.characterId !== authority.characterId ||
            baseline.authority.conversationId !== authority.conversationId ||
            baseline.authority.sessionToken !== authority.sessionToken ||
            baseline.authority.persistedSessionVersion !== authority.persistedSessionVersion ||
            baseline.authority.sessionVersion !== authority.sessionVersion ||
            baseline.authority.totalMessages !== authority.totalMessages ||
            baseline.authority.storeRevision > revision ||
            (pendingActivation !== null &&
                !sameWindowedAuthority(pendingActivation.authority, baseline.authority))
        )
            return false
        baseline.authority = safeStructuredClone(authority)
        if (pendingActivation) {
            pendingActivation.authority = {
                ...safeStructuredClone(pendingActivation.authority),
                storeRevision: revision,
            }
        }
        return true
    }

    adoptMaterializedDatabase(
        revision: DataRevision,
        mutationGeneration: number,
        database: Database,
    ): boolean {
        if (
            this.destructiveReplacementFence !== null ||
            this.currentRevision !== revision ||
            this.dirtyGeneration !== mutationGeneration
        )
            return false
        const captured = this.captureDatabase(database)
        this.rootBaseline = captured.rootCanonical
        this.pluginStorageBaseline = captured.pluginStorageCanonical
        this.presetsBaseline = captured.presetsCanonical
        this.setCharacterBaseline(captured)
        return true
    }

    markPersistentDataDirty(estimatedBytes: number): void {
        this.assertInitialized()
        this.assertSelectedConversationTransitionInactive()
        if (this.committedRefreshRevision !== null) throw new PersistentMutationFencedError()
        const fence = this.destructiveReplacementFence
        if (fence?.state === 'acquiring') throw new PersistentMutationFencedError()
        if (fence?.state === 'held') {
            const matchesFenceBaseline = fence.refreshBaseline
                ? this.captureMatchesCapturedState(fence.refreshBaseline)
                : this.captureMatchesBaseline()
            if (!matchesFenceBaseline) fence.blockedPrePublicationDirty = true
            throw new PersistentMutationFencedError()
        }
        this.dirtyGeneration++
        this.persistenceWasBusy = true
        const bytes = Number.isFinite(estimatedBytes) && estimatedBytes > 0 ? estimatedBytes : 0
        const previousBytes = this.pendingByteCount
        this.pendingByteCount = Math.max(previousBytes, bytes)
        this.cancelDebounce()
        if (this.pendingByteCount >= PENDING_BYTE_LIMIT && previousBytes < PENDING_BYTE_LIMIT) {
            this.startBackgroundFlush('byte-limit')
            return
        }
        if (!this.flushPromise) this.armDebounce()
    }

    recordSelectedCharacterLastInteraction(
        authority: WindowedConversationPersistenceAuthority,
        before: number | undefined,
        after: number,
    ): boolean {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const current = this.dependencies.captureSelectedConversationAuthority?.()
        const character = this.dependencies.captureSelectedCharacter()
        const baseline = this.windowedCharacterBaseline
        const pending = this.pendingCharacterRecency
        if (!current || !baseline || !character || !Number.isFinite(after) || after < 0 ||
            !sameWindowedAuthority(current, authority) || authority.storeRevision !== this.revision ||
            baseline.authority.sessionToken !== authority.sessionToken ||
            baseline.authority.characterId !== authority.characterId || baseline.authority.conversationId !== authority.conversationId ||
            character.chaId !== authority.characterId || character.lastInteraction !== before ||
            (pending && (pending.sessionToken !== authority.sessionToken || pending.after !== before))) return false
        const persisted = pending?.before ?? this.pendingWindowedActivationChange?.change.character?.after.lastInteraction ?? baseline.shell.lastInteraction
        if (!pending && persisted !== before) return false
        if (before === after) return true
        this.markPersistentDataDirty(32)
        this.pendingCharacterRecency = {
            characterId: authority.characterId, conversationId: authority.conversationId,
            sessionToken: authority.sessionToken, before: pending ? pending.before : before, after,
        }
        character.lastInteraction = after
        return true
    }

    recordActiveConversationMutation(
        event: ActiveConversationMutationEvent,
        estimatedBytes = 0,
    ): void {
        this.assertInitialized()
        if (!event.characterId || !event.conversationId || !event.sessionToken) {
            throw new Error('Conversation mutation ownership evidence is incomplete')
        }
        if (
            !Number.isSafeInteger(event.previousVersion) ||
            event.previousVersion < 0 ||
            !Number.isSafeInteger(event.sessionVersion) ||
            event.sessionVersion <= event.previousVersion
        ) {
            throw new RangeError('Conversation mutation versions are invalid')
        }
        if (event.mutations.length === 0) {
            throw new RangeError('Conversation mutation has no replacement evidence')
        }
        let previousMutationVersion = event.previousVersion
        for (const mutation of event.mutations) {
            if (
                !Number.isSafeInteger(mutation.start) ||
                mutation.start < 0 ||
                !Number.isSafeInteger(mutation.deleteCount) ||
                mutation.deleteCount < 0 ||
                !Number.isSafeInteger(mutation.sessionVersion) ||
                mutation.sessionVersion !== previousMutationVersion + 1 ||
                mutation.sessionVersion > event.sessionVersion ||
                (mutation.completeOwner === true && mutation.start !== 0)
            ) {
                throw new RangeError('Conversation replacement evidence is invalid')
            }
            previousMutationVersion = mutation.sessionVersion
        }
        if (previousMutationVersion !== event.sessionVersion) {
            throw new RangeError(
                'Conversation replacement evidence does not reach its session version',
            )
        }
        const previous = this.pendingConversationMutations.findLast(
            (pending) =>
                pending.event.characterId === event.characterId &&
                pending.event.conversationId === event.conversationId,
        )
        if (previous) {
            const continuesSession =
                previous.event.sessionToken === event.sessionToken &&
                previous.event.sessionVersion === event.previousVersion
            const startsReplacementSession =
                previous.event.sessionToken !== event.sessionToken && event.previousVersion === 0
            if (!continuesSession && !startsReplacementSession) {
                throw new Error('Conversation mutation session sequence changed before persistence')
            }
        }

        const detached = safeStructuredClone(event)
        this.markPersistentDataDirty(estimatedBytes)
        const newest = this.pendingConversationMutations.at(-1)
        if (newest && foldConversationMutation(newest, detached, [detached.sessionVersion])) return
        this.pendingConversationMutations.push({
            event: detached,
            recordedAt: this.currentTime(),
            versions: [detached.sessionVersion],
            captures: 0,
        })
    }

    /** Folds adjacent uncaptured entries, so retries after a failed save do not pile up entries. */
    private foldPendingConversationMutations(): void {
        const folded: PendingConversationMutation[] = []
        for (const pending of this.pendingConversationMutations) {
            const previous = folded.at(-1)
            if (
                previous &&
                pending.captures === 0 &&
                foldConversationMutation(previous, pending.event, pending.versions)
            )
                continue
            folded.push(pending)
        }
        if (folded.length !== this.pendingConversationMutations.length) {
            this.pendingConversationMutations = folded
        }
    }

    /**
     * Records a chat-list edit of the windowed selected character as exact evidence.
     * `after` carries the complete bodies of added chats. Returns false when the edit
     * cannot be described against the persisted baseline, before anything is recorded.
     */
    recordWindowedChatListChange(
        authority: WindowedConversationPersistenceAuthority,
        before: CompleteCharacter,
        after: CompleteCharacter,
    ): boolean {
        this.assertInitialized()
        const baseline = this.windowedCharacterBaseline
        if (
            this.destructiveReplacementFence !== null ||
            this.committedRefreshRevision !== null ||
            this.selectedConversationTransitionActive ||
            !baseline ||
            !validWindowedAuthority(authority) ||
            !sameWindowedAuthority(authority, baseline.authority) ||
            authority.sessionVersion !== authority.persistedSessionVersion ||
            authority.storeRevision !== this.revision ||
            this.pendingConversationMutations.length > 0 ||
            this.pendingWindowedActivationChange !== null ||
            this.pendingWindowedChatListChange !== null ||
            before.chaId !== authority.characterId ||
            after.chaId !== authority.characterId
        )
            return false
        const beforeShell = captureWindowedCharacterShell(before)
        const beforeCanonical = canonicalJson(beforeShell)
        if (beforeCanonical !== baseline.shellCanonical) return false
        const afterShell = captureWindowedCharacterShell(after)
        if (canonicalJson(afterShell) === beforeCanonical) return true

        const afterIds = new Set<string>()
        for (const conversation of afterShell.chats) {
            if (!conversation.id || afterIds.has(conversation.id)) return false
            afterIds.add(conversation.id)
        }
        if (!afterIds.has(authority.conversationId)) return false
        const beforeIds = new Set(beforeShell.chats.map((conversation) => conversation.id))
        const insertedMessages = new Map<string, Message[]>()
        let estimatedBytes = jsonByteLength(afterShell)
        for (const conversation of after.chats) {
            if (beforeIds.has(conversation.id)) continue
            if (
                isConversationSummaryStub(conversation) ||
                isMetadataOnlySelectedConversation(conversation) ||
                !Array.isArray(conversation.message)
            )
                return false
            const pages = captureMessagePages(conversation.message, undefined, true)
            insertedMessages.set(conversation.id!, pages.flatMap((page) => page.messages))
            for (const page of pages) estimatedBytes += page.bytes
        }
        this.markPersistentDataDirty(estimatedBytes)
        this.pendingWindowedChatListChange = {
            characterId: authority.characterId,
            conversationId: authority.conversationId,
            sessionToken: authority.sessionToken,
            beforeCanonical,
            before: beforeShell,
            after: afterShell,
            insertedMessages,
        }
        return true
    }

    flushPendingData(reason: string): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        if (this.additionPromise) return this.additionPromise
        if (this.flushPromise) return this.flushPromise
        const promise = this.enqueue(() => this.flushIterations(reason, true))
        this.trackPromiseSlot(
            promise,
            () => this.flushPromise,
            (value) => {
                this.flushPromise = value
            },
        )
        return promise
    }

    flushPendingDataLocally(reason: string): Promise<void> {
        this.assertInitialized()
        try {
            this.assertPersistentMutationAllowed()
        } catch (error) {
            return Promise.reject(error)
        }
        this.cancelDebounce()
        if (this.localFlushPromise) return this.localFlushPromise
        const promise = this.runLocalFlush(reason)
        this.trackPromiseSlot(
            promise,
            () => this.localFlushPromise,
            (value) => {
                this.localFlushPromise = value
            },
        )
        return promise
    }

    /**
     * A local flush at a generation checkpoint. A rejection takes the background save path, which
     * reports it and schedules any retry; a storage failure then resolves false and a fence rejects.
     */
    async flushPendingDataLocallyForCheckpoint(reason: string): Promise<boolean> {
        try {
            await this.flushPendingDataLocally(reason)
            return true
        } catch (error) {
            this.handleBackgroundSaveFailure(error)
            if (error instanceof PersistentMutationFencedError) throw error
            return false
        }
    }

    /** Whether complete conversations are saved against immutable captured values instead of being replaced whole. */
    get capturesImmutableCharacters(): boolean {
        return this.dependencies.canonicalCapture?.materializedCharacters !== undefined
    }

    /** Unacknowledged conversation mutation events and the age of the oldest, for diagnostics. */
    get pendingConversationMutationDiagnostics(): { pendingEvents: number; unacknowledgedMs?: number } {
        const oldest = this.pendingConversationMutations[0]
        return {
            pendingEvents: this.pendingConversationMutations.reduce(
                (count, pending) => count + pending.versions.length,
                0,
            ),
            unacknowledgedMs: oldest ? Math.max(0, this.currentTime() - oldest.recordedAt) : undefined,
        }
    }

    private async runLocalFlush(reason: string): Promise<void> {
        const authorityEpoch = this.authorityEpoch
        while (this.queuedOperationCount > 0 && !this.publicationInProgress) {
            await this.waitForOperationStateChange()
            this.assertQueuedMutationAllowed(authorityEpoch)
        }
        this.assertPersistentMutationAllowed(authorityEpoch)
        if (this.publicationInProgress) {
            const promise = this.flushIterations(reason, false)
            this.localFlushDuringPublicationPromise = promise
            try {
                await promise
                return
            } finally {
                if (this.localFlushDuringPublicationPromise === promise) {
                    this.localFlushDuringPublicationPromise = null
                }
            }
        }
        await this.enqueue(async () => {
            this.assertQueuedMutationAllowed(authorityEpoch)
            await this.flushIterations(reason, false)
        }, authorityEpoch)
    }

    replacePersistentDatabase(
        database: Database,
        reason: string,
        options: PersistentReplacementOptions = {},
    ): Promise<CommittedApplyOutcome> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        options = { ...options }
        if (options.pluginStorageValues) {
            options.pluginStorageValues = options.pluginStorageValues.map((entry) => ({
                owner: entry.owner,
                key: entry.key,
                value: canonicalClone(entry.value),
            }))
        }
        const expectationError = this.replacementExpectationError(options)
        if (expectationError) return Promise.reject(expectationError)
        if (!options.authoritative && this.dependencies.isIncompleteWorkingSet?.(database)) {
            return Promise.reject(
                new Error(
                    'Cannot replace persistent data from an incomplete persistent working set',
                ),
            )
        }
        const candidate = canonicalDatabaseClone(database)
        return this.enqueueReplacement(candidate, this.captureReplacementAdmission(options), reason, options)
    }

    replacePreparedPersistentDatabase(
        prepare: () => Promise<Database>,
        reason: string,
        options: PersistentReplacementOptions = {},
    ): Promise<CommittedApplyOutcome> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        options = { ...options }
        const expectationError = this.replacementExpectationError(options)
        if (expectationError) return Promise.reject(expectationError)
        const admission = this.captureReplacementAdmission(options)
        // Detach immediately, but do not reserve the write queue or pause local autosave.
        return (async () => {
            const prepared = await prepare()
            if (!options.authoritative && this.dependencies.isIncompleteWorkingSet?.(prepared)) {
                throw new Error(
                    'Cannot replace persistent data from an incomplete persistent working set',
                )
            }
            const candidate = canonicalDatabaseClone(prepared)
            return this.enqueueReplacement(candidate, admission, reason, options)
        })()
    }

    private captureReplacementAdmission(options: PersistentReplacementOptions): ReplacementAdmission {
        return {
            revision: options.expectedRevision ?? this.revision,
            mutationGeneration: options.expectedMutationGeneration ?? this.dirtyGeneration,
            authorityEpoch: this.authorityEpoch,
            navigationGeneration: this.dependencies.getNavigationGeneration?.(),
            baseline: this.capture(),
            hadPendingDebounce: this.debounceHandle !== undefined,
            supersededAdditionToken:
                (this.pendingCharacterAddition ?? this.reservedCharacterAddition)?.token ?? null,
        }
    }

    private enqueueReplacement(
        candidate: Database,
        admission: ReplacementAdmission,
        reason: string,
        options: PersistentReplacementOptions,
    ): Promise<CommittedApplyOutcome> {
        this.assertPersistentMutationAllowed(admission.authorityEpoch)
        if (this.dependencies.isConversationOperationActive?.() || this.publicationInProgress) {
            return Promise.reject(new PersistentMutationFencedError())
        }
        const owner = Symbol('prepared-database-replacement')
        this.destructiveReplacementFence = {
            owner,
            state: 'acquiring',
            blockedPrePublicationDirty: false,
        }
        this.cancelDebounce()
        return this.enqueue(async () => {
            try {
                // Earlier queued writes and pre-existing dirty state remain durable even
                // when they make this prepared replacement's exact token stale.
                await this.flushIterations(reason, false)
                const expectationError = this.replacementExpectationError({
                    expectedRevision: admission.revision,
                    expectedMutationGeneration: admission.mutationGeneration,
                })
                if (expectationError) throw expectationError
                if (
                    this.authorityEpoch !== admission.authorityEpoch ||
                    this.dependencies.getNavigationGeneration?.() !== admission.navigationGeneration ||
                    !this.captureMatchesCapturedState(admission.baseline) ||
                    this.dependencies.isConversationOperationActive?.()
                ) {
                    throw new PersistentMutationFencedError()
                }
                if (admission.baseline.windowedCharacter || this.windowedCharacterBaseline) {
                    throw new WindowedConversationRequiresCompatibilityError(
                        'database replacement requires complete ownership',
                    )
                }
                if (this.destructiveReplacementFence?.owner !== owner) {
                    throw new Error('Destructive persistent replacement fence ownership changed')
                }
                this.destructiveReplacementFence.state = 'held'
                return await this.runReplacement(candidate, admission, options)
            } finally {
                if (this.destructiveReplacementFence?.owner === owner) {
                    // A failed admission owns the same input guard as an applied replacement.
                    this.destructiveReplacementFence.state = 'held'
                    this.releaseDestructiveReplacementFence(owner)
                }
            }
        }, null).catch((error) => {
            this.rearmDebounceAfterReplacementFailure(
                admission.mutationGeneration,
                admission.hadPendingDebounce,
            )
            throw error
        })
    }

    mutatePersistentPresets(reason: string, mutate: PersistentPresetMutation): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const operationStart = this.capture()
            const livePresetsBefore = operationStart.presetsCanonical
            const revision = this.revision
            const [rootValue, catalog] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.queryPresets(),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            this.assertReadRevision(revision, catalog.revision)

            const ordered = [...catalog.items].sort(
                (left, right) => left.configuredIndex - right.configuredIndex,
            )
            const presets = await Promise.all(
                ordered.map(async (summary) => {
                    const value = await this.dependencies.store.readPreset(summary.id)
                    if (!value) throw new Error(`Preset ${summary.id} was not found`)
                    this.assertReadRevision(revision, value.revision)
                    return canonicalClone(value.value)
                }),
            )
            const state: PersistentPresetMutationState = {
                root: { ...canonicalClone(rootValue.value),
                    botPresetsId: typeof rootValue.value.botPresetsId === 'string' ? Math.max(0, presets.findIndex((value) => value['id'] === rootValue.value.botPresetsId)) : rootValue.value.botPresetsId,
                    selectedPersona: typeof rootValue.value.selectedPersona === 'string' ? Math.max(0, rootValue.value.personas?.findIndex((value) => value.id === rootValue.value.selectedPersona)) : rootValue.value.selectedPersona },
                presets,
            }
            const presetRootBaseline = canonicalClone(state.root)
            // Operations edit `state.presets` in place, so the stored list is diffed from a copy.
            const storedPresets = canonicalClone(presets)
            await mutate(state)

            const liveBeforeCommit = this.capture()
            const persistedMutation = { ...state.root,
                botPresetsId: typeof state.presets[state.root.botPresetsId]?.['id'] === 'string' ? state.presets[state.root.botPresetsId]['id'] as string : state.root.botPresetsId,
                selectedPersona: typeof state.root.personas?.[state.root.selectedPersona]?.id === 'string' ? state.root.personas![state.root.selectedPersona].id : state.root.selectedPersona }
            const mutatedRoot = rebaseRootMutation({ ...presetRootBaseline, botPresetsId: rootValue.value.botPresetsId, selectedPersona: rootValue.value.selectedPersona }, persistedMutation, rootValue.value)
            const committedRoot = rebaseConcurrentLiveDelta(
                operationStart.root,
                liveBeforeCommit.root,
                mutatedRoot,
            )
            const committedPresets = canonicalClone(state.presets)
            const presetUnits = committedPresets.every((value) => typeof value['id'] === 'string') && storedPresets.every((value) => typeof value['id'] === 'string')
                ? this.diffPresets(storedPresets, committedPresets) : null
            const presetRootMutations = diffRootMutations(rootValue.value, committedRoot)
            if (!presetRootMutations.length && presetUnits?.length === 0) return
            const committed = await this.commitRoutine({
                expectedRevision: revision,
                rootMutations: presetRootMutations,
                ...(presetUnits ? { unitMutations: presetUnits } : { replacePresets: committedPresets }),
            })
            if (presetUnits && this.dependencies.onRoutineUnitsCommitted) {
                this.currentRevision = committed.revision
                const keys = [...presetUnits.map((value) => value.key), ...presetRootMutations.map((value) => JSON.stringify(['root', value.key]))]
                try { await this.dependencies.onRoutineUnitsCommitted(committed.revision, keys) }
                catch (error) { this.markCommittedWorkingSetRefreshRequired(committed.revision, error); throw error }
                this.dependencies.onLocalRevision?.(committed.revision)
                await this.finishExplicitCommit(committed.revision)
                return
            }
            const liveAfterCommit = this.capture()
            if (
                livePresetsBefore !== null &&
                liveAfterCommit.presetsCanonical !== livePresetsBefore
            ) {
                this.currentRevision = committed.revision
                this.rootBaseline = canonicalJson(committedRoot)
                this.presetsBaseline = canonicalJson(committedPresets)
                this.dependencies.onLocalRevision?.(committed.revision)
                await this.flushIterations(`${reason}-concurrent-live-presets`, true)
                throw new Error('Persistent working set changed during preset mutation')
            }
            this.currentRevision = committed.revision
            this.dirtyGeneration++
            this.rootBaseline = canonicalJson(committedRoot)
            this.presetsBaseline = null
            const publishedRoot = rebaseRootMutation(
                liveBeforeCommit.root,
                liveAfterCommit.root,
                committedRoot,
            )
            this.dependencies.publishPresetWorkingSet?.({
                revision: committed.revision,
                root: publishedRoot,
                presets: committedPresets,
            })
            const published = this.capture()
            this.presetsBaseline = published.presetsCanonical
            this.dependencies.onLocalRevision?.(committed.revision)
            this.pendingByteCount = 0
            this.lastBackgroundErrorMessage = null
            await this.finishExplicitCommit(committed.revision)
        })
    }

    appendPersistentRootModule(
        reason: string,
        input: PersistentRootModuleAppend,
        signal?: AbortSignal,
    ): Promise<void> {
        try {
            this.assertInitialized()
            this.assertPersistentMutationAllowed()
            input = canonicalClone(input)
        } catch (error) {
            throw persistentRootModuleAppendRejected(error)
        }
        let commitStarted = false
        return this.enqueue(async () => {
            signal?.throwIfAborted()
            this.cancelDebounce()
            await this.flushIterations(reason, true)
            signal?.throwIfAborted()
            const revision = this.revision
            const lease = await this.dependencies.store.acquireRevision(revision)
            const snapshot = await withPersistentRevisionLease(lease, async (reader) => {
                signal?.throwIfAborted()
                this.assertReadRevision(revision, reader.revision)
                const rootValue = await reader.readRoot()
                signal?.throwIfAborted()
                this.assertReadRevision(revision, rootValue.revision)
                const root = canonicalClone(rootValue.value)
                const aliases: Extract<AssetAlias, { kind: 'asset' }>[] = []
                const uniqueAliases = new Map<string, Extract<AssetAlias, { kind: 'asset' }>>()
                for (const alias of input.assetAliases) {
                    signal?.throwIfAborted()
                    const previous = uniqueAliases.get(alias.key)
                    if (
                        previous &&
                        (previous.objectHash !== alias.objectHash || previous.size !== alias.size)
                    ) {
                        throw new PersistentRootModuleAppendRejectedError(
                            `Imported module alias conflicts with duplicate ${alias.key}`,
                        )
                    }
                    if (!previous) uniqueAliases.set(alias.key, canonicalClone(alias))
                }
                const uniqueAliasValues = [...uniqueAliases.values()]
                for (let index = 0; index < uniqueAliasValues.length; index += 512) {
                    signal?.throwIfAborted()
                    const batchAliases = uniqueAliasValues.slice(index, index + 512)
                    const keys = batchAliases.map((alias) => alias.key)
                    const existing = await reader.readAssetAliasesByKeys('asset', keys)
                    signal?.throwIfAborted()
                    this.assertReadRevision(revision, existing.revision)
                    const existingByKey = new Map(existing.value.map((alias) => [alias.key, alias]))
                    for (const alias of batchAliases) {
                        signal?.throwIfAborted()
                        const stored = existingByKey.get(alias.key)
                        if (!stored) {
                            aliases.push(alias)
                            continue
                        }
                        if (stored.objectHash !== alias.objectHash || stored.size !== alias.size) {
                            throw new PersistentRootModuleAppendRejectedError(
                                `Imported module alias conflicts with existing ${alias.key}`,
                            )
                        }
                    }
                }
                if (!input.module.id || root.modules?.some((value) => value.id === input.module.id)) throw new PersistentRootModuleAppendRejectedError('Imported module requires a new stable ID')
                return { root, aliases }
            })
            signal?.throwIfAborted()
            const modules = Array.isArray(snapshot.root.modules) ? snapshot.root.modules : []
            snapshot.root.modules = [...modules, input.module]
            const ownerHead: AssetOwnerHead = {
                owner: { kind: 'root-module-assets', moduleId: input.module.id },
                ...input.ownerHead,
            } as AssetOwnerHead
            const liveBeforeCommit = this.capture()
            signal?.throwIfAborted()
            commitStarted = true
            let committed: { revision: DataRevision }
            try {
                committed = await this.commitRoutine({
                    expectedRevision: revision,
                    unitMutations: [
                        {key: JSON.stringify(['exists', 'modules', input.module.id]), type: 'set', value: true},
                        {key: JSON.stringify(['record', 'modules', input.module.id]), type: 'set', value: input.module},
                        {key: JSON.stringify(['order', 'modules']), type: 'set', value: snapshot.root.modules!.map((value) => value.id)},
                    ],
                    assetAliases: snapshot.aliases,
                    assetOwnerHeads: [ownerHead],
                })
            } catch (error) {
                if (error instanceof RevisionConflictError) {
                    throw persistentRootModuleAppendRejected(error)
                }
                throw error
            }
            const liveAfterCommit = this.capture()
            const publishedRoot = rebaseRootMutation(
                liveBeforeCommit.root,
                liveAfterCommit.root,
                snapshot.root,
            )
            this.currentRevision = committed.revision
            this.dirtyGeneration++
            if (this.dependencies.onRoutineUnitsCommitted) {
                try { await this.dependencies.onRoutineUnitsCommitted(committed.revision, [JSON.stringify(['record', 'modules', input.module.id]), JSON.stringify(['order', 'modules'])]) }
                catch (error) { this.markCommittedWorkingSetRefreshRequired(committed.revision, error); throw error }
            } else {
                this.rootBaseline = canonicalJson(snapshot.root)
                this.dependencies.publishRootWorkingSet?.(publishedRoot)
            }
            this.dependencies.onLocalRevision?.(committed.revision)
            this.pendingByteCount = 0
            this.lastBackgroundErrorMessage = null
            if (this.dependencies.officialPublisher) {
                this.deferPublication(committed.revision)
                this.armOfficialPublishRetry(this.officialPublishDelayMs())
            }
        }).catch((error) => {
            if (commitStarted) throw error
            if (signal?.aborted && error === signal.reason) throw error
            throw persistentRootModuleAppendRejected(error)
        })
    }

    mutatePersistentPluginStorage(
        reason: string,
        mutations: readonly PluginStorageMutation[],
        check?: PluginStorageWriteCheck,
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const committedMutations = mutations.map((mutation): PluginStorageMutation => {
            if (mutation.type === 'clear') return { type: 'clear', owner: mutation.owner }
            if (mutation.type === 'delete' || mutation.value === undefined) {
                return { type: 'delete', owner: mutation.owner, key: mutation.key }
            }
            return {
                type: 'set',
                owner: mutation.owner,
                key: mutation.key,
                value: canonicalClone(mutation.value),
            }
        })
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            if (committedMutations.length === 0) return
            const revision = this.revision
            const baseline = this.pluginStorageBaselineEntries
            const readLiveStorage = () =>
                this.dependencies.capturePluginStorage
                    ? this.dependencies.capturePluginStorage()
                    : ((
                          this.dependencies.captureRoot() as RootDatabase & {
                              pluginCustomStorage?: Database['pluginCustomStorage']
                          }
                      ).pluginCustomStorage ?? null)
            const liveBeforeCommit = baseline === null ? null : readLiveStorage()
            const beforeScope =
                liveBeforeCommit === null
                    ? null
                    : capturePluginMutationScope(liveBeforeCommit, committedMutations)
            const clearOwners = new Set(committedMutations.filter(mutation => mutation.type === 'clear').map(mutation => mutation.owner))
            const clearedKeys = check && clearOwners.size
                ? (await Promise.all([...clearOwners].map(owner => this.dependencies.store.queryPluginStorage(owner))))
                    .flatMap(catalog => catalog.items) : []
            check?.validate()
            const committed = await this.commitRoutine({
                expectedRevision: revision,
                pluginStorage: committedMutations,
            })
            check?.committed(clearedKeys)
            this.currentRevision = committed.revision
            this.dirtyGeneration++
            if (baseline !== null) {
                baseline.apply(committedMutations)
                const liveAfterCommit = readLiveStorage()
                if (beforeScope !== null && liveAfterCommit !== null) {
                    const publication = rebasePluginMutationPublication(
                        committedMutations,
                        beforeScope,
                        capturePluginMutationScope(liveAfterCommit, committedMutations),
                    )
                    if (this.dependencies.publishPluginStorageMutations) {
                        this.dependencies.publishPluginStorageMutations(
                            publication.mutations,
                            publication.keys,
                        )
                    } else {
                        this.dependencies.publishPluginStorageWorkingSet?.(
                            orderPluginStorageKeys(
                                applyPluginStorageMutations(liveAfterCommit, publication.mutations),
                                publication.keys,
                            ),
                        )
                    }
                }
            }
            this.dependencies.onLocalRevision?.(committed.revision)
            this.pendingByteCount = 0
            this.lastBackgroundErrorMessage = null
            await this.finishExplicitCommit(committed.revision, false)
        })
    }

    /**
     * Commits persona or toggle bindings of a conversation that has no active session (a summary
     * stub or the windowed selected shell) using metadata only, then lets the caller publish the
     * same change to the in-memory record so the shell matches its baseline. With `expected`, the
     * patch is dropped unless the stored conversation still holds those fields when it commits.
     */
    mutateConversationBinding(
        characterId: string,
        conversationId: string,
        patch: ConversationBindingPatch,
        publish: (committedPatch: ConversationBindingPatch) => void,
        expected?: ConversationBindingPatch,
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const cloneBinding = (binding: ConversationBindingPatch): ConversationBindingPatch =>
            Object.fromEntries(Object.entries(binding).map(([key, value]) => [
                key, value === undefined ? undefined : canonicalClone(value),
            ]))
        const clonePatch = () => cloneBinding(patch)
        patch = clonePatch()
        if (expected) expected = cloneBinding(expected)
        return this.enqueue(async () => {
            await this.flushIterations('chat-binding', true)
            const committed = await this.commitConversationBinding(characterId, conversationId, patch, expected)
            if (!committed) return
            this.currentRevision = committed.revision
            // Advance only these fields in the baseline; concurrent unrelated edits remain dirty.
            if (this.windowedCharacterBaseline?.authority.characterId === characterId) {
                const baseline = this.windowedCharacterBaseline
                const conversation = baseline.shell.chats.find((chat) => chat.id === conversationId)
                if (conversation) applyConversationBindingPatch(conversation, patch)
                baseline.shellCanonical = canonicalJson(baseline.shell)
            } else if (this.characterBaselineId === characterId && this.characterBaseline) {
                const baseline = JSON.parse(this.characterBaseline) as CompleteCharacter
                const conversation = baseline.chats.find((chat) => chat.id === conversationId)
                if (conversation) applyConversationBindingPatch(conversation, patch)
                this.characterBaseline = canonicalJson(baseline)
            }
            // The UI must not share nested values with the persisted windowed baseline.
            publish(clonePatch())
            this.dependencies.onStorageOnlyRevision?.(committed.revision)
            this.dependencies.onLocalRevision?.(committed.revision)
            await this.finishExplicitCommit(committed.revision)
        })
    }

    /**
     * Writes the binding fields that differ from the stored conversation, or returns null when
     * nothing is written. When another writer commits first, a binding with `expected` reads the
     * conversation again so the condition is checked against that write instead of resending.
     */
    private async commitConversationBinding(
        characterId: string,
        conversationId: string,
        patch: ConversationBindingPatch,
        expected: ConversationBindingPatch | undefined,
    ): Promise<{ revision: DataRevision } | null> {
        while (true) {
            const metadata = await this.dependencies.store.readConversationMetadata(characterId, conversationId)
            if (!metadata) throw new Error('Binding conversation is unavailable')
            this.assertReadRevision(this.revision, metadata.revision)
            if (expected && !conversationBindingHolds(metadata.value.conversation, expected)) return null
            const conversation = applyConversationBindingPatch({ ...metadata.value.conversation }, patch)
            const changes = diffFields(['conversation', characterId, conversationId], metadata.value.conversation, conversation, new Set(['id']))
            if (!changes.length) return null
            const commit = { expectedRevision: this.revision, unitMutations: changes }
            if (!expected) return this.commitRoutine(commit)
            try {
                return await this.dependencies.store.commit(canonicalClone(commit))
            } catch (error) {
                if (!(error instanceof RevisionConflictError) || error.actualRevision <= commit.expectedRevision) throw error
                this.currentRevision = error.actualRevision
            }
        }
    }

    mutatePersistentCharacterDetail(
        characterId: string,
        reason: string,
        mutate: PersistentCharacterDetailMutation,
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const operationStart = this.capture()
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const [rootValue, characterValue] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.readCharacter(characterId),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            if (!characterValue) return false
            this.assertReadRevision(revision, characterValue.revision)
            if (characterValue.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }

            const state: PersistentCharacterMutationState = {
                root: canonicalClone(rootValue.value),
                character: canonicalClone(characterValue.value),
            }
            const outcome = await mutate(state)
            const deleting = typeof outcome === 'object' && outcome?.delete === true
            const liveBeforeCommit = this.capture()
            const committedRoot = rebaseRootMutation(
                rootValue.value,
                state.root,
                rebaseRootMutation(operationStart.root, liveBeforeCommit.root, rootValue.value),
            )
            const rootChanged = canonicalJson(committedRoot) !== canonicalJson(rootValue.value)
            const committedDetail = deleting ? null : canonicalClone(state.character)
            const commit: WorkingSetCommit = { expectedRevision: revision }
            if (rootChanged) commit.root = committedRoot
            if (deleting) commit.deleteCharacterIds = [characterId]
            else commit.unitMutations = diffMaterializedCharacter({ ...characterValue.value, chats:[] } as CompleteCharacter, { ...committedDetail!, chats:[] } as CompleteCharacter).unitMutations
            if (commit.root) { commit.rootMutations = diffRootMutations(rootValue.value, commit.root); delete commit.root }
            if (!commit.deleteCharacterIds?.length && !commit.unitMutations?.length && !commit.rootMutations?.length) return true
            const committed = await this.commitRoutine(commit)
            if (!deleting && (this.dependencies.onRoutineUnitsCommitted || !this.residentCharactersMatch(residentBefore, this.captureResidentCharacter(characterId)))) {
                await this.finishRoutineCharacterIntent(committed.revision, commit, characterId, residentBefore)
                return true
            }
            const residentAfterCommit = this.captureResidentCharacter(characterId)
            if (!this.residentCharactersMatch(residentBefore, residentAfterCommit)) {
                const committedCharacter = committedDetail === null
                    ? null
                    : this.mergeCommittedDetailWithResident(
                          committedDetail,
                          residentBefore?.character ?? null,
                      )
                this.finishCharacterMutation(
                    {
                        revision: committed.revision,
                        root: committedRoot,
                        characterId,
                        kind: deleting ? 'delete' : 'detail',
                        character: committedDetail,
                    },
                    rebaseRootMutation(rootValue.value, committedRoot, operationStart.root),
                    {
                        preservePendingWork: true,
                        publish: false,
                        committedSelectedCharacter: {
                            id: characterId,
                            character: committedCharacter,
                        },
                    },
                )
                await this.finishExplicitCommit(committed.revision)
                return true
            }
            const liveAfterCommit = this.capture()
            this.finishCharacterMutation(
                {
                    revision: committed.revision,
                    root: rebaseRootMutation(
                        liveBeforeCommit.root,
                        liveAfterCommit.root,
                        committedRoot,
                    ),
                    characterId,
                    kind: deleting ? 'delete' : 'detail',
                    character: committedDetail,
                },
                rebaseRootMutation(rootValue.value, committedRoot, operationStart.root),
            )
            await this.finishExplicitCommit(committed.revision)
            return true
        })
    }

    deletePersistentCharacter(
        characterId: string,
        reason: string,
    ): Promise<boolean> {
        return this.deletePersistentCharacters([characterId], reason)
            .then((count) => count > 0)
    }

    async expirePersistentTrash(now = Date.now()): Promise<number> {
        const cutoff = now - 3 * 24 * 60 * 60 * 1000
        const expired: string[] = []
        const revision = this.revision
        let cursor: string | undefined
        do {
            const page = await this.dependencies.store.queryCharacters({
                order: 'configured', trash: true, limit: 200, cursor,
            })
            this.assertReadRevision(revision, page.revision)
            for (const summary of page.items) {
                if (isTauri ? summary.trashStampMs !== undefined && BigInt(summary.trashStampMs) < BigInt(cutoff) : summary.trashTime !== undefined && summary.trashTime < cutoff) expired.push(summary.id)
            }
            cursor = page.nextCursor
        } while (cursor)
        let removed = 0
        for (let offset = 0; offset < expired.length; offset += 128) {
            removed += await this.deletePersistentCharacters(
                expired.slice(offset, offset + 128), 'trash-expiry', cutoff,
            )
        }
        return removed
    }

    deletePersistentCharacters(
        characterIds: readonly string[],
        reason: string,
        expiryCutoff?: number,
    ): Promise<number> {
        const deletedIds = new Set(characterIds)
        if (deletedIds.size === 0) return Promise.resolve(0)
        if (deletedIds.size > 128 || deletedIds.size !== characterIds.length || [...deletedIds].some((id) => !id)) {
            return Promise.reject(new TypeError('Deletion requires at most 128 unique nonempty character IDs'))
        }
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        // The native store compares this against its own wall clock, so no injected time.
        const startedAtMs = Date.now()
        const ownedAssetKeys = new Map<string, string[]>()
        let conversationAssetKeys: string[] = []
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const residentsBefore = new Map([...deletedIds].map((id) => [id, this.captureResidentCharacter(id)]))
            const revision = this.revision
            const mutationGeneration = this.dirtyGeneration
            const lease = await this.dependencies.store.acquireRevision(revision)
            let rootValue: { revision: DataRevision; value: RootDatabase } | undefined
            const found = await withPersistentRevisionLease(lease, async (reader) => {
                this.assertReadRevision(revision, reader.revision)
                rootValue = await reader.readRoot()
                this.assertReadRevision(revision, rootValue.revision)
                for (const id of deletedIds) {
                    const summary = isTauri ? await reader.readCharacterSummary(id) : null
                    // The native store keeps an archived character's details in its archive and refuses to read them, so
                    // its owned keys come from the store with the conversation keys.
                    const archived = Boolean(summary?.archived)
                    const targetValue = archived ? null : await reader.readCharacter(id)
                    const expired = expiryCutoff === undefined || (isTauri
                        ? summary?.trashed && summary.trashStampMs !== undefined && BigInt(summary.trashStampMs) < BigInt(expiryCutoff)
                        : targetValue?.value.trashTime !== undefined && targetValue.value.trashTime < expiryCutoff)
                    if (!(archived || targetValue) || !expired) {
                        deletedIds.delete(id)
                        continue
                    }
                    if (!targetValue) continue
                    this.assertReadRevision(revision, targetValue.revision)
                    if (targetValue.value.chaId !== id) throw new Error(`Character ${id} returned mismatched detail`)
                    ownedAssetKeys.set(id, collectOwnedAssetKeys(targetValue.value))
                }
                if (deletedIds.size === 0) return false

                return true
            })
            if (!found) return 0
            if (!rootValue) return 0
            // A detail read leaves out conversations, and the delete removes them.
            conversationAssetKeys = await this.dependencies.deletedCharacterAssets?.collectConversationKeys([...deletedIds]) ?? []
            for (const id of deletedIds) this.assertResidentCharacterUnchanged(id, residentsBefore.get(id) ?? null)
            const mutatedRoot = canonicalClone(rootValue.value)
            for (const id of deletedIds) removeCharacterIdFromOrder(mutatedRoot, id)
            for (const loadout of mutatedRoot.loadouts ?? []) {
                if (Array.isArray(loadout.characterIds)) loadout.characterIds = loadout.characterIds.filter((id) => !deletedIds.has(id))
            }
            const liveBeforeCommit = this.capture()
            const committedRoot = rebaseRootMutation(
                rootValue.value,
                mutatedRoot,
                liveBeforeCommit.root,
            )
            const previousLoadouts = new Map((rootValue.value.loadouts ?? []).map(value => [value.id, value]))
            const orderChanged = (committedRoot.characterOrder === undefined ? null : canonicalJson(committedRoot.characterOrder)) !==
                (rootValue.value.characterOrder === undefined ? null : canonicalJson(rootValue.value.characterOrder))
            const commit: WorkingSetCommit = {
                expectedRevision: revision,
                unitMutations: [
                    ...[...deletedIds].map((id): PersistentUnitMutation => ({type:'delete',key:JSON.stringify(['exists','character',id])})),
                    ...(orderChanged ? [{type:'set' as const,key:JSON.stringify(['order','characters']),value:committedRoot.characterOrder}] : []),
                    ...(committedRoot.loadouts ?? []).filter(loadout => {
                        const previous = previousLoadouts.get(loadout.id)
                        return previous !== undefined && canonicalJson(loadout) !== canonicalJson(previous)
                    }).map((loadout): PersistentUnitMutation => ({type:'set',key:JSON.stringify(['record','loadouts',loadout.id]),value:loadout})),
                ],
            }
            const committedBaselineRoot = this.capturePersistentBaselineRoot()
            if (orderChanged) {
                if (committedRoot.characterOrder === undefined) delete committedBaselineRoot.characterOrder
                else committedBaselineRoot.characterOrder = canonicalClone(committedRoot.characterOrder)
            }
            const committedLoadouts = new Map((commit.unitMutations ?? [])
                .filter((mutation): mutation is Extract<PersistentUnitMutation, {type:'set'}> => { const key = JSON.parse(mutation.key); return mutation.type === 'set' && key[0] === 'record' && key[1] === 'loadouts' })
                .map((mutation) => [JSON.parse(mutation.key)[2], mutation.value]))
            if (committedLoadouts.size) committedBaselineRoot.loadouts = committedBaselineRoot.loadouts!.map((value) =>
                (committedLoadouts.get(value.id) ?? value) as typeof value)
            const committed = await this.dependencies.store.commit(commit)
            const changedDuringCommit = this.dirtyGeneration !== mutationGeneration
            const liveAfterCommit = this.capture()
            const [characterId, ...additionalDeletedCharacterIds] = deletedIds
            this.finishCharacterMutation(
                {
                    revision: committed.revision,
                    root: rebaseRootMutation(
                        liveBeforeCommit.root,
                        liveAfterCommit.root,
                        committedRoot,
                    ),
                    characterId,
                    kind: 'delete',
                    character: null,
                },
                committedBaselineRoot,
                {
                    preservePendingWork: changedDuringCommit,
                    additionalDeletedCharacterIds,
                },
            )
            await this.finishExplicitCommit(committed.revision)
            return deletedIds.size
        }).then((count) => {
            if (count > 0) {
                const keys = new Set([...deletedIds].flatMap((id) => ownedAssetKeys.get(id) ?? []).concat(conversationAssetKeys))
                this.dependencies.deletedCharacterAssets?.cleanup([...keys], startedAtMs)
            }
            return count
        })
    }

    replacePersistentCompleteCharacter(
        characterId: string,
        reason: string,
        mutate: PersistentCompleteCharacterMutation,
        options: PersistentScopedReplacementOptions = {},
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const { expectedRevision } = options
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            if (expectedRevision !== undefined && expectedRevision !== this.revision)
                throw new RevisionConflictError(expectedRevision, this.revision)
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const [rootValue, characterValue] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.readCharacter(characterId),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            if (!characterValue) return false
            this.assertReadRevision(revision, characterValue.revision)
            if (characterValue.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            const current = await this.readCompleteCharacter(
                characterId,
                revision,
                characterValue.value,
            )
            const beforeCanonical = canonicalJson(current)
            const replacement = canonicalClone(await mutate(current))
            if (expectedRevision !== undefined) this.assertResidentCharacterUnchanged(characterId, residentBefore)
            if (replacement.chaId !== characterId) {
                throw new Error(`Replacement character ID must remain ${characterId}`)
            }
            if (canonicalJson(replacement) === beforeCanonical) return true

            const changed = diffMaterializedCharacter(JSON.parse(beforeCanonical), replacement)
            const committed = expectedRevision === undefined
                ? await this.commitRoutine({ expectedRevision: revision, ...changed })
                : await this.dependencies.store.commit({ expectedRevision: revision, replaceCharacter: replacement })
            if (expectedRevision === undefined && (this.dependencies.onRoutineUnitsCommitted || !this.residentCharactersMatch(residentBefore, this.captureResidentCharacter(characterId)))) {
                await this.finishRoutineCharacterIntent(committed.revision, { expectedRevision: revision, ...changed }, characterId, residentBefore)
                return true
            }
            const residentAfterCommit = this.captureResidentCharacter(characterId)
            if (!this.residentCharactersMatch(residentBefore, residentAfterCommit)) {
                this.finishCharacterMutation(
                    {
                        revision: committed.revision,
                        root: rootValue.value,
                        characterId,
                        kind: 'replace',
                        character: replacement,
                    },
                    rootValue.value,
                    {
                        preservePendingWork: true,
                        publish: false,
                        committedSelectedCharacter: { id: characterId, character: replacement },
                    },
                )
                await this.finishExplicitCommit(committed.revision)
                return true
            }
            this.finishCharacterMutation(
                {
                    revision: committed.revision,
                    root: this.capture().root,
                    characterId,
                    kind: 'replace',
                    character: replacement,
                },
                rootValue.value,
            )
            await this.finishExplicitCommit(committed.revision)
            return true
        })
    }

    replacePersistentConversation(
        characterId: string,
        conversationId: string,
        reason: string,
        replacement: Chat,
        options: PersistentScopedReplacementOptions = {},
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const candidate = canonicalClone(replacement)
        const { expectedRevision } = options
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            if (expectedRevision !== undefined && expectedRevision !== this.revision)
                throw new RevisionConflictError(expectedRevision, this.revision)

            const authority = this.dependencies.captureSelectedConversationAuthority?.() ?? null
            if (
                authority !== null &&
                authority.characterId === characterId &&
                authority.conversationId === conversationId
            ) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'windowed selected conversation replacement requires session coordination',
                )
            }
            const mutationGeneration = this.dirtyGeneration
            const residentBefore = this.captureResidentConversation(characterId, conversationId)
            const summaryBefore =
                residentBefore === null &&
                this.isResidentConversationSummary(characterId, conversationId)
            const current = await this.dependencies.store.readConversation(
                characterId,
                conversationId,
            )
            if (!current) return false
            this.assertReadRevision(this.revision, current.revision)
            if (candidate.id !== conversationId) {
                throw new Error(`Replacement conversation ID must remain ${conversationId}`)
            }
            if (!Array.isArray(candidate.message)) {
                throw new TypeError('Replacement conversation messages must be an array')
            }
            const { message, ...conversation } = candidate
            if (canonicalJson(candidate) === canonicalJson(current.value)) return true
            let committed: {revision:DataRevision}
            if (expectedRevision !== undefined) {
                committed = await this.dependencies.store.commit({ expectedRevision:this.revision, conversations:[{
                    type:'replace-range', characterId, conversationId, start:0, deleteCount:current.value.message.length, messages:message, conversation,
                }] })
            } else {
                const {message: _messages, ...beforeMetadata} = current.value
                const units = diffFields(['conversation',characterId,conversationId], beforeMetadata, conversation, new Set(['id']))
                const messagesChanged = canonicalJson(current.value.message) !== canonicalJson(message)
                while (true) {
                    const revision = this.revision
                    const latest = messagesChanged ? await this.dependencies.store.readConversationMetadata(characterId, conversationId) : null
                    if (messagesChanged && !latest) return false
                    try {
                        committed = await this.dependencies.store.commit({ expectedRevision:revision, unitMutations:units,
                            ...(messagesChanged ? {conversations:[{type:'replace-range', characterId, conversationId, start:0, deleteCount:latest!.value.totalMessages, messages:message}]} : {}) })
                        break
                    } catch (error) {
                        if (!(error instanceof RevisionConflictError) || error.actualRevision <= revision) throw error
                        this.currentRevision = error.actualRevision
                    }
                }
            }
            const residentAfterCommit = this.captureResidentConversation(
                characterId,
                conversationId,
            )
            if (!this.residentConversationsMatch(residentBefore, residentAfterCommit)) {
                await this.finishPersistentConversationReplacement(
                    {
                        revision: committed.revision,
                        characterId,
                        conversationId,
                        conversation: candidate,
                    },
                    {
                        preservePendingWork: true,
                        publish: false,
                        residentBefore,
                        summaryBefore,
                    },
                )
                return true
            }
            await this.finishPersistentConversationReplacement(
                {
                    revision: committed.revision,
                    characterId,
                    conversationId,
                    conversation: candidate,
                },
                {
                    preservePendingWork: this.dirtyGeneration !== mutationGeneration,
                    residentBefore,
                    summaryBefore,
                },
            )
            return true
        })
    }

    upsertPersistentCompleteCharacter(
        characterId: string,
        reason: string,
        createOrMutate: PersistentCompleteCharacterUpsert,
        options: PersistentCompleteCharacterUpsertOptions = {},
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const { includeInCharacterOrder } = options
        const assetAliases = options.assetAliases === undefined
            ? undefined
            : [...canonicalClone(options.assetAliases)]
        const assetOwnerHeads = options.assetOwnerHeads === undefined
            ? undefined
            : [...canonicalClone(options.assetOwnerHeads)]
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const operationStart = this.capture()
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const [rootValue, characterValue] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.readCharacter(characterId),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            if (characterValue) {
                this.assertReadRevision(revision, characterValue.revision)
                if (characterValue.value.chaId !== characterId) {
                    throw new Error(`Character ${characterId} returned mismatched detail`)
                }
            }
            const current = characterValue
                ? await this.readCompleteCharacter(characterId, revision, characterValue.value)
                : null
            const beforeCanonical = current ? canonicalJson(current) : null
            const replacement = canonicalClone(await createOrMutate(current))
            if (replacement.chaId !== characterId) {
                throw new Error(`Upserted character ID must remain ${characterId}`)
            }
            if (beforeCanonical !== null && canonicalJson(replacement) === beforeCanonical &&
                !assetAliases?.length && !assetOwnerHeads?.length) return true

            const commit: WorkingSetCommit = { expectedRevision: revision }
            if (assetAliases !== undefined) {
                if (assetAliases.some((alias) => alias.kind !== 'asset')) {
                    throw new TypeError('Prepared character aliases must be ordinary assets')
                }
                commit.assetAliases = assetAliases
            }
            if (assetOwnerHeads !== undefined) {
                if (
                    assetOwnerHeads.some(
                        (head) =>
                            head.owner.kind !== 'character-additional-assets' ||
                            head.owner.characterId !== characterId,
                    )
                ) {
                    throw new TypeError(
                        `Prepared character owner heads must belong to ${characterId}`,
                    )
                }
                commit.assetOwnerHeads = assetOwnerHeads
            }
            let committedRoot = canonicalClone(rootValue.value)
            const mutatedRoot = canonicalClone(rootValue.value)
            if (current) {
                const changes = diffMaterializedCharacter(JSON.parse(beforeCanonical!) as CompleteCharacter, replacement)
                commit.unitMutations = changes.unitMutations
                commit.conversations = changes.conversations
                if (assetOwnerHeads?.length) {
                    const key = JSON.stringify(['character', characterId, 'additionalAssets'])
                    if (!commit.unitMutations.some((mutation) => mutation.key === key)) {
                        commit.unitMutations.push(Object.hasOwn(replacement, 'additionalAssets')
                            ? { key, type: 'set', value: replacement.additionalAssets }
                            : { key, type: 'delete' })
                    }
                }
            } else {
                if (includeInCharacterOrder !== false) {
                    appendCharacterIdToOrder(mutatedRoot, characterId)
                    committedRoot = rebaseRootMutation(
                        rootValue.value,
                        mutatedRoot,
                        rebaseRootMutation(operationStart.root, this.capture().root, rootValue.value),
                    )
                    commit.root = committedRoot
                }
                commit.addCharacter = replacement
            }
            if (commit.root) { commit.rootMutations = diffRootMutations(rootValue.value, commit.root); delete commit.root }
            const committed = await this.commitRoutine(commit)
            if (current && (this.dependencies.onRoutineUnitsCommitted || !this.residentCharactersMatch(residentBefore, this.captureResidentCharacter(characterId)))) {
                await this.finishRoutineCharacterIntent(committed.revision, commit, characterId, residentBefore)
                return true
            }
            const residentAfterCommit = this.captureResidentCharacter(characterId)
            if (!this.residentCharactersMatch(residentBefore, residentAfterCommit)) {
                this.finishCharacterMutation(
                    {
                        revision: committed.revision,
                        root: committedRoot,
                        characterId,
                        kind: current ? 'replace' : 'add',
                        character: replacement,
                    },
                    committedRoot,
                    {
                        preservePendingWork: true,
                        publish: false,
                        committedSelectedCharacter: { id: characterId, character: replacement },
                    },
                )
                await this.finishExplicitCommit(committed.revision)
                return true
            }
            this.finishCharacterMutation(
                {
                    revision: committed.revision,
                    root:
                        current || includeInCharacterOrder === false
                            ? this.capture().root
                            : rebaseRootMutation(rootValue.value, mutatedRoot, this.capture().root),
                    characterId,
                    kind: current ? 'replace' : 'add',
                    character: replacement,
                },
                rebaseRootMutation(rootValue.value, committedRoot, operationStart.root),
            )
            await this.finishExplicitCommit(committed.revision)
            return true
        })
    }

    readPersistentCharacterDetail(
        characterId: string,
        reason: string,
    ): Promise<CharacterDetail | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const value = await this.dependencies.store.readCharacter(characterId)
            if (!value) return null
            this.assertReadRevision(revision, value.revision)
            if (value.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            return canonicalClone(value.value)
        })
    }

    readPersistentCompleteCharacter(
        characterId: string,
        reason: string,
    ): Promise<CompleteCharacter | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const value = await this.dependencies.store.readCharacter(characterId)
            if (!value) return null
            this.assertReadRevision(revision, value.revision)
            if (value.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            return this.readCompleteCharacter(characterId, revision, value.value)
        })
    }

    readPersistentConversation(
        characterId: string,
        conversationId: string,
        reason: string,
    ): Promise<Chat | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const value = await this.dependencies.store.readConversation(
                characterId,
                conversationId,
            )
            if (!value) return null
            this.assertReadRevision(revision, value.revision)
            if (value.value.id !== conversationId) {
                throw new Error(`Conversation ${conversationId} returned mismatched content`)
            }
            return cloneConversationByMessage(value.value)
        })
    }

    readPersistentConversationAt(
        characterId: string,
        orderedPosition: number,
        reason: string,
    ): Promise<Chat | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        if (!Number.isInteger(orderedPosition) || orderedPosition < 0) {
            return Promise.reject(
                new RangeError('Conversation position must be a nonnegative integer'),
            )
        }
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            return this.readConversationAtRevision(characterId, orderedPosition, this.revision)
        })
    }

    readPersistentSelectedConversation(
        characterId: string,
        reason: string,
    ): Promise<PersistentSelectedConversation | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const character = await this.dependencies.store.readCharacter(characterId)
            if (!character) return null
            this.assertReadRevision(revision, character.revision)
            if (character.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            const orderedPosition = character.value.chatPage ?? 0
            if (!Number.isInteger(orderedPosition) || orderedPosition < 0) {
                throw new RangeError('Selected conversation position must be a nonnegative integer')
            }
            const conversation = await this.readConversationAtRevision(
                characterId,
                orderedPosition,
                revision,
            )
            return {
                character: canonicalClone(character.value),
                conversation,
            }
        })
    }

    materializePersistentDatabaseSnapshot(reason: string): Promise<Database> {
        return this.materializePersistentDatabaseSnapshotWithRevision(reason).then(
            (snapshot) => snapshot.database,
        )
    }

    capturePersistentMutationToken(
        reason: string,
        options: { publishOfficial?: boolean } = {},
    ): Promise<PersistentMutationToken> {
        options = { ...options }
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, options.publishOfficial ?? true)
            return {
                revision: this.revision,
                mutationGeneration: this.dirtyGeneration,
            }
        })
    }

    /** Drains local writes without advancing the preparation's expected token. */
    acquireDestructiveReplacementFence(expected: PersistentMutationToken): Promise<symbol> {
        this.assertPersistentMutationAllowed()
        if (this.dependencies.isConversationOperationActive?.() || this.publicationInProgress) {
            throw new PersistentMutationFencedError()
        }
        expected = { ...expected }
        const authorityEpoch = this.authorityEpoch
        const navigationGeneration = this.dependencies.getNavigationGeneration?.()
        const owner = Symbol('destructive-persistent-replacement')
        this.destructiveReplacementFence = {
            owner,
            state: 'acquiring',
            blockedPrePublicationDirty: false,
        }
        this.cancelDebounce()
        return this.enqueue(async () => {
            try {
                await this.flushIterations('destructive-persistent-replacement', false)
                if (this.revision !== expected.revision) {
                    throw new RevisionConflictError(expected.revision, this.revision)
                }
                if (
                    this.dirtyGeneration !== expected.mutationGeneration ||
                    this.authorityEpoch !== authorityEpoch ||
                    this.dependencies.getNavigationGeneration?.() !== navigationGeneration ||
                    this.dependencies.isConversationOperationActive?.()
                ) {
                    throw new PersistentMutationFencedError()
                }
                if (this.destructiveReplacementFence?.owner !== owner) {
                    throw new Error('Destructive persistent replacement fence ownership changed')
                }
                this.destructiveReplacementFence.state = 'held'
                return owner
            } catch (error) {
                if (this.destructiveReplacementFence?.owner === owner) {
                    this.destructiveReplacementFence = null
                }
                throw error
            }
        }, null)
    }

    /**
     * Fences projection of an already-committed authoritative revision. It intentionally
     * preserves dirty state instead of flushing it, and captures that live state after
     * earlier queued operations drain so only later projection-time edits invalidate it.
     */
    acquireCommittedWorkingSetRefreshFence(): Promise<symbol> {
        this.assertInitialized()
        if (this.destructiveReplacementFence) throw new PersistentMutationFencedError()
        const owner = Symbol('committed-working-set-refresh')
        this.destructiveReplacementFence = {
            owner,
            state: 'acquiring',
            blockedPrePublicationDirty: false,
        }
        this.cancelDebounce()
        return this.enqueue(async () => {
            try {
                if (this.destructiveReplacementFence?.owner !== owner) {
                    throw new Error('Committed working-set refresh fence ownership changed')
                }
                this.destructiveReplacementFence.refreshBaseline = this.capture()
                this.destructiveReplacementFence.state = 'held'
                return owner
            } catch (error) {
                if (this.destructiveReplacementFence?.owner === owner) {
                    this.destructiveReplacementFence = null
                }
                throw error
            }
        }, null)
    }

    assertDestructiveReplacementFence(owner: symbol): void {
        if (
            this.destructiveReplacementFence?.owner !== owner ||
            this.destructiveReplacementFence.state !== 'held'
        ) {
            throw new Error('Destructive persistent replacement fence is not held')
        }
        if (this.destructiveReplacementFence.blockedPrePublicationDirty) {
            throw new PersistentMutationFencedError()
        }
        if (!(this.destructiveReplacementFence.refreshBaseline
            ? this.captureMatchesCapturedState(this.destructiveReplacementFence.refreshBaseline)
            : this.captureMatchesBaseline())) {
            this.destructiveReplacementFence.blockedPrePublicationDirty = true
            throw new PersistentMutationFencedError()
        }
    }

    releaseDestructiveReplacementFence(owner: symbol): void {
        if (
            this.destructiveReplacementFence?.owner !== owner ||
            this.destructiveReplacementFence.state !== 'held'
        ) {
            throw new Error('Destructive persistent replacement fence is not held')
        }
        this.destructiveReplacementFence = null
        if (this.committedRefreshRevision === null) {
            if (this.hasPendingOfficialPublication || this.publishAfterFenceRelease) {
                this.publishAfterFenceRelease = false
                this.armOfficialPublishRetry(this.officialPublishDelayMs())
            }
            if (this.flushAfterFenceRelease) {
                this.flushAfterFenceRelease = false
                this.armDebounce()
            }
        }
    }

    materializePersistentDatabaseSnapshotWithRevision(
        reason: string,
        options: PersistentDatabaseMaterializationOptions = {},
    ): Promise<PersistentDatabaseSnapshot> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const generation = this.dirtyGeneration
            const database = await this.dependencies.store.materializeDatabase(revision)
            const pluginStorageValues = options.includePluginStorageValues
                ? await this.materializePluginStorageValues(revision)
                : undefined
            return {
                database: canonicalDatabaseClone(database),
                revision,
                mutationGeneration: generation,
                ...(pluginStorageValues !== undefined ? { pluginStorageValues } : {}),
            }
        })
    }

    private async materializePluginStorageValues(
        revision: DataRevision,
    ): Promise<PluginStorageValue[]> {
        const values: PluginStorageValue[] = []
        let afterKey: PluginStorageValueCursor | undefined
        do {
            const page = await this.dependencies.store.readPluginStorageValues({ afterKey })
            this.assertReadRevision(revision, page.revision)
            for (const item of page.items) values.push({ ...item, value: canonicalClone(item.value) })
            afterKey = page.nextCursor ?? undefined
        } while (afterKey)
        return values
    }

    publishCurrentOfficialRevision(): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        if (!this.dependencies.officialPublisher) return Promise.resolve()
        return this.enqueue(async () => {
            await this.applyDeferredPublication()
            this.pendingPublicationRevision ??= this.revision
            await this.publishPendingRevision()
        })
    }

    get hasPendingOfficialPublication(): boolean {
        return this.pendingPublicationRevision !== null || this.deferredPublicationRevision !== null
    }

    commitCharacterAddition(request: CharacterAdditionRequest, reason: string): Promise<void> {
        request = { ...request }
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        if (!request.characterId) {
            throw new Error('Character addition requires a nonempty character ID')
        }
        const authorityEpoch = this.authorityEpoch
        const inFlight = this.additionPromise
        if (inFlight) {
            // Two imports can overlap, so the later one waits instead of failing.
            return inFlight
                .catch(() => undefined)
                .then(() => {
                    this.assertPersistentMutationAllowed(authorityEpoch)
                    return this.commitCharacterAddition(request, reason)
                })
        }
        if (this.pendingCharacterAddition) {
            // A previous addition failed and left its work pending; retry it before this import.
            return this.flushPendingData(reason).then(() => {
                this.assertPersistentMutationAllowed(authorityEpoch)
                return this.commitCharacterAddition(request, reason)
            })
        }
        if (this.reservedCharacterAddition) {
            throw new Error('A character addition is already pending')
        }
        const reserved: ReservedCharacterAddition = {
            request,
            token: {},
        }
        this.reservedCharacterAddition = reserved
        const promise = this.enqueue(async () => {
            if (this.reservedCharacterAddition === reserved) {
                this.beginReservedAddition(reserved)
            }
            if (this.pendingCharacterAddition?.token !== reserved.token) return
            await this.flushIterations(reason, true)
        }).catch((error) => {
            if (this.reservedCharacterAddition === reserved) {
                this.reservedCharacterAddition = null
                this.notifyOperationStateChange()
            }
            throw error
        })
        this.trackPromiseSlot(
            promise,
            () => this.additionPromise,
            (value) => {
                this.additionPromise = value
            },
        )
        return promise
    }

    private enqueue<T>(
        operation: () => Promise<T>,
        expectedAuthorityEpoch: number | null = this.authorityEpoch,
        storageOnly = false,
    ): Promise<T> {
        this.queuedOperationCount += 1
        if (storageOnly) this.queuedStorageOnlyOperationCount += 1
        this.persistenceWasBusy = true
        this.notifyOperationStateChange()
        return this.operationMutex.runExclusive(async () => {
            try {
                if (expectedAuthorityEpoch !== null) {
                    this.assertQueuedMutationAllowed(expectedAuthorityEpoch)
                }
                return await operation()
            } finally {
                this.queuedOperationCount -= 1
                if (storageOnly) this.queuedStorageOnlyOperationCount -= 1
                this.notifyOperationStateChange()
            }
        })
    }

    private waitForOperationStateChange(): Promise<void> {
        return new Promise((resolve) => this.operationStateWaiters.add(resolve))
    }

    private notifyOperationStateChange(): void {
        const waiters = [...this.operationStateWaiters]
        this.operationStateWaiters.clear()
        for (const resolve of waiters) resolve()
        this.reportPersistenceIdleIfNeeded()
    }

    private async flushIterations(reason: string, publishOfficial: boolean): Promise<void> {
        const localState = { settled: true }
        const capturedConversationMutations: PendingConversationMutation[] = []
        try {
            await this.flushIterationsUntilIdle(
                reason,
                publishOfficial,
                localState,
                capturedConversationMutations,
            )
        } catch (error) {
            if (!localState.settled && !(error instanceof PersistentMutationFencedError)) {
                this.setLocalSaveFailure(error)
            }
            throw error
        } finally {
            releaseConversationMutationCaptures(capturedConversationMutations)
        }
    }

    private async flushIterationsUntilIdle(
        _reason: string,
        publishOfficial: boolean,
        localState: { settled: boolean },
        capturedConversationMutations: PendingConversationMutation[],
    ): Promise<void> {
        if (this.committedRefreshRevision !== null) throw new PersistentMutationFencedError()
        if (this.destructiveReplacementFence) publishOfficial = false
        if (publishOfficial && this.deferredPublicationRevision !== null) {
            await this.applyDeferredPublication()
        }
        if (publishOfficial && this.pendingPublicationCleanup.size > 0) {
            await this.retryPublicationCleanup()
        }
        while (true) {
            localState.settled = false
            // The previous pass no longer reads its entries, so they may fold again.
            releaseConversationMutationCaptures(capturedConversationMutations)
            if (this.pendingCharacterAddition && !this.pendingCharacterAddition.locallyAdded) {
                await this.commitPendingCharacterAddition(publishOfficial)
            }
            const generation = this.dirtyGeneration
            const captured = this.capture()
            this.foldPendingConversationMutations()
            const pendingConversationMutations = [...this.pendingConversationMutations]
            for (const pending of pendingConversationMutations) {
                pending.captures += 1
                capturedConversationMutations.push(pending)
            }
            const windowedCapture = this.requireMatchingWindowedCapture(captured)
            let yieldedAfterCapture = windowedCapture !== null
            const selectionSwitched =
                windowedCapture === null &&
                captured.characterCanonical !== null &&
                this.characterBaselineId !== null &&
                (captured.characterId ?? captured.character?.chaId) !== this.characterBaselineId
            const detached =
                windowedCapture === null &&
                (captured.characterCanonical === null || selectionSwitched)
                    ? this.captureDetachedCharacter()
                    : null
            const addition = this.capturePendingAddition()
            let conversationProjection: ConversationMutationProjection | null
            try {
                conversationProjection = windowedCapture
                    ? await this.projectWindowedConversationMutations(
                          captured,
                          pendingConversationMutations,
                      )
                    : this.projectConversationMutations(captured, pendingConversationMutations)
            } catch (error) {
                if (
                    !windowedCapture ||
                    !(error instanceof WindowedConversationRequiresCompatibilityError) ||
                    !(await this.completeWindowedConversation(
                        windowedCapture,
                        pendingConversationMutations,
                        error,
                    ))
                )
                    throw error
                continue
            }
            const recordedConversations = conversationProjection?.exactMutations ?? null
            const commit: WorkingSetCommit = { expectedRevision: this.revision,
                ...(conversationProjection?.unitMutations?.length ? { unitMutations: conversationProjection.unitMutations } : {}) }
            const materialized = await this.captureMaterializedChanges(commit, windowedCapture?.authority.characterId)
            const presetRecords = new Map<string, botPreset>()
            for (const value of this.dependencies.capturePresetRecords?.() ?? []) {
                const id = value['id']
                if (typeof id !== 'string' || !id) throw new TypeError('Preset requires stable ID')
                const previous = this.presetRecordBaselines.get(id) ?? (await this.dependencies.store.readPreset(id))?.value
                if (!previous) continue
                const next = canonicalClone(value)
                commit.unitMutations = [...(commit.unitMutations ?? []), ...diffFields(['preset', id], previous, next, new Set(['id']))]
                presetRecords.set(id, next)
            }
            if (captured.rootCanonical !== this.rootBaseline) {
                if (this.rootBaseline === null) commit.root = captured.root
                else
                    commit.rootMutations = this.dependencies.canonicalCapture
                        ? this.dependencies.canonicalCapture.diffRoot(
                              this.rootBaseline,
                              captured.rootCanonical,
                          )
                        : diffRootMutations(
                              JSON.parse(this.rootBaseline) as RootDatabase,
                              captured.root,
                          )
            }
            const baselineFields = this.rootBaseline === null ? undefined : this.dependencies.canonicalCapture?.rootFields?.(this.rootBaseline)
            let decodedBaselineRoot: Record<string, unknown> | undefined
            const baselineField = (key: string): unknown => baselineFields
                ? baselineFields.get(key)
                : (decodedBaselineRoot ??= JSON.parse(this.rootBaseline!))[key]
            const rootUnitStart = commit.unitMutations?.length ?? 0
            if (commit.rootMutations && this.rootBaseline !== null) {
                commit.rootMutations = commit.rootMutations.filter((mutation) => {
                    if (!recordCollections.has(mutation.key)) return true
                    const after = mutation.type === 'set' ? mutation.value : []
                    commit.unitMutations = [...(commit.unitMutations ?? []), ...diffRecordCollection(
                        mutation.key, (baselineField(mutation.key) ?? []) as unknown[], after as unknown[],
                    )]
                    return false
                })
            }
            if (commit.rootMutations && this.rootBaseline !== null) {
                commit.rootMutations = commit.rootMutations.filter((mutation) => {
                    if (!['explicitGlobalChatVariables', 'protectedPresetValues', 'personas'].includes(mutation.key)) return true
                    const after = mutation.type === 'set' ? mutation.value : mutation.key === 'personas' ? [] : {}
                    const prefix = mutation.key === 'protectedPresetValues' ? ['preset-protected'] : []
                    if (mutation.key === 'personas') {
                        const previous = new Map(((baselineField('personas') ?? []) as {id:string}[]).map((value) => [value.id, value]))
                        for (const value of after as {id:string}[]) { if (!previous.has(value.id)) commit.unitMutations = [...(commit.unitMutations ?? []), {key: JSON.stringify(['exists', 'persona', value.id]), type:'set', value:true}]; commit.unitMutations = [...(commit.unitMutations ?? []), ...diffFields(['persona', value.id], previous.get(value.id) as object ?? {}, value, new Set(['id']))] }
                        const next = new Set((after as {id:string}[]).map((value) => value.id))
                        for (const id of previous.keys()) if (!next.has(id as string)) commit.unitMutations = [...(commit.unitMutations ?? []), {key: JSON.stringify(['exists', 'persona', id]), type: 'delete'}]
                        if (canonicalJson([...previous.keys()]) !== canonicalJson([...next])) commit.unitMutations = [...(commit.unitMutations ?? []), {key: JSON.stringify(['order', 'personas']), type: 'set', value: [...next]}]
                    } else {
                        const fields = diffFields(prefix, (baselineField(mutation.key) ?? {}) as object, after as object)
                        if (mutation.key === 'explicitGlobalChatVariables') for (const value of fields) { const name = JSON.parse(value.key)[0]; value.key = JSON.stringify([name.startsWith('toggle_') ? 'toggle' : 'variable', name]) }
                        commit.unitMutations = [...(commit.unitMutations ?? []), ...fields]
                    }
                    return false
                })
            }
            // Empty split collections have no durable units, but their captured projection must converge.
            if (commit.rootMutations?.length === 0 && (commit.unitMutations?.length ?? 0) === rootUnitStart) {
                this.rootBaseline = captured.rootCanonical
            }
            if (!this.pluginStorageMatchesBaseline(captured)) {
                commit.pluginStorage = diffPluginStorage(
                    this.pluginStorageBaseline,
                    captured.pluginStorage!,
                )
            }
            if (
                captured.presetsCanonical !== null &&
                captured.presetsCanonical !== this.presetsBaseline
            ) {
                const before = this.presetsBaseline === null ? [] : JSON.parse(this.presetsBaseline) as botPreset[]
                if (before.every((value) => typeof value['id'] === 'string') && captured.presets!.every((value) => typeof value['id'] === 'string')) {
                    commit.unitMutations = [...(commit.unitMutations ?? []), ...this.diffPresets(before, captured.presets!)]
                } else commit.replacePresets = captured.presets
            }
            if (this.dependencies.captureCharacters && !windowedCapture) {
                if (recordedConversations) {
                    const recordedTargets = new Set(recordedConversations.flatMap((mutation) => mutation.type === 'reorder'
                        ? [] : [JSON.stringify([mutation.characterId, mutation.conversationId])]))
                    commit.conversations = [...(commit.conversations ?? []).filter((mutation) => mutation.type === 'reorder'
                        || !recordedTargets.has(JSON.stringify([mutation.characterId, mutation.conversationId]))), ...recordedConversations]
                }
            } else if (windowedCapture) {
                if (conversationProjection?.character) {
                    commit.character = conversationProjection.character
                }
                if (recordedConversations) commit.conversations = recordedConversations
            } else if (detached) {
                commit.replaceCharacter = detached.character
            } else if (
                captured.characterCanonical !== null &&
                (captured.characterCanonical !== this.characterBaseline ||
                    recordedConversations !== null)
            ) {
                const conversations =
                    recordedConversations ?? this.diffSelectedConversations(captured)
                if (conversations) {
                    if (conversations.length > 0) commit.conversations = conversations
                    if (this.characterBaseline !== null) {
                        const baseline = JSON.parse(this.characterBaseline) as CompleteCharacter
                        if (
                            baseline.chaId === captured.character.chaId &&
                            baseline.chatPage !== captured.character.chatPage
                        ) {
                            const { chats: _chats, ...character } = captured.character
                            commit.character = character
                        }
                    }
                } else
                    commit.replaceCharacter =
                        captured.conversationStubIds.size > 0
                            ? await this.reconstructCapturedCharacter(captured)
                            : captured.character
            }

            let replacementIsAddition = false
            if (addition) {
                if (
                    addition.canonical !== addition.pending.baseline &&
                    !commit.replaceCharacter &&
                    !commit.conversations
                ) {
                    commit.replaceCharacter = addition.character
                    replacementIsAddition = true
                }
            }

            if (
                commit.unitMutations?.length ||
                commit.root ||
                commit.rootMutations?.length ||
                commit.pluginStorage ||
                commit.replacePresets ||
                commit.character ||
                commit.replaceCharacter ||
                commit.addCharacter ||
                commit.conversations
            ) {
                yieldedAfterCapture = true
                const committedConversationKeys = new Set(
                    (commit.conversations ?? []).flatMap((mutation) =>
                        mutation.type === 'reorder'
                            ? []
                            : [`${mutation.characterId}\u0000${mutation.conversationId}`],
                    ),
                )
                const replacedCharacterId = commit.replaceCharacter?.chaId
                const persistedConversationMutations =
                    conversationProjection?.coveredPending.filter(
                        ({ event }) =>
                            event.characterId === replacedCharacterId ||
                            committedConversationKeys.has(
                                `${event.characterId}\u0000${event.conversationId}`,
                            ),
                        ) ?? []
                const persistedConversationMutationSet = new Set(
                    persistedConversationMutations,
                )
                const fallbackPersistedConversationMutations =
                    pendingConversationMutations.filter(
                        (pending) =>
                            !persistedConversationMutationSet.has(pending) &&
                            (pending.event.characterId === replacedCharacterId ||
                                committedConversationKeys.has(
                                    `${pending.event.characterId}\u0000${pending.event.conversationId}`,
                                )),
                    )
                const persistenceHandles: ConversationMutationPersistenceHandle[] = []
                for (const { event } of persistedConversationMutations) {
                    try {
                        const handle =
                            this.dependencies.onConversationMutationPersistenceStarted?.(event)
                        if (handle) persistenceHandles.push(handle)
                    } catch (error) {
                        this.reportBackgroundError(error)
                    }
                }
                try {
                    let committed: { revision: DataRevision }
                    try {
                        committed = await this.commitFlush(commit)
                    } catch (error) {
                        if (!(await this.mergeConcurrentRecordChange(error))) throw error
                        continue
                    }
                    for (const [id, value] of materialized) this.materializedBaselines.set(id, value)
                    if (this.dependencies.captureCharacters && !windowedCapture) this.setCharacterBaseline(captured)
                    this.currentRevision = committed.revision
                    if (commit.root || commit.rootMutations || commit.unitMutations?.some((value) => {
                        const kind = JSON.parse(value.key)[0]
                        return ['record', 'order', 'persona', 'toggle', 'variable', 'preset-protected'].includes(kind)
                    }))
                        this.rootBaseline = captured.rootCanonical
                    if (commit.pluginStorage) {
                        this.pluginStorageBaseline = captured.pluginStorageCanonical
                    }
                    if (commit.replacePresets || commit.unitMutations?.some((value) => { const [kind, scope] = JSON.parse(value.key); return kind === 'preset' || (kind === 'exists' && scope === 'preset') || (kind === 'order' && scope === 'presets') })) this.presetsBaseline = captured.presetsCanonical
                    if (commit.replaceCharacter && !replacementIsAddition) {
                        if (detached && captured.character) {
                            this.characterBaseline = detached.canonical
                            this.characterBaselineId = detached.character.chaId
                        } else {
                            this.setCharacterBaseline(captured)
                        }
                        if (
                            addition &&
                            commit.replaceCharacter.chaId === addition.pending.characterId
                        ) {
                            addition.pending.baseline = detached
                                ? detached.canonical
                                : captured.characterCanonical!
                        }
                    }
                    if (commit.character && captured.character) {
                        this.setCharacterBaseline(captured)
                    }
                    if (commit.conversations && captured.character) {
                        this.setCharacterBaseline(captured)
                        if (addition && captured.character.chaId === addition.pending.characterId) {
                            addition.pending.baseline = captured.characterCanonical!
                        }
                    }
                    if (persistedConversationMutations.length > 0) {
                        this.acknowledgeConversationMutations(
                            persistedConversationMutations,
                            committed.revision,
                        )
                    }
                    if (fallbackPersistedConversationMutations.length > 0) {
                        this.acknowledgeFallbackConversationMutations(
                            fallbackPersistedConversationMutations,
                            committed.revision,
                        )
                    }
                    if (windowedCapture) {
                        const recency = conversationProjection?.coveredRecency
                        if (recency && this.pendingCharacterRecency) {
                            this.pendingCharacterRecency = this.pendingCharacterRecency === recency ? null
                                : { ...this.pendingCharacterRecency, before: recency.after }
                        }
                        if (
                            conversationProjection?.coveredActivation &&
                            this.pendingWindowedActivationChange ===
                                conversationProjection.coveredActivation
                        ) {
                            this.pendingWindowedActivationChange = null
                        }
                        if (
                            conversationProjection?.coveredChatList &&
                            this.pendingWindowedChatListChange ===
                                conversationProjection.coveredChatList
                        ) {
                            this.pendingWindowedChatListChange = null
                        }
                        const persistedSessionVersion =
                            persistedConversationMutations.at(-1)?.event.sessionVersion ??
                            windowedCapture.authority.persistedSessionVersion
                        this.setWindowedCharacterBaseline(
                            windowedCapture,
                            committed.revision,
                            persistedSessionVersion,
                        )
                    }
                    if (replacementIsAddition && addition) {
                        addition.pending.baseline = addition.canonical
                    }
                    if (addition && replacementIsAddition) {
                        this.materializedBaselines.set(addition.pending.characterId, addition.character)
                        if (!this.dependencies.canonicalCapture?.materializedCharacters) {
                            const json = this.dependencies.canonicalCapture?.characters?.().get(addition.pending.characterId)
                            if (json !== undefined && canonicalJson(captureMaterializedCharacter(this.dependencies.captureCharacter(addition.pending.characterId)!)) === canonicalJson(addition.character)) this.materializedCanonicalBaselines.set(addition.pending.characterId, json)
                        }
                    }
                    this.dependencies.onLocalRevision?.(committed.revision)
                    if (this.dependencies.officialPublisher) {
                        if (publishOfficial) await this.stagePublication(committed.revision)
                        else this.deferPublication(committed.revision)
                    }
                } finally {
                    for (const handle of persistenceHandles) {
                        try {
                            handle.release()
                        } catch (error) {
                            this.reportBackgroundError(error)
                        }
                    }
                }
            }

            for (const [id, value] of materialized) this.materializedBaselines.set(id, value)
            for (const [id, json] of (this.dependencies.canonicalCapture?.materializedCharacters ? [] : this.dependencies.canonicalCapture?.characters?.()) ?? []) {
                if (materialized.has(id) && (this.dependencies.canonicalCapture?.materializedCharacters?.().get(id) === materialized.get(id) || (!this.dependencies.canonicalCapture?.materializedCharacters && canonicalJson(captureMaterializedCharacter(this.dependencies.captureCharacter(id)!)) === canonicalJson(materialized.get(id))))) this.materializedCanonicalBaselines.set(id, json)
            }
            for (const [id, value] of presetRecords) this.presetRecordBaselines.set(id, value)
            if (
                windowedCapture === null &&
                captured.characterCanonical === null &&
                !commit.replaceCharacter
            )
                this.setCharacterBaseline(captured)

            if (this.dependencies.captureCharacters && !windowedCapture && !commit.unitMutations?.length && !commit.conversations?.length) this.setCharacterBaseline(captured)

            // With no await or commit, the live state cannot have changed between
            // these captures. Reuse the snapshot instead of serializing it twice.
            const current = yieldedAfterCapture ? this.capture() : captured
            const currentAddition = this.capturePendingAddition()
            if (
                generation === this.dirtyGeneration &&
                current.rootCanonical === this.rootBaseline &&
                this.pluginStorageMatchesBaseline(current) &&
                (current.presetsCanonical === null ||
                    current.presetsCanonical === this.presetsBaseline) &&
                this.selectedCaptureMatchesBaseline(current) &&
                this.materializedCaptureMatchesBaseline() &&
                (this.dependencies.capturePresetRecords?.() ?? []).every((value) => canonicalJson(value) === canonicalJson(this.presetRecordBaselines.get(value['id'] as string) ?? null)) &&
                (!currentAddition ||
                    (currentAddition.pending.locallyAdded &&
                        currentAddition.canonical === currentAddition.pending.baseline))
            ) {
                if (this.pendingConversationMutations.length > 0) {
                    if (pendingConversationMutations.some((pending) =>
                        this.pendingConversationMutations.includes(pending),
                    )) {
                        throw new Error('Pending conversation mutations could not be persisted')
                    }
                    continue
                }
                this.pendingByteCount = 0
                this.persistedDirtyGeneration = generation
                localState.settled = true
                this.setLocalSaveFailure(null)
                this.backgroundRetryDelay = 2_000
                if (publishOfficial && this.pendingPublicationRevision !== null) {
                    const delay = this.officialPublishDelayMs()
                    if (delay <= 0) {
                        await this.publishPendingRevision()
                        continue
                    }
                    this.armOfficialPublishRetry(delay)
                    this.pendingCharacterAddition = null
                    return
                }
                if (
                    !publishOfficial &&
                    this.hasPendingOfficialPublication &&
                    !this.publicationInProgress
                ) {
                    this.armOfficialPublishRetry(this.officialPublishDelayMs())
                }
                this.pendingCharacterAddition = null
                this.lastBackgroundErrorMessage = null
                return
            }
        }
    }

    /** Marks a committed revision for official publication, superseding any stale pinned one. */
    private async stagePublication(revision: DataRevision): Promise<void> {
        if (this.pendingPublicationRevision !== revision && this.pendingPublication) {
            const stale = this.pendingPublication
            this.pendingPublication = null
            await this.disposeOrQueuePublication(stale)
        }
        this.pendingPublicationRevision = revision
    }

    private deferPublication(revision: DataRevision): void {
        if (!this.publicationInProgress && !this.pendingPublication) {
            this.pendingPublicationRevision = revision
            this.deferredPublicationRevision = null
            return
        }
        this.deferredPublicationRevision = revision
    }

    private async applyDeferredPublication(): Promise<void> {
        const revision = this.deferredPublicationRevision
        if (revision === null || this.publicationInProgress) return
        this.deferredPublicationRevision = null
        await this.stagePublication(revision)
    }

    private async runReplacement(
        candidate: Database,
        admission: ReplacementAdmission,
        options: PersistentReplacementOptions,
    ): Promise<CommittedApplyOutcome> {
        const candidateCapture = this.captureDatabase(candidate)
        const replaced = options.pluginStorageValues
            ? await this.dependencies.store.replaceFromDatabase(
                  candidate,
                  admission.revision,
                  [],
                  options.pluginStorageValues,
              )
            : await this.dependencies.store.replaceFromDatabase(candidate, admission.revision)
        // This revision is authoritative even when a later projection or notification fails.
        this.authorityEpoch++
        this.currentRevision = replaced.revision
        const stalePublication = options.publishOfficial ? null : this.pendingPublication
        if (!options.publishOfficial) {
            this.pendingPublication = null
            this.pendingPublicationRevision = null
            this.deferredPublicationRevision = null
            this.cancelOfficialPublishRetry()
        }
        if (this.pendingCharacterAddition?.token === admission.supersededAdditionToken) {
            this.pendingCharacterAddition = null
        }
        if (this.reservedCharacterAddition?.token === admission.supersededAdditionToken) {
            this.reservedCharacterAddition = null
        }
        this.rootBaseline = candidateCapture.rootCanonical
        this.pluginStorageBaseline = candidateCapture.pluginStorageCanonical
        this.presetsBaseline = candidateCapture.presetsCanonical
        this.setCharacterBaseline(candidateCapture)
        this.pendingByteCount = 0
        this.cancelDebounce()

        try {
            if (
                this.destructiveReplacementFence?.blockedPrePublicationDirty ||
                this.dependencies.getNavigationGeneration?.() !== admission.navigationGeneration ||
                !this.captureMatchesCapturedState(admission.baseline)
            ) {
                throw new PersistentMutationFencedError()
            }
            this.dependencies.onLocalRevision?.(replaced.revision)
            this.dependencies.replaceDatabase(candidate)
            // Projection may select a different character when the old selection is absent.
            // Baseline the committed candidate for that selection, never the mutable live view.
            this.setCharacterBaseline(this.captureDatabase(candidate))
        } catch (error) {
            this.markCommittedWorkingSetRefreshRequired(replaced.revision, error)
        }
        // Cleanup and official publication are queued while the input guard is held.
        // Neither operation can turn a completed local replacement into a retryable write.
        try {
            if (stalePublication) await this.disposeOrQueuePublication(stalePublication)
            if (options.publishOfficial) await this.finishExplicitCommit(replaced.revision)
        } catch (error) {
            this.reportBackgroundError(error)
        }
        return {
            kind: 'committed',
            revision: replaced.revision,
            projection: this.committedRefreshRevision === null ? 'applied' : 'refresh-required',
        }
    }

    private async readCompleteCharacter(
        characterId: string,
        revision: DataRevision,
        detail: CharacterDetail,
    ): Promise<CompleteCharacter> {
        const conversations: Chat[] = []
        let cursor: string | undefined
        do {
            const page = await this.dependencies.store.queryConversations({
                characterId,
                order: 'configured',
                limit: 100,
                cursor,
            })
            this.assertReadRevision(revision, page.revision)
            for (const summary of page.items) {
                if (summary.characterId !== characterId) {
                    throw new Error(`Conversation ${summary.id} belongs to another character`)
                }
                const value = await this.dependencies.store.readConversation(
                    characterId,
                    summary.id,
                )
                if (!value) throw new Error(`Conversation ${summary.id} was not found`)
                this.assertReadRevision(revision, value.revision)
                if (value.value.id !== summary.id) {
                    throw new Error(`Conversation ${summary.id} returned mismatched content`)
                }
                conversations.push(canonicalClone(value.value))
            }
            cursor = page.nextCursor
        } while (cursor)
        return {
            ...canonicalClone(detail),
            chats: conversations,
        } as CompleteCharacter
    }

    private async readConversationAtRevision(
        characterId: string,
        orderedPosition: number,
        revision: DataRevision,
    ): Promise<Chat | null> {
        const page = await this.dependencies.store.queryConversations({
            characterId,
            order: 'configured',
            limit: 1,
            cursor: orderedPosition === 0 ? undefined : String(orderedPosition),
        })
        this.assertReadRevision(revision, page.revision)
        const summary = page.items[0]
        if (!summary) return null
        if (summary.characterId !== characterId) {
            throw new Error(`Conversation position ${orderedPosition} returned mismatched content`)
        }
        const value = await this.dependencies.store.readConversation(characterId, summary.id)
        if (!value) throw new Error(`Conversation ${summary.id} was not found`)
        this.assertReadRevision(revision, value.revision)
        if (value.value.id !== summary.id) {
            throw new Error(`Conversation ${summary.id} returned mismatched content`)
        }
        return canonicalClone(value.value)
    }

    private captureResidentCharacter(characterId: string): {
        character: CompleteCharacter
        canonical: string
        conversationStubIds: ReadonlySet<string>
    } | null {
        if (
            this.windowedCharacterBaseline?.authority.characterId === characterId ||
            this.dependencies.captureSelectedConversationAuthority?.()?.characterId === characterId
        ) {
            throw new WindowedConversationRequiresCompatibilityError(
                'resident character capture requires complete ownership',
            )
        }
        const character = this.dependencies.captureCharacter(characterId)
        if (!character) return null
        const conversationStubIds = new Set(
            character.chats
                .filter(isConversationSummaryStub)
                .map((conversation) => conversation.id)
                .filter((id): id is string => Boolean(id)),
        )
        const canonical = canonicalJson(character)
        return {
            character: JSON.parse(canonical) as CompleteCharacter,
            canonical,
            conversationStubIds,
        }
    }

    private residentCharactersMatch(
        left: { canonical: string } | null,
        right: { canonical: string } | null,
    ): boolean {
        return left?.canonical === right?.canonical
    }

    private captureResidentConversation(
        characterId: string,
        conversationId: string,
    ): { conversation: Chat; canonical: string } | null {
        const character = this.dependencies.captureCharacter(characterId)
        const conversation = character?.chats.find(
            (candidate) => candidate.id === conversationId && !isConversationSummaryStub(candidate),
        )
        // Metadata-only shells hold no messages (their message getter throws),
        // so they must not be treated as a resident complete conversation.
        if (!conversation || isMetadataOnlySelectedConversation(conversation)) return null
        const canonical = canonicalJson(conversation)
        return {
            conversation: JSON.parse(canonical) as Chat,
            canonical,
        }
    }

    private isResidentConversationSummary(characterId: string, conversationId: string): boolean {
        return (
            this.dependencies
                .captureCharacter(characterId)
                ?.chats.some(
                    (candidate) =>
                        candidate.id === conversationId && isConversationSummaryStub(candidate),
                ) === true
        )
    }

    private residentConversationsMatch(
        left: { canonical: string } | null,
        right: { canonical: string } | null,
    ): boolean {
        return left?.canonical === right?.canonical
    }

    private async finishPersistentConversationReplacement(
        result: PersistentConversationReplacementResult,
        options: {
            preservePendingWork?: boolean
            publish?: boolean
            residentBefore?: { conversation: Chat; canonical: string } | null
            summaryBefore?: boolean
        } = {},
    ): Promise<void> {
        this.currentRevision = result.revision
        this.dirtyGeneration++
        if (options.publish !== false) this.dependencies.publishConversationReplacement?.(result)
        this.advanceCharacterBaselineForConversation(result, options.residentBefore ?? null)
        if (options.summaryBefore === true) {
            this.advanceCharacterSummaryBaselineForConversation(result)
        }
        if (!options.preservePendingWork) this.pendingByteCount = 0
        this.lastBackgroundErrorMessage = null
        this.dependencies.onLocalRevision?.(result.revision)
        await this.finishExplicitCommit(result.revision)
    }

    private assertResidentCharacterUnchanged(
        characterId: string,
        before: { canonical: string } | null,
    ): void {
        if (!this.residentCharactersMatch(before, this.captureResidentCharacter(characterId))) {
            throw new Error(`Resident character changed during persistent mutation: ${characterId}`)
        }
    }

    private mergeCommittedDetailWithResident(
        detail: CharacterDetail,
        resident: CompleteCharacter | null,
    ): CompleteCharacter {
        return {
            ...canonicalClone(detail),
            chats: canonicalClone(resident?.chats ?? []),
        } as CompleteCharacter
    }

    private finishCharacterMutation(
        result: PersistentCharacterMutationResult,
        committedRoot: RootDatabase,
        options: {
            preservePendingWork?: boolean
            additionalDeletedCharacterIds?: readonly string[]
            publish?: boolean
            committedSelectedCharacter?: { id: string; character: CompleteCharacter | null }
        } = {},
    ): void {
        this.currentRevision = result.revision
        this.dirtyGeneration++
        this.rootBaseline = canonicalJson(committedRoot)
        const previousMaterialized = this.materializedBaselines.get(result.characterId)
        if (result.kind === 'delete') { this.materializedBaselines.delete(result.characterId); this.materializedCanonicalBaselines.delete(result.characterId) }
        else if (result.character) this.materializedBaselines.set(result.characterId,
            'chats' in result.character ? captureMaterializedCharacter(result.character as CompleteCharacter)
                : {...canonicalClone(result.character), chats: previousMaterialized?.chats ?? []} as CompleteCharacter)
        if (options.publish !== false) {
            for (const id of options.additionalDeletedCharacterIds ?? []) {
                this.dependencies.publishCharacterMutation?.({ ...result, characterId: id })
            }
            this.dependencies.publishCharacterMutation?.(result)
            const published = this.capture()
            if (
                published.character?.chaId === result.characterId ||
                (result.kind === 'delete' && !published.character && !published.windowedCharacter)
            ) {
                this.setCharacterBaseline(published)
            }
        }
        // A character kept in the catalog as a stub holds no detail to diff against.
        if (result.kind !== 'delete') this.forgetMaterializedCharacter(result.characterId)
        if (
            options.committedSelectedCharacter !== undefined &&
            this.capture().character?.chaId === options.committedSelectedCharacter.id
        ) {
            this.characterBaseline = options.committedSelectedCharacter.character
                ? canonicalJson(options.committedSelectedCharacter.character)
                : null
            this.characterBaselineId = options.committedSelectedCharacter.character?.chaId ?? null
        }
        this.dependencies.onLocalRevision?.(result.revision)
        if (!options.preservePendingWork) this.pendingByteCount = 0
        this.lastBackgroundErrorMessage = null
    }

    private advanceCharacterBaselineForConversation(
        result: PersistentConversationReplacementResult,
        residentBefore: { conversation: Chat; canonical: string } | null,
    ): void {
        if (
            residentBefore === null ||
            this.windowedCharacterBaseline !== null ||
            this.characterBaselineId !== result.characterId ||
            this.characterBaseline === null
        )
            return
        const baseline = JSON.parse(this.characterBaseline) as CompleteCharacter
        const index = baseline.chats.findIndex(
            (conversation) => conversation.id === result.conversationId,
        )
        if (index < 0) return
        baseline.chats[index] = canonicalClone(result.conversation)
        this.characterBaseline = canonicalJson(baseline)
    }

    private advanceCharacterSummaryBaselineForConversation(
        result: PersistentConversationReplacementResult,
    ): void {
        if (this.windowedCharacterBaseline !== null) {
            const baseline = this.windowedCharacterBaseline
            if (
                baseline.authority.characterId !== result.characterId ||
                baseline.authority.conversationId === result.conversationId
            )
                return
            const matches = baseline.shell.chats
                .map((conversation, index) => ({ conversation, index }))
                .filter(({ conversation }) => conversation.id === result.conversationId)
            if (matches.length !== 1) return
            const summary = canonicalClone(
                createConversationSummaryStubFromChat(
                    result.characterId,
                    result.conversation,
                    matches[0].index,
                ),
            )
            const { message: _message, ...shell } = summary
            baseline.shell.chats[matches[0].index] = shell
            baseline.shellCanonical = canonicalJson(baseline.shell)
            return
        }
        if (this.characterBaselineId !== result.characterId || this.characterBaseline === null)
            return
        const baseline = JSON.parse(this.characterBaseline) as CompleteCharacter
        const index = baseline.chats.findIndex(
            (conversation) => conversation.id === result.conversationId,
        )
        if (index < 0) return
        baseline.chats[index] = canonicalClone(
            createConversationSummaryStubFromChat(result.characterId, result.conversation, index),
        )
        this.characterBaseline = canonicalJson(baseline)
    }

    finishUpstreamReplacementPublication(revision: DataRevision, publishOfficial = false): Promise<void> {
        this.assertPersistentMutationAllowed()
        return this.enqueue(async () => {
            if (publishOfficial) {
                await this.finishExplicitCommit(Math.max(revision, this.pendingPublicationRevision ?? revision,
                    this.deferredPublicationRevision ?? revision))
                return
            }
            const stale = this.pendingPublicationRevision !== null && this.pendingPublicationRevision <= revision
                ? this.pendingPublication : null
            if (this.pendingPublicationRevision !== null && this.pendingPublicationRevision <= revision) {
                this.pendingPublication = null
                this.pendingPublicationRevision = null
            }
            if (this.deferredPublicationRevision !== null && this.deferredPublicationRevision <= revision) {
                this.deferredPublicationRevision = null
            }
            if (this.pendingPublicationRevision === null && this.deferredPublicationRevision === null) this.cancelOfficialPublishRetry()
            else this.armOfficialPublishRetry(this.officialPublishDelayMs())
            if (stale) await this.disposeOrQueuePublication(stale)
        })
    }

    private async finishExplicitCommit(revision: DataRevision, verifyBaseline = true): Promise<void> {
        if (verifyBaseline && this.captureMatchesBaseline() && this.pendingConversationMutations.length === 0 &&
            this.pendingWindowedActivationChange === null && this.pendingWindowedChatListChange === null) {
            this.persistedDirtyGeneration = this.dirtyGeneration
            this.setLocalSaveFailure(null)
        } else {
            this.armDebounce()
        }
        if (!this.dependencies.officialPublisher) return
        this.deferPublication(revision)
        this.armOfficialPublishRetry(this.officialPublishDelayMs())
    }

    private trackPromiseSlot(
        promise: Promise<void>,
        get: () => Promise<void> | null,
        set: (value: Promise<void> | null) => void,
    ): void {
        set(promise)
        this.reportActivePromise()
        const clear = () => {
            if (get() === promise) {
                set(null)
                this.reportActivePromise()
            }
        }
        void promise.then(clear, clear)
    }

    private setCharacterBaseline(captured: CapturedState): void {
        this.characterBaseline = captured.characterSnapshot ? null : captured.characterCanonical
        this.characterBaselineId = captured.characterId !== undefined ? captured.characterId : captured.character?.chaId ?? null
        this.windowedCharacterBaseline = null
        this.pendingWindowedActivationChange = null
        this.pendingWindowedChatListChange = null
        this.pendingCharacterRecency = null
    }

    /**
     * Saves a windowed selection whose live state the recorded edits cannot explain: the
     * conversation is rebuilt from the persisted messages and those edits, published complete,
     * and diffed against the persisted state on the next pass. Returns false when this cannot
     * run now, leaving the original failure to the usual retry.
     */
    private async completeWindowedConversation(
        current: WindowedSelectedCharacterCapture,
        pending: readonly PendingConversationMutation[],
        cause: WindowedConversationRequiresCompatibilityError,
    ): Promise<boolean> {
        const complete = this.dependencies.completeWindowedSelectedConversation
        const baseline = this.windowedCharacterBaseline
        if (
            !complete ||
            !this.dependencies.captureCharacters ||
            !baseline ||
            this.dependencies.isConversationOperationActive?.() === true
        )
            return false
        const authority = current.authority
        let persisted: Awaited<ReturnType<PersistentDataStore['readConversation']>>
        try {
            persisted = await this.dependencies.store.readConversation(
                authority.characterId,
                authority.conversationId,
            )
        } catch (error) {
            throw new WindowedConversationSaveError('the persisted conversation could not be read', error)
        }
        if (
            !persisted ||
            persisted.revision !== this.revision ||
            persisted.value.id !== authority.conversationId ||
            !Array.isArray(persisted.value.message) ||
            persisted.value.message.length !== baseline.authority.totalMessages
        )
            throw new WindowedConversationSaveError('the persisted conversation changed', cause)
        const persistedConversation = persisted.value
        let messages = persistedConversation.message
        let version = baseline.authority.persistedSessionVersion
        for (const { event } of pending) {
            if (
                event.characterId !== authority.characterId ||
                event.conversationId !== authority.conversationId ||
                event.sessionToken !== authority.sessionToken ||
                event.previousVersion !== version ||
                event.sessionVersion <= version
            )
                throw new WindowedConversationSaveError('recorded edits are not a contiguous prefix', cause)
            for (const range of event.mutations) {
                if (
                    range.completeOwner ||
                    range.start > messages.length ||
                    range.deleteCount > messages.length - range.start
                )
                    throw new WindowedConversationSaveError('a recorded edit exceeds the conversation', cause)
                messages = messages
                    .slice(0, range.start)
                    .concat(range.messages, messages.slice(range.start + range.deleteCount))
            }
            version = event.sessionVersion
        }
        if (version !== authority.sessionVersion || messages.length !== authority.totalMessages)
            throw new WindowedConversationSaveError('recorded edits do not reach the live conversation', cause)

        let published: CompleteCharacter | null
        this.selectedConversationTransitionActive = true
        try {
            published = complete(authority, messages)
        } catch (error) {
            throw new WindowedConversationSaveError('the complete conversation could not be published', error)
        } finally {
            this.selectedConversationTransitionActive = false
        }
        if (!published) return false
        const persistedCharacter = {
            ...baseline.shell,
            chats: baseline.shell.chats.map((conversation) =>
                conversation.id === authority.conversationId ? persistedConversation : conversation),
        } as CompleteCharacter
        this.materializedBaselines.set(authority.characterId, captureMaterializedCharacter(persistedCharacter))
        this.materializedCanonicalBaselines.delete(authority.characterId)
        this.characterBaseline = canonicalJson(persistedCharacter)
        this.characterBaselineId = authority.characterId
        this.windowedCharacterBaseline = null
        this.pendingWindowedActivationChange = null
        this.pendingWindowedChatListChange = null
        this.pendingCharacterRecency = null
        const covered = new Set(pending)
        this.pendingConversationMutations = this.pendingConversationMutations.filter(
            (value) => !covered.has(value),
        )
        return true
    }

    private setWindowedCharacterBaseline(
        captured: WindowedSelectedCharacterCapture,
        revision: DataRevision,
        persistedSessionVersion: number,
    ): void {
        this.windowedCharacterBaseline = {
            shell: captured.shell,
            shellCanonical: captured.shellCanonical,
            authority: {
                ...safeStructuredClone(captured.authority),
                storeRevision: revision,
                persistedSessionVersion,
            },
        }
        this.characterBaseline = null
        this.characterBaselineId = null
        this.dependencies.onWindowedSelectedConversationRevision?.(revision)
    }

    private requireMatchingWindowedCapture(
        captured: CapturedState,
    ): WindowedSelectedCharacterCapture | null {
        const baseline = this.windowedCharacterBaseline
        const current = captured.windowedCharacter
        if (!baseline && !current) return null
        if (!baseline || !current) {
            throw new WindowedConversationRequiresCompatibilityError(
                'authority mode changed without explicit adoption',
            )
        }
        if (
            baseline.authority.characterId !== current.authority.characterId ||
            baseline.authority.conversationId !== current.authority.conversationId ||
            baseline.authority.sessionToken !== current.authority.sessionToken ||
            baseline.authority.storeRevision !== this.revision ||
            current.authority.storeRevision !== this.revision ||
            baseline.authority.persistedSessionVersion !==
                current.authority.persistedSessionVersion ||
            baseline.authority.sessionVersion !== baseline.authority.persistedSessionVersion
        ) {
            throw new WindowedConversationRequiresCompatibilityError(
                'session authority no longer matches the adopted baseline',
            )
        }
        return current
    }

    private async projectWindowedConversationMutations(
        captured: CapturedState,
        pending: readonly PendingConversationMutation[],
    ): Promise<ConversationMutationProjection> {
        const current = captured.windowedCharacter
        const baseline = this.windowedCharacterBaseline
        if (!current || !baseline) {
            throw new WindowedConversationRequiresCompatibilityError(
                'windowed projection has no adopted baseline',
            )
        }
        const authority = current.authority
        const relevantPending = pending.filter(
            ({ event }) =>
                event.characterId === authority.characterId &&
                event.conversationId === authority.conversationId &&
                event.sessionToken === authority.sessionToken,
        )
        if (relevantPending.length !== pending.length) {
            throw new WindowedConversationRequiresCompatibilityError(
                'pending evidence belongs to another conversation or session',
            )
        }
        let projectedShell = { ...baseline.shell, chats: baseline.shell.chats.map((conversation) => ({ ...conversation })) }
        const mutations: ConversationMutation[] = []
        const activation = this.pendingWindowedActivationChange
        let activationCharacter: CharacterDetail | undefined
        if (activation) {
            if (!sameWindowedAuthority(activation.authority, baseline.authority)) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'activation evidence belongs to another authority',
                )
            }
            const characterChange = activation.change.character
            if (characterChange) {
                const projectedDetail = cloneOwnPropertiesExcept(
                    projectedShell as unknown as Record<string, unknown>,
                    'chats',
                ) as CharacterDetail
                if (canonicalJson(projectedDetail) !== canonicalJson(characterChange.before)) {
                    throw new WindowedConversationRequiresCompatibilityError(
                        'activation character evidence does not match the persisted baseline',
                    )
                }
                projectedShell = {
                    ...safeStructuredClone(characterChange.after),
                    chats: projectedShell.chats,
                } as WindowedCharacterShell
                activationCharacter = safeStructuredClone(characterChange.after)
            }
            const conversationChange = activation.change.conversation
            if (conversationChange) {
                const matches = projectedShell.chats.filter(
                    (conversation) => conversation.id === authority.conversationId,
                )
                if (
                    matches.length !== 1 ||
                    canonicalJson(matches[0]) !== canonicalJson(conversationChange.before)
                ) {
                    throw new WindowedConversationRequiresCompatibilityError(
                        'activation conversation evidence does not match the persisted baseline',
                    )
                }
                projectedShell.chats[projectedShell.chats.indexOf(matches[0])] =
                    safeStructuredClone(conversationChange.after)
                mutations.push({
                    type: 'replace-range',
                    characterId: authority.characterId,
                    conversationId: authority.conversationId,
                    start: 0,
                    deleteCount: 0,
                    messages: [],
                    conversation: safeStructuredClone(conversationChange.after),
                })
            }
        }
        const chatList = this.pendingWindowedChatListChange
        if (chatList) {
            if (
                chatList.characterId !== authority.characterId ||
                chatList.conversationId !== authority.conversationId ||
                chatList.sessionToken !== authority.sessionToken
            ) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'chat-list evidence belongs to another authority',
                )
            }
            const projected = await this.projectWindowedChatListChange(
                chatList,
                projectedShell,
                authority.conversationId,
            )
            projectedShell = projected.shell
            mutations.push(...projected.mutations)
            if (projected.character) activationCharacter = projected.character
        }
        const recency = this.pendingCharacterRecency
        const unitMutations: PersistentUnitMutation[] = []
        if (recency) {
            if (recency.characterId !== authority.characterId || recency.conversationId !== authority.conversationId ||
                recency.sessionToken !== authority.sessionToken || projectedShell.lastInteraction !== recency.before) {
                throw new WindowedConversationRequiresCompatibilityError('character recency evidence does not match the selected baseline')
            }
            projectedShell.lastInteraction = recency.after
            if (activationCharacter) activationCharacter.lastInteraction = recency.after
            else unitMutations.push({ key: JSON.stringify(['character', authority.characterId, 'lastInteraction']), type: 'set', value: recency.after })
        }
        // Paged chat creation publishes the character last; asset tuple edits also need
        // the complete detail for owner validation and retained manifests.
        if (activationCharacter && !chatList && activationCharacter.chaId === baseline.shell.chaId &&
            activationCharacter.type === baseline.shell.type &&
            canonicalJson({ value: activationCharacter.additionalAssets }) === canonicalJson({ value: baseline.shell.additionalAssets })) {
            unitMutations.push(...diffFields(['character', authority.characterId], baseline.shell, activationCharacter,
                new Set(['chats', 'chaId', 'type'])))
            activationCharacter = undefined
        }
        if (relevantPending.length === 0) {
            if (
                authority.sessionVersion !== authority.persistedSessionVersion ||
                authority.totalMessages !== baseline.authority.totalMessages ||
                current.shellCanonical !== canonicalJson(projectedShell)
            ) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'selected changes have no exact mutation evidence',
                )
            }
            return {
                exactMutations: mutations.length > 0 ? mutations : null,
                coveredPending: [],
                character: activationCharacter,
                coveredActivation: activation ?? undefined,
                coveredChatList: chatList ?? undefined,
                unitMutations, coveredRecency: recency ?? undefined,
            }
        }

        const persisted = await this.dependencies.store.readConversationWindow({
            characterId: authority.characterId,
            conversationId: authority.conversationId,
            startIndex: 0,
            limit: 1,
        })
        if (
            !persisted ||
            persisted.revision !== this.revision ||
            persisted.value.characterId !== authority.characterId ||
            persisted.value.conversationId !== authority.conversationId ||
            persisted.value.startIndex !== 0 ||
            !Number.isSafeInteger(persisted.value.totalMessages) ||
            persisted.value.totalMessages < 0 ||
            persisted.value.totalMessages !== baseline.authority.totalMessages
        ) {
            throw new WindowedConversationRequiresCompatibilityError(
                'persistent conversation count or revision changed',
            )
        }

        const projectedMatches = projectedShell.chats.filter(
            (conversation) => conversation.id === authority.conversationId,
        )
        const currentMatches = current.shell.chats.filter(
            (conversation) => conversation.id === authority.conversationId,
        )
        if (projectedMatches.length !== 1 || currentMatches.length !== 1) {
            throw new WindowedConversationRequiresCompatibilityError(
                'selected conversation identity is ambiguous',
            )
        }

        let expectedVersion = baseline.authority.persistedSessionVersion
        let messageCount = persisted.value.totalMessages
        for (const pendingMutation of relevantPending) {
            const { event } = pendingMutation
            if (
                event.previousVersion !== expectedVersion ||
                event.sessionVersion <= expectedVersion
            ) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'mutation versions are not a contiguous prefix',
                )
            }
            const conversationMetadata = cloneOwnPropertiesExcept(
                event.conversation as Record<string, unknown>,
                'message',
            ) as Omit<Chat, 'message'>
            for (const range of event.mutations) {
                if (range.completeOwner) {
                    throw new WindowedConversationRequiresCompatibilityError(
                        'complete-owner evidence requires complete ownership',
                    )
                }
                const deleteCount = range.deleteCount
                if (range.start > messageCount || deleteCount > messageCount - range.start) {
                    throw new WindowedConversationRequiresCompatibilityError(
                        'absolute mutation range exceeds the persistent conversation',
                    )
                }
                const messages = safeStructuredClone(range.messages)
                messageCount = messageCount - deleteCount + messages.length
                mutations.push({
                    type: 'replace-range',
                    characterId: event.characterId,
                    conversationId: event.conversationId,
                    start: range.start,
                    deleteCount,
                    messages,
                    conversation: safeStructuredClone(conversationMetadata),
                })
            }
            const target = projectedMatches[0] as Record<string, unknown>
            const metadata = event.conversation as Record<string, unknown>
            for (const key of Object.keys(target)) {
                if (!Object.hasOwn(metadata, key)) delete target[key]
            }
            for (const key of Object.keys(metadata)) {
                if (key !== 'message') target[key] = safeStructuredClone(metadata[key])
            }
            expectedVersion = event.sessionVersion
        }
        if (
            expectedVersion !== authority.sessionVersion ||
            messageCount !== authority.totalMessages ||
            canonicalJson(projectedShell) !== current.shellCanonical
        ) {
            throw new WindowedConversationRequiresCompatibilityError(
                'mutation evidence does not explain the selected projection',
            )
        }
        return {
            exactMutations: mutations.length > 0 ? mutations : null,
            coveredPending: [...relevantPending],
            character: activationCharacter,
            coveredActivation: activation ?? undefined,
            coveredChatList: chatList ?? undefined,
            unitMutations, coveredRecency: recency ?? undefined,
        }
    }

    /**
     * Turns recorded chat-list evidence into store mutations. Unselected chats may be
     * summary stubs with placeholder bodies, so their metadata changes are applied key
     * by key onto the persisted metadata instead of being written whole.
     */
    private async projectWindowedChatListChange(
        change: PendingWindowedChatListChange,
        projectedShell: WindowedCharacterShell,
        selectedConversationId: string,
    ): Promise<{
        shell: WindowedCharacterShell
        mutations: ConversationMutation[]
        character?: CharacterDetail
    }> {
        if (canonicalJson(projectedShell) !== change.beforeCanonical) {
            throw new WindowedConversationRequiresCompatibilityError(
                'chat-list evidence does not match the persisted baseline',
            )
        }
        const characterId = change.before.chaId
        const beforeById = new Map(
            change.before.chats.map((conversation) => [conversation.id, conversation]),
        )
        const afterIds = change.after.chats.map((conversation) => conversation.id!)
        const surviving = new Set(afterIds)
        const mutations: ConversationMutation[] = []
        const order: string[] = []
        for (const conversation of change.before.chats) {
            if (surviving.has(conversation.id!)) {
                order.push(conversation.id!)
                continue
            }
            mutations.push({ type: 'delete', characterId, conversationId: conversation.id! })
        }
        for (const conversation of change.after.chats) {
            const previous = beforeById.get(conversation.id)
            if (!previous || canonicalJson(previous) === canonicalJson(conversation)) continue
            let metadata: Omit<Chat, 'message'>
            if (conversation.id === selectedConversationId) {
                metadata = safeStructuredClone(conversation)
            } else {
                const persisted = await this.dependencies.store.readConversationMetadata(
                    characterId,
                    conversation.id!,
                )
                if (!persisted || persisted.revision !== this.revision) {
                    throw new WindowedConversationRequiresCompatibilityError(
                        'persistent conversation metadata changed',
                    )
                }
                const patched = safeStructuredClone(persisted.value.conversation) as Record<
                    string,
                    unknown
                >
                const before = previous as Record<string, unknown>
                const after = conversation as Record<string, unknown>
                for (const key of Object.keys(before)) {
                    if (!Object.hasOwn(after, key)) delete patched[key]
                }
                for (const key of Object.keys(after)) {
                    const unchanged =
                        Object.hasOwn(before, key) &&
                        (before[key] === after[key] ||
                            (before[key] !== undefined &&
                                after[key] !== undefined &&
                                canonicalJson(before[key]) === canonicalJson(after[key])))
                    if (!unchanged) patched[key] = safeStructuredClone(after[key])
                }
                metadata = patched as Omit<Chat, 'message'>
            }
            mutations.push({
                type: 'replace-range',
                characterId,
                conversationId: conversation.id!,
                start: 0,
                deleteCount: 0,
                messages: [],
                conversation: metadata,
            })
        }
        for (const conversation of change.after.chats) {
            if (beforeById.has(conversation.id)) continue
            const messages = change.insertedMessages.get(conversation.id!)
            if (!messages) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'added chat has no recorded body',
                )
            }
            mutations.push({
                type: 'replace-range',
                characterId,
                conversationId: conversation.id!,
                start: 0,
                deleteCount: 0,
                // The recorded body is already a private copy, and the commit copies it again before sending.
                messages,
                conversation: safeStructuredClone(conversation),
                // An explicit append position makes the store refuse an existing id.
                configuredIndex: order.length,
            })
            order.push(conversation.id!)
        }
        if (order.some((id, index) => id !== afterIds[index])) {
            mutations.push({ type: 'reorder', characterId, conversationIds: afterIds })
        }
        const beforeDetail = cloneOwnPropertiesExcept(
            change.before as unknown as Record<string, unknown>,
            'chats',
        )
        const afterDetail = cloneOwnPropertiesExcept(
            change.after as unknown as Record<string, unknown>,
            'chats',
        ) as CharacterDetail
        return {
            shell: { ...change.after, chats: change.after.chats.map((conversation) => ({ ...conversation })) },
            mutations,
            character:
                canonicalJson(beforeDetail) === canonicalJson(afterDetail)
                    ? undefined
                    : afterDetail,
        }
    }

    private selectedCaptureMatchesBaseline(captured: CapturedState): boolean {
        if (captured.characterSnapshot) return captured.characterSnapshot === this.materializedBaselines.get(captured.characterSnapshot.chaId)
        if (this.windowedCharacterBaseline) {
            return (
                captured.windowedCharacter !== null &&
                captured.windowedCharacter.shellCanonical ===
                    this.windowedCharacterBaseline.shellCanonical &&
                sameWindowedAuthority(
                    captured.windowedCharacter.authority,
                    this.windowedCharacterBaseline.authority,
                )
            )
        }
        return (
            captured.windowedCharacter === null &&
            captured.characterCanonical === this.characterBaseline
        )
    }

    private projectConversationMutations(
        captured: CapturedState,
        pending: readonly PendingConversationMutation[],
    ): ConversationMutationProjection | null {
        const materializedBaseline = captured.characterSnapshot
            ? this.materializedBaselines.get(captured.characterSnapshot.chaId) : undefined
        if (
            pending.length === 0 ||
            !captured.character ||
            (this.characterBaseline === null && !materializedBaseline) ||
            this.characterBaselineId !== captured.character.chaId
        )
            return null

        const projected = materializedBaseline
            ? { ...materializedBaseline, chats: [...materializedBaseline.chats] }
            : JSON.parse(this.characterBaseline!) as CompleteCharacter
        const relevantPending = pending.filter(({ event }) => event.characterId === projected.chaId)
        if (relevantPending.length === 0) return null
        const pendingByConversation = new Map<string, PendingConversationMutation[]>()
        for (const pendingMutation of relevantPending) {
            const conversationPending =
                pendingByConversation.get(pendingMutation.event.conversationId) ?? []
            conversationPending.push(pendingMutation)
            pendingByConversation.set(pendingMutation.event.conversationId, conversationPending)
        }

        let exactConversationValues = true
        const coveredSet = new Set<PendingConversationMutation>()
        const mutationsByPending = new Map<PendingConversationMutation, ConversationMutation[]>()
        for (const [conversationId, conversationPending] of pendingByConversation) {
            if (captured.conversationStubIds.has(conversationId)) continue
            const projectedMatches = projected.chats.filter(
                (candidate) => candidate.id === conversationId,
            )
            const capturedMatches = captured.character.chats.filter(
                (candidate) => candidate.id === conversationId,
            )
            if (projectedMatches.length !== 1 || capturedMatches.length !== 1) continue
            const projectedIndex = projected.chats.indexOf(projectedMatches[0])
            const baselineConversation = projectedMatches[0]
            if (!Array.isArray(baselineConversation.message)) continue
            // Metadata-only events leave the baseline's messages as they are, so the projection
            // shares that array; when the capture holds the same array, only metadata is compared.
            const sharesMessages = conversationPending.every(({ event }) => event.mutations.every((range) =>
                !range.completeOwner && range.deleteCount === 0 && range.messages.length === 0))
            // The projection owns its array and metadata; unchanged messages stay the baseline's
            // values, so comparing them with a capture that shares them is a reference check.
            const conversation = cloneConversationSharingMessages(baselineConversation)
            if (!sharesMessages) conversation.message = conversation.message.slice()

            let previousEvent: ActiveConversationMutationEvent | undefined
            const conversationMutations: (ConversationMutation & { type: 'replace-range' })[][] = []
            for (const pendingMutation of conversationPending) {
                const { event } = pendingMutation
                const previous = previousEvent
                const continuesSession =
                    previous !== undefined &&
                    previous.sessionToken === event.sessionToken &&
                    previous.sessionVersion === event.previousVersion
                const startsReplacementSession =
                    previous !== undefined &&
                    previous.sessionToken !== event.sessionToken &&
                    event.previousVersion === 0
                if (previous && !continuesSession && !startsReplacementSession) break

                const eventMutations: (ConversationMutation & { type: 'replace-range' })[] = []
                const conversationMetadata = safeStructuredClone(event.conversation) as Omit<
                    Chat,
                    'message'
                >
                let valid = true
                for (const range of event.mutations) {
                    const deleteCount = range.completeOwner
                        ? conversation.message.length
                        : range.deleteCount
                    if (
                        range.start > conversation.message.length ||
                        deleteCount > conversation.message.length - range.start
                    ) {
                        valid = false
                        break
                    }
                    const messages = safeStructuredClone(range.messages)
                    if (!sharesMessages) {
                        replaceArrayRange(conversation.message, range.start, deleteCount, messages)
                    }
                    eventMutations.push({
                        type: 'replace-range',
                        characterId: event.characterId,
                        conversationId: event.conversationId,
                        start: range.start,
                        deleteCount,
                        messages,
                        conversation: safeStructuredClone(conversationMetadata),
                    })
                }
                if (!valid) break
                const target = conversation as unknown as Record<string, unknown>
                const metadata = event.conversation as Record<string, unknown>
                for (const key of Object.keys(target)) {
                    if (key !== 'message' && !Object.hasOwn(metadata, key)) delete target[key]
                }
                for (const [key, value] of Object.entries(metadata)) {
                    if (key !== 'message') target[key] = safeStructuredClone(value)
                }
                conversationMutations.push(eventMutations)
                previousEvent = event
            }
            // The longest covered prefix wins. The whole recorded sequence is tested first and
            // shorter prefixes are rebuilt from the baseline only while it does not match.
            let coveredCount = 0
            let coveredConversation: Chat | undefined
            for (let count = conversationMutations.length; count > 0; count--) {
                const candidate = count === conversationMutations.length
                    ? conversation
                    : replayConversationPrefix(baselineConversation, conversationMutations.slice(0, count), sharesMessages)
                if (conversationMatchesAfterIdNormalization(candidate, capturedMatches[0])) {
                    coveredCount = count
                    coveredConversation = candidate
                    break
                }
            }
            if (coveredCount === 0 || !coveredConversation) continue
            if (!conversationJsonEquals(coveredConversation, capturedMatches[0])) exactConversationValues = false
            projected.chats[projectedIndex] = coveredConversation
            for (let index = 0; index < coveredCount; index++) {
                const pendingMutation = conversationPending[index]
                coveredSet.add(pendingMutation)
                mutationsByPending.set(pendingMutation, conversationMutations[index])
            }
        }
        const coveredPending = relevantPending.filter((pendingMutation) =>
            coveredSet.has(pendingMutation),
        )
        const mutations = coveredPending.flatMap(
            (pendingMutation) => mutationsByPending.get(pendingMutation) ?? [],
        )
        return {
            exactMutations:
                mutations.length > 0 && ((this.dependencies.captureCharacters && exactConversationValues) || canonicalJson(projected) === captured.characterCanonical)
                    ? mutations
                    : null,
            coveredPending,
        }
    }

    private acknowledgeConversationMutations(
        persisted: readonly PendingConversationMutation[],
        revision: DataRevision,
    ): void {
        const persistedSet = new Set(persisted)
        this.pendingConversationMutations = this.pendingConversationMutations.filter(
            (pending) => !persistedSet.has(pending),
        )
        for (const pending of persisted) {
            for (const sessionVersion of pending.versions) {
                try {
                    this.dependencies.onConversationMutationPersisted?.({
                        characterId: pending.event.characterId,
                        conversationId: pending.event.conversationId,
                        sessionToken: pending.event.sessionToken,
                        sessionVersion,
                        revision,
                    })
                } catch (error) {
                    this.reportBackgroundError(error)
                }
            }
        }
    }

    private acknowledgeFallbackConversationMutations(
        persisted: readonly PendingConversationMutation[],
        revision: DataRevision,
    ): void {
        const persistedSet = new Set(persisted)
        this.pendingConversationMutations = this.pendingConversationMutations.filter(
            (pending) => !persistedSet.has(pending),
        )
        for (const pending of persisted) {
            for (const sessionVersion of pending.versions) {
                try {
                    this.dependencies.onConversationMutationFallbackPersisted?.({
                        characterId: pending.event.characterId,
                        conversationId: pending.event.conversationId,
                        sessionToken: pending.event.sessionToken,
                        sessionVersion,
                        revision,
                    })
                } catch (error) {
                    this.reportBackgroundError(error)
                }
            }
        }
    }

    /**
     * Builds conversation-level mutations when only chat content changed for the tracked
     * selected character. Returns null whenever a full character replacement is required:
     * detail changes, added/removed/reordered chats, or chats the mutation channel cannot
     * address safely (missing or duplicate ids).
     */
    private diffSelectedConversations(captured: CapturedState): ConversationMutation[] | null {
        const character = captured.character
        if (!character || this.characterBaseline === null) return null
        if (this.characterBaselineId !== character.chaId) return null
        const baseline = JSON.parse(this.characterBaseline) as CompleteCharacter
        const capturedChats = character.chats
        const baselineChats = baseline.chats
        if (!Array.isArray(capturedChats) || !Array.isArray(baselineChats)) return null
        if (capturedChats.length !== baselineChats.length) return null
        const { chats: _capturedChats, chatPage: capturedChatPage, ...capturedDetail } = character
        const { chats: _baselineChats, chatPage: baselineChatPage, ...baselineDetail } = baseline
        if (JSON.stringify(capturedDetail) !== JSON.stringify(baselineDetail)) return null

        const mutations: ConversationMutation[] = []
        const seenIds = new Set<string>()
        for (let index = 0; index < capturedChats.length; index++) {
            const capturedChat = capturedChats[index]
            const baselineChat = baselineChats[index]
            const conversationId = capturedChat?.id
            if (!conversationId || conversationId !== baselineChat?.id) return null
            if (seenIds.has(conversationId)) return null
            seenIds.add(conversationId)
            if (JSON.stringify(capturedChat) === JSON.stringify(baselineChat)) continue
            if (captured.conversationStubIds.has(conversationId)) return null
            if (!Array.isArray(capturedChat.message) || !Array.isArray(baselineChat.message)) {
                return null
            }
            const { message, ...conversation } = capturedChat
            const range = messageReplaceRange(baselineChat.message, message)
            mutations.push({
                type: 'replace-range',
                characterId: character.chaId,
                conversationId,
                ...range,
                conversation,
            })
        }
        return mutations.length > 0 ? mutations : capturedChatPage !== baselineChatPage ? [] : null
    }

    /** Returns the last tracked character when the selection moved away before its edits were committed. */
    private captureDetachedCharacter(): { character: CompleteCharacter; canonical: string } | null {
        if (this.windowedCharacterBaseline) {
            throw new WindowedConversationRequiresCompatibilityError(
                'detached character capture requires complete ownership',
            )
        }
        if (this.characterBaseline === null || this.characterBaselineId === null) return null
        const retained = this.dependencies.captureCharacter(this.characterBaselineId)
        if (!retained) return null
        const canonical = canonicalJson(retained)
        if (canonical === this.characterBaseline) return null
        return { character: JSON.parse(canonical) as CompleteCharacter, canonical }
    }

    private capture(): CapturedState {
        this.dependencies.beforeCapture?.()
        const optimized = this.dependencies.canonicalCapture
        const capturedRoot = (optimized ? {} : this.dependencies.captureRoot()) as RootDatabase & {
            characters?: Database['characters']
            botPresets?: botPreset[]
            pluginCustomStorage?: Database['pluginCustomStorage']
            pluginStorageMeta?: Database['pluginStorageMeta']
        }
        const {
            characters: _characters,
            botPresets: legacyPresets,
            pluginCustomStorage: legacyPluginStorage,
            pluginStorageMeta: _pluginStorageMeta,
            ...rootValue
        } = capturedRoot
        const rootCanonical = optimized ? optimized.root() : canonicalJson(rootValue)
        const pluginStorageValue = this.dependencies.capturePluginStorage
            ? this.dependencies.capturePluginStorage()
            : (legacyPluginStorage ?? null)
        if (pluginStorageValue === null) this.pluginStorageCaptureCache.clear()
        const pluginStorageCapture = optimized
            ? optimized.pluginStorage()
            : pluginStorageValue === null
              ? null
              : this.pluginStorageCaptureCache.capture(pluginStorageValue)
        const presetsValue = this.dependencies.capturePresets
            ? this.dependencies.capturePresets()
            : (legacyPresets ?? [])
        const presetsCanonical = optimized
            ? optimized.presets()
            : presetsValue === null
              ? null
              : canonicalJson(presetsValue)
        let detachedRoot: RootDatabase | undefined
        let detachedPresets: botPreset[] | null | undefined
        const readRoot = () => (detachedRoot ??= JSON.parse(rootCanonical) as RootDatabase)
        const readPresets = () => {
            if (detachedPresets === undefined)
                detachedPresets =
                    presetsCanonical === null ? null : (JSON.parse(presetsCanonical) as botPreset[])
            return detachedPresets
        }
        const characterValue = this.dependencies.captureSelectedCharacter()
        const windowedAuthority = this.dependencies.captureSelectedConversationAuthority?.() ?? null
        if (windowedAuthority !== null) {
            if (
                !characterValue ||
                !validWindowedAuthority(windowedAuthority) ||
                characterValue.chaId !== windowedAuthority.characterId
            ) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'selected authority does not match the selected character',
                )
            }
            const ownedShell = optimized?.characterShell?.()
            const shell = ownedShell?.value ?? captureWindowedCharacterShell(characterValue)
            return {
                get root() {
                    return readRoot()
                },
                rootCanonical,
                get pluginStorage() {
                    return pluginStorageCapture?.value ?? null
                },
                get pluginStorageCanonical() {
                    return pluginStorageCapture?.json ?? null
                },
                pluginStorageCapture,
                get presets() {
                    return readPresets()
                },
                presetsCanonical,
                character: null,
                characterCanonical: null,
                conversationStubIds: new Set(),
                windowedCharacter: {
                    shell,
                    shellCanonical: ownedShell?.json ?? canonicalJson(shell),
                    authority: safeStructuredClone(windowedAuthority),
                },
            }
        }
        const conversationStubIds = new Set(
            characterValue?.chats
                .filter(isConversationSummaryStub)
                .map((conversation) => conversation.id)
                .filter((id): id is string => Boolean(id)) ?? [],
        )
        const characterSnapshot = optimized?.materializedCharacters?.().get(characterValue?.chaId ?? '')
        const characterCanonical = characterSnapshot ? null : optimized
            ? optimized.character()
            : characterValue
              ? canonicalJson(characterValue)
              : null
        let detachedCharacter: CompleteCharacter | null | undefined
        return {
            get root() {
                return readRoot()
            },
            rootCanonical,
            get pluginStorage() {
                return pluginStorageCapture?.value ?? null
            },
            get pluginStorageCanonical() {
                return pluginStorageCapture?.json ?? null
            },
            pluginStorageCapture,
            get presets() {
                return readPresets()
            },
            presetsCanonical,
            characterId: characterValue?.chaId ?? null,
            characterSnapshot,
            get character() {
                if (characterSnapshot) return characterSnapshot
                if (detachedCharacter === undefined)
                    detachedCharacter = characterCanonical
                        ? (JSON.parse(characterCanonical) as CompleteCharacter)
                        : null
                return detachedCharacter
            },
            characterCanonical,
            conversationStubIds,
            windowedCharacter: null,
        }
    }

    private captureDatabase(database: Database): CapturedState {
        const {
            characters,
            botPresets,
            pluginCustomStorage,
            pluginStorageMeta: _pluginStorageMeta,
            ...rootValue
        } = database
        if (isTauri) delete rootValue.account
        if (typeof rootValue.botPresetsId === 'number' && typeof botPresets?.[rootValue.botPresetsId]?.['id'] === 'string') (rootValue as RootDatabase).botPresetsId = botPresets[rootValue.botPresetsId]['id'] as string
        if (typeof rootValue.selectedPersona === 'number' && typeof rootValue.personas?.[rootValue.selectedPersona]?.id === 'string') (rootValue as RootDatabase).selectedPersona = rootValue.personas[rootValue.selectedPersona].id
        const rootCanonical = canonicalJson(rootValue)
        const presetsCanonical = canonicalJson(botPresets ?? [])
        const pluginStorageUnavailable =
            !Object.prototype.hasOwnProperty.call(database, 'pluginCustomStorage') &&
            this.dependencies.isIncompleteWorkingSet?.(database) === true
        const pluginStorageCapture = pluginStorageUnavailable
            ? null
            : this.pluginStorageCaptureCache.capture(pluginCustomStorage ?? {})
        let detachedPluginStorage: Database['pluginCustomStorage'] | null | undefined
        const selectedId = this.dependencies.captureSelectedCharacter()?.chaId
        const character = selectedId
            ? (characters.find((candidate) => candidate.chaId === selectedId) ?? null)
            : null
        const characterCanonical = character ? canonicalJson(character) : null
        return {
            root: JSON.parse(rootCanonical) as RootDatabase,
            rootCanonical,
            get pluginStorage() {
                if (detachedPluginStorage === undefined) {
                    detachedPluginStorage = pluginStorageCapture?.value ?? null
                }
                return detachedPluginStorage
            },
            get pluginStorageCanonical() {
                return pluginStorageCapture?.json ?? null
            },
            pluginStorageCapture,
            presets: JSON.parse(presetsCanonical) as botPreset[],
            presetsCanonical,
            character: characterCanonical
                ? (JSON.parse(characterCanonical) as CompleteCharacter)
                : null,
            characterCanonical,
            conversationStubIds: new Set(),
            windowedCharacter: null,
        }
    }

    private async reconstructCapturedCharacter(
        captured: CapturedState,
    ): Promise<CompleteCharacter> {
        if (captured.windowedCharacter) {
            throw new WindowedConversationRequiresCompatibilityError(
                'character reconstruction requires complete ownership',
            )
        }
        return this.reconstructCharacterWithStubBodies(
            captured.character!,
            captured.conversationStubIds,
        )
    }

    private async reconstructResidentCharacter(
        resident: NonNullable<ReturnType<SaveCoordinator['captureResidentCharacter']>>,
    ): Promise<CompleteCharacter> {
        if (resident.conversationStubIds.size === 0) return resident.character
        return this.reconstructCharacterWithStubBodies(
            resident.character,
            resident.conversationStubIds,
        )
    }

    private async reconstructCharacterWithStubBodies(
        character: CompleteCharacter,
        conversationStubIds: ReadonlySet<string>,
    ): Promise<CompleteCharacter> {
        const { chats: _chats, ...detail } = character
        const authoritative = await this.readCompleteCharacter(
            character.chaId,
            this.revision,
            detail,
        )
        const authoritativeById = new Map(
            authoritative.chats.map((conversation) => [conversation.id, conversation]),
        )
        return {
            ...character,
            chats: character.chats.map((conversation) => {
                if (!conversation.id || !conversationStubIds.has(conversation.id)) {
                    return conversation
                }
                const full = authoritativeById.get(conversation.id)
                if (!full) {
                    throw new Error(
                        `Conversation ${conversation.id} was not found during resident reconstruction`,
                    )
                }
                return {
                    ...full,
                    id: conversation.id,
                    name: conversation.name,
                    folderId: conversation.folderId,
                    bindedPersona: conversation.bindedPersona,
                    lastDate: conversation.lastDate,
                }
            }),
        } as CompleteCharacter
    }

    private diffPresets(before: botPreset[], after: botPreset[]): PersistentUnitMutation[] {
        const previous = new Map(before.map((value) => [value['id'] as string, value]))
        const next = new Map(after.map((value) => [value['id'] as string, value]))
        const mutations: PersistentUnitMutation[] = []
        for (const [id, value] of next) {
            if (!previous.has(id)) mutations.push({ key: JSON.stringify(['exists', 'preset', id]), type: 'set', value: true })
            mutations.push(...diffFields(['preset', id], previous.get(id) ?? {}, value, new Set(['id'])))
        }
        for (const id of previous.keys()) if (!next.has(id)) mutations.push({ key: JSON.stringify(['exists', 'preset', id]), type: 'delete' })
        if (canonicalJson([...previous.keys()]) !== canonicalJson([...next.keys()])) mutations.push({ key: JSON.stringify(['order', 'presets']), type: 'set', value: [...next.keys()] })
        return mutations
    }

    private async captureMaterializedChanges(commit: WorkingSetCommit, windowedId?: string): Promise<Map<string, CompleteCharacter>> {
        const captured = new Map<string, CompleteCharacter>()
        const canonicalCharacters = this.dependencies.canonicalCapture?.materializedCharacters ? undefined : this.dependencies.canonicalCapture?.characters?.()
        const immutableCharacters = this.dependencies.canonicalCapture?.materializedCharacters?.()
        for (const value of this.dependencies.captureCharacters?.() ?? []) {
            if (value.chaId === windowedId || value.chaId === this.pendingCharacterAddition?.characterId) continue
            const json = canonicalCharacters?.get(value.chaId)
            if (json !== undefined && json === this.materializedCanonicalBaselines.get(value.chaId)) continue
            const current = immutableCharacters?.get(value.chaId) ?? captureMaterializedCharacter(value)
            let previous = this.materializedBaselines.get(value.chaId)
            if (immutableCharacters && current === previous) continue
            if (!previous) {
                const detail = await this.dependencies.store.readCharacter(value.chaId)
                if (!detail) continue
                previous = { ...detail.value, chats: [] } as CompleteCharacter
                for (const chat of current.chats) {
                    const stored = Object.hasOwn(chat, 'message')
                        ? await this.dependencies.store.readConversation(value.chaId, chat.id)
                        : await this.dependencies.store.readConversationMetadata(value.chaId, chat.id)
                    if (stored) previous.chats.push('conversation' in stored.value ? stored.value.conversation as Chat : stored.value as Chat)
                }
            }
            const changes = diffMaterializedCharacter(previous, current)
            if (changes.unitMutations.length) commit.unitMutations = [...(commit.unitMutations ?? []), ...changes.unitMutations]
            if (changes.conversations.length) commit.conversations = [...(commit.conversations ?? []), ...changes.conversations]
            captured.set(value.chaId, current)
        }
        return captured
    }

    private materializedCaptureMatchesBaseline(): boolean {
        const immutable = this.dependencies.canonicalCapture?.materializedCharacters?.()
        if (immutable) return [...immutable].every(([id, value]) => this.dependencies.captureSelectedConversationAuthority?.()?.characterId === id || value === this.materializedBaselines.get(id))
        const canonicalCharacters = this.dependencies.canonicalCapture?.materializedCharacters ? undefined : this.dependencies.canonicalCapture?.characters?.()
        if (canonicalCharacters) return [...canonicalCharacters].every(([id, json]) => this.dependencies.captureSelectedConversationAuthority?.()?.characterId === id || json === this.materializedCanonicalBaselines.get(id))
        return (this.dependencies.captureCharacters?.() ?? []).every((value) =>
            this.dependencies.captureSelectedConversationAuthority?.()?.characterId === value.chaId ||
            canonicalJson(captureMaterializedCharacter(value)) === canonicalJson(this.materializedBaselines.get(value.chaId) ?? null))
    }

    capturePersistentBaselineRoot(): RootDatabase {
        const fields = this.rootBaseline === null ? undefined : this.dependencies.canonicalCapture?.rootFields?.(this.rootBaseline)
        if (fields) return clonePersistentRootFields(Object.fromEntries(fields) as RootDatabase)
        return JSON.parse(this.rootBaseline ?? canonicalJson(this.dependencies.captureRoot()))
    }

    private async finishRoutineCharacterIntent(revision: DataRevision, commit: WorkingSetCommit, characterId: string,
        before: { character: CompleteCharacter; canonical: string } | null): Promise<void> {
        this.currentRevision = revision
        const keys = (commit.unitMutations ?? []).map((value) => value.key)
        for (const value of commit.rootMutations ?? []) keys.push(JSON.stringify(['root', value.key]))
        for (const value of commit.conversations ?? []) {
            keys.push(JSON.stringify([value.type === 'delete' ? 'exists' : 'messages', ...(value.type === 'delete' ? ['conversation'] : []), value.characterId, 'conversationId' in value ? value.conversationId : '']))
        }
        if (this.dependencies.onRoutineUnitsCommitted) {
            try { await this.dependencies.onRoutineUnitsCommitted(revision, keys) }
            catch (error) { this.markCommittedWorkingSetRefreshRequired(revision, error); throw error }
        } else {
            const live = this.dependencies.captureCharacter(characterId)
            const baseline = this.materializedBaselines.get(characterId) ?? before?.character
            const patch = (target: object, mutation: PersistentUnitMutation, field: string) => {
                const value = target as Record<string, unknown>
                if (mutation.type === 'delete') delete value[field]
                else value[field] = canonicalClone(mutation.value)
            }
            for (const mutation of commit.unitMutations ?? []) {
                const [kind, id, field] = JSON.parse(mutation.key)
                if (kind !== 'character' || id !== characterId || !live || !baseline) continue
                if (canonicalJson({value:(live as unknown as Record<string,unknown>)[field]}) === canonicalJson({value:(before?.character as unknown as Record<string,unknown>)?.[field]})) patch(live, mutation, field)
                patch(baseline, mutation, field)
            }
            if (baseline) {
                this.materializedBaselines.set(characterId, baseline)
                if (this.characterBaselineId === characterId) this.characterBaseline = canonicalJson(baseline)
            }
            const root = this.capturePersistentBaselineRoot()
            for (const mutation of commit.rootMutations ?? []) {
                const value = root as unknown as Record<string, unknown>
                if (mutation.type === 'delete') delete value[mutation.key]
                else value[mutation.key] = canonicalClone(mutation.value)
            }
            this.rootBaseline = canonicalJson(root)
        }
        this.dependencies.onLocalRevision?.(revision)
        await this.finishExplicitCommit(revision)
    }

    /** Merges another writer's change to the records a flush replaces into the working set, so the next pass captures against it. */
    private async mergeConcurrentRecordChange(error: unknown): Promise<boolean> {
        const project = this.dependencies.onRoutineUnitsCommitted
        if (!(error instanceof ConcurrentRecordChangeError) || !project || this.committedRefreshRevision !== null) return false
        try {
            await project(error.actualRevision, error.keys)
        } catch (projectionError) {
            this.markCommittedWorkingSetRefreshRequired(error.actualRevision, projectionError)
            throw projectionError
        }
        for (const id of error.characterIds) this.materializedCanonicalBaselines.delete(id)
        return true
    }

    /** Commits a flush, writing created conversations in pages when the whole save is too large. */
    private async commitFlush(commit: WorkingSetCommit): Promise<{ revision: DataRevision }> {
        const plan = planConversationInsertPages(commit, this.dependencies.conversationInsertPageBytes)
        return plan ? this.commitInsertPages(commit.expectedRevision, plan) : this.commitRoutine(commit)
    }

    private async commitInsertPages(expectedRevision: DataRevision, plan: ConversationInsertPlan): Promise<{ revision: DataRevision }> {
        let revision: DataRevision | null = null
        const created: { characterId: string; conversationId: string }[] = []
        try {
            for (const step of plan.steps) {
                revision = (await this.commitRoutine({ ...step, expectedRevision: revision ?? expectedRevision })).revision
                for (const mutation of step.conversations) {
                    if (createsConversation(mutation)) created.push({ characterId: mutation.characterId, conversationId: mutation.conversationId })
                }
            }
            return { revision: revision! }
        } catch (error) {
            if (revision === null) throw error
            // Earlier pages are already stored; remove the conversations this save
            // created so no partial chat stays, then reload what storage holds.
            if (created.length > 0 || plan.addedCharacterId) {
                try {
                    revision = (await this.commitRoutine({
                        expectedRevision: revision,
                        ...(plan.addedCharacterId ? { deleteCharacterIds: [plan.addedCharacterId] } : {}),
                        conversations: created.filter(({ characterId }) => characterId !== plan.addedCharacterId)
                            .map(({ characterId, conversationId }) => ({ type: 'delete', characterId, conversationId })),
                    })).revision
                } catch (cleanupError) {
                    this.reportBackgroundError(cleanupError)
                }
            }
            this.markCommittedWorkingSetRefreshRequired(revision, error)
            throw error
        }
    }

    private async commitRoutine(input: WorkingSetCommit): Promise<{ revision: DataRevision }> {
        const captured = canonicalClone(input)
        const replacedChange = replacedRecordChange(captured)
        while (true) {
            try { return await this.dependencies.store.commit(captured) } catch (error) {
                if (!(error instanceof RevisionConflictError)) throw error
                if (error.actualRevision <= captured.expectedRevision) throw error
                // A replaced record is resent only when no other writer changed it since the capture.
                const rebase = replacedChange
                    ? await this.replacedRecordRebase(captured.expectedRevision, error.actualRevision, replacedChange)
                    : { revision: error.actualRevision }
                if (rebase === null) throw error
                if (rebase.merge) {
                    throw new ConcurrentRecordChangeError(captured.expectedRevision, rebase.revision, rebase.merge.keys, rebase.merge.characterIds)
                }
                captured.expectedRevision = rebase.revision
                this.currentRevision = rebase.revision
            }
        }
    }

    /**
     * The newest revision at which no change after `base` matches, or, when one does, that
     * revision with the matching changes as unit keys the working set can merge. Null when the
     * store cannot tell or a change cannot be merged.
     */
    private async replacedRecordRebase(base: DataRevision, actual: DataRevision, matches: (key: ContentChangeKey) => boolean):
        Promise<{ revision: DataRevision; merge?: { keys: string[]; characterIds: string[] } } | null> {
        let revision = actual
        while (true) {
            let lease
            try {
                lease = await this.dependencies.store.acquireRevision?.(revision)
            } catch (error) {
                if (error instanceof RevisionConflictError && error.actualRevision > revision) {
                    revision = error.actualRevision
                    continue
                }
                return null
            }
            if (!lease) return { revision }
            return withPersistentRevisionLease(lease, async (reader) => {
                // A store without a change index cannot show what moved, so its commit is resent as before.
                if (!reader.readWorkingSetChangePage) return { revision }
                const changed: ContentChangeKey[] = []
                let after: ContentChangeKey | null = null
                while (true) {
                    let page: ContentChangeKey[]
                    try { page = await reader.readWorkingSetChangePage(base, after, CONTENT_CHANGE_PAGE_LIMIT) }
                    catch { return null }
                    changed.push(...page.filter(matches))
                    if (page.length < CONTENT_CHANGE_PAGE_LIMIT) break
                    after = page[page.length - 1]
                }
                if (changed.length === 0) return { revision }
                const merge = await this.concurrentRecordChanges(reader, changed)
                if (!merge) return null
                // Records that still equal their baselines are resent as captured.
                return merge.keys.length > 0 ? { revision, merge } : { revision }
            })
        }
    }

    /**
     * The units another writer changed in resident characters, found by diffing their baselines
     * against the stored records, or null when one of the changes cannot be merged into the working set.
     */
    private async concurrentRecordChanges(reader: PersistentRevisionReader, changed: readonly ContentChangeKey[]):
        Promise<{ keys: string[]; characterIds: string[] } | null> {
        if (!this.dependencies.onRoutineUnitsCommitted) return null
        // A windowed selection keeps its own baseline, which a unit projection does not rebase.
        const windowedIds = new Set([this.windowedCharacterBaseline?.authority.characterId,
            this.dependencies.captureSelectedConversationAuthority?.()?.characterId])
        const conversationIds = new Map<string, Set<string>>()
        for (const key of changed) {
            if ((key.kind !== 'character' && key.kind !== 'conversation') ||
                windowedIds.has(key.key1) || !this.materializedBaselines.has(key.key1)) return null
            const ids = conversationIds.get(key.key1) ?? new Set<string>()
            if (key.kind === 'conversation') ids.add(key.key2)
            conversationIds.set(key.key1, ids)
        }
        const keys: string[] = []
        for (const [characterId, ids] of conversationIds) {
            const before = this.materializedBaselines.get(characterId)!
            const detail = await reader.readCharacter(characterId)
            if (!detail) return null
            const chats: Chat[] = []
            for (const chat of before.chats) {
                if (!ids.has(chat.id)) {
                    chats.push(chat)
                    continue
                }
                // A message range can only be rebased against the messages it was taken from.
                if (!Object.hasOwn(chat, 'message')) return null
                const stored = await reader.readConversation(characterId, chat.id)
                if (!stored) return null
                chats.push(stored.value)
                ids.delete(chat.id)
            }
            if (ids.size > 0) return null
            const changes = diffMaterializedCharacter(before, { ...detail.value, chats } as CompleteCharacter)
            keys.push(...changes.unitMutations.map((value) => value.key),
                ...changes.conversations.flatMap((value) => value.type === 'reorder' ? [] : [JSON.stringify(['messages', value.characterId, value.conversationId])]))
        }
        return { keys, characterIds: [...conversationIds.keys()] }
    }

    beginActivatedLibraryGuard(token: PersistentMutationToken): symbol {
        if (this.activePausedWriteToken !== token || this.activatedLibraryGuardOwner ||
            this.destructiveReplacementFence || token.revision !== this.revision) {
            throw new PersistentMutationFencedError()
        }
        const baseline = this.capture()
        const owner = Symbol('activated-library')
        this.activatedLibraryGuardOwner = owner
        this.destructiveReplacementFence = {
            owner, state: 'held', blockedPrePublicationDirty: false,
            refreshBaseline: baseline,
        }
        this.cancelDebounce()
        return owner
    }

    assertActivatedLibraryGuard(owner: symbol, token: PersistentMutationToken, validateCapture = true): void {
        if (this.activatedLibraryGuardOwner !== owner || this.activePausedWriteToken !== token ||
            this.destructiveReplacementFence?.owner !== owner || this.destructiveReplacementFence.state !== 'held') {
            throw new PersistentMutationFencedError()
        }
        if (validateCapture) this.assertDestructiveReplacementFence(owner)
    }

    finishActivatedLibraryGuard(owner: symbol): void {
        if (this.activatedLibraryGuardOwner !== owner) throw new PersistentMutationFencedError()
        this.activatedLibraryGuardOwner = null
        this.committedRefreshRevision = null
        try { this.dependencies.onWorkingSetRefreshRequired?.(null) }
        catch (error) { this.reportBackgroundError(error) }
        if (this.destructiveReplacementFence?.owner === owner) this.releaseDestructiveReplacementFence(owner)
    }

    withPausedPersistentWrites<T>(reason: string, operation: (token: PersistentMutationToken) => Promise<T>): Promise<T> {
        this.assertPersistentMutationAllowed()
        return this.enqueue(async () => {
            await this.flushIterations(reason, false)
            const token = { revision: this.revision, mutationGeneration: this.dirtyGeneration }
            this.activePausedWriteToken = token
            try {
                return await operation(token)
            } finally {
                // Fence the queue before releasing the pause, even when activation's
                // outcome could not be read. Recovery must never flush the old view.
                const owner = this.activatedLibraryGuardOwner
                if (owner && this.destructiveReplacementFence?.owner === owner) {
                    this.markCommittedWorkingSetRefreshRequired(this.revision, new PersistentMutationFencedError())
                    this.releaseDestructiveReplacementFence(owner)
                }
                this.activePausedWriteToken = null
            }
        })
    }

    captureMaterializedBaseline(): CompleteCharacter[] {
        const baselines = new Map(this.materializedBaselines)
        if (this.windowedCharacterBaseline) {
            const { shell, authority } = this.windowedCharacterBaseline
            baselines.set(authority.characterId, shell as CompleteCharacter)
        }
        return [...baselines.values()].map((value) => ({...value, chats: value.chats.map((chat) => ({...chat}))} as CompleteCharacter))
    }

    /** Drops the baselines of a character the working set no longer holds; hydration captures new ones. */
    forgetMaterializedCharacter(characterId: string): void {
        if (this.dependencies.captureCharacter(characterId)) return
        this.materializedBaselines.delete(characterId)
        this.materializedCanonicalBaselines.delete(characterId)
    }

    capturePresetRecordBaseline(): botPreset[] {
        return (this.dependencies.capturePresetRecords?.() ?? []).flatMap((value) => {
            const baseline = this.presetRecordBaselines.get(value['id'] as string)
            return baseline ? [canonicalClone(baseline)] : []
        })
    }

    adoptAppliedUnitState(revision: DataRevision, root: RootDatabase | null, presets: botPreset[] | null, characters: readonly CompleteCharacter[], presetRecords: readonly botPreset[] = presets ?? [],
        windowedConversation?: { characterId: string; conversationId: string; totalMessages: number }, preserveSelectedRows = false): void {
        this.currentRevision = revision
        if (root) this.rootBaseline = canonicalJson(root)
        if (presets) this.presetsBaseline = canonicalJson(presets)
        for (const preset of presetRecords) {
            if (preset['id']) this.presetRecordBaselines.set(preset['id'] as string, canonicalClone(preset))
        }
        for (const character of characters) this.materializedBaselines.set(character.chaId, captureMaterializedCharacter(character))
        const selected = this.dependencies.captureSelectedCharacter()
        const persisted = characters.find((value) => value.chaId === selected?.chaId)
        if (persisted && !this.windowedCharacterBaseline) {
            this.characterBaseline = canonicalJson(persisted)
            this.characterBaselineId = persisted.chaId
        }
        if (this.windowedCharacterBaseline) {
            const authority = this.windowedCharacterBaseline.authority
            if (persisted?.chaId === authority.characterId) {
                const shell = captureWindowedCharacterShell(persisted)
                this.windowedCharacterBaseline.shell = shell
                this.windowedCharacterBaseline.shellCanonical = canonicalJson(shell)
            }
            authority.storeRevision = revision
            const totalMessages = windowedConversation?.characterId === authority.characterId &&
                windowedConversation.conversationId === authority.conversationId ? windowedConversation.totalMessages : undefined
            if (totalMessages !== undefined) authority.totalMessages = totalMessages
            this.dependencies.onWindowedSelectedConversationRevision?.(revision, totalMessages, preserveSelectedRows)
        }
    }

    commitPersistentUnitIntent(reason: string, unitMutations: readonly PersistentUnitMutation[], conversations: readonly ConversationMutation[] = [], wholeMessages: readonly WholeMessageIntent[] = [], onCommitted?: (revision: DataRevision) => Promise<void>, check?: PluginStorageWriteCheck): Promise<DataRevision> {
        this.assertPersistentMutationAllowed()
        const mutations = canonicalClone([...unitMutations])
        const ranges = canonicalClone([...conversations])
        const messages = canonicalClone([...wholeMessages])
        return this.enqueue(async () => {
            await this.flushIterations(reason, false)
            if (!mutations.length && !ranges.length && !messages.length) return this.revision
            return (await this.commitUnitIntentAttempts(async () => {
                const replacements: ConversationMutation[] = []
                for (const target of messages) {
                    const metadata = await this.dependencies.store.readConversationMetadata(target.characterId, target.conversationId)
                    if (!metadata && !mutations.some((value) => value.key === JSON.stringify(['exists', 'conversation', target.characterId, target.conversationId]) && value.type === 'set')) throw new TypeError('Missing conversation parent')
                    replacements.push({ type: 'replace-range', characterId: target.characterId, conversationId: target.conversationId,
                        start: 0, deleteCount: metadata?.value.totalMessages ?? 0, messages: target.messages })
                }
                return { unitMutations: mutations, conversations: [...ranges, ...replacements] }
            }, async revision => { check?.committed(); await onCommitted?.(revision) }, check))!
        })
    }

    /**
     * Builds the commit at each attempt's revision after pending data is flushed, so
     * checks made while preparing hold for the commit that lands. `null` commits nothing.
     */
    commitPreparedUnitIntent(reason: string, prepare: (revision: DataRevision) => Promise<PreparedUnitIntent | null>, onCommitted?: (revision: DataRevision) => Promise<void>): Promise<DataRevision | null> {
        this.assertPersistentMutationAllowed()
        return this.enqueue(async () => {
            await this.flushIterations(reason, false)
            return this.commitUnitIntentAttempts(async (revision) => {
                const prepared = await prepare(revision)
                return prepared && canonicalClone({ unitMutations: [...prepared.unitMutations], conversations: [...prepared.conversations] })
            }, onCommitted)
        })
    }

    private async commitUnitIntentAttempts(prepare: (revision: DataRevision) => Promise<PreparedUnitIntent | null>, onCommitted?: (revision: DataRevision) => Promise<void>, check?: PluginStorageWriteCheck): Promise<DataRevision | null> {
        while (true) {
            const revision = this.revision
            const prepared = await prepare(revision)
            if (!prepared) return null
            if (!prepared.unitMutations.length && !prepared.conversations.length) return revision
            let committedRevision: DataRevision | undefined
            try {
                check?.validate()
                const result = await this.dependencies.store.commit({ expectedRevision: revision, unitMutations: [...prepared.unitMutations], conversations: [...prepared.conversations] })
                committedRevision = result.revision
                this.currentRevision = result.revision
                this.dependencies.onLocalRevision?.(result.revision)
                await onCommitted?.(result.revision)
                await this.finishExplicitCommit(result.revision, false)
                return result.revision
            } catch (error) {
                if (committedRevision !== undefined || !(error instanceof RevisionConflictError) || error.actualRevision <= revision) throw error
                this.currentRevision = error.actualRevision
            }
        }
    }

    private capturePendingAddition(): {
        pending: PendingCharacterAddition
        character: CompleteCharacter
        canonical: string | CompleteCharacter
    } | null {
        const pending = this.pendingCharacterAddition
        if (!pending) return null
        const value = this.dependencies.captureCharacter(pending.characterId)
        if (!value || value.chaId !== pending.characterId) {
            throw new Error(`Installed character ${pending.characterId} is not available`)
        }
        const immutable = this.dependencies.canonicalCapture?.materializedCharacters?.().get(value.chaId)
        if (immutable) return { pending, character: immutable, canonical: immutable }
        const canonical = canonicalJson(value)
        return { pending, character: JSON.parse(canonical) as CompleteCharacter, canonical }
    }

    private async commitPendingCharacterAddition(publishOfficial: boolean): Promise<void> {
        const pending = this.pendingCharacterAddition!
        let addition: NonNullable<ReturnType<SaveCoordinator['capturePendingAddition']>>
        let committed: { revision: DataRevision }
        try {
            addition = this.capturePendingAddition()!
            committed = await this.commitFlush({ expectedRevision: this.revision, addCharacter: addition.character })
        } catch (error) {
            const refusal = error as { code?: string; name?: string }
            const permanent = refusal?.code === 'validation' || refusal?.code === 'unsaveable-value' || refusal?.code === 'payload-too-large'
                || refusal?.name === 'UnsaveableValueError' || refusal?.name === 'PayloadTooLargeError'
            if (permanent && this.committedRefreshRevision === null) {
                let stored: Awaited<ReturnType<PersistentDataStore['readCharacter']>>
                try { stored = await this.dependencies.store.readCharacter(pending.characterId) }
                catch (readError) { this.reportBackgroundError(readError); throw error }
                if (stored) {
                    this.markCommittedWorkingSetRefreshRequired(stored.revision, error)
                    throw error
                }
                const root = canonicalClone(this.dependencies.captureRoot())
                removeCharacterIdFromOrder(root, pending.characterId)
                this.dependencies.publishCharacterMutation?.({ revision: this.revision, root,
                    characterId: pending.characterId, kind: 'delete', character: null })
                this.pendingCharacterAddition = null
                this.materializedBaselines.delete(pending.characterId)
                this.materializedCanonicalBaselines.delete(pending.characterId)
                if (this.characterBaselineId === pending.characterId) this.setCharacterBaseline(this.capture())
            }
            throw error
        }
        pending.locallyAdded = true
        pending.baseline = addition.canonical
        this.currentRevision = committed.revision
        this.materializedBaselines.set(pending.characterId, addition.character)
        if (this.dependencies.captureSelectedCharacter()?.chaId === pending.characterId) {
            this.characterBaseline = typeof addition.canonical === 'string' ? addition.canonical : canonicalJson(addition.character)
            this.characterBaselineId = pending.characterId
        }
        if (this.windowedCharacterBaseline) {
            this.setWindowedCharacterBaseline(this.windowedCharacterBaseline, committed.revision,
                this.windowedCharacterBaseline.authority.persistedSessionVersion)
            if (this.pendingWindowedActivationChange) this.pendingWindowedActivationChange.authority.storeRevision = committed.revision
        } else this.dependencies.onStorageOnlyRevision?.(committed.revision)
        this.dependencies.onLocalRevision?.(committed.revision)
        if (this.dependencies.officialPublisher) {
            if (publishOfficial) await this.stagePublication(committed.revision)
            else this.deferPublication(committed.revision)
        }
    }

    private beginReservedAddition(reserved: ReservedCharacterAddition): void {
        const request = reserved.request
        if (!request) return
        try {
            request.install()
        } finally {
            reserved.request = null
            if (this.reservedCharacterAddition === reserved) this.reservedCharacterAddition = null
        }
        this.pendingCharacterAddition = {
            characterId: request.characterId,
            token: reserved.token,
            locallyAdded: false,
            baseline: null,
        }
        this.dirtyGeneration++
        const bytes =
            Number.isFinite(request.estimatedBytes) && request.estimatedBytes > 0
                ? request.estimatedBytes
                : 0
        this.pendingByteCount += bytes
    }

    private armDebounce(delay = SAVE_DEBOUNCE_MS): void {
        if (this.debounceHandle !== undefined || this.committedRefreshRevision !== null) return
        this.debounceHandle = this.clock.setTimeout(() => {
            this.debounceHandle = undefined
            this.startBackgroundFlush('debounce')
        }, delay)
    }

    private startBackgroundFlush(reason: string): void {
        try {
            void this.flushPendingData(reason).catch((error) => this.handleBackgroundSaveFailure(error))
        } catch (error) {
            this.handleBackgroundSaveFailure(error)
        }
    }

    private handleBackgroundSaveFailure(error: unknown): void {
        if (error instanceof PersistentMutationFencedError) {
            if (this.destructiveReplacementFence || this.committedRefreshRevision !== null) {
                this.flushAfterFenceRelease = true
            } else {
                this.armDebounce(this.backgroundRetryDelay)
            }
            return
        }
        this.reportBackgroundError(error)
        if (this.dirtyGeneration !== this.persistedDirtyGeneration &&
            !(error instanceof WindowedConversationSaveError) &&
            !(error instanceof TypeError) && !(error instanceof RevisionConflictError) &&
            !(error instanceof Error && (error.name === 'QuotaExceededError' || error.name === 'UnsaveableValueError' || error.name === 'PayloadTooLargeError' || error.message === 'retired-record-id'))) {
            this.armDebounce(this.backgroundRetryDelay)
            this.backgroundRetryDelay = Math.min(60_000, this.backgroundRetryDelay * 2)
        }
        if (this.hasPendingOfficialPublication) {
            this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
        }
    }

    private setLocalSaveFailure(error: unknown | null): void {
        if (this.localSaveFailure === error) return
        this.localSaveFailure = error
        try {
            this.dependencies.onLocalSaveFailure?.(error)
        } catch {
            // Notification failures cannot change the outcome of a durable write.
        }
    }

    private reportBackgroundError(error: unknown): void {
        // Native errors are plain objects, which would all repeat as one string.
        const native = typeof error === 'object' && error !== null ? error as { code?: unknown; message?: unknown } : null
        const message = error instanceof Error ? error.message : native ? `${String(native.code)}:${String(native.message)}` : String(error)
        if (message === this.lastBackgroundErrorMessage) return
        this.lastBackgroundErrorMessage = message
        try {
            this.dependencies.onBackgroundError?.(error)
        } catch {
            // Diagnostic observers cannot change the outcome of a durable write.
        }
    }

    private reportActivePromise(): void {
        const active = this.additionPromise ?? this.flushPromise ?? this.localFlushPromise
        if (active === this.lastReportedFlushPromise) return
        this.lastReportedFlushPromise = active
        this.dependencies.onFlushPromise?.(active)
        this.reportPersistenceIdleIfNeeded()
    }

    private reportPersistenceIdleIfNeeded(): void {
        if (!this.persistenceWasBusy || this.hasPendingPersistenceWork) return
        this.persistenceWasBusy = false
        this.dependencies.onPersistenceIdle?.()
    }

    private async publishPendingRevision(): Promise<void> {
        if (this.destructiveReplacementFence) {
            this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
            return
        }
        this.publicationInProgress = true
        this.notifyOperationStateChange()
        const revision = this.pendingPublicationRevision
        if (revision === null || !this.dependencies.officialPublisher) {
            this.publicationInProgress = false
            this.notifyOperationStateChange()
            return
        }
        let publication = this.pendingPublication
        let failure: unknown = null
        try {
            if (!publication) {
                publication = await this.dependencies.officialPublisher.pin(revision)
                this.pendingPublication = publication
            }
            await publication.publish()
        } catch (error) {
            if (publication) this.lastOfficialPublishAttemptAt = this.currentTime()
            this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
            failure = error
        } finally {
            this.publicationInProgress = false
            this.notifyOperationStateChange()
        }
        if (this.localFlushDuringPublicationPromise) {
            await this.localFlushDuringPublicationPromise.catch(() => undefined)
        }
        if (failure !== null) {
            await this.applyDeferredPublication()
            if (this.pendingPublicationRevision !== null) {
                this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
            }
            throw failure
        }
        this.lastOfficialPublishAttemptAt = this.currentTime()
        this.cancelOfficialPublishRetry()
        this.pendingPublication = null
        this.pendingPublicationRevision = null
        await this.disposeOrQueuePublication(publication)
        await this.applyDeferredPublication()
        if (this.pendingPublicationRevision !== null) {
            this.armOfficialPublishRetry(this.officialPublishDelayMs())
        }
        this.armPublicationCleanupRetryIfNeeded()
    }

    private officialPublishDelayMs(): number {
        if (this.lastOfficialPublishAttemptAt === null) return 0
        const elapsed = this.currentTime() - this.lastOfficialPublishAttemptAt
        return Math.max(0, OFFICIAL_PUBLISH_MIN_INTERVAL_MS - elapsed)
    }

    private armOfficialPublishRetry(delay: number): void {
        if (this.officialPublishRetryHandle !== undefined) return
        this.officialPublishRetryHandle = this.clock.setTimeout(() => {
            this.officialPublishRetryHandle = undefined
            if (this.destructiveReplacementFence || this.committedRefreshRevision !== null) {
                this.publishAfterFenceRelease = true
                return
            }
            this.startBackgroundFlush('official-publish-interval')
        }, delay)
    }

    private cancelOfficialPublishRetry(): void {
        if (this.officialPublishRetryHandle === undefined) return
        this.clock.clearTimeout(this.officialPublishRetryHandle)
        this.officialPublishRetryHandle = undefined
    }

    private currentTime(): number {
        return this.dependencies.now?.() ?? Date.now()
    }

    private async disposeOrQueuePublication(publication: PinnedPublication): Promise<void> {
        if (this.destructiveReplacementFence) {
            this.pendingPublicationCleanup.add(publication)
            this.armPublicationCleanupRetryIfNeeded()
            return
        }
        try {
            await publication.dispose()
            this.pendingPublicationCleanup.delete(publication)
        } catch (error) {
            this.pendingPublicationCleanup.add(publication)
            this.reportBackgroundError(error)
            this.armPublicationCleanupRetryIfNeeded()
        }
    }

    private async retryPublicationCleanup(): Promise<void> {
        if (this.destructiveReplacementFence) return
        for (const publication of [...this.pendingPublicationCleanup]) {
            try {
                await publication.dispose()
                this.pendingPublicationCleanup.delete(publication)
            } catch (error) {
                this.reportBackgroundError(error)
            }
        }
        this.armPublicationCleanupRetryIfNeeded()
    }

    private armPublicationCleanupRetryIfNeeded(): void {
        if (this.pendingPublicationCleanup.size > 0) {
            this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
        }
    }

    private cancelDebounce(): void {
        if (this.debounceHandle === undefined) return
        this.clock.clearTimeout(this.debounceHandle)
        this.debounceHandle = undefined
    }

    private rearmDebounceAfterReplacementFailure(
        capturedGeneration: number,
        hadPendingDebounce: boolean,
    ): void {
        if (
            (hadPendingDebounce ||
                this.dirtyGeneration !== capturedGeneration ||
                this.pendingByteCount > 0) &&
            !this.flushPromise &&
            !this.additionPromise
        ) {
            this.armDebounce()
        }
    }

    private replacementExpectationError(options: PersistentReplacementOptions): Error | null {
        if (options.expectedRevision !== undefined && options.expectedRevision !== this.revision) {
            return new RevisionConflictError(options.expectedRevision, this.revision)
        }
        if (
            options.expectedMutationGeneration !== undefined &&
            options.expectedMutationGeneration !== this.dirtyGeneration
        ) {
            return new Error(
                `Expected mutation generation ${options.expectedMutationGeneration}, ` +
                    `but current generation is ${this.dirtyGeneration}`,
            )
        }
        return null
    }

    private assertReadRevision(expected: DataRevision, actual: DataRevision): void {
        if (actual !== expected) throw new RevisionConflictError(expected, actual)
    }

    private assertInitialized(): void {
        if (this.currentRevision === null) throw new Error('Save coordinator is not initialized')
    }

    assertPersistentMutationAllowed(expectedAuthorityEpoch?: number): void {
        this.assertInitialized()
        this.assertSelectedConversationTransitionInactive()
        if (
            this.destructiveReplacementFence ||
            this.committedRefreshRevision !== null ||
            (expectedAuthorityEpoch !== undefined && expectedAuthorityEpoch !== this.authorityEpoch)
        ) {
            throw new PersistentMutationFencedError()
        }
    }

    private assertQueuedMutationAllowed(expectedAuthorityEpoch: number): void {
        this.assertSelectedConversationTransitionInactive()
        if (
            this.committedRefreshRevision !== null ||
            this.authorityEpoch !== expectedAuthorityEpoch ||
            this.destructiveReplacementFence?.state === 'held'
        ) {
            throw new PersistentMutationFencedError()
        }
    }

    private assertSelectedConversationTransitionInactive(): void {
        if (this.selectedConversationTransitionActive) {
            throw new SelectedConversationTransitionInProgressError()
        }
    }

    private captureMatchesBaseline(): boolean {
        const captured = this.capture()
        return (
            captured.rootCanonical === this.rootBaseline &&
            this.pluginStorageMatchesBaseline(captured) &&
            (captured.presetsCanonical === null ||
                captured.presetsCanonical === this.presetsBaseline) &&
            this.selectedCaptureMatchesBaseline(captured)
        )
    }

    private pluginStorageMatchesBaseline(captured: CapturedState): boolean {
        const snapshot = captured.pluginStorageCapture
        if (snapshot === null) return true
        if (snapshot !== undefined) {
            return this.pluginStorageBaselineEntries?.matches(snapshot) ?? false
        }
        return (
            captured.pluginStorageCanonical === null ||
            captured.pluginStorageCanonical === this.pluginStorageBaseline
        )
    }

    private captureMatchesCapturedState(expected: CapturedState): boolean {
        const captured = this.capture()
        if (
            captured.rootCanonical !== expected.rootCanonical ||
            captured.pluginStorageCanonical !== expected.pluginStorageCanonical ||
            captured.presetsCanonical !== expected.presetsCanonical
        )
            return false
        if (expected.windowedCharacter || captured.windowedCharacter) {
            return (
                expected.windowedCharacter !== null &&
                captured.windowedCharacter !== null &&
                captured.windowedCharacter.shellCanonical ===
                    expected.windowedCharacter.shellCanonical &&
                sameWindowedAuthority(
                    captured.windowedCharacter.authority,
                    expected.windowedCharacter.authority,
                )
            )
        }
        return captured.characterSnapshot || expected.characterSnapshot
            ? captured.characterSnapshot === expected.characterSnapshot
            : captured.characterCanonical === expected.characterCanonical
    }
}
