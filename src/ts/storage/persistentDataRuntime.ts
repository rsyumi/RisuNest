import type { PersistenceCanonicalCapture } from './reactivePersistenceCapture.svelte'
import type { Chat, Database, botPreset, character, groupChat } from './database.svelte'
import { removeGroupMemberReferences } from './groupMembership'
import {
    ActiveWorkingSet,
    type ActiveConversationViewportSourceListener,
    type CharacterActivationOptions,
    type ConversationPublicationOptions,
    type CompleteConversationLease,
    type SelectedConversationTarget,
} from './activeWorkingSet.svelte'
import type { ActiveConversationSession } from './activeConversationSession'
import type { ConversationViewportSource } from '../conversationViewportSource'
import type {
    CharacterDetail,
    DataRevision,
    PersistentDataStore,
    PersistentRevisionLease,
    PersistentRoot,
    PluginStorageMutation,
} from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'
import {
    SaveCoordinator,
    PersistentMutationFencedError,
    type CharacterAdditionRequest,
    type DestructiveReplacementFenceOptions,
    type OfficialRevisionPublisher,
    type PersistentPresetMutation,
    type PersistentPresetMutationResult,
    type PersistentCharacterDetailMutation,
    type PersistentCharacterMutationResult,
    type PersistentDatabaseSnapshot,
    type PersistentMutationToken,
    type PersistentSelectedConversation,
    type PersistentCompleteCharacterMutation,
    type PersistentCompleteCharacterUpsert,
    type PersistentCompleteCharacterUpsertOptions,
    type PersistentConversationReplacementResult,
    type PersistentReplacementOptions,
    type PersistentScopedReplacementOptions,
    type SaveCoordinatorClock,
    type WindowedConversationPersistenceAuthority,
} from './saveCoordinator'
import {
    type WorkingSetResidencyRegistry,
    workingSetResidency,
} from './workingSetResidency'
import {
    createCatalogCharacterStub,
    getCatalogCharacterMetadata,
    hasIncompletePersistentWorkingSet,
    isCatalogCharacterStub,
    isWorkingSetCharacterStub,
    isCatalogPresetWorkingSet,
    patchWorkingSetCharacterDetail,
    projectPinnedScalableWorkingSet,
    projectScalableWorkingSetAtRevision,
} from './workingSetCatalog'
import {
    applyTargetedWorkingSetInvalidation,
    readWorkingSetChangeWindow,
} from './targetedWorkingSetInvalidation'
import {
    assertPinnedRevision,
    releasePersistentRevisionLease,
} from './persistentRecordIterator'
import {
    createConversationSummaryStubFromChat,
    isConversationSummaryStub,
} from './conversationResidency'
import { isMetadataOnlySelectedConversation } from './selectedConversationLifecycle'

type CompleteCharacter = character | groupChat
type RootDatabase = PersistentRoot

export function capturePersistentRoot(database: Database): RootDatabase {
    const {
        characters: _characters,
        botPresets: _botPresets,
        pluginCustomStorage: _pluginCustomStorage,
        ...root
    } = database
    return root
}

export function capturePersistentPluginStorage(
    database: Database,
): Database['pluginCustomStorage'] | null {
    if (isCatalogPresetWorkingSet(database.botPresets)) return null
    return Object.prototype.hasOwnProperty.call(database, 'pluginCustomStorage')
        ? database.pluginCustomStorage ?? {}
        : null
}

export function capturePersistentPresets(database: Database): botPreset[] | null {
    const presets = database.botPresets ?? []
    if (isCatalogPresetWorkingSet(presets)) return null
    return presets
}

export function publishPersistentConversationReplacementToWorkingSet(
    database: Database,
    result: PersistentConversationReplacementResult,
): void {
    const character = database.characters.find(
        (candidate) => candidate.chaId === result.characterId,
    )
    if (!character || isWorkingSetCharacterStub(character)) return
    const index = character.chats.findIndex(
        (candidate) => candidate.id === result.conversationId,
    )
    if (index < 0) return
    // A metadata-only selected-conversation shell is owned by the windowed
    // authority; swapping it for a full clone would defeat eviction and
    // desynchronize the windowed baseline. Rehydration picks up the durable
    // replacement instead.
    if (isMetadataOnlySelectedConversation(character.chats[index])) return
    character.chats[index] = isConversationSummaryStub(character.chats[index])
        ? createConversationSummaryStubFromChat(
            result.characterId,
            result.conversation,
            index,
        )
        : structuredClone(result.conversation)
}

export function captureSelectedPersistentCharacter(
    database: Database,
    selectedIndex: number,
): CompleteCharacter | null {
    return database.characters[selectedIndex] ?? null
}

export function restoreStableWorkingSetSelection(
    database: Database,
    characterId: string | null,
    conversationId: string | null,
    selectCharacterIndex: (index: number) => void,
): void {
    if (!characterId) {
        selectCharacterIndex(-1)
        return
    }
    const characterIndex = database.characters.findIndex(
        (candidate) => candidate.chaId === characterId,
    )
    if (characterIndex < 0) {
        selectCharacterIndex(-1)
        return
    }
    const character = database.characters[characterIndex]
    if (conversationId) {
        const conversationIndex = character.chats.findIndex(
            (candidate) => candidate.id === conversationId,
        )
        if (conversationIndex >= 0) character.chatPage = conversationIndex
    }
    selectCharacterIndex(characterIndex)
}

export function captureResidentPersistentCharacter(
    database: Database,
    id: string,
    residency: WorkingSetResidencyRegistry = workingSetResidency,
): CompleteCharacter | null {
    if (residency.isCharacterReleased(id)) return null
    const character = database.characters.find((candidate) => candidate.chaId === id) ?? null
    if (character && isWorkingSetCharacterStub(character)) return null
    return character
}

export function publishPersistentCharacterMutationToWorkingSet(
    database: Database,
    state: PersistentCharacterMutationResult,
    residency: WorkingSetResidencyRegistry,
    selectedIndex: number,
    selectCharacterIndex: (index: number) => void,
): void {
    Object.assign(database, state.root)
    for (const detail of state.relatedCharacters ?? []) {
        const relatedIndex = database.characters.findIndex(
            (candidate) => candidate.chaId === detail.chaId,
        )
        if (relatedIndex >= 0) {
            patchWorkingSetCharacterDetail(database.characters[relatedIndex], detail)
        }
    }
    const index = database.characters.findIndex(
        (candidate) => candidate.chaId === state.characterId,
    )
    if (state.kind === 'delete') {
        const selected = database.characters[selectedIndex]
        if (
            selected?.type === 'group' &&
            Array.isArray(selected.characters) &&
            selected.chaId !== state.characterId
        ) {
            const retained = removeGroupMemberReferences(
                selected,
                new Set([state.characterId]),
            )
            selected.characters = retained.characters
            selected.characterTalks = retained.characterTalks
            selected.characterActive = retained.characterActive
        }
        if (index >= 0) database.characters.splice(index, 1)
        residency.forgetCharacter(state.characterId)
        if (selectedIndex === index) selectCharacterIndex(-1)
        else if (index >= 0 && selectedIndex > index) selectCharacterIndex(selectedIndex - 1)
        return
    }
    if (!state.character) return
    if (state.kind === 'detail') {
        if (index >= 0) patchWorkingSetCharacterDetail(database.characters[index], state.character)
        return
    }

    const complete = state.character as CompleteCharacter
    const keepBounded = residency.allowsEviction && (
        state.kind === 'add' ||
        index < 0 ||
        isCatalogCharacterStub(database.characters[index]) ||
        residency.isCharacterReleased(state.characterId)
    )
    if (keepBounded) {
        const configuredIndex = index >= 0
            ? getCatalogCharacterMetadata(database.characters[index])?.configuredIndex ?? index
            : database.characters.length
        const stub = createCatalogCharacterStub({
            id: complete.chaId,
            name: complete.name,
            image: complete.image,
            configuredIndex,
            recentAt: complete.lastInteraction ?? 0,
            trashed: complete.trashTime !== undefined,
            conversationCount: complete.chats.length,
            type: complete.type,
            creatorNotes: complete.creatorNotes,
            trashTime: complete.trashTime,
        })
        if (index < 0) database.characters.push(stub)
        else database.characters[index] = stub
        residency.markCharacterReleased(state.characterId)
        return
    }
    if (index < 0) database.characters.push(complete)
    else database.characters[index] = complete
    residency.markCharacterHydrated(state.characterId)
}

export interface PersistentDataRuntimeStateAdapter {
    canonicalCapture?: PersistenceCanonicalCapture
    captureRoot(): RootDatabase
    capturePluginStorage?(): Database['pluginCustomStorage'] | null
    publishPluginStorageWorkingSet?(storage: Database['pluginCustomStorage']): void
    publishPluginStorageMutations?(
        mutations: readonly PluginStorageMutation[],
        keys: readonly string[],
    ): void
    capturePresets?(): botPreset[] | null
    captureSelectedCharacter(): CompleteCharacter | null
    captureCharacter(id: string): CompleteCharacter | null
    getSelectedCharacterId(): string | null | undefined
    getSelectedConversationId?(): string | null | undefined
    replaceDatabase(
        database: Database,
        activeCharacterIds?: ReadonlySet<string>,
        forceScalableProjection?: boolean,
    ): void
    publishPresetWorkingSet?(state: PersistentPresetMutationResult): void
    publishRootWorkingSet?(root: RootDatabase): void
    publishCharacterMutation?(state: PersistentCharacterMutationResult): void
    publishConversationReplacement?(result: PersistentConversationReplacementResult): void
    installCompleteDatabase?(database: Database): void
    restoreSelection?(characterId: string | null, conversationId: string | null): void
    publishCharacter(character: CompleteCharacter): void
    publishCharacterSet?(primary: CompleteCharacter, related: CharacterDetail[]): void
    publishConversation(
        characterId: string,
        conversation: Chat,
        nextCharacter?: CompleteCharacter,
        options?: ConversationPublicationOptions,
    ): void
    captureActivationRollback?(characterIds: readonly string[]): () => void
    shouldHydrateFullCharacter?(): boolean
    canReleaseConversation?(
        character: CompleteCharacter,
        conversationId: string,
        nextConversationId: string,
    ): boolean
    canUseWindowedSelectedConversation?(): boolean
    isConversationOperationActive?(): boolean
    subscribeConversationOperationActive?(listener: (active: boolean) => void): () => void
    /** The working set a targeted pass patches in place of a full reprojection. */
    captureWorkingSetDatabase?(): Database | null
    /** Host caches outside the working set that a remote change invalidates. */
    onPluginStorageChanged?(owner: string, key: string): void
    getGeneratingConversation?(): { characterId: string; conversationId: string } | null
    conversationViewportRowBudget?: number
    canActivateWorkingSet?(): boolean
    canDeactivateWorkingSet?(): boolean
    canDeactivateCharacter?(id: string): boolean
    releaseInactiveCharacter?(id: string): void
}

export interface PersistentDataRuntimeDependencies {
    store: PersistentDataStore
    state: PersistentDataRuntimeStateAdapter
    officialPublisher?: OfficialRevisionPublisher | null
    getOfficialPublisher?(): OfficialRevisionPublisher | null
    clock?: SaveCoordinatorClock
    now?(): number
    onLocalRevision?(revision: DataRevision): void
    onFlushPromise?(promise: Promise<void> | null): void
    onBackgroundError?(error: unknown): void
    /** Detaches the input synchronously before any asynchronous preparation. */
    prepareDatabase(database: Database): Promise<Database>
}

export interface PersistentDataRuntime {
    readonly store: PersistentDataStore
    readonly revision: DataRevision
    getStorageAuthorityEpoch(): number
    initializeActiveWorkingSet(database: Database): Promise<void>
    refreshActiveWorkingSetFromStore(revision: DataRevision): Promise<void>
    runStorageOnlyMutation(
        operation: (expectedRevision: DataRevision) => Promise<DataRevision>,
    ): Promise<void>
    markPersistentDataDirty(estimatedBytes: number): void
    flushPendingData(reason: string): Promise<void>
    flushPendingDataLocally(reason: string): Promise<void>
    acknowledgeGenerationCompletion(): Promise<void>
    commitCharacterAddition(
        request: CharacterAdditionRequest,
        reason: string,
    ): Promise<void>
    activateCharacter(
        id: string,
        options?: CharacterActivationOptions,
    ): Promise<boolean>
    activateConversation(id: string): Promise<boolean>
    getActiveConversationSession(): ActiveConversationSession | null
    getSelectedConversationMode(): 'complete' | 'windowed' | null
    getActiveConversationViewportSource(): ConversationViewportSource | null
    subscribeActiveConversationViewportSource(
        listener: ActiveConversationViewportSourceListener,
    ): () => void
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    captureSelectedConversationAuthority(): WindowedConversationPersistenceAuthority | null
    acquireCompleteConversation(
        reason: string,
        target?: SelectedConversationTarget | null,
    ): Promise<CompleteConversationLease>
    tryDemoteSelectedConversation(
        target?: SelectedConversationTarget | null,
    ): boolean
    refreshSelectedConversationAfterReplacement(
        target: SelectedConversationTarget,
        expectedSession: ActiveConversationSession,
    ): boolean
    invalidateActiveConversationSession(): void
    deactivateActiveWorkingSet(): Promise<boolean>
    reconcileActiveCharacterIds(
        database: Database,
        selectedCharacterId: string | null,
    ): ReadonlySet<string>
    getNavigationGeneration(): number
    fenceNavigation(): number
    invalidateNavigation(): void
    replacePersistentDatabase(
        database: Database,
        reason: string,
        options?: PersistentReplacementOptions,
    ): Promise<void>
    mutatePersistentPluginStorage(
        reason: string,
        mutations: readonly PluginStorageMutation[],
    ): Promise<void>
    mutatePersistentPresets(
        reason: string,
        mutate: PersistentPresetMutation,
    ): Promise<void>
    appendPersistentRootModule(
        reason: string,
        input: import('./saveCoordinator').PersistentRootModuleAppend,
        signal?: AbortSignal,
    ): Promise<void>
    mutateConversationBinding(
        characterId: string,
        conversationId: string,
        patch: import('./conversationBinding').ConversationBindingPatch,
        publish: (committedPatch: import('./conversationBinding').ConversationBindingPatch) => void,
    ): Promise<void>
    mutatePersistentCharacterDetail(
        characterId: string,
        reason: string,
        mutate: PersistentCharacterDetailMutation,
    ): Promise<boolean>
    deletePersistentCharacterWithGroupReferences(
        characterId: string,
        reason: string,
    ): Promise<boolean>
    replacePersistentCompleteCharacter(
        characterId: string,
        reason: string,
        mutate: PersistentCompleteCharacterMutation,
        options?: PersistentScopedReplacementOptions,
    ): Promise<boolean>
    replacePersistentConversation(
        characterId: string,
        conversationId: string,
        reason: string,
        replacement: Chat,
        options?: PersistentScopedReplacementOptions,
    ): Promise<boolean>
    upsertPersistentCompleteCharacter(
        characterId: string,
        reason: string,
        createOrMutate: PersistentCompleteCharacterUpsert,
        options?: PersistentCompleteCharacterUpsertOptions,
    ): Promise<boolean>
    readPersistentCharacterDetail(
        characterId: string,
        reason: string,
    ): Promise<CharacterDetail | null>
    readPersistentCompleteCharacter(
        characterId: string,
        reason: string,
    ): Promise<CompleteCharacter | null>
    readPersistentConversation(
        characterId: string,
        conversationId: string,
        reason: string,
    ): Promise<Chat | null>
    readPersistentConversationAt(
        characterId: string,
        orderedPosition: number,
        reason: string,
    ): Promise<Chat | null>
    readPersistentSelectedConversation(
        characterId: string,
        reason: string,
    ): Promise<PersistentSelectedConversation | null>
    capturePersistentMutationToken(
        reason: string,
        options?: { publishOfficial?: boolean },
    ): Promise<PersistentMutationToken>
    acquireDestructiveReplacementFence(
        expected: PersistentMutationToken,
        options?: DestructiveReplacementFenceOptions,
    ): Promise<PersistentDestructiveReplacementFence>
    acquireCommittedWorkingSetRefreshFence(): Promise<PersistentDestructiveReplacementFence>
    materializePersistentDatabaseSnapshot(reason: string): Promise<Database>
    materializePersistentDatabaseSnapshotWithRevision(
        reason: string,
    ): Promise<PersistentDatabaseSnapshot>
    releaseInactiveWorkingSet(
        canRelease?: () => boolean | Promise<boolean>,
        isCurrent?: () => boolean,
    ): Promise<boolean>
    publishCurrentOfficialRevision(): Promise<void>
    hasPendingOfficialPublication(): boolean
}

export interface PersistentDestructiveReplacementFence {
    /** Revision the working set is pinned to while the fence is held. */
    readonly revision: DataRevision
    refreshCommittedWorkingSet(
        revision: DataRevision,
        options?: PersistentCommittedWorkingSetRefreshOptions,
    ): Promise<void>
    release(): void
}

export interface PersistentCommittedWorkingSetRefreshOptions {
    forceScalableProjection?: boolean
}

function createDynamicOfficialPublisher(
    getPublisher: () => OfficialRevisionPublisher | null,
): OfficialRevisionPublisher {
    return {
        async pin(revision) {
            const publisher = getPublisher()
            if (!publisher) {
                return {
                    publish: async () => undefined,
                    dispose: async () => undefined,
                }
            }
            return publisher.pin(revision)
        },
    }
}

export function createPersistentDataRuntime(
    dependencies: PersistentDataRuntimeDependencies,
): PersistentDataRuntime {
    let workingSet: ActiveWorkingSet
    const coordinator = new SaveCoordinator({
        canonicalCapture: dependencies.state.canonicalCapture,
        store: dependencies.store,
        captureRoot: dependencies.state.captureRoot,
        capturePluginStorage: dependencies.state.capturePluginStorage,
        publishPluginStorageWorkingSet: dependencies.state.publishPluginStorageWorkingSet,
        publishPluginStorageMutations: dependencies.state.publishPluginStorageMutations,
        capturePresets: dependencies.state.capturePresets,
        captureSelectedCharacter: dependencies.state.captureSelectedCharacter,
        captureSelectedConversationAuthority: () =>
            workingSet.captureSelectedConversationAuthority(),
        captureCharacter: dependencies.state.captureCharacter,
        replaceDatabase: (database) => {
            workingSet.invalidateActiveConversationSession()
            const activeCharacterIds = workingSet.reconcileActiveCharacterIds(
                database,
                dependencies.state.getSelectedCharacterId() ?? null,
            )
            dependencies.state.replaceDatabase(database, activeCharacterIds)
        },
        publishPresetWorkingSet: dependencies.state.publishPresetWorkingSet,
        publishRootWorkingSet: dependencies.state.publishRootWorkingSet,
        publishCharacterMutation: dependencies.state.publishCharacterMutation,
        publishConversationReplacement: dependencies.state.publishConversationReplacement,
        isIncompleteWorkingSet: (database) =>
            hasIncompletePersistentWorkingSet(database, workingSetResidency),
        getNavigationGeneration: () => workingSet.navigationGenerationToken,
        officialPublisher: dependencies.officialPublisher || dependencies.getOfficialPublisher
            ? createDynamicOfficialPublisher(
                dependencies.getOfficialPublisher
                    ?? (() => dependencies.officialPublisher ?? null),
            )
            : undefined,
        clock: dependencies.clock,
        now: dependencies.now,
        onLocalRevision: (revision) => {
            workingSet.advanceStoreRevision(revision)
            dependencies.onLocalRevision?.(revision)
        },
        onStorageOnlyRevision: (revision) => workingSet.advanceStoreRevision(revision),
        onWindowedSelectedConversationRevision: (revision) =>
            workingSet.advanceStoreRevision(revision),
        onConversationMutationPersistenceStarted: (event) =>
            workingSet.beginConversationMutationPersistence(event),
        onConversationMutationPersisted: (event) => {
            workingSet.acknowledgeConversationMutationPersisted(event)
        },
        onConversationMutationFallbackPersisted: (event) => {
            workingSet.acknowledgeConversationMutationFallbackPersisted(event)
        },
        onPersistenceIdle: () => workingSet.scheduleSelectedConversationDemotion(),
        onFlushPromise: dependencies.onFlushPromise,
        onBackgroundError: dependencies.onBackgroundError,
    })
    workingSet = new ActiveWorkingSet({
        store: dependencies.store,
        coordinator,
        getSelectedCharacterId: dependencies.state.getSelectedCharacterId,
        getResidentCharacter: dependencies.state.captureCharacter,
        publishCharacter: dependencies.state.publishCharacter,
        publishCharacterSet: dependencies.state.publishCharacterSet ?? ((primary, related) => {
            for (const detail of related) {
                const resident = dependencies.state.captureCharacter(detail.chaId)
                if (resident) dependencies.state.publishCharacter({
                    ...detail,
                    chats: resident.chats,
                } as CompleteCharacter)
            }
            dependencies.state.publishCharacter(primary)
        }),
        publishConversation: dependencies.state.publishConversation,
        captureActivationRollback: dependencies.state.captureActivationRollback,
        canActivateWorkingSet: dependencies.state.canActivateWorkingSet,
        canDeactivateWorkingSet: dependencies.state.canDeactivateWorkingSet,
        canDeactivateCharacter: dependencies.state.canDeactivateCharacter,
        releaseInactiveCharacter: dependencies.state.releaseInactiveCharacter,
        shouldHydrateFullCharacter: dependencies.state.shouldHydrateFullCharacter,
        canReleaseConversation: dependencies.state.canReleaseConversation,
        canUseWindowedSelectedConversation:
            dependencies.state.canUseWindowedSelectedConversation,
        isConversationOperationActive: dependencies.state.isConversationOperationActive,
        subscribeConversationOperationActive:
            dependencies.state.subscribeConversationOperationActive,
        conversationViewportRowBudget: dependencies.state.conversationViewportRowBudget,
    })
    const activateCharacter = (
        id: string,
        options?: CharacterActivationOptions,
    ): Promise<boolean> => {
        const prepare = options?.prepare
        const normalize = options?.normalize
        if (!prepare) {
            return workingSet.activateCharacter(
                id,
                normalize ? { normalize } : undefined,
            )
        }
        return workingSet.activateCharacter(id, {
            normalize,
            async prepare() {
                const prepared = await prepare()
                if (!prepared) return null
                return {
                    ...prepared,
                    database: await dependencies.prepareDatabase(prepared.database),
                }
            },
        })
    }
    let deferredContentPending = false
    const commitContentCursor = async (revision: DataRevision): Promise<void> => {
        const commit = dependencies.store.commitWorkingSetChangeCursor
        if (!commit) return
        try {
            await commit.call(dependencies.store, revision)
        } catch (error) {
            // The projection is installed either way; a stale cursor only costs
            // the next window an idempotent replay.
            dependencies.onBackgroundError?.(error)
        }
    }
    /// The change window and every record reprojected for it are read through
    /// one lease, so a targeted pass sees exactly the content it explains.
    const projectRefreshedWorkingSet = async (
        revision: DataRevision,
        pinned: {
            selectedCharacterId: string | null
            selectedConversationId: string | null
            activeCharacterIds: ReadonlySet<string>
        },
    ): Promise<{ database: Database; deferred: boolean }> => {
        const lease = await dependencies.store.acquireRevision(revision)
        let primaryError: unknown
        try {
            assertPinnedRevision(revision, lease.revision, 'Revision lease')
            const previous = dependencies.state.captureWorkingSetDatabase?.() ?? null
            if (previous) {
                try {
                    const keys = await readWorkingSetChangeWindow(lease)
                    if (keys) {
                        const generating =
                            dependencies.state.isConversationOperationActive?.() === true
                                ? dependencies.state.getGeneratingConversation?.() ?? null
                                : null
                        const targeted = await applyTargetedWorkingSetInvalidation(
                            previous,
                            keys,
                            lease,
                            {
                                ...pinned,
                                deferredConversation: generating,
                                onPluginStorageChanged:
                                    dependencies.state.onPluginStorageChanged,
                            },
                        )
                        if (targeted) return targeted
                    }
                } catch (error) {
                    // A failed pass leaves the cursor where it was and the whole
                    // working set is reprojected instead.
                    dependencies.onBackgroundError?.(error)
                }
            }
            return {
                database: await projectPinnedScalableWorkingSet(lease, pinned),
                deferred: false,
            }
        } catch (error) {
            primaryError = error
            throw error
        } finally {
            try {
                await releasePersistentRevisionLease(lease)
            } catch (error) {
                if (primaryError === undefined) throw error
            }
        }
    }
    const refreshCommittedWorkingSet = async (
        revision: DataRevision,
        fenceOwner?: symbol,
        options?: PersistentCommittedWorkingSetRefreshOptions,
    ): Promise<void> => {
        if (
            fenceOwner === undefined &&
            coordinator.hasDestructiveReplacementFence
        ) {
            throw new PersistentMutationFencedError()
        }
        if (fenceOwner !== undefined) {
            coordinator.assertDestructiveReplacementFence(fenceOwner)
        }
        const selectedCharacterId =
            dependencies.state.getSelectedCharacterId() ?? null
        const selectedConversationId =
            dependencies.state.getSelectedConversationId?.() ?? null
        const activeCharacterIds = workingSet.activeCharacterIds
        const projected = await projectRefreshedWorkingSet(revision, {
            selectedCharacterId,
            selectedConversationId,
            activeCharacterIds,
        })
        if (fenceOwner !== undefined) {
            coordinator.assertDestructiveReplacementFence(fenceOwner)
        }
        workingSet.invalidateNavigation()
        dependencies.state.replaceDatabase(
            projected.database,
            activeCharacterIds,
            options?.forceScalableProjection ?? true,
        )
        workingSet.installCommittedWorkingSet(projected.database, revision)
        if (projected.deferred) {
            deferredContentPending = true
            return
        }
        await commitContentCursor(revision)
    }
    const acquireCommittedWorkingSetRefreshFence =
        async (): Promise<PersistentDestructiveReplacementFence> => {
            const owner =
                await coordinator.acquireCommittedWorkingSetRefreshFence()
            const heldRevision = coordinator.revision
            let released = false
            return {
                revision: heldRevision,
                async refreshCommittedWorkingSet(minimumRevision, options) {
                    if (released) {
                        throw new Error(
                            'Destructive persistent replacement fence was released',
                        )
                    }
                    coordinator.assertDestructiveReplacementFence(owner)
                    const latest = await dependencies.store.readRoot()
                    coordinator.assertDestructiveReplacementFence(owner)
                    if (latest.revision < minimumRevision) {
                        throw new RevisionConflictError(
                            minimumRevision,
                            latest.revision,
                        )
                    }
                    return refreshCommittedWorkingSet(
                        latest.revision,
                        owner,
                        options,
                    )
                },
                release() {
                    if (released) return
                    coordinator.releaseDestructiveReplacementFence(owner)
                    released = true
                },
            }
        }
    // A change held back from a generating conversation is applied once that
    // generation ends; until then the cursor stays behind it. The generated
    // reply is persisted first, so reprojecting cannot discard it.
    dependencies.state.subscribeConversationOperationActive?.((active) => {
        if (active || !deferredContentPending) return
        deferredContentPending = false
        void (async () => {
            try {
                await coordinator.flushPendingDataLocally('deferred-content-change')
                const token = await coordinator.capturePersistentMutationToken(
                    'deferred-content-change',
                )
                const owner = await coordinator.acquireDestructiveReplacementFence(
                    token,
                    { allowRevisionAdvance: true },
                )
                try {
                    await refreshCommittedWorkingSet(token.revision, owner)
                } finally {
                    coordinator.releaseDestructiveReplacementFence(owner)
                }
            } catch (error) {
                // The cursor still sits behind the held change, so the next
                // refresh applies it.
                dependencies.onBackgroundError?.(error)
            }
        })()
    })
    return {
        store: dependencies.store,
        get revision() {
            return coordinator.revision
        },
        getStorageAuthorityEpoch: () => coordinator.storageAuthorityEpoch,
        async initializeActiveWorkingSet(database) {
            const result = await workingSet.initializeActiveWorkingSet(database)
            // A recreated WebView starts from a projection of the current
            // revision, so the cursor is realigned with it.
            await commitContentCursor(coordinator.revision)
            return result
        },
        refreshActiveWorkingSetFromStore: (revision) =>
            refreshCommittedWorkingSet(revision),
        runStorageOnlyMutation: (operation) =>
            coordinator.runStorageOnlyMutation(operation),
        markPersistentDataDirty: (estimatedBytes) =>
            coordinator.markPersistentDataDirty(estimatedBytes),
        flushPendingData: (reason) => coordinator.flushPendingData(reason),
        flushPendingDataLocally: (reason) =>
            coordinator.flushPendingDataLocally(reason),
        acknowledgeGenerationCompletion: () =>
            coordinator.flushPendingDataLocally('generation-completion'),
        commitCharacterAddition: (request, reason) =>
            coordinator.commitCharacterAddition(request, reason),
        activateCharacter,
        activateConversation: (id) => workingSet.activateConversation(id),
        getActiveConversationSession: () =>
            workingSet.activeConversationSession,
        getSelectedConversationMode: () => workingSet.selectedConversationMode,
        getActiveConversationViewportSource: () =>
            workingSet.activeConversationViewportSource,
        subscribeActiveConversationViewportSource: (listener) =>
            workingSet.subscribeActiveConversationViewportSource(listener),
        captureSelectedConversationTarget: () =>
            workingSet.captureSelectedConversationTarget(),
        captureSelectedConversationAuthority: () =>
            workingSet.captureSelectedConversationAuthority(),
        acquireCompleteConversation: (reason, target) =>
            workingSet.acquireCompleteConversation(reason, target ?? undefined),
        tryDemoteSelectedConversation: (target) =>
            workingSet.tryDemoteSelectedConversation(target ?? undefined),
        refreshSelectedConversationAfterReplacement: (
            target,
            expectedSession,
        ) =>
            workingSet.refreshSelectedConversationAfterReplacement(
                target,
                expectedSession,
            ),
        invalidateActiveConversationSession: () =>
            workingSet.invalidateActiveConversationSession(),
        deactivateActiveWorkingSet: () => workingSet.deactivate(),
        reconcileActiveCharacterIds: (database, selectedCharacterId) =>
            workingSet.reconcileActiveCharacterIds(
                database,
                selectedCharacterId,
            ),
        getNavigationGeneration: () => workingSet.navigationGenerationToken,
        fenceNavigation: () => workingSet.fenceNavigation(),
        invalidateNavigation: () => workingSet.invalidateNavigation(),
        replacePersistentDatabase: (database, reason, options) => {
            if (
                !options?.authoritative &&
                hasIncompletePersistentWorkingSet(database, workingSetResidency)
            ) {
                return Promise.reject(
                    new Error(
                        'Cannot replace persistent data from an incomplete persistent working set',
                    ),
                )
            }
            return coordinator.replacePreparedPersistentDatabase(
                () => dependencies.prepareDatabase(database),
                reason,
                options,
            )
        },
        mutatePersistentPluginStorage: (reason, mutations) =>
            coordinator.mutatePersistentPluginStorage(reason, mutations),
        mutatePersistentPresets: (reason, mutate) =>
            coordinator.mutatePersistentPresets(reason, mutate),
        appendPersistentRootModule: (reason, input, signal) =>
            coordinator.appendPersistentRootModule(reason, input, signal),
        mutateConversationBinding: (
            characterId,
            conversationId,
            patch,
            publish,
        ) =>
            coordinator.mutateConversationBinding(
                characterId,
                conversationId,
                patch,
                publish,
            ),
        mutatePersistentCharacterDetail: (characterId, reason, mutate) =>
            coordinator.mutatePersistentCharacterDetail(
                characterId,
                reason,
                mutate,
            ),
        deletePersistentCharacterWithGroupReferences: (characterId, reason) =>
            coordinator.deletePersistentCharacterWithGroupReferences(
                characterId,
                reason,
            ),
        replacePersistentCompleteCharacter: (
            characterId,
            reason,
            mutate,
            options,
        ) =>
            coordinator.replacePersistentCompleteCharacter(
                characterId,
                reason,
                mutate,
                options,
            ),
        replacePersistentConversation: (
            characterId,
            conversationId,
            reason,
            replacement,
            options,
        ) =>
            coordinator.replacePersistentConversation(
                characterId,
                conversationId,
                reason,
                replacement,
                options,
            ),
        upsertPersistentCompleteCharacter: (
            characterId,
            reason,
            createOrMutate,
            options,
        ) =>
            coordinator.upsertPersistentCompleteCharacter(
                characterId,
                reason,
                createOrMutate,
                options,
            ),
        readPersistentCharacterDetail: (characterId, reason) =>
            coordinator.readPersistentCharacterDetail(characterId, reason),
        readPersistentCompleteCharacter: (characterId, reason) =>
            coordinator.readPersistentCompleteCharacter(characterId, reason),
        readPersistentConversation: (characterId, conversationId, reason) =>
            coordinator.readPersistentConversation(
                characterId,
                conversationId,
                reason,
            ),
        readPersistentConversationAt: (characterId, orderedPosition, reason) =>
            coordinator.readPersistentConversationAt(
                characterId,
                orderedPosition,
                reason,
            ),
        readPersistentSelectedConversation: (characterId, reason) =>
            coordinator.readPersistentSelectedConversation(characterId, reason),
        capturePersistentMutationToken: (reason, options) =>
            coordinator.capturePersistentMutationToken(reason, options),
        async acquireDestructiveReplacementFence(expected, options) {
            const owner = await coordinator.acquireDestructiveReplacementFence(
                expected,
                options,
            )
            const heldRevision = coordinator.revision
            let released = false
            return {
                revision: heldRevision,
                refreshCommittedWorkingSet(revision, options) {
                    if (released) {
                        return Promise.reject(
                            new Error(
                                'Destructive persistent replacement fence was released',
                            ),
                        )
                    }
                    return refreshCommittedWorkingSet(revision, owner, options)
                },
                release() {
                    if (released) return
                    coordinator.releaseDestructiveReplacementFence(owner)
                    released = true
                },
            }
        },
        acquireCommittedWorkingSetRefreshFence,
        materializePersistentDatabaseSnapshot: (reason) =>
            coordinator.materializePersistentDatabaseSnapshot(reason),
        materializePersistentDatabaseSnapshotWithRevision: (reason) =>
            coordinator.materializePersistentDatabaseSnapshotWithRevision(
                reason,
            ),
        async releaseInactiveWorkingSet(canRelease, isCurrent) {
            const token = await coordinator.capturePersistentMutationToken(
                'plugin-scalable-working-set',
            )
            const selectedCharacterId =
                dependencies.state.getSelectedCharacterId() ?? null
            const selectedConversationId =
                dependencies.state.getSelectedConversationId?.() ?? null
            const navigationGeneration = workingSet.navigationGenerationToken
            const database = await projectScalableWorkingSetAtRevision(
                dependencies.store,
                token.revision,
                {
                    selectedCharacterId,
                    selectedConversationId,
                    activeCharacterIds: workingSet.activeCharacterIds,
                },
            )
            const releaseAllowed = canRelease ? await canRelease() : true
            if (
                !releaseAllowed ||
                isCurrent?.() === false ||
                token.revision !== coordinator.revision ||
                token.mutationGeneration !== coordinator.mutationGeneration ||
                navigationGeneration !== workingSet.navigationGenerationToken ||
                selectedCharacterId !==
                    (dependencies.state.getSelectedCharacterId() ?? null) ||
                selectedConversationId !==
                    (dependencies.state.getSelectedConversationId?.() ?? null)
            )
                return false
            workingSet.invalidateNavigation()
            const activeCharacterIds = workingSet.reconcileActiveCharacterIds(
                database,
                selectedCharacterId,
            )
            dependencies.state.replaceDatabase(
                database,
                activeCharacterIds,
                true,
            )
            dependencies.state.restoreSelection?.(
                selectedCharacterId,
                selectedConversationId,
            )
            const resident = dependencies.state.captureSelectedCharacter()
            if (
                resident &&
                !coordinator.adoptHydratedCharacter(
                    token.revision,
                    token.mutationGeneration,
                    resident,
                )
            )
                return false
            return true
        },
        publishCurrentOfficialRevision: () =>
            coordinator.publishCurrentOfficialRevision(),
        hasPendingOfficialPublication: () =>
            coordinator.hasPendingOfficialPublication,
    }
}
