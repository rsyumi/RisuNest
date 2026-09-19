import { createPersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'
import { derived, get, readonly, writable } from 'svelte/store'
import { doingChat } from '../process/generationState'
import { ReloadGUIPointer, selectedCharID, selIdState } from '../stores.svelte'
import type { ActiveConversationSession } from './activeConversationSession'
import type {
    ActiveConversationViewportSourceListener,
    CompleteConversationLease,
    ConversationPublicationOptions,
    SelectedConversationTarget,
    WindowedConversationMutationController,
} from './activeWorkingSet.svelte'
import type { ConversationViewportSource } from '../conversationViewportSource'
import type { Chat, Database, character, groupChat } from './database.svelte'
import { getDatabase, setDatabase } from './database.svelte'
import { prepareDatabaseForPersistence } from './databasePreparation'
import { getPersistentDataStore, getPersistentStorageAuthority } from './persistentDataStoreFactory'
import type {
    CharacterDetail,
    DataRevision,
    PluginStorageMutation,
} from './persistentDataStore'
import type {
    CharacterAdditionRequest,
    PersistentCharacterDetailMutation,
    PersistentCompleteCharacterMutation,
    PersistentCompleteCharacterUpsert,
    PersistentCompleteCharacterUpsertOptions,
    PersistentDatabaseMaterializationOptions,
    PersistentScopedReplacementOptions,
    PersistentDatabaseSnapshot,
    PersistentMutationToken,
    PersistentSelectedConversation,
} from './saveCoordinator'
import type { CharacterActivationOptions } from './activeWorkingSet.svelte'
import { notifyLocalPersistentRevision } from './persistentRevisionEvents'
import { retryCommittedWorkingSetRefreshWithContinuation } from './committedWorkingSetContinuation'
import {
    capturePersistentRoot,
    capturePersistentPluginStorage,
    capturePersistentPresets,
    captureResidentPersistentCharacter,
    captureSelectedPersistentCharacter,
    createPersistentDataRuntime,
    publishPersistentCharacterMutationToWorkingSet,
    publishPersistentConversationReplacementToWorkingSet,
    restoreStableWorkingSetSelection,
    type CommittedApplyOutcome,
    type PersistentDestructiveReplacementFence,
    type PersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
import type { OfficialRevisionPublisher } from './saveCoordinator'
import type { PersistentPresetMutation } from './saveCoordinator'
import type { PersistentReplacementOptions } from './saveCoordinator'
import { workingSetResidency } from './workingSetResidency'
import {
    createPresetCatalogWorkingSetFromValues,
    hydrateWorkingSetCharacterDetail,
    isArchivedCharacter,
    isCatalogCharacterStub,
    isCatalogPresetWorkingSet,
} from './workingSetCatalog'
import {
    notifyPluginStorageAuthorityReplacement,
    notifyPluginStorageCompatibilityMutation,
    notifyPluginStorageOwnerChanged,
    resolveLifecyclePluginStorageOwner,
} from '../plugins/pluginStorageStore'
import {
    applyPluginStorageMutationsInPlace,
    orderPluginStorageKeys,
} from './saveCoordinatorHelpers'
import { getRuntimePerformanceBudgets } from '../runtimePerformanceProfile'
import type { WindowedConversationPersistenceAuthority } from './saveCoordinator'
import {
    readPinnedSelectedConversationWindow,
    type PersistentSelectedConversationWindow,
} from './persistentConversationRead'

export type {
    CommittedApplyOutcome,
    ReplacementChangeSet,
    PersistentDestructiveReplacementFence,
    PersistentDataRuntime,
    PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
export { createPersistentDataRuntime } from './persistentDataRuntime'

type CompleteCharacter = character | groupChat

export function createProductionStateAdapter(): PersistentDataRuntimeStateAdapter {
    const readSelectedCharacter = () => {
        const database = getDatabase()
        const selected = captureSelectedPersistentCharacter(database, selIdState.selId)
        return selected ? captureResidentPersistentCharacter(database, selected.chaId) : null
    }
    const canonicalCapture = createPersistenceCanonicalCapture({
        root: getDatabase,
        pluginStorage: () => capturePersistentPluginStorage(getDatabase()),
        presets: () => capturePersistentPresets(getDatabase()),
        character: readSelectedCharacter,
    })
    return {
        canonicalCapture,
        captureRoot() {
            return capturePersistentRoot(getDatabase())
        },
        capturePluginStorage() {
            return capturePersistentPluginStorage(getDatabase())
        },
        publishPluginStorageWorkingSet(storage) {
            getDatabase().pluginCustomStorage = storage
            notifyPluginStorageAuthorityReplacement()
        },
        publishPluginStorageMutations(mutations, keys) {
            const storage = (getDatabase().pluginCustomStorage ??= {})
            applyPluginStorageMutationsInPlace(
                storage,
                mutations,
                resolveLifecyclePluginStorageOwner,
            )
            const ordered = orderPluginStorageKeys(storage, keys)
            if (ordered !== storage) getDatabase().pluginCustomStorage = ordered
            for (const mutation of mutations) {
                notifyPluginStorageCompatibilityMutation(mutation)
            }
        },
        capturePresets() {
            return capturePersistentPresets(getDatabase())
        },
        captureSelectedCharacter(): CompleteCharacter | null {
            return readSelectedCharacter()
        },
        captureCharacter(id) {
            return captureResidentPersistentCharacter(getDatabase(), id)
        },
        getSelectedCharacterId() {
            return getDatabase().characters[get(selectedCharID)]?.chaId
        },
        getSelectedConversationId() {
            const character = getDatabase().characters[get(selectedCharID)]
            return character?.chats[character.chatPage ?? 0]?.id
        },
        replaceDatabase(database, activeCharacterIds, forceScalableProjection) {
            const liveDatabase = getDatabase()
            const selectedCharacter = liveDatabase.characters[get(selectedCharID)]
            const selectedCharacterId = selectedCharacter?.chaId ?? null
            const selectedConversationId = selectedCharacter
                ?.chats[selectedCharacter.chatPage ?? 0]?.id ?? null
            workingSetResidency.clear()
            const replacement = productionConfiguration.projectWorkingSet?.(
                database,
                selectedCharacterId,
                selectedConversationId,
                activeCharacterIds,
                forceScalableProjection,
            ) ?? database
            setDatabase(replacement)
            notifyPluginStorageAuthorityReplacement()
            restoreStableWorkingSetSelection(
                replacement,
                selectedCharacterId,
                selectedConversationId,
                (index) => selectedCharID.set(index),
            )
        },
        publishPresetWorkingSet({ revision, root, presets }) {
            const database = getDatabase()
            const scalable = isCatalogPresetWorkingSet(database.botPresets)
            Object.assign(database, root)
            if (!scalable) {
                database.botPresets = presets
                return
            }
            database.botPresets = createPresetCatalogWorkingSetFromValues(
                presets,
                revision,
                root.botPresetsId,
            )
        },
        publishRootWorkingSet(root) {
            Object.assign(getDatabase(), root)
        },
        publishCharacterMutation(state) {
            const target = productionRuntime?.captureSelectedConversationTarget()
            const session = productionRuntime?.getActiveConversationSession()
            const database = getDatabase()
            publishPersistentCharacterMutationToWorkingSet(
                database,
                state,
                workingSetResidency,
                get(selectedCharID),
                (index) => selectedCharID.set(index),
            )
            if (target && session)
                productionRuntime?.refreshSelectedConversationAfterReplacement(target, session)
            const selectedCharacterId = database.characters[get(selectedCharID)]?.chaId ?? null
            productionRuntime?.reconcileActiveCharacterIds(database, selectedCharacterId)
        },
        publishConversationReplacement(result) {
            const target = productionRuntime?.captureSelectedConversationTarget()
            const session = productionRuntime?.getActiveConversationSession()
            publishPersistentConversationReplacementToWorkingSet(getDatabase(), result)
            if (target && session)
                productionRuntime?.refreshSelectedConversationAfterReplacement(target, session)
        },
        installCompleteDatabase(database) {
            workingSetResidency.clear()
            setDatabase(database)
            notifyPluginStorageAuthorityReplacement()
        },
        restoreSelection(characterId, conversationId) {
            restoreStableWorkingSetSelection(
                getDatabase(),
                characterId,
                conversationId,
                (index) => selectedCharID.set(index),
            )
        },
        publishCharacter(character) {
            const database = getDatabase()
            const index = database.characters.findIndex((candidate) => candidate.chaId === character.chaId)
            if (index < 0) return
            workingSetResidency.markCharacterHydrated(character.chaId)
            workingSetResidency.reconcileConversationResidency(character)
            database.characters[index] = character
            selectedCharID.set(index)
        },
        publishCharacterSet(primary, related) {
            const database = getDatabase()
            const relatedIndices = related.map((character) =>
                database.characters.findIndex(
                    (candidate) => candidate.chaId === character.chaId,
                ),
            )
            const primaryIndex = database.characters.findIndex(
                (candidate) => candidate.chaId === primary.chaId,
            )
            if (primaryIndex < 0 || relatedIndices.some((index) => index < 0)) return
            for (let index = 0; index < related.length; index++) {
                const detail = related[index]
                const character = hydrateWorkingSetCharacterDetail(
                    database,
                    relatedIndices[index],
                    detail,
                )
                workingSetResidency.markCharacterHydrated(character.chaId)
            }
            workingSetResidency.markCharacterHydrated(primary.chaId)
            workingSetResidency.reconcileConversationResidency(primary)
            database.characters[primaryIndex] = primary
            selectedCharID.set(primaryIndex)
        },
        publishConversation(
            characterId,
            conversation: Chat,
            nextCharacter?: CompleteCharacter,
            options?: ConversationPublicationOptions,
        ) {
            const database = getDatabase()
            const characterIndex = database.characters.findIndex(
                (candidate) => candidate.chaId === characterId,
            )
            if (characterIndex < 0) return
            const character = nextCharacter ?? database.characters[characterIndex]
            const conversationIndex = character.chats.findIndex(
                (candidate) => candidate.id === conversation.id,
            )
            if (conversationIndex < 0) return
            if (!nextCharacter) character.chats[conversationIndex] = conversation
            character.chatPage = conversationIndex
            database.characters[characterIndex] = character
            workingSetResidency.reconcileConversationResidency(character)
            selectedCharID.set(characterIndex)
            // The viewport source publishes representation changes itself. A
            // global reload here resets parser caches and images during each
            // compatibility promotion and eviction of the same conversation.
            if (!options?.representationOnly) ReloadGUIPointer.set(Math.random())
        },
        captureActivationRollback(characterIds) {
            const database = getDatabase()
            const ids = [...new Set(characterIds)]
            const entries = ids.map((id) => {
                const index = database.characters.findIndex(
                    (character) => character.chaId === id,
                )
                return {
                    id,
                    index,
                    value: index < 0 ? null : database.characters[index],
                    released: workingSetResidency.isCharacterReleased(id),
                }
            })
            const selected =
                database.characters[get(selectedCharID)]?.chaId ?? null
            let restored = false
            return () => {
                if (restored) return
                restored = true
                const current = getDatabase()
                for (const entry of entries) {
                    const currentIndex = current.characters.findIndex(
                        (character) => character.chaId === entry.id,
                    )
                    if (entry.value === null) {
                        if (currentIndex >= 0)
                            current.characters.splice(currentIndex, 1)
                        continue
                    }
                    if (currentIndex >= 0)
                        current.characters[currentIndex] = entry.value
                    else
                        current.characters.splice(
                            Math.min(entry.index, current.characters.length),
                            0,
                            entry.value,
                        )
                }
                for (const entry of entries) {
                    workingSetResidency.forgetCharacter(entry.id)
                    if (entry.released || entry.value === null) {
                        if (entry.released)
                            workingSetResidency.markCharacterReleased(entry.id)
                        continue
                    }
                    workingSetResidency.markCharacterHydrated(entry.id)
                    workingSetResidency.reconcileConversationResidency(
                        entry.value,
                    )
                }
                selectedCharID.set(
                    selected === null
                        ? -1
                        : current.characters.findIndex(
                              (character) => character.chaId === selected,
                          ),
                )
            }
        },
        shouldHydrateFullCharacter() {
            return !workingSetResidency.allowsEviction
        },
        canReleaseConversation(character, conversationId, nextConversationId) {
            return workingSetResidency.canReleaseConversation(
                character,
                conversationId,
                nextConversationId,
            )
        },
        canUseWindowedSelectedConversation() {
            return workingSetResidency.allowsEviction
        },
        isConversationOperationActive() {
            return get(doingChat)
        },
        captureWorkingSetDatabase() {
            return getDatabase()
        },
        onPluginStorageChanged(owner) {
            notifyPluginStorageOwnerChanged(owner)
        },
        getGeneratingConversation() {
            const database = getDatabase()
            const character = database.characters[get(selectedCharID)]
            if (!character) return null
            const conversation = character.chats[character.chatPage ?? 0]
            if (!conversation) return null
            return { characterId: character.chaId, conversationId: conversation.id }
        },
        subscribeConversationOperationActive(listener) {
            return doingChat.subscribe(listener)
        },
        conversationViewportRowBudget: getRuntimePerformanceBudgets().chatMountedMessageBudget,
        canActivateWorkingSet() {
            return !get(doingChat)
        },
        canDeactivateWorkingSet() {
            return !get(doingChat)
        },
        canDeactivateCharacter(id) {
            const character = getDatabase().characters.find((candidate) => candidate.chaId === id)
            return !character?.chats.some((chat) => chat.isStreaming)
        },
        releaseInactiveCharacter(id) {
            workingSetResidency.releaseCharacterToCatalog(getDatabase(), id)
        },
    }
}

export interface ProductionRuntimeConfiguration {
    officialPublisher: OfficialRevisionPublisher | null
    onLocalRevision?: (revision: DataRevision) => void
    onFlushPromise?: (promise: Promise<void> | null) => void
    onBackgroundError?: (error: unknown) => void
    projectWorkingSet?(
        database: Database,
        selectedCharacterId: string | null,
        selectedConversationId: string | null,
        activeCharacterIds?: ReadonlySet<string>,
        forceScalableProjection?: boolean,
    ): Database
}

const productionConfiguration: ProductionRuntimeConfiguration = {
    officialPublisher: null,
}
let productionRuntime: PersistentDataRuntime | null = null
const workingSetRefreshRevision = writable<DataRevision | null>(null)
const destructiveReplacementActive = writable(false)
export const persistentWorkingSetRefreshRevision = readonly(workingSetRefreshRevision)
export const persistentWorkingSetInputBlocked = derived(
    [destructiveReplacementActive, workingSetRefreshRevision],
    ([active, revision]) => active || revision !== null,
)

export function configurePersistentDataRuntime(
    configuration: Partial<ProductionRuntimeConfiguration>,
): void {
    Object.assign(productionConfiguration, configuration)
}

export function getPersistentDataRuntime(): PersistentDataRuntime {
    if (!productionRuntime) {
        productionRuntime = createPersistentDataRuntime({
            store: getPersistentDataStore(),
            state: createProductionStateAdapter(),
            getOfficialPublisher: () => productionConfiguration.officialPublisher,
            onLocalRevision: (revision) => {
                productionConfiguration.onLocalRevision?.(revision)
                notifyLocalPersistentRevision(revision)
            },
            onFlushPromise: (promise) => productionConfiguration.onFlushPromise?.(promise),
            onBackgroundError: (error) => productionConfiguration.onBackgroundError?.(error),
            onWorkingSetRefreshRequired: (revision) => workingSetRefreshRevision.set(revision),
            onDestructiveReplacementFenceChanged: (active) => destructiveReplacementActive.set(active),
            prepareDatabase: prepareDatabaseForPersistence,
        })
    }
    return productionRuntime
}

export const initializeActiveWorkingSet = (database: Database): Promise<void> =>
    getPersistentDataRuntime().initializeActiveWorkingSet(database)
export const refreshActiveWorkingSetFromStore = (revision: DataRevision): Promise<CommittedApplyOutcome> =>
    getPersistentDataRuntime().refreshActiveWorkingSetFromStore(revision)
export const retryCommittedWorkingSetRefresh = async (): Promise<CommittedApplyOutcome | null> => {
    const runtime = getPersistentDataRuntime()
    return retryCommittedWorkingSetRefreshWithContinuation(
        runtime,
        productionConfiguration.onBackgroundError,
    )
}
export const assertPersistentMutationAllowed = (expectedAuthorityEpoch?: number): void =>
    getPersistentDataRuntime().assertPersistentMutationAllowed(expectedAuthorityEpoch)
export const getPersistentStorageAuthorityEpoch = (): number =>
    getPersistentDataRuntime().getStorageAuthorityEpoch()
export const markPersistentDataDirty = (estimatedBytes: number): void =>
    getPersistentDataRuntime().markPersistentDataDirty(estimatedBytes)
export const flushPendingData = (reason: string): Promise<void> =>
    getPersistentDataRuntime().flushPendingData(reason)
export const flushPendingDataLocally = (reason: string): Promise<void> =>
    getPersistentDataRuntime().flushPendingDataLocally(reason)
export const acknowledgeGenerationCompletion = async (expectedAuthorityEpoch?: number): Promise<void> => {
    const runtime = getPersistentDataRuntime()
    const authorityEpoch = expectedAuthorityEpoch ?? runtime.getStorageAuthorityEpoch()
    await runtime.acknowledgeGenerationCompletion(authorityEpoch)
    runtime.assertPersistentMutationAllowed(authorityEpoch)
    notifyLocalPersistentRevision(runtime.revision, 'generation-complete')
}
export const commitCharacterAddition = (
    request: CharacterAdditionRequest,
    reason: string,
): Promise<void> => getPersistentDataRuntime().commitCharacterAddition(request, reason)
export const activateCharacter = (
    id: string,
    options?: CharacterActivationOptions,
): Promise<boolean> => {
    // An archived character has no detail to hydrate, so selection stops here
    // instead of failing inside the read.
    const member = getDatabase().characters.find((candidate) => candidate.chaId === id)
    if (member && isArchivedCharacter(member)) return Promise.resolve(false)
    return getPersistentDataRuntime().activateCharacter(id, options)
}
export function hydrateCurrentGroupMemberDetail(
    groupId: string,
    detail: CharacterDetail,
): boolean {
    const database = getDatabase()
    const selectedIndex = get(selectedCharID)
    const selectedGroup = database.characters[selectedIndex]
    if (selectedGroup?.type !== 'group' || selectedGroup.chaId !== groupId) return false
    const memberIndex = database.characters.findIndex(
        (character) => character.chaId === detail.chaId,
    )
    if (memberIndex < 0 || detail.chaId === groupId) return false
    const member = database.characters[memberIndex]
    // An archived member has no detail to hydrate, so it is unusable rather
    // than loadable. This must come before the stub check.
    if (isArchivedCharacter(member)) return false
    if (!isCatalogCharacterStub(member)) return true
    const hydrated = hydrateWorkingSetCharacterDetail(database, memberIndex, detail)
    workingSetResidency.markCharacterHydrated(hydrated.chaId)
    return true
}
export const activateConversation = (id: string): Promise<boolean> =>
    getPersistentDataRuntime().activateConversation(id)
export const getActiveConversationSession = (): ActiveConversationSession | null =>
    getPersistentDataRuntime().getActiveConversationSession()
export const getSelectedConversationMode = (): 'complete' | 'windowed' | null =>
    getPersistentDataRuntime().getSelectedConversationMode()
export const getActiveConversationViewportSource = (): ConversationViewportSource | null =>
    getPersistentDataRuntime().getActiveConversationViewportSource()
export const subscribeActiveConversationViewportSource = (
    listener: ActiveConversationViewportSourceListener,
): (() => void) => getPersistentDataRuntime().subscribeActiveConversationViewportSource(listener)
export const captureSelectedConversationTarget = (): SelectedConversationTarget | null =>
    getPersistentDataRuntime().captureSelectedConversationTarget()
export const captureSelectedConversationAuthority = ():
    WindowedConversationPersistenceAuthority | null =>
    getPersistentDataRuntime().captureSelectedConversationAuthority()
export const acquireCompleteConversation = (
    reason: string,
    target?: SelectedConversationTarget | null,
): Promise<CompleteConversationLease> =>
    getPersistentDataRuntime().acquireCompleteConversation(reason, target)
export const captureWindowedConversationMutationController = (
    target: SelectedConversationTarget,
    chat: Chat,
    absoluteStartIndex: number,
): WindowedConversationMutationController | null =>
    getPersistentDataRuntime().captureWindowedConversationMutationController(
        target,
        chat,
        absoluteStartIndex,
    )
export const tryDemoteSelectedConversation = (
    target?: SelectedConversationTarget | null,
): boolean => getPersistentDataRuntime().tryDemoteSelectedConversation(target)
export const refreshSelectedConversationAfterReplacement = (
    target: SelectedConversationTarget,
    expectedSession: ActiveConversationSession,
): boolean => getPersistentDataRuntime().refreshSelectedConversationAfterReplacement(
    target,
    expectedSession,
)
export const invalidateActiveConversationSession = (): void =>
    getPersistentDataRuntime().invalidateActiveConversationSession()
export const peekActiveConversationSession = (): ActiveConversationSession | null =>
    productionRuntime?.getActiveConversationSession() ?? null
export const deactivateActiveWorkingSet = (): Promise<boolean> =>
    getPersistentDataRuntime().deactivateActiveWorkingSet()
export const reconcilePersistentActiveCharacterIds = (
    database: Database,
    selectedCharacterId: string | null,
): ReadonlySet<string> => getPersistentDataRuntime().reconcileActiveCharacterIds(
    database,
    selectedCharacterId,
)
export const getPersistentNavigationGeneration = (): number =>
    getPersistentDataRuntime().getNavigationGeneration()
export const fencePersistentNavigation = (): number =>
    getPersistentDataRuntime().fenceNavigation()
export const invalidatePersistentNavigation = (): void =>
    getPersistentDataRuntime().invalidateNavigation()
export const replacePersistentDatabase = (
    database: Database,
    reason: string,
    options?: PersistentReplacementOptions,
): Promise<CommittedApplyOutcome> => getPersistentDataRuntime().replacePersistentDatabase(database, reason, options)
export const mutatePersistentPluginStorage = (
    reason: string,
    mutations: readonly PluginStorageMutation[],
): Promise<void> => getPersistentDataRuntime().mutatePersistentPluginStorage(reason, mutations)
export const mutatePersistentPresets = (
    reason: string,
    mutate: PersistentPresetMutation,
): Promise<void> => getPersistentDataRuntime().mutatePersistentPresets(reason, mutate)
export const appendPersistentRootModule = (
    input: import('./saveCoordinator').PersistentRootModuleAppend,
    signal?: AbortSignal,
): Promise<void> => getPersistentDataRuntime().appendPersistentRootModule(
    'native-risum-import',
    input,
    signal,
)
export const mutatePersistentCharacterDetail = (
    characterId: string,
    reason: string,
    mutate: PersistentCharacterDetailMutation,
): Promise<boolean> => getPersistentDataRuntime().mutatePersistentCharacterDetail(
    characterId,
    reason,
    mutate,
)
export const deletePersistentCharacterWithGroupReferences = (
    characterId: string,
    reason: string,
): Promise<boolean> => getPersistentDataRuntime().deletePersistentCharacterWithGroupReferences(
    characterId,
    reason,
)
export const replacePersistentCompleteCharacter = (
    characterId: string,
    reason: string,
    mutate: PersistentCompleteCharacterMutation,
    options?: PersistentScopedReplacementOptions,
): Promise<boolean> => getPersistentDataRuntime().replacePersistentCompleteCharacter(
    characterId,
    reason,
    mutate,
    options,
)
export const replacePersistentConversation = (
    characterId: string,
    conversationId: string,
    reason: string,
    replacement: Chat,
    options?: PersistentScopedReplacementOptions,
): Promise<boolean> => getPersistentDataRuntime().replacePersistentConversation(
    characterId,
    conversationId,
    reason,
    replacement,
    options,
)
export const upsertPersistentCompleteCharacter = (
    characterId: string,
    reason: string,
    createOrMutate: PersistentCompleteCharacterUpsert,
    options?: PersistentCompleteCharacterUpsertOptions,
): Promise<boolean> => getPersistentDataRuntime().upsertPersistentCompleteCharacter(
    characterId,
    reason,
    createOrMutate,
    options,
)
export const readPersistentCharacterDetail = (
    characterId: string,
    reason: string,
): Promise<CharacterDetail | null> => getPersistentDataRuntime().readPersistentCharacterDetail(
    characterId,
    reason,
)
export const readPersistentCompleteCharacter = (
    characterId: string,
    reason: string,
): Promise<CompleteCharacter | null> => getPersistentDataRuntime().readPersistentCompleteCharacter(
    characterId,
    reason,
)
export const readPersistentConversation = (
    characterId: string,
    conversationId: string,
    reason: string,
): Promise<Chat | null> => getPersistentDataRuntime().readPersistentConversation(
    characterId,
    conversationId,
    reason,
)
export const readPersistentConversationAt = (
    characterId: string,
    orderedPosition: number,
    reason: string,
): Promise<Chat | null> => getPersistentDataRuntime().readPersistentConversationAt(
    characterId,
    orderedPosition,
    reason,
)
export const readPersistentSelectedConversation = (
    characterId: string,
    reason: string,
): Promise<PersistentSelectedConversation | null> =>
    getPersistentDataRuntime().readPersistentSelectedConversation(characterId, reason)
export const readPersistentSelectedConversationWindow = (
    characterId: string,
    count: number,
    offset: number,
    reason: string,
    signal?: AbortSignal,
): Promise<PersistentSelectedConversationWindow | null> => {
    const runtime = getPersistentDataRuntime()
    return readPinnedSelectedConversationWindow({
        store: runtime.store,
        flushPendingData: (readReason) => runtime.flushPendingData(readReason),
        getNavigationGeneration: () => runtime.getNavigationGeneration(),
    }, {
        characterId,
        count,
        offset,
        reason,
        signal,
    })
}
export const capturePersistentMutationToken = (
    reason: string,
    options?: { publishOfficial?: boolean },
): Promise<PersistentMutationToken> =>
    getPersistentDataRuntime().capturePersistentMutationToken(reason, options)
export const acquireDestructiveReplacementFence = (
    expected: PersistentMutationToken,
): Promise<PersistentDestructiveReplacementFence> =>
    getPersistentDataRuntime().acquireDestructiveReplacementFence(expected)
export const acquireCommittedWorkingSetRefreshFence =
(): Promise<PersistentDestructiveReplacementFence> =>
    getPersistentDataRuntime().acquireCommittedWorkingSetRefreshFence()
export const materializePersistentDatabaseSnapshot = (reason: string): Promise<Database> =>
    getPersistentDataRuntime().materializePersistentDatabaseSnapshot(reason)
export const materializePersistentDatabaseSnapshotWithRevision = (
    reason: string,
    options?: PersistentDatabaseMaterializationOptions,
): Promise<PersistentDatabaseSnapshot> =>
    getPersistentDataRuntime().materializePersistentDatabaseSnapshotWithRevision(reason, options)

export const releaseInactiveWorkingSet = (
    canRelease?: () => boolean | Promise<boolean>,
    isCurrent?: () => boolean,
): Promise<boolean> => getPersistentDataRuntime().releaseInactiveWorkingSet(
    canRelease,
    isCurrent,
)

export const publishCurrentOfficialRevision = (): Promise<void> =>
    getPersistentDataRuntime().publishCurrentOfficialRevision()

export const hasPendingOfficialPublication = (): boolean =>
    getPersistentDataRuntime().hasPendingOfficialPublication()
