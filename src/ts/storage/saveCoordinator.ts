import { Mutex } from '../mutex'
import { diffRootMutations } from './rootMutation'
import type { PersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'
import type { Chat, Database, Message, botPreset, character, groupChat } from './database.svelte'
import type {
    AssetAlias,
    AssetOwnerHead,
    CharacterDetail,
    ConversationMutation,
    DataRevision,
    PersistentDataStore,
    PluginStorageMutation,
    PluginStorageValue,
    PersistentRoot,
    WorkingSetCommit,
} from './persistentDataStore'
import type { RisuModule } from '../process/modules'
import type { CommittedApplyOutcome } from './persistentDataRuntime'
import { RevisionConflictError } from './persistentDataStore'
import { appendCharacterIdToOrder, removeCharacterIdFromOrder } from './characterOrderMutation'
import {
    createConversationSummaryStubFromChat,
    isConversationSummaryStub,
} from './conversationResidency'
import { removeGroupMemberReferences } from './groupMembership'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import { isMetadataOnlySelectedConversation } from './selectedConversationLifecycle'
import { applyConversationBindingPatch, type ConversationBindingPatch } from './conversationBinding'
import { withPersistentRevisionLease } from './persistentRecordIterator'
import type {
    ActiveConversationMutationEvent,
    ConversationSessionToken,
} from './activeConversationSession'
import { safeStructuredClone } from '../polyfill'
import {
    applyPluginStorageMutations,
    orderPluginStorageKeys,
    PluginStorageBaseline,
    PluginStorageCaptureCache,
    type PluginStorageCapture,
    canonicalClone,
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

export { canonicalJson }

const SAVE_DEBOUNCE_MS = 500
/** Official publishes upload the full database snapshot, so they are spaced like upstream's save loop. */
const OFFICIAL_PUBLISH_MIN_INTERVAL_MS = 3_000
const CHARACTER_MUTATION_PAGE_SIZE = 100

type CompleteCharacter = character | groupChat
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
    capturePresets?(): botPreset[] | null
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
    onLocalRevision?(revision: DataRevision): void
    /** Advances revision-only working-set state synchronously and must not throw. */
    onStorageOnlyRevision?(revision: DataRevision): void
    /** Advances an adopted windowed selected-conversation authority synchronously. */
    onWindowedSelectedConversationRevision?(revision: DataRevision): void
    onConversationMutationPersistenceStarted?(
        event: ActiveConversationMutationEvent,
    ): ConversationMutationPersistenceHandle | null | undefined
    onConversationMutationPersisted?(event: PersistedConversationMutationEvent): void
    onConversationMutationFallbackPersisted?(event: PersistedConversationMutationEvent): void
    onPersistenceIdle?(): void
    onFlushPromise?(promise: Promise<void> | null): void
    onBackgroundError?(error: unknown): void
    onWorkingSetRefreshRequired?(revision: DataRevision | null): void
    onDestructiveReplacementFenceChanged?(active: boolean): void
    isConversationOperationActive?(): boolean
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
    baseline: string | null
}

interface PendingConversationMutation {
    event: ActiveConversationMutationEvent
}

interface PendingWindowedActivationChange {
    authority: WindowedConversationPersistenceAuthority
    change: WindowedConversationActivationChange
}

export interface ConversationMutationPersistenceHandle {
    release(): void
}

interface ConversationMutationProjection {
    exactMutations: ConversationMutation[] | null
    coveredPending: PendingConversationMutation[]
    character?: CharacterDetail
    coveredActivation?: PendingWindowedActivationChange
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
    if (canonicalJson(projected) === canonicalJson(captured)) return true
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
        projectedMessages.length === capturedMessages.length &&
        projectedMessages.every((message, index) =>
            messageMatchesAfterIdNormalization(message, capturedMessages[index]),
        )
    )
}

interface ReplacementAdmission extends PersistentMutationToken {
    authorityEpoch: number
    navigationGeneration: number | undefined
    baseline: CapturedState
    hadPendingDebounce: boolean
    supersededAdditionToken: object | null
}

export interface PersistentReplacementOptions {
    publishOfficial?: boolean
    authoritative?: boolean
    expectedRevision?: DataRevision
    expectedMutationGeneration?: number
    pluginStorageValues?: PluginStorageValue[]
}

export interface PersistentPresetMutationState {
    root: RootDatabase
    presets: botPreset[]
}

export interface PersistentPresetMutationResult extends PersistentPresetMutationState {
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
    relatedCharacters?: CharacterDetail[]
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
    private presetsBaseline: string | null = null
    private characterBaseline: string | null = null
    private characterBaselineId: string | null = null
    private windowedCharacterBaseline: WindowedSelectedCharacterCapture | null = null
    private pendingWindowedActivationChange: PendingWindowedActivationChange | null = null
    private dirtyGeneration = 0
    private pendingByteCount = 0
    private debounceHandle: unknown
    private readonly operationMutex = new Mutex()
    private flushPromise: Promise<void> | null = null
    private localFlushPromise: Promise<void> | null = null
    private localFlushDuringPublicationPromise: Promise<void> | null = null
    private queuedOperationCount = 0
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
        return (
            this.pendingByteCount > 0 ||
            this.debounceHandle !== undefined ||
            this.flushPromise !== null ||
            this.localFlushPromise !== null ||
            this.localFlushDuringPublicationPromise !== null ||
            this.queuedOperationCount > 0 ||
            this.additionPromise !== null ||
            this.pendingCharacterAddition !== null ||
            this.reservedCharacterAddition !== null ||
            this.pendingConversationMutations.length > 0 ||
            this.pendingWindowedActivationChange !== null
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
        this.setCharacterBaseline(captured)
        this.dirtyGeneration = 0
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
        this.persistenceWasBusy = false
        this.lastBackgroundErrorMessage = null
        if (this.destructiveReplacementFence?.state === 'held') {
            this.destructiveReplacementFence.refreshBaseline = captured
        }
        if (this.committedRefreshRevision !== null) {
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
        })
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
        this.characterBaseline = canonicalJson(character)
        this.characterBaselineId = character.chaId
        this.windowedCharacterBaseline = null
        this.pendingWindowedActivationChange = null
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
        const shell = captureWindowedCharacterShell(character)
        const currentShell = captureWindowedCharacterShell(currentCharacter)
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
        this.pendingConversationMutations.push({ event: detached })
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
                root: canonicalClone(rootValue.value),
                presets,
            }
            await mutate(state)

            const liveBeforeCommit = this.capture()
            const mutatedRoot = rebaseRootMutation(rootValue.value, state.root, operationStart.root)
            const committedRoot = rebaseConcurrentLiveDelta(
                operationStart.root,
                liveBeforeCommit.root,
                mutatedRoot,
            )
            const committedPresets = canonicalClone(state.presets)
            const committed = await this.dependencies.store.commit({
                expectedRevision: revision,
                root: committedRoot,
                replacePresets: committedPresets,
            })
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
            const operationStart = this.capture()
            const lease = await this.dependencies.store.acquireRevision(revision)
            const snapshot = await withPersistentRevisionLease(lease, async (reader) => {
                signal?.throwIfAborted()
                this.assertReadRevision(revision, reader.revision)
                const rootValue = await reader.readRoot()
                signal?.throwIfAborted()
                this.assertReadRevision(revision, rootValue.revision)
                const root = canonicalClone(rootValue.value)
                const ownerHeads: AssetOwnerHead[] = []
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
                const modules = Array.isArray(root.modules) ? root.modules : []
                for (let index = 0; index < modules.length; index++) {
                    signal?.throwIfAborted()
                    const value = await reader.readAssetOwnerHead({
                        kind: 'root-module-assets',
                        index,
                    })
                    signal?.throwIfAborted()
                    if (!value) continue
                    this.assertReadRevision(revision, value.revision)
                    ownerHeads.push(canonicalClone(value.value))
                }
                const personas = Array.isArray(root.personas) ? root.personas : []
                for (let index = 0; index < personas.length; index++) {
                    signal?.throwIfAborted()
                    if (!personas[index]?.embeddedModule) continue
                    const value = await reader.readAssetOwnerHead({
                        kind: 'persona-embedded-module-assets',
                        index,
                    })
                    signal?.throwIfAborted()
                    if (!value) continue
                    this.assertReadRevision(revision, value.revision)
                    ownerHeads.push(canonicalClone(value.value))
                }
                return { root, ownerHeads, aliases }
            })
            signal?.throwIfAborted()
            if (this.capture().rootCanonical !== operationStart.rootCanonical) {
                throw new PersistentRootModuleAppendRejectedError(
                    'Persistent root changed during module import',
                )
            }
            const modules = Array.isArray(snapshot.root.modules) ? snapshot.root.modules : []
            const moduleIndex = modules.length
            snapshot.root.modules = [...modules, input.module]
            const ownerHead: AssetOwnerHead = {
                owner: { kind: 'root-module-assets', index: moduleIndex },
                ...input.ownerHead,
            } as AssetOwnerHead
            const liveBeforeCommit = this.capture()
            signal?.throwIfAborted()
            commitStarted = true
            let committed: { revision: DataRevision }
            try {
                committed = await this.dependencies.store.commit({
                    expectedRevision: revision,
                    root: snapshot.root,
                    assetAliases: snapshot.aliases,
                    assetOwnerHeads: [...snapshot.ownerHeads, ownerHead],
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
            this.rootBaseline = canonicalJson(snapshot.root)
            this.dependencies.publishRootWorkingSet?.(publishedRoot)
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
            const committed = await this.dependencies.store.commit({
                expectedRevision: revision,
                pluginStorage: committedMutations,
            })
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
            await this.finishExplicitCommit(committed.revision)
        })
    }

    /**
     * Commits persona or toggle bindings of a conversation that has no active session (a summary
     * stub or the windowed selected shell) using metadata only, then lets the caller publish the
     * same change to the in-memory record so the shell matches its baseline.
     */
    mutateConversationBinding(
        characterId: string,
        conversationId: string,
        patch: ConversationBindingPatch,
        publish: (committedPatch: ConversationBindingPatch) => void,
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const clonePatch = (): ConversationBindingPatch =>
            Object.fromEntries(Object.entries(patch).map(([key, value]) => [
                key, value === undefined ? undefined : canonicalClone(value),
            ]))
        patch = clonePatch()
        return this.enqueue(async () => {
            await this.flushIterations('chat-binding', true)
            const metadata = await this.dependencies.store.readConversationMetadata(
                characterId,
                conversationId,
            )
            if (!metadata) throw new Error('Binding conversation is unavailable')
            this.assertReadRevision(this.revision, metadata.revision)
            const committed = await this.dependencies.store.commit({
                expectedRevision: this.revision,
                conversations: [
                    {
                        type: 'replace-range',
                        characterId,
                        conversationId,
                        start: metadata.value.totalMessages,
                        deleteCount: 0,
                        messages: [],
                        conversation: applyConversationBindingPatch(
                            { ...metadata.value.conversation },
                            patch,
                        ),
                    },
                ],
            })
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
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            const deleting = typeof outcome === 'object' && outcome?.delete === true
            const liveBeforeCommit = this.capture()
            const committedRoot = rebaseRootMutation(
                rootValue.value,
                state.root,
                liveBeforeCommit.root,
            )
            const rootChanged = canonicalJson(committedRoot) !== canonicalJson(rootValue.value)
            const committedDetail = deleting ? null : canonicalClone(state.character)
            const commit: WorkingSetCommit = { expectedRevision: revision }
            if (rootChanged) commit.root = committedRoot
            if (deleting) commit.deleteCharacterId = characterId
            else commit.character = committedDetail!

            const committed = await this.dependencies.store.commit(commit)
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
                    committedRoot,
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
                committedRoot,
            )
            await this.finishExplicitCommit(committed.revision)
            return true
        })
    }

    deletePersistentCharacterWithGroupReferences(
        characterId: string,
        reason: string,
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const mutationGeneration = this.dirtyGeneration
            const lease = await this.dependencies.store.acquireRevision(revision)
            let rootValue: { revision: DataRevision; value: RootDatabase } | undefined
            const relatedCharacters: CharacterDetail[] = []
            const relatedResidentsBefore = new Map<
                string,
                ReturnType<SaveCoordinator['captureResidentCharacter']>
            >()
            const found = await withPersistentRevisionLease(lease, async (reader) => {
                this.assertReadRevision(revision, reader.revision)
                rootValue = await reader.readRoot()
                this.assertReadRevision(revision, rootValue.revision)
                const targetValue = await reader.readCharacter(characterId)
                if (!targetValue) return false
                this.assertReadRevision(revision, targetValue.revision)
                if (targetValue.value.chaId !== characterId) {
                    throw new Error(`Character ${characterId} returned mismatched detail`)
                }

                for (const trash of [false, true]) {
                    let cursor: string | undefined
                    do {
                        const page = await reader.queryCharacters({
                            order: 'configured',
                            trash,
                            limit: CHARACTER_MUTATION_PAGE_SIZE,
                            cursor,
                        })
                        this.assertReadRevision(revision, page.revision)
                        for (const summary of page.items) {
                            if (summary.id === characterId || summary.type !== 'group') continue
                            const value = await reader.readCharacter(summary.id)
                            if (!value) throw new Error(`Character ${summary.id} was not found`)
                            this.assertReadRevision(revision, value.revision)
                            if (value.value.chaId !== summary.id || value.value.type !== 'group') {
                                throw new Error(
                                    `Character ${summary.id} returned mismatched detail`,
                                )
                            }
                            const group = canonicalClone(value.value) as Omit<groupChat, 'chats'>
                            if (!this.removeGroupCharacterReference(group, characterId)) continue
                            relatedResidentsBefore.set(
                                summary.id,
                                this.captureResidentCharacter(summary.id),
                            )
                            relatedCharacters.push(group)
                        }
                        cursor = page.nextCursor
                    } while (cursor)
                }
                return true
            })
            if (!found) return false
            if (!rootValue) return false
            if (this.dirtyGeneration !== mutationGeneration) {
                throw new Error(`Persistent data changed during character deletion: ${characterId}`)
            }
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            for (const [relatedId, before] of relatedResidentsBefore) {
                this.assertResidentCharacterUnchanged(relatedId, before)
            }

            const mutatedRoot = canonicalClone(rootValue.value)
            removeCharacterIdFromOrder(mutatedRoot, characterId)
            const liveBeforeCommit = this.capture()
            const committedRoot = rebaseRootMutation(
                rootValue.value,
                mutatedRoot,
                liveBeforeCommit.root,
            )
            const commit: WorkingSetCommit = {
                expectedRevision: revision,
                deleteCharacterId: characterId,
                characterDetails: canonicalClone(relatedCharacters),
            }
            if (canonicalJson(committedRoot) !== canonicalJson(rootValue.value)) {
                commit.root = committedRoot
            }
            const committed = await this.dependencies.store.commit(commit)
            const changedDuringCommit = this.dirtyGeneration !== mutationGeneration
            const relatedRaces = new Map<
                string,
                NonNullable<ReturnType<SaveCoordinator['captureResidentCharacter']>>
            >()
            for (const [relatedId, before] of relatedResidentsBefore) {
                const after = this.captureResidentCharacter(relatedId)
                if (!this.residentCharactersMatch(before, after) && after) {
                    relatedRaces.set(relatedId, after)
                }
            }
            const publishedRelatedCharacters = relatedCharacters.map((detail) => {
                const raced = relatedRaces.get(detail.chaId)
                if (!raced) return canonicalClone(detail)
                const resident = canonicalClone(raced.character)
                this.removeGroupCharacterReference(resident, characterId)
                const { chats: _chats, ...residentDetail } = resident
                return residentDetail as CharacterDetail
            })
            const liveAfterCommit = this.capture()
            const selectedRelatedDetail = relatedCharacters.find(
                (detail) => detail.chaId === liveAfterCommit.character?.chaId,
            )
            const selectedRelatedBefore = liveAfterCommit.character
                ? relatedResidentsBefore.get(liveAfterCommit.character.chaId)
                : null
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
                    relatedCharacters: publishedRelatedCharacters,
                },
                committedRoot,
                {
                    preservePendingWork: changedDuringCommit || relatedRaces.size > 0,
                    committedSelectedCharacter: selectedRelatedDetail
                        ? {
                              id: selectedRelatedDetail.chaId,
                              character: this.mergeCommittedDetailWithResident(
                                  selectedRelatedDetail,
                                  selectedRelatedBefore?.character ?? null,
                              ),
                          }
                        : undefined,
                },
            )
            await this.finishExplicitCommit(committed.revision)
            return true
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
            const replacement = canonicalClone(await mutate(current))
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            if (replacement.chaId !== characterId) {
                throw new Error(`Replacement character ID must remain ${characterId}`)
            }

            const committed = await this.dependencies.store.commit({
                expectedRevision: revision,
                replaceCharacter: replacement,
            })
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
            const committed = await this.dependencies.store.commit({
                expectedRevision: this.revision,
                conversations: [
                    {
                        type: 'replace-range',
                        characterId,
                        conversationId,
                        start: 0,
                        deleteCount: current.value.message.length,
                        messages: message,
                        conversation,
                    },
                ],
            })
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
            const replacement = canonicalClone(await createOrMutate(current))
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            if (replacement.chaId !== characterId) {
                throw new Error(`Upserted character ID must remain ${characterId}`)
            }

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
                commit.replaceCharacter = replacement
            } else {
                if (includeInCharacterOrder !== false) {
                    appendCharacterIdToOrder(mutatedRoot, characterId)
                    committedRoot = rebaseRootMutation(
                        rootValue.value,
                        mutatedRoot,
                        this.capture().root,
                    )
                    commit.root = committedRoot
                }
                commit.addCharacter = replacement
            }
            const committed = await this.dependencies.store.commit(commit)
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
                committedRoot,
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
            return canonicalClone(value.value)
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
        if (this.committedRefreshRevision === null && this.hasPendingOfficialPublication) {
            this.armOfficialPublishRetry(this.officialPublishDelayMs())
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
            const navigationGeneration = this.dependencies.getNavigationGeneration?.()
            const database = await this.dependencies.store.materializeDatabase(revision)
            const pluginStorageValues = options.includePluginStorageValues
                ? await this.materializePluginStorageValues(revision)
                : undefined
            if (
                this.revision !== revision ||
                this.dirtyGeneration !== generation ||
                this.dependencies.getNavigationGeneration?.() !== navigationGeneration
            ) {
                throw new Error('Working set changed during persistent database materialization')
            }
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
        const catalog = await this.dependencies.store.queryPluginStorage()
        this.assertReadRevision(revision, catalog.revision)
        return Promise.all(
            catalog.items.map(async ({ owner, key }) => {
                const stored = await this.dependencies.store.readPluginStorage(owner, key)
                if (!stored) throw new Error(`Missing plugin storage value for ${key}`)
                this.assertReadRevision(revision, stored.revision)
                return { owner, key, value: canonicalClone(stored.value) }
            }),
        )
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
    ): Promise<T> {
        this.queuedOperationCount += 1
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

    private async flushIterations(_reason: string, publishOfficial: boolean): Promise<void> {
        if (this.committedRefreshRevision !== null) throw new PersistentMutationFencedError()
        if (this.destructiveReplacementFence) publishOfficial = false
        if (publishOfficial && this.deferredPublicationRevision !== null) {
            await this.applyDeferredPublication()
        }
        if (publishOfficial && this.pendingPublicationCleanup.size > 0) {
            await this.retryPublicationCleanup()
        }
        while (true) {
            const generation = this.dirtyGeneration
            const captured = this.capture()
            const pendingConversationMutations = [...this.pendingConversationMutations]
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
            if (windowedCapture && addition) {
                throw new WindowedConversationRequiresCompatibilityError(
                    'character addition requires a complete selected character',
                )
            }
            const conversationProjection = windowedCapture
                ? await this.projectWindowedConversationMutations(
                      captured,
                      pendingConversationMutations,
                  )
                : this.projectConversationMutations(captured, pendingConversationMutations)
            const recordedConversations = conversationProjection?.exactMutations ?? null
            const commit: WorkingSetCommit = { expectedRevision: this.revision }
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
                commit.replacePresets = captured.presets
            }
            if (windowedCapture) {
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
                if (!addition.pending.locallyAdded) {
                    commit.addCharacter = addition.character
                } else if (
                    addition.canonical !== addition.pending.baseline &&
                    !commit.replaceCharacter &&
                    !commit.conversations
                ) {
                    commit.replaceCharacter = addition.character
                    replacementIsAddition = true
                }
            }

            if (
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
                    (commit.conversations ?? []).map(
                        (mutation) => `${mutation.characterId}\u0000${mutation.conversationId}`,
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
                    const committed = await this.dependencies.store.commit(commit)
                    this.currentRevision = committed.revision
                    if (commit.root || commit.rootMutations)
                        this.rootBaseline = captured.rootCanonical
                    if (commit.pluginStorage) {
                        this.pluginStorageBaseline = captured.pluginStorageCanonical
                    }
                    if (commit.replacePresets) this.presetsBaseline = captured.presetsCanonical
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
                        if (
                            conversationProjection?.coveredActivation &&
                            this.pendingWindowedActivationChange ===
                                conversationProjection.coveredActivation
                        ) {
                            this.pendingWindowedActivationChange = null
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
                    if (commit.addCharacter && addition) {
                        addition.pending.locallyAdded = true
                        addition.pending.baseline = addition.canonical
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

            if (
                windowedCapture === null &&
                captured.characterCanonical === null &&
                !commit.replaceCharacter
            )
                this.setCharacterBaseline(captured)

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
            this.windowedCharacterBaseline ||
            (this.dependencies.captureSelectedConversationAuthority?.() ?? null) !== null
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
            publish?: boolean
            committedSelectedCharacter?: { id: string; character: CompleteCharacter | null }
        } = {},
    ): void {
        this.currentRevision = result.revision
        this.dirtyGeneration++
        this.rootBaseline = canonicalJson(committedRoot)
        if (options.publish !== false) {
            this.dependencies.publishCharacterMutation?.(result)
            const published = this.capture()
            if (
                published.character?.chaId === result.characterId ||
                (result.kind === 'delete' && !published.character) ||
                (this.dependencies.publishCharacterMutation !== undefined &&
                    result.relatedCharacters?.some(
                        (detail) => detail.chaId === published.character?.chaId,
                    ))
            ) {
                this.setCharacterBaseline(published)
            }
        }
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

    private removeGroupCharacterReference(
        character: CharacterDetail | CompleteCharacter,
        characterId: string,
    ): boolean {
        if (character.type !== 'group') return false
        const group = character as Omit<groupChat, 'chats'> | groupChat
        const retained = removeGroupMemberReferences(group, new Set([characterId]))
        if (retained.characters.length === group.characters.length) return false
        group.characters = retained.characters
        group.characterTalks = retained.characterTalks
        group.characterActive = retained.characterActive
        return true
    }

    private async finishExplicitCommit(revision: DataRevision): Promise<void> {
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
        this.characterBaseline = captured.characterCanonical
        this.characterBaselineId = captured.character?.chaId ?? null
        this.windowedCharacterBaseline = null
        this.pendingWindowedActivationChange = null
    }

    private setWindowedCharacterBaseline(
        captured: WindowedSelectedCharacterCapture,
        revision: DataRevision,
        persistedSessionVersion: number,
    ): void {
        this.windowedCharacterBaseline = {
            shell: safeStructuredClone(captured.shell),
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
        let projectedShell = safeStructuredClone(baseline.shell)
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
        }
    }

    private selectedCaptureMatchesBaseline(captured: CapturedState): boolean {
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
        if (
            pending.length === 0 ||
            !captured.character ||
            this.characterBaseline === null ||
            this.characterBaselineId !== captured.character.chaId
        )
            return null

        const projected = JSON.parse(this.characterBaseline) as CompleteCharacter
        const relevantPending = pending.filter(({ event }) => event.characterId === projected.chaId)
        if (relevantPending.length === 0) return null
        const pendingByConversation = new Map<string, PendingConversationMutation[]>()
        for (const pendingMutation of relevantPending) {
            const conversationPending =
                pendingByConversation.get(pendingMutation.event.conversationId) ?? []
            conversationPending.push(pendingMutation)
            pendingByConversation.set(pendingMutation.event.conversationId, conversationPending)
        }

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
            let conversation = safeStructuredClone(projectedMatches[0])
            if (!Array.isArray(conversation.message)) continue

            let previousEvent: ActiveConversationMutationEvent | undefined
            let coveredCount = 0
            let coveredConversation: Chat | undefined
            const conversationMutations: ConversationMutation[][] = []
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

                const eventMutations: ConversationMutation[] = []
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
                    conversation.message.splice(range.start, deleteCount, ...messages)
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
                if (conversationMatchesAfterIdNormalization(conversation, capturedMatches[0])) {
                    coveredCount = conversationMutations.length
                    coveredConversation = safeStructuredClone(conversation)
                }
            }
            if (coveredCount === 0 || !coveredConversation) continue
            conversation = coveredConversation
            projected.chats[projectedIndex] = conversation
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
                mutations.length > 0 && canonicalJson(projected) === captured.characterCanonical
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
            try {
                this.dependencies.onConversationMutationPersisted?.({
                    characterId: pending.event.characterId,
                    conversationId: pending.event.conversationId,
                    sessionToken: pending.event.sessionToken,
                    sessionVersion: pending.event.sessionVersion,
                    revision,
                })
            } catch (error) {
                this.reportBackgroundError(error)
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
            try {
                this.dependencies.onConversationMutationFallbackPersisted?.({
                    characterId: pending.event.characterId,
                    conversationId: pending.event.conversationId,
                    sessionToken: pending.event.sessionToken,
                    sessionVersion: pending.event.sessionVersion,
                    revision,
                })
            } catch (error) {
                this.reportBackgroundError(error)
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
            const shell = captureWindowedCharacterShell(characterValue)
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
                    shellCanonical: canonicalJson(shell),
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
        const characterCanonical = optimized
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
            get character() {
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

    private capturePendingAddition(): {
        pending: PendingCharacterAddition
        character: CompleteCharacter
        canonical: string
    } | null {
        const pending = this.pendingCharacterAddition
        if (!pending) return null
        const value = this.dependencies.captureCharacter(pending.characterId)
        if (!value || value.chaId !== pending.characterId) {
            throw new Error(`Installed character ${pending.characterId} is not available`)
        }
        const canonical = canonicalJson(value)
        return { pending, character: JSON.parse(canonical) as CompleteCharacter, canonical }
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

    private armDebounce(): void {
        if (this.debounceHandle !== undefined || this.committedRefreshRevision !== null) return
        this.debounceHandle = this.clock.setTimeout(() => {
            this.debounceHandle = undefined
            this.startBackgroundFlush('debounce')
        }, SAVE_DEBOUNCE_MS)
    }

    private startBackgroundFlush(reason: string): void {
        try {
            void this.flushPendingData(reason).catch((error) => this.reportBackgroundError(error))
        } catch (error) {
            this.reportBackgroundError(error)
            if (this.hasPendingOfficialPublication) {
                this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
            }
        }
    }

    private reportBackgroundError(error: unknown): void {
        const message = error instanceof Error ? error.message : String(error)
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
        return captured.characterCanonical === expected.characterCanonical
    }
}
