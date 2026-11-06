import type { PluginStorageWriteCheck } from '../plugins/pluginStorageProtection'
import { generatingConversations } from './generatingConversationRegistry'
import type { LwwStageReceive, LwwApplyResult } from './persistentDataStore'
import { flushPersistentIdentityEdits, derivePersistentIdentityMirrors } from './persistentIdentityHooks'
import { applyReceivedDisplaySettings } from '../gui/receivedDisplaySettings'
import { isConversationStreaming } from './streamingConversationRegistry'
import { createPersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'
import { isTauri } from '../platform'
import { restoreStreamingConversationState } from './streamingConversationRegistry'
import { derived, get, readonly, writable } from 'svelte/store'
import { doingChat } from '../process/generationState'
import { ReloadGUIPointer, selectedCharID, selIdState } from '../stores.svelte'
import type { ActiveConversationSession } from './activeConversationSession'
import type {
    ActiveConversationViewportSourceListener,
    CompleteConversationLease,
    ConversationPublicationOptions,
    SelectedConversationTarget,
    WindowedChatListEditResult,
    WindowedConversationMutationController,
} from './activeWorkingSet.svelte'
import type { ConversationViewportSource } from '../conversationViewportSource'
import type { Chat, Database, botPreset, character } from './database.svelte'
import { fillRootFieldDefaults, getDatabase, setDatabase, setEffectivePresetOverride } from './database.svelte'
import { getEffectivePresetOverride } from './effectiveIdentityState'
import { prepareDatabaseForPersistence, preparePersistentRootForWorkingSet } from './databasePreparation'
import { getPersistentDataStore, getPersistentStorageAuthority } from './persistentDataStoreFactory'
import { collectCharacterConversationAssetKeys, scheduleDeletedOwnerAssetCleanup, type AssetAliasCleanupTarget } from './deletedOwnerAssetCleanup'
import type {
    CharacterDetail,
    DataRevision,
    PluginStorageMutation,
} from './persistentDataStore'
import type {
    CharacterAdditionRequest,
    CharacterDetailFieldsMutation,
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
import { invalidatePluginDeviceKeyspaces } from '../plugins/pluginDeviceKeyspace'
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
import { createPresetCatalogWorkingSetFromValues, carryCatalogCharacterMetadata, patchWorkingSetRoot, isArchivedCharacter, isWorkingSetCharacterStub, isCatalogPresetWorkingSet } from './workingSetCatalog'
import {
    notifyPluginStorageAuthorityReplacement,
    notifyPluginStorageCompatibilityMutation,
    notifyPluginStorageOwnerChanged,
    resolveLifecyclePluginStorageOwner,
} from '../plugins/pluginStorageStore'
import {
    canonicalClone,
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

type CompleteCharacter = character

// Catalog entries carry only their summary and the local fields normalization adds.
const catalogPresetKeys = new Set(['id', 'name', 'image', 'localNetworkMode', 'localNetworkTimeoutSec'])
export const isLoadedPreset = (value: botPreset | undefined): boolean =>
    !!value && Object.keys(value).some((key) => !catalogPresetKeys.has(key))

/// A whole replacement starts a new identity state, so a preset chain override
/// is applied again when its preset is still in the library.
function carryPresetOverride(id: string, readPreset?: (id: string) => Promise<{ value: botPreset } | null>): void {
    const database = getDatabase()
    const record = database.botPresets?.find((value) => value?.id === id)
    if (!record) return
    if (isLoadedPreset(record)) {
        setEffectivePresetOverride(id, database)
        return
    }
    if (!readPreset) return
    const selectedId = database.botPresets[database.botPresetsId]?.id
    void readPreset(id).then((value) => {
        const current = getDatabase()
        if (!value || current !== database || getEffectivePresetOverride(current) !== null ||
            current.botPresets[current.botPresetsId]?.id !== selectedId) return
        const index = current.botPresets.findIndex((preset) => preset?.id === id)
        if (index < 0) return
        if (!isLoadedPreset(current.botPresets[index])) current.botPresets[index] = value.value
        setEffectivePresetOverride(id, current)
    }).catch((error) => productionConfiguration.onBackgroundError?.(error))
}

export function createProductionStateAdapter(options: {
    readPreset?(id: string): Promise<{ value: botPreset } | null>
} = {}): PersistentDataRuntimeStateAdapter {
    const characterIndex = $derived.by(() => new Map(getDatabase().characters.map((value) => [value.chaId,value])))
    const residentIndex = $derived.by(() => new Map([...characterIndex].filter(([,value]) => !isWorkingSetCharacterStub(value))))
    const residentCharacters = () => [...residentIndex.values()].filter((value) => !workingSetResidency.isCharacterReleased(value.chaId))
    const residentPresets = $derived.by(() => {
        const presets = getDatabase().botPresets ?? []
        return isCatalogPresetWorkingSet(presets) ? presets.filter((value) => value && Object.keys(value).some((key) => !['id','name','image'].includes(key))) : []
    })
    const readSelectedCharacter = () => {
        const selected = getDatabase().characters[selIdState.selId]
        return selected && !isWorkingSetCharacterStub(selected) && !workingSetResidency.isCharacterReleased(selected.chaId) ? selected : null
    }
    const canonicalCapture = createPersistenceCanonicalCapture({
        root: getDatabase,
        rootField(key, value) {
            const database = getDatabase()
            if (key === 'botPresetsId') return database.botPresets?.[database.botPresetsId]?.['id'] ?? value
            if (key === 'selectedPersona') return database.personas?.[database.selectedPersona]?.id ?? value
            return value
        },
        characters: residentCharacters,
        pluginStorage: () => capturePersistentPluginStorage(getDatabase()),
        presets: () => capturePersistentPresets(getDatabase()),
        character: readSelectedCharacter,
    })
    return {
        canonicalCapture,
        beforeCapture: flushPersistentIdentityEdits,
        afterRemoteApply: derivePersistentIdentityMirrors,
        fillRootFieldDefaults(fields) {
            fillRootFieldDefaults(getDatabase(), fields)
        },
        afterRemoteRootChange(fields) {
            void applyReceivedDisplaySettings(fields).catch((error) => console.error(error))
        },
        afterRemotePluginChange() {
            void import('../plugins/plugins.svelte').then(({ requestPluginReloadAfterSync }) => requestPluginReloadAfterSync())
                .catch((error) => productionConfiguration.onBackgroundError?.(error))
        },
        captureCharacterIndex: () => characterIndex,
        captureCharacters() {
            return residentCharacters()
        },
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
        capturePresetRecords() {
            return residentPresets
        },
        capturePresets() {
            return capturePersistentPresets(getDatabase())
        },
        captureSelectedCharacter(): CompleteCharacter | null {
            return readSelectedCharacter()
        },
        captureCharacter(id) {
            return workingSetResidency.isCharacterReleased(id) ? null : residentIndex.get(id) ?? null
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
            const presetOverride = getEffectivePresetOverride(liveDatabase)
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
            if (isTauri) replacement.account = getDatabase().account
            setDatabase(replacement)
            // Loading clears every streaming flag, so running generations restore theirs afterwards.
            for (const character of getDatabase().characters) {
                for (const chat of character.chats) restoreStreamingConversationState(chat)
            }
            notifyPluginStorageAuthorityReplacement()
            restoreStableWorkingSetSelection(
                replacement,
                selectedCharacterId,
                selectedConversationId,
                (index) => selectedCharID.set(index),
            )
            if (presetOverride) carryPresetOverride(presetOverride, options.readPreset)
        },
        publishPresetWorkingSet({ revision, root, presets }) {
            const database = getDatabase()
            const scalable = isCatalogPresetWorkingSet(database.botPresets)
            const loadedIds = new Set(database.botPresets.filter((value) => Object.keys(value).some((key) => !['id','name','image'].includes(key))).map((value) => value['id']))
            if (!scalable) {
                database.botPresets = presets
                patchWorkingSetRoot(database, root)
                derivePersistentIdentityMirrors()
                return
            }
            database.botPresets = createPresetCatalogWorkingSetFromValues(
                presets,
                revision,
                root.botPresetsId,
            )
            for (let index = 0; index < database.botPresets.length; index++) {
                const id = database.botPresets[index]?.['id']
                if (loadedIds.has(id)) database.botPresets[index] = canonicalClone(presets.find((value) => value['id'] === id)!)
            }
            patchWorkingSetRoot(database, root)
            derivePersistentIdentityMirrors()
        },
        publishRootWorkingSet(root) {
            patchWorkingSetRoot(getDatabase(), root)
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
            for (const chat of character.chats) restoreStreamingConversationState(chat)
            database.characters[index] = carryCatalogCharacterMetadata(database.characters[index], character)
            selectedCharID.set(index)
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
            for (const chat of character.chats) restoreStreamingConversationState(chat)
            character.chatPage = conversationIndex
            database.characters[characterIndex] = carryCatalogCharacterMetadata(database.characters[characterIndex], character)
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
        onPluginDeviceStorageChanged(owner) {
            invalidatePluginDeviceKeyspaces(owner)
        },
        getGeneratingConversations: () => generatingConversations.snapshot(),
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
            return !character?.chats.some((chat) => chat.isStreaming || isConversationStreaming(chat.id))
        },
        releaseInactiveCharacter(id) {
            const database = getDatabase()
            if (!workingSetResidency.canReleaseCharacterToCatalog(database, id)) return
            // A released character has no chats loaded, so views of the
            // selection must leave it in the same update that releases it.
            if (database.characters[get(selectedCharID)]?.chaId === id) selectedCharID.set(-1)
            workingSetResidency.releaseCharacterToCatalog(database, id)
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
const localSaveFailure = writable<unknown | null>(null)
export const persistentLocalSaveFailure = readonly(localSaveFailure)
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
        const store = getPersistentDataStore()
        productionRuntime = createPersistentDataRuntime({
            store,
            state: createProductionStateAdapter({ readPreset: (id) => store.readPreset(id) }),
            getOfficialPublisher: () => productionConfiguration.officialPublisher,
            onLocalRevision: (revision) => {
                productionConfiguration.onLocalRevision?.(revision)
                notifyLocalPersistentRevision(revision)
            },
            onFlushPromise: (promise) => productionConfiguration.onFlushPromise?.(promise),
            onBackgroundError: (error) => productionConfiguration.onBackgroundError?.(error),
            onLocalSaveFailure: (error) => localSaveFailure.set(error),
            onWorkingSetRefreshRequired: (revision) => workingSetRefreshRevision.set(revision),
            onDestructiveReplacementFenceChanged: (active) => destructiveReplacementActive.set(active),
            prepareDatabase: prepareDatabaseForPersistence,
            prepareRoot: (root) => preparePersistentRootForWorkingSet(root),
            deletedCharacterAssets: {
                collectConversationKeys: collectCharacterConversationAssetKeys,
                cleanup: (keys, startedAtMs) =>
                    scheduleDeletedOwnerAssetCleanup(keys, startedAtMs, 'deleted-owner-assets', assetAliasCleanupTarget),
            },
        })
    }
    return productionRuntime
}

/** The production save queue and write gate, for an asset alias cleanup. */
export const assetAliasCleanupTarget = (): AssetAliasCleanupTarget => ({
    runtime: getPersistentDataRuntime(),
    gate: getPersistentStorageAuthority().gate,
})

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
export const flushPendingDataLocallyForCheckpoint = (reason: string): Promise<boolean> =>
    getPersistentDataRuntime().flushPendingDataLocallyForCheckpoint(reason)
export const capturesImmutableCharacters = (): boolean =>
    getPersistentDataRuntime().capturesImmutableCharacters()
export const pendingConversationMutationDiagnostics = (): { pendingEvents: number; unacknowledgedMs?: number } =>
    getPersistentDataRuntime().pendingConversationMutationDiagnostics()
export const acknowledgeGenerationCompletion = async (expectedAuthorityEpoch?: number): Promise<void> => {
    const runtime = getPersistentDataRuntime()
    const authorityEpoch = expectedAuthorityEpoch ?? runtime.getStorageAuthorityEpoch()
    await runtime.acknowledgeGenerationCompletion(authorityEpoch)
    runtime.assertPersistentMutationAllowed(authorityEpoch)
    notifyLocalPersistentRevision(runtime.revision, 'generation-complete')
}
export const drainDeferredLwwReceives = async (expectedAuthorityEpoch?: number): Promise<void> => {
    const runtime = getPersistentDataRuntime()
    const authorityEpoch = expectedAuthorityEpoch ?? runtime.getStorageAuthorityEpoch()
    await runtime.drainLwwDeferred(authorityEpoch)
    runtime.assertPersistentMutationAllowed(authorityEpoch)
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
export const recordSelectedCharacterLastInteraction = (
    authority: WindowedConversationPersistenceAuthority,
    before: number | undefined,
    after: number,
): boolean => getPersistentDataRuntime().recordSelectedCharacterLastInteraction(authority, before, after)
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
export const editWindowedChatList = (
    target: SelectedConversationTarget,
    edit: (character: character) => string | null | false,
): WindowedChatListEditResult =>
    getPersistentDataRuntime().editWindowedChatList(target, edit)
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
    check?: PluginStorageWriteCheck,
): Promise<void> => getPersistentDataRuntime().mutatePersistentPluginStorage(reason, mutations, check)
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
/**
 * Edits fields of a character's own record (outside its chats) without reading any of its
 * conversations, then writes the stored fields to the in-memory character, except fields edited
 * in memory meanwhile. Resolves false when the store holds no such character.
 */
export const mutateCharacterDetailFields = (
    characterId: string,
    reason: string,
    mutate: CharacterDetailFieldsMutation,
): Promise<boolean> => getPersistentDataRuntime().mutateCharacterDetailFields(
    characterId,
    reason,
    mutate,
    () => {
        const current = getDatabase().characters.find((value) => value.chaId === characterId)
        // A stub holds only catalog fields; it takes the rest from the store when it is loaded.
        return current && !isWorkingSetCharacterStub(current) ? current : undefined
    },
)
export const deletePersistentCharacter = (
    characterId: string,
    reason: string,
): Promise<boolean> => getPersistentDataRuntime().deletePersistentCharacter(
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

export const getPersistentRevision = (): DataRevision => getPersistentDataRuntime().revision
export const applyPersistentLwwReceive = (request: LwwStageReceive): Promise<LwwApplyResult> => getPersistentDataRuntime().applyLwwReceive(request)
export const commitPersistentUnitIntent: PersistentDataRuntime['commitPersistentUnitIntent'] = (reason, mutations, conversations, wholeMessages, projectionObserver, check) =>
    getPersistentDataRuntime().commitPersistentUnitIntent(reason, mutations, conversations, wholeMessages, projectionObserver, check)
export const commitPreparedUnitIntent: PersistentDataRuntime['commitPreparedUnitIntent'] = (reason, prepare) =>
    getPersistentDataRuntime().commitPreparedUnitIntent(reason, prepare)
export const withPausedPersistentWrites = <T>(reason: string, operation: (token: PersistentMutationToken) => Promise<T>): Promise<T> => getPersistentDataRuntime().withPausedPersistentWrites(reason, operation)

export const beginActivatedLibraryGuard = (token: PersistentMutationToken) => getPersistentDataRuntime().beginActivatedLibraryGuard(token)

export const refreshActivatedLibraryUnderPause = (token: import("./saveCoordinator").PersistentMutationToken) => getPersistentDataRuntime().refreshActivatedLibraryUnderPause(token)
