import { get } from 'svelte/store'
import { doingChat } from '../process/generationState'
import { ReloadGUIPointer, selectedCharID } from '../stores.svelte'
import type { Chat, Database, character, groupChat } from './database.svelte'
import { getDatabase, setDatabase } from './database.svelte'
import { prepareDatabaseForPersistence } from './databasePreparation'
import { getPersistentDataStore, getPersistentStorageAuthority } from './persistentDataStoreFactory'
import type { CharacterDetail, DataRevision } from './persistentDataStore'
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
    capturePersistentPresets,
    captureResidentPersistentCharacter,
    captureSelectedPersistentCharacter,
    createPersistentDataRuntime,
    publishPersistentCharacterMutationToWorkingSet,
    restoreStableWorkingSetSelection,
    type PersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
import type { OfficialRevisionPublisher } from './saveCoordinator'
import type { PersistentPresetMutation } from './saveCoordinator'
import type { PersistentReplacementOptions } from './saveCoordinator'
import { workingSetResidency } from './workingSetResidency'
import {
    createCatalogPresetWorkingSet,
    isCatalogPresetWorkingSet,
} from './workingSetCatalog'

export type {
    PersistentDataRuntime,
    PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'
export { createPersistentDataRuntime } from './persistentDataRuntime'

type CompleteCharacter = character | groupChat

function productionStateAdapter(): PersistentDataRuntimeStateAdapter {
    return {
        captureRoot() {
            return capturePersistentRoot(getDatabase())
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
                const character = related[index]
                database.characters[relatedIndices[index]] = character
                workingSetResidency.markCharacterHydrated(character.chaId)
            }
            workingSetResidency.markCharacterHydrated(primary.chaId)
            database.characters[primaryIndex] = primary
            selectedCharID.set(primaryIndex)
        },
        publishConversation(characterId, conversation: Chat) {
            const database = getDatabase()
            const characterIndex = database.characters.findIndex(
                (candidate) => candidate.chaId === characterId,
            )
            if (characterIndex < 0) return
            const character = database.characters[characterIndex]
            const conversationIndex = character.chats.findIndex(
                (candidate) => candidate.id === conversation.id,
            )
            if (conversationIndex < 0) return
            character.chats[conversationIndex] = conversation
            character.chatPage = conversationIndex
            selectedCharID.set(characterIndex)
            ReloadGUIPointer.set(Math.random())
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
            state: productionStateAdapter(),
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
export const markPersistentDataDirty = (estimatedBytes: number): void =>
    getPersistentDataRuntime().markPersistentDataDirty(estimatedBytes)
export const flushPendingData = (reason: string): Promise<void> =>
    getPersistentDataRuntime().flushPendingData(reason)
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
export const mutatePersistentPresets = (
    reason: string,
    mutate: PersistentPresetMutation,
): Promise<void> => getPersistentDataRuntime().mutatePersistentPresets(reason, mutate)
export const mutatePersistentCharacterDetail = (
    characterId: string,
    reason: string,
    mutate: PersistentCharacterDetailMutation,
): Promise<boolean> => getPersistentDataRuntime().mutatePersistentCharacterDetail(
    characterId,
    reason,
    mutate,
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
export const capturePersistentMutationToken = (
    reason: string,
): Promise<PersistentMutationToken> =>
    getPersistentDataRuntime().capturePersistentMutationToken(reason)
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
