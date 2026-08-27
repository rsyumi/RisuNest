import { get } from 'svelte/store'
import { doingChat } from '../process/generationState'
import { ReloadGUIPointer, selectedCharID } from '../stores.svelte'
import type { ActiveConversationSession } from './activeConversationSession'
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
    PersistentDatabaseSnapshot,
    PersistentMutationToken,
    PersistentSelectedConversation,
} from './saveCoordinator'
import type { CharacterActivationOptions } from './activeWorkingSet.svelte'
import {
    capturePersistentRoot,
    capturePersistentPluginStorage,
    capturePersistentPresets,
    captureResidentPersistentCharacter,
    captureSelectedPersistentCharacter,
    createPersistentDataRuntime,
    publishPersistentCharacterMutationToWorkingSet,
    restoreStableWorkingSetSelection,
    type PersistentDestructiveReplacementFence,
    type PersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
import type { OfficialRevisionPublisher } from './saveCoordinator'
import type { PersistentPresetMutation } from './saveCoordinator'
import type { PersistentReplacementOptions } from './saveCoordinator'
import { workingSetResidency } from './workingSetResidency'
import {
    createCatalogPresetWorkingSet,
    hydrateWorkingSetCharacterDetail,
    isCatalogPresetWorkingSet,
} from './workingSetCatalog'
import { notifyPluginStorageAuthorityReplacement } from '../plugins/pluginStorageStore'
import {
    readPinnedSelectedConversationWindow,
    type PersistentSelectedConversationWindow,
} from './persistentConversationRead'

export type {
    PersistentDestructiveReplacementFence,
    PersistentDataRuntime,
    PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
export { createPersistentDataRuntime } from './persistentDataRuntime'

type CompleteCharacter = character | groupChat

export function createProductionStateAdapter(): PersistentDataRuntimeStateAdapter {
    return {
        captureRoot() {
            return capturePersistentRoot(getDatabase())
        },
        capturePluginStorage() {
            return capturePersistentPluginStorage(getDatabase())
        },
        publishPluginStorageWorkingSet(storage) {
            getDatabase().pluginCustomStorage = storage
            notifyPluginStorageAuthorityReplacement(storage)
        },
        capturePresets() {
            return capturePersistentPresets(getDatabase())
        },
        captureSelectedCharacter(): CompleteCharacter | null {
            const database = getDatabase()
            const selected = captureSelectedPersistentCharacter(database, get(selectedCharID))
            return selected
                ? captureResidentPersistentCharacter(database, selected.chaId)
                : null
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
            notifyPluginStorageAuthorityReplacement(
                forceScalableProjection || isCatalogPresetWorkingSet(replacement.botPresets)
                    ? null
                    : replacement.pluginCustomStorage ?? {},
            )
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
            const catalog = {
                revision,
                items: presets.map((preset, configuredIndex) => ({
                    id: String(configuredIndex),
                    configuredIndex,
                    name: preset.name ?? '',
                    image: preset.image,
                })),
            }
            const activeSummary = catalog.items.find(
                (summary) => summary.configuredIndex === root.botPresetsId,
            )
            database.botPresets = createCatalogPresetWorkingSet(
                catalog,
                activeSummary ? {
                    summary: activeSummary,
                    value: presets[activeSummary.configuredIndex],
                } : null,
            )
        },
        publishRootWorkingSet(root) {
            Object.assign(getDatabase(), root)
        },
        publishCharacterMutation(state) {
            const database = getDatabase()
            publishPersistentCharacterMutationToWorkingSet(
                database,
                state,
                workingSetResidency,
                get(selectedCharID),
                (index) => selectedCharID.set(index),
            )
            const selectedCharacterId = database.characters[get(selectedCharID)]?.chaId ?? null
            productionRuntime?.reconcileActiveCharacterIds(database, selectedCharacterId)
        },
        installCompleteDatabase(database) {
            workingSetResidency.clear()
            setDatabase(database)
            notifyPluginStorageAuthorityReplacement(database.pluginCustomStorage ?? {})
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
        publishConversation(characterId, conversation: Chat, nextCharacter?: CompleteCharacter) {
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
            ReloadGUIPointer.set(Math.random())
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
        releaseInactiveCharacters(selectedId, activeIds) {
            const database = getDatabase()
            for (const character of [...database.characters]) {
                if (character.chaId !== selectedId && !activeIds?.has(character.chaId)) {
                    workingSetResidency.releaseCharacterToCatalog(database, character.chaId)
                }
            }
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
            onLocalRevision: (revision) => productionConfiguration.onLocalRevision?.(revision),
            onFlushPromise: (promise) => productionConfiguration.onFlushPromise?.(promise),
            onBackgroundError: (error) => productionConfiguration.onBackgroundError?.(error),
            prepareDatabase: prepareDatabaseForPersistence,
        })
    }
    return productionRuntime
}

export const initializeActiveWorkingSet = (database: Database): Promise<void> =>
    getPersistentDataRuntime().initializeActiveWorkingSet(database)
export const refreshActiveWorkingSetFromStore = (revision: DataRevision): Promise<void> =>
    getPersistentDataRuntime().refreshActiveWorkingSetFromStore(revision)
export const markPersistentDataDirty = (estimatedBytes: number): void =>
    getPersistentDataRuntime().markPersistentDataDirty(estimatedBytes)
export const flushPendingData = (reason: string): Promise<void> =>
    getPersistentDataRuntime().flushPendingData(reason)
export const acknowledgeGenerationCompletion = (): Promise<void> =>
    getPersistentDataRuntime().acknowledgeGenerationCompletion()
export const commitCharacterAddition = (
    request: CharacterAdditionRequest,
    reason: string,
): Promise<void> => getPersistentDataRuntime().commitCharacterAddition(request, reason)
export const activateCharacter = (
    id: string,
    options?: CharacterActivationOptions,
): Promise<boolean> => getPersistentDataRuntime().activateCharacter(id, options)
export const activateConversation = (id: string): Promise<boolean> =>
    getPersistentDataRuntime().activateConversation(id)
export const getActiveConversationSession = (): ActiveConversationSession | null =>
    getPersistentDataRuntime().getActiveConversationSession()
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
export const invalidatePersistentNavigation = (): void =>
    getPersistentDataRuntime().invalidateNavigation()
export const replacePersistentDatabase = (
    database: Database,
    reason: string,
    options?: PersistentReplacementOptions,
): Promise<void> => getPersistentDataRuntime().replacePersistentDatabase(database, reason, options)
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
): Promise<boolean> => getPersistentDataRuntime().replacePersistentCompleteCharacter(
    characterId,
    reason,
    mutate,
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
): Promise<PersistentMutationToken> =>
    getPersistentDataRuntime().capturePersistentMutationToken(reason)
export const acquireDestructiveReplacementFence = (
    expected: PersistentMutationToken,
): Promise<PersistentDestructiveReplacementFence> =>
    getPersistentDataRuntime().acquireDestructiveReplacementFence(expected)
export const materializePersistentDatabaseSnapshot = (reason: string): Promise<Database> =>
    getPersistentDataRuntime().materializePersistentDatabaseSnapshot(reason)
export const materializePersistentDatabaseSnapshotWithRevision = (
    reason: string,
): Promise<PersistentDatabaseSnapshot> =>
    getPersistentDataRuntime().materializePersistentDatabaseSnapshotWithRevision(reason)

export const materializeMaximumCompatibilityWorkingSet = (): Promise<void> =>
    getPersistentDataRuntime().materializeMaximumCompatibilityWorkingSet()

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
