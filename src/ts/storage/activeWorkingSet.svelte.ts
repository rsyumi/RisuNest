import type { Chat, Database, character, groupChat } from './database.svelte'
import {
    ActiveConversationSession,
    type ActiveConversationMutationEvent,
} from './activeConversationSession'
import type {
    CharacterDetail,
    ConversationSummary,
    DataRevision,
    PersistentDataStore,
} from './persistentDataStore'
import {
    createConversationSummaryStub,
    createConversationSummaryStubFromChat,
} from './conversationResidency'
import type { PersistedConversationMutationEvent } from './saveCoordinator'
import type { WindowedConversationPersistenceAuthority } from './saveCoordinator'
import {
    PersistentConversationViewportSource,
    SynchronousSessionConversationViewportSource,
    type ConversationViewportSource,
} from '../conversationViewportSource'
import { createMetadataOnlySelectedConversation } from './selectedConversationLifecycle'
import { removeGroupMemberReferences } from './groupMembership'

type CompleteCharacter = character | groupChat

const CONVERSATION_HYDRATION_CONCURRENCY = 8
const RELATED_CHARACTER_HYDRATION_CONCURRENCY = 4

class MissingCharacterError extends Error {}

export interface WorkingSetCoordinator {
    readonly revision: DataRevision
    readonly mutationGeneration: number
    initialize(revision: DataRevision, database: Database): void
    flushPendingData(reason: string): Promise<void>
    replacePersistentDatabase(database: Database, reason: string): Promise<void>
    adoptHydratedCharacter(
        revision: DataRevision,
        mutationGeneration: number,
        character: CompleteCharacter,
    ): boolean
    readonly hasPendingPersistenceWork?: boolean
    adoptWindowedSelectedConversation?(
        revision: DataRevision,
        mutationGeneration: number,
        character: CompleteCharacter,
        authority: WindowedConversationPersistenceAuthority,
    ): boolean
    advanceWindowedSelectedConversationRevision?(
        revision: DataRevision,
        authority: WindowedConversationPersistenceAuthority,
    ): boolean
    runSelectedConversationTransition?<T>(transition: () => T): T
    recordActiveConversationMutation?(event: ActiveConversationMutationEvent): void
}

export interface CharacterActivationOptions {
    prepare?(): Promise<{ database: Database; reason: string } | null>
}

export interface ActiveWorkingSetDependencies {
    store: PersistentDataStore
    coordinator: WorkingSetCoordinator
    getSelectedCharacterId(): string | null | undefined
    getResidentCharacter?(id: string): CompleteCharacter | null
    publishCharacter(character: CompleteCharacter): void
    publishCharacterSet(primary: CompleteCharacter, related: CharacterDetail[]): void
    publishConversation(
        characterId: string,
        conversation: Chat,
        nextCharacter?: CompleteCharacter,
    ): void
    canActivateWorkingSet?(): boolean
    canDeactivateWorkingSet?(): boolean
    canDeactivateCharacter?(id: string): boolean
    releaseInactiveCharacter?(id: string): void
    shouldHydrateFullCharacter?(): boolean
    canReleaseConversation?(
        character: CompleteCharacter,
        conversationId: string,
        nextConversationId: string,
    ): boolean
    canUseWindowedSelectedConversation?(): boolean
    isMaximumCompatibilityMode?(): boolean
    isConversationOperationActive?(): boolean
    subscribeConversationOperationActive?(listener: (active: boolean) => void): () => void
    conversationViewportRowBudget?: number
}

const selectedConversationTargetBrand = Symbol('selectedConversationTarget')

export interface SelectedConversationTarget {
    readonly characterId: string
    readonly conversationId: string
    readonly navigationGeneration: number
    readonly storeRevision: DataRevision
    readonly [selectedConversationTargetBrand]: symbol
}

export function isSameSelectedConversationTarget(
    left: SelectedConversationTarget,
    right: SelectedConversationTarget,
): boolean {
    return left.characterId === right.characterId &&
        left.conversationId === right.conversationId &&
        left.navigationGeneration === right.navigationGeneration &&
        left.storeRevision === right.storeRevision &&
        left[selectedConversationTargetBrand] === right[selectedConversationTargetBrand]
}

export interface CompleteConversationLease {
    readonly reason: string
    readonly session: ActiveConversationSession
    readonly target: SelectedConversationTarget
    release(): void
}

export class SelectedConversationPromotionStaleError extends Error {
    constructor() {
        super('Selected conversation changed during complete promotion')
        this.name = 'SelectedConversationPromotionStaleError'
    }
}

interface CompleteSelectedConversationState {
    kind: 'complete'
    stateToken: symbol
    navigationGeneration: number
    characterId: string
    conversationId: string
    conversation: Chat
    session: ActiveConversationSession
    viewportSource: ConversationViewportSource
}

interface WindowedSelectedConversationState {
    kind: 'windowed'
    stateToken: symbol
    navigationGeneration: number
    characterId: string
    conversationId: string
    conversation: Chat
    authority: WindowedConversationPersistenceAuthority
    viewportSource: PersistentConversationViewportSource
}

type SelectedConversationState =
    | CompleteSelectedConversationState
    | WindowedSelectedConversationState

export type ActiveConversationViewportSourceListener = (
    source: ConversationViewportSource | null,
) => void

export class ActiveWorkingSet {
    private navigationGeneration = 0
    private activeIds = new Set<string>()
    private readonly conversationFlights = new Map<
        string,
        { generation: number; promise: Promise<boolean> }
    >()
    private activeSession: ActiveConversationSession | null = null
    private selectedConversationState: SelectedConversationState | null = null
    private promotionFlight: Promise<CompleteSelectedConversationState> | null = null
    private demotionScheduled = false
    private readonly viewportSourceListeners = new Set<
        ActiveConversationViewportSourceListener
    >()

    constructor(private readonly dependencies: ActiveWorkingSetDependencies) {
        dependencies.subscribeConversationOperationActive?.((active) => {
            if (!active) this.scheduleSelectedConversationDemotion()
        })
    }

    get navigationGenerationToken(): number {
        return this.navigationGeneration
    }

    get activeCharacterIds(): ReadonlySet<string> {
        return new Set(this.activeIds)
    }

    get activeConversationSession(): ActiveConversationSession | null {
        return this.activeSession
    }

    get selectedConversationMode(): SelectedConversationState['kind'] | null {
        return this.selectedConversationState?.kind ?? null
    }

    get activeConversationViewportSource(): ConversationViewportSource | null {
        return this.selectedConversationState?.viewportSource ?? null
    }

    subscribeActiveConversationViewportSource(
        listener: ActiveConversationViewportSourceListener,
    ): () => void {
        this.viewportSourceListeners.add(listener)
        let subscribed = true
        return () => {
            if (!subscribed) return
            subscribed = false
            this.viewportSourceListeners.delete(listener)
        }
    }

    captureSelectedConversationTarget(): SelectedConversationTarget | null {
        const state = this.selectedConversationState
        if (!state) return null
        return {
            characterId: state.characterId,
            conversationId: state.conversationId,
            navigationGeneration: state.navigationGeneration,
            storeRevision: state.kind === 'complete'
                ? state.session.storeRevision
                : state.authority.storeRevision,
            [selectedConversationTargetBrand]: state.stateToken,
        }
    }

    captureSelectedConversationAuthority(): WindowedConversationPersistenceAuthority | null {
        const state = this.selectedConversationState
        return state?.kind === 'windowed' ? { ...state.authority } : null
    }

    async acquireCompleteConversation(
        reason: string,
        target = this.captureSelectedConversationTarget(),
    ): Promise<CompleteConversationLease> {
        const state = this.selectedConversationState
        if (!state || !target || !this.matchesTarget(state, target)) {
            throw new SelectedConversationPromotionStaleError()
        }
        let complete: CompleteSelectedConversationState
        if (state.kind === 'complete') {
            complete = state
        } else {
            let flight = this.promotionFlight
            if (!flight) {
                flight = this.promoteWindowedConversation(state, target, reason)
                this.promotionFlight = flight
                const clearFlight = () => {
                    if (this.promotionFlight === flight) this.promotionFlight = null
                }
                void flight.then(clearFlight, clearFlight)
            }
            complete = await flight
        }
        const recaptured = this.captureSelectedConversationTarget()
        if (
            !recaptured ||
            this.selectedConversationState !== complete ||
            !this.matchesTarget(complete, recaptured)
        ) {
            throw new SelectedConversationPromotionStaleError()
        }
        const pin = complete.session.acquirePin('compatibility')
        let released = false
        return {
            reason,
            session: complete.session,
            target: recaptured,
            release() {
                if (released) return
                released = true
                pin.release()
            },
        }
    }

    tryDemoteSelectedConversation(target = this.captureSelectedConversationTarget()): boolean {
        const state = this.selectedConversationState
        const transition = this.dependencies.coordinator.runSelectedConversationTransition
        if (
            state?.kind !== 'complete' ||
            !target ||
            !this.matchesTarget(state, target) ||
            this.promotionFlight !== null ||
            this.dependencies.canUseWindowedSelectedConversation?.() !== true ||
            this.dependencies.isMaximumCompatibilityMode?.() === true ||
            this.dependencies.isConversationOperationActive?.() === true ||
            this.dependencies.coordinator.hasPendingPersistenceWork !== false ||
            !transition ||
            !this.dependencies.coordinator.adoptWindowedSelectedConversation ||
            state.session.version !== state.session.persistedVersion ||
            state.session.storeRevision !== this.dependencies.coordinator.revision ||
            state.session.isTransactionActive ||
            state.session.activePinReasons.some((reason) => reason !== 'viewport') ||
            state.conversation.isStreaming === true
        ) return false

        const resident = this.dependencies.getResidentCharacter?.(state.characterId)
        if (!resident) return false
        const conversationIndex = resident.chats.findIndex(
            (conversation) => conversation === state.conversation,
        )
        if (conversationIndex < 0) return false
        let prepared: {
            shell: Chat
            nextCharacter: CompleteCharacter
            authority: WindowedConversationPersistenceAuthority
            viewportSource: PersistentConversationViewportSource
            windowedState: WindowedSelectedConversationState
        }
        try {
            const shell = createMetadataOnlySelectedConversation(state.conversation)
            const nextCharacter = {
                ...resident,
                chats: resident.chats.map((conversation, index) =>
                    index === conversationIndex ? shell : conversation),
            } as CompleteCharacter
            const authority: WindowedConversationPersistenceAuthority = {
                kind: 'windowed',
                characterId: state.characterId,
                conversationId: state.conversationId,
                sessionToken: state.session.sessionToken,
                storeRevision: state.session.storeRevision,
                persistedSessionVersion: state.session.persistedVersion,
                sessionVersion: state.session.version,
                totalMessages: state.session.totalMessages,
            }
            const viewportSource = new PersistentConversationViewportSource({
                reader: this.dependencies.store,
                characterId: state.characterId,
                conversationId: state.conversationId,
                revision: authority.storeRevision,
                totalMessages: authority.totalMessages,
                rowBudget: this.dependencies.conversationViewportRowBudget ?? 64,
            })
            prepared = {
                shell,
                nextCharacter,
                authority,
                viewportSource,
                windowedState: {
                    kind: 'windowed',
                    stateToken: Symbol('windowed selected conversation'),
                    navigationGeneration: state.navigationGeneration,
                    characterId: state.characterId,
                    conversationId: state.conversationId,
                    conversation: shell,
                    authority,
                    viewportSource,
                },
            }
        } catch {
            return false
        }
        const { shell, nextCharacter, authority, viewportSource, windowedState } = prepared
        try {
            const adopted = transition.call(this.dependencies.coordinator, () => {
                this.selectedConversationState = windowedState
                this.activeSession = null
                this.dependencies.publishConversation(
                    state.characterId,
                    shell,
                    nextCharacter,
                )
                return this.dependencies.coordinator.adoptWindowedSelectedConversation!(
                    authority.storeRevision,
                    this.dependencies.coordinator.mutationGeneration,
                    nextCharacter,
                    authority,
                )
            })
            if (!adopted) throw new Error('Windowed selected conversation was not adopted')
        } catch {
            this.selectedConversationState = state
            this.activeSession = state.session
            try {
                this.dependencies.publishConversation(
                    state.characterId,
                    state.conversation,
                    resident,
                )
            } catch {
                this.selectedConversationState = windowedState
                this.activeSession = null
                state.viewportSource.dispose()
                state.session.invalidate()
                this.notifyActiveConversationViewportSource()
                return false
            }
            viewportSource.dispose()
            return false
        }
        state.viewportSource.dispose()
        state.session.invalidate()
        this.notifyActiveConversationViewportSource()
        return true
    }

    advanceStoreRevision(revision: DataRevision): void {
        const state = this.selectedConversationState
        if (state?.kind !== 'windowed') {
            const previousRevision = this.activeSession?.storeRevision
            this.activeSession?.advanceStoreRevision(revision)
            if (
                previousRevision !== undefined &&
                this.activeSession?.storeRevision !== previousRevision
            ) this.notifyActiveConversationViewportSource()
            return
        }
        if (revision < state.authority.storeRevision) {
            throw new RangeError('Selected conversation store revision moved backwards')
        }
        if (revision === state.authority.storeRevision) return
        const advance = this.dependencies.coordinator
            .advanceWindowedSelectedConversationRevision
        if (!advance) {
            throw new Error('Windowed selected conversation revision advance is unavailable')
        }
        const authority = { ...state.authority, storeRevision: revision }
        const viewportSource = new PersistentConversationViewportSource({
            reader: this.dependencies.store,
            characterId: state.characterId,
            conversationId: state.conversationId,
            revision,
            totalMessages: authority.totalMessages,
            rowBudget: this.dependencies.conversationViewportRowBudget ?? 64,
        })
        const advancedState: WindowedSelectedConversationState = {
            ...state,
            stateToken: Symbol('advanced windowed selected conversation'),
            authority,
            viewportSource,
        }
        this.selectedConversationState = advancedState
        if (!advance.call(this.dependencies.coordinator, revision, authority)) {
            this.selectedConversationState = state
            viewportSource.dispose()
            throw new Error('Windowed selected conversation revision was not adopted')
        }
        state.viewportSource.dispose()
        this.notifyActiveConversationViewportSource()
    }

    beginConversationMutationPersistence(event: ActiveConversationMutationEvent) {
        const session = this.activeSession
        if (
            !session ||
            !session.isActive ||
            session.characterId !== event.characterId ||
            session.conversationId !== event.conversationId ||
            !session.ownsSessionToken(event.sessionToken)
        ) return null
        return session.beginPersistence(event.sessionVersion)
    }

    acknowledgeConversationMutationPersisted(
        event: PersistedConversationMutationEvent,
    ): boolean {
        const session = this.activeSession
        if (
            !session ||
            !session.isActive ||
            session.characterId !== event.characterId ||
            session.conversationId !== event.conversationId ||
            !session.ownsSessionToken(event.sessionToken)
        ) return false
        const acknowledged = session.acknowledgePersisted(
            event.sessionToken,
            event.sessionVersion,
            event.revision,
        )
        if (acknowledged) this.scheduleSelectedConversationDemotion()
        return acknowledged
    }

    reconcileActiveCharacterIds(
        database: Database,
        selectedCharacterId: string | null,
    ): ReadonlySet<string> {
        const selected = selectedCharacterId
            ? database.characters.find((character) => character.chaId === selectedCharacterId)
            : undefined
        if (!selected || !Array.isArray(selected.chats)) {
            this.activeIds = new Set()
            return this.activeCharacterIds
        }
        const existingIds = new Set(database.characters.map((character) => character.chaId))
        const relatedIds = selected.type === 'group' && Array.isArray(selected.characters)
            ? selected.characters.filter(
                (id, index) => existingIds.has(id) && selected.characters.indexOf(id) === index,
            )
            : []
        this.activeIds = new Set([selected.chaId, ...relatedIds])
        return this.activeCharacterIds
    }

    invalidateNavigation(): void {
        this.navigationGeneration++
        this.clearActiveConversationSession()
    }

    invalidateActiveConversationSession(): void {
        this.clearActiveConversationSession()
    }

    refreshSelectedConversationAfterReplacement(
        target: SelectedConversationTarget,
        expectedSession: ActiveConversationSession,
    ): boolean {
        const state = this.selectedConversationState
        if (
            state?.kind !== 'complete' ||
            state.session !== expectedSession ||
            this.activeSession !== expectedSession ||
            !expectedSession.isActive ||
            target.characterId !== state.characterId ||
            target.conversationId !== state.conversationId ||
            target.navigationGeneration !== state.navigationGeneration ||
            state.navigationGeneration !== this.navigationGeneration ||
            target[selectedConversationTargetBrand] !== state.stateToken
        ) return false

        if (this.dependencies.getSelectedCharacterId() !== target.characterId) {
            this.clearActiveConversationSession()
            return false
        }
        const resident = this.dependencies.getResidentCharacter?.(target.characterId)
        const conversation = resident?.chats[resident.chatPage ?? 0]
        if (!conversation || conversation.id !== target.conversationId) {
            this.clearActiveConversationSession()
            return false
        }
        if (conversation === state.conversation) return false

        this.publishActiveConversationSession(
            target.characterId,
            conversation,
            this.dependencies.coordinator.revision,
        )
        return true
    }

    async deactivate(): Promise<boolean> {
        if (this.dependencies.canDeactivateWorkingSet?.() === false) return false
        const generation = ++this.navigationGeneration
        const activeIds = this.activeIds.size > 0
            ? new Set(this.activeIds)
            : new Set(
                this.dependencies.getSelectedCharacterId()
                    ? [this.dependencies.getSelectedCharacterId()!]
                    : [],
            )
        await this.dependencies.coordinator.flushPendingData('deactivate-working-set')
        if (generation !== this.navigationGeneration) return false
        if (this.dependencies.canDeactivateWorkingSet?.() === false) return false
        if ([...activeIds].some(
            (id) => this.dependencies.canDeactivateCharacter?.(id) === false,
        )) return false
        this.activeIds = new Set()
        this.clearActiveConversationSession()
        for (const id of activeIds) this.dependencies.releaseInactiveCharacter?.(id)
        return true
    }

    async initializeActiveWorkingSet(database: Database): Promise<void> {
        await this.dependencies.store.open()
        const root = await this.dependencies.store.readRoot()
        this.installCommittedWorkingSet(database, root.revision)
    }

    installCommittedWorkingSet(database: Database, revision: DataRevision): void {
        this.dependencies.coordinator.initialize(revision, database)
        const selectedId = this.dependencies.getSelectedCharacterId()
        this.activeIds = selectedId ? new Set([selectedId]) : new Set()
        const selected = selectedId
            ? database.characters.find((character) => character.chaId === selectedId)
            : undefined
        const conversation = selected?.chats[selected.chatPage ?? 0]
        if (selected && conversation) {
            this.publishActiveConversationSession(selected.chaId, conversation, revision)
        } else this.clearActiveConversationSession()
    }

    async activateCharacter(
        id: string,
        options: CharacterActivationOptions = {},
    ): Promise<boolean> {
        const preparation = this.prepareCompleteNavigation(
            `activate-character:${id}`,
        )
        let lease: CompleteConversationLease | null = null
        try {
            if (preparation) lease = await preparation
            return await this.activateCompleteCharacter(id, options)
        } catch (error) {
            if (error instanceof SelectedConversationPromotionStaleError) return false
            throw error
        } finally {
            lease?.release()
        }
    }

    private async activateCompleteCharacter(
        id: string,
        options: CharacterActivationOptions = {},
    ): Promise<boolean> {
        if (this.dependencies.canActivateWorkingSet?.() === false) return false
        const generation = ++this.navigationGeneration
        const previousCharacterId = this.dependencies.getSelectedCharacterId()
        const previousActiveIds = this.activeIds.size > 0
            ? new Set(this.activeIds)
            : new Set(previousCharacterId ? [previousCharacterId] : [])
        await this.dependencies.coordinator.flushPendingData('activate-character')
        if (
            generation !== this.navigationGeneration ||
            this.dependencies.canActivateWorkingSet?.() === false
        ) return false
        let mutationGeneration = this.dependencies.coordinator.mutationGeneration
        if (options.prepare) {
            const prepared = await options.prepare()
            if (
                generation !== this.navigationGeneration ||
                this.dependencies.canActivateWorkingSet?.() === false
            ) return false
            if (!prepared) return false
            await this.dependencies.coordinator.replacePersistentDatabase(
                prepared.database,
                prepared.reason,
            )
            if (
                generation !== this.navigationGeneration ||
                this.dependencies.canActivateWorkingSet?.() === false
            ) return false
            mutationGeneration = this.dependencies.coordinator.mutationGeneration
        }
        const revision = this.dependencies.coordinator.revision
        let characterValue = await this.hydrateCharacter(
            id,
            revision,
            mutationGeneration,
            generation,
        )
        if (!characterValue) return false
        let relatedIds = characterValue.type === 'group'
            ? [...new Set(characterValue.characters)].filter((memberId) => memberId !== id)
            : []
        const relatedValues: Array<CharacterDetail | null | undefined> = []
        for (
            let start = 0;
            start < relatedIds.length;
            start += RELATED_CHARACTER_HYDRATION_CONCURRENCY
        ) {
            const chunk = relatedIds.slice(
                start,
                start + RELATED_CHARACTER_HYDRATION_CONCURRENCY,
            )
            const hydratedChunk = await Promise.all(chunk.map(async (memberId) => {
                try {
                    return await this.hydrateCharacterDetail(
                        memberId,
                        revision,
                        mutationGeneration,
                        generation,
                    )
                } catch (error) {
                    if (error instanceof MissingCharacterError) return undefined
                    throw error
                }
            }))
            relatedValues.push(...hydratedChunk)
            if (hydratedChunk.some((value) => value === null)) return false
        }
        const missingRelatedIds = new Set(
            relatedIds.filter((_memberId, index) => relatedValues[index] === undefined),
        )
        const persistedCharacterValue = characterValue
        if (characterValue.type === 'group' && missingRelatedIds.size > 0) {
            const groupValue = characterValue
            characterValue = {
                ...groupValue,
                ...removeGroupMemberReferences(groupValue, missingRelatedIds),
            }
            relatedIds = relatedIds.filter((memberId) => !missingRelatedIds.has(memberId))
        }
        if (!this.isCurrent(generation, revision, mutationGeneration)) return false
        if (!this.dependencies.coordinator.adoptHydratedCharacter(
            revision,
            mutationGeneration,
            persistedCharacterValue,
        )) {
            return false
        }
        if (this.dependencies.canActivateWorkingSet?.() === false) return false
        const completeRelated = relatedValues.filter(
            (value): value is CharacterDetail => value !== null && value !== undefined,
        )
        if (completeRelated.length > 0) {
            this.dependencies.publishCharacterSet(characterValue, completeRelated)
        } else {
            this.dependencies.publishCharacter(characterValue)
        }
        const selectedConversation = characterValue.chats[characterValue.chatPage ?? 0]
        if (selectedConversation) {
            this.publishActiveConversationSession(id, selectedConversation, revision)
        } else this.clearActiveConversationSession()
        const nextActiveIds = new Set([id, ...relatedIds])
        this.activeIds = nextActiveIds
        for (const previousId of previousActiveIds) {
            if (!nextActiveIds.has(previousId)) {
                this.dependencies.releaseInactiveCharacter?.(previousId)
            }
        }
        return true
    }

    activateConversation(id: string): Promise<boolean> {
        const preparation = this.prepareCompleteNavigation(
            `activate-conversation:${id}`,
        )
        if (!preparation) return this.startConversationActivation(id)
        return this.activateConversationAfterPreparation(id, preparation)
    }

    private async activateConversationAfterPreparation(
        id: string,
        preparation: Promise<CompleteConversationLease>,
    ): Promise<boolean> {
        let lease: CompleteConversationLease | null = null
        try {
            lease = await preparation
            return await this.startConversationActivation(id)
        } catch (error) {
            if (error instanceof SelectedConversationPromotionStaleError) return false
            throw error
        } finally {
            lease?.release()
        }
    }

    private startConversationActivation(id: string): Promise<boolean> {
        if (this.dependencies.canActivateWorkingSet?.() === false) return Promise.resolve(false)
        const characterId = this.dependencies.getSelectedCharacterId()
        if (!characterId) return Promise.reject(new Error('No character is selected'))
        const key = `${characterId}\u0000${id}`
        const existing = this.conversationFlights.get(key)
        if (existing?.generation === this.navigationGeneration) return existing.promise
        const generation = ++this.navigationGeneration
        const pending = this.activateConversationOnce(characterId, id, generation).finally(() => {
            if (this.conversationFlights.get(key)?.promise === pending) {
                this.conversationFlights.delete(key)
            }
        })
        this.conversationFlights.set(key, { generation, promise: pending })
        return pending
    }

    private prepareCompleteNavigation(
        reason: string,
    ): Promise<CompleteConversationLease> | null {
        if (this.selectedConversationState?.kind !== 'windowed') return null
        const target = this.captureSelectedConversationTarget()
        if (!target) return null
        return this.acquireCompleteConversation(reason, target)
    }

    private async activateConversationOnce(
        characterId: string,
        id: string,
        generation: number,
    ): Promise<boolean> {
        await this.dependencies.coordinator.flushPendingData('activate-conversation')
        if (
            generation !== this.navigationGeneration ||
            characterId !== this.dependencies.getSelectedCharacterId() ||
            this.dependencies.canActivateWorkingSet?.() === false
        ) return false
        const revision = this.dependencies.coordinator.revision
        const mutationGeneration = this.dependencies.coordinator.mutationGeneration
        const conversation = await this.dependencies.store.readConversation(characterId, id)
        if (
            characterId !== this.dependencies.getSelectedCharacterId() ||
            !this.isCurrent(generation, revision, mutationGeneration)
        ) return false
        if (!conversation) throw new Error(`Conversation ${id} was not found for ${characterId}`)
        if (conversation.revision !== revision) return false
        if (conversation.value.id !== id) {
            throw new Error(`Conversation ${id} returned mismatched ID ${conversation.value.id ?? ''}`)
        }
        const resident = this.dependencies.getResidentCharacter?.(characterId)
        const conversationIndex = resident?.chats.findIndex((candidate) => candidate.id === id) ?? -1
        let nextCharacter: CompleteCharacter | undefined
        if (resident && conversationIndex >= 0) {
            const chats = [...resident.chats]
            const previousIndex = resident.chatPage ?? 0
            const previous = chats[previousIndex]
            if (
                previous &&
                previousIndex !== conversationIndex &&
                this.dependencies.canReleaseConversation?.(
                    resident,
                    previous.id ?? '',
                    id,
                ) === true
            ) {
                chats[previousIndex] = createConversationSummaryStubFromChat(
                    characterId,
                    previous,
                    previousIndex,
                )
            }
            chats[conversationIndex] = conversation.value
            nextCharacter = {
                ...resident,
                chats,
                chatPage: conversationIndex,
            } as CompleteCharacter
            if (!this.dependencies.coordinator.adoptHydratedCharacter(
                revision,
                mutationGeneration,
                { ...nextCharacter, chatPage: resident.chatPage } as CompleteCharacter,
            )) return false
        }
        this.dependencies.publishConversation(characterId, conversation.value, nextCharacter)
        this.publishActiveConversationSession(characterId, conversation.value, revision)
        return true
    }

    private publishActiveConversationSession(
        characterId: string,
        fallbackConversation: Chat,
        storeRevision: DataRevision,
    ): void {
        const resident = this.dependencies.getResidentCharacter?.(characterId)
        const conversation = resident?.chats.find(
            (candidate) => candidate.id === fallbackConversation.id,
        ) ?? fallbackConversation
        const conversationId = conversation.id ?? fallbackConversation.id
        const cleared = this.clearActiveConversationSession(false)
        if (!conversationId) {
            if (cleared) this.notifyActiveConversationViewportSource()
            return
        }
        const complete = this.createCompleteSelectedConversationState(
            characterId,
            conversation,
            storeRevision,
        )
        this.activeSession = complete.session
        this.selectedConversationState = complete
        this.notifyActiveConversationViewportSource()
        this.scheduleSelectedConversationDemotion()
    }

    private clearActiveConversationSession(notify = true): boolean {
        const changed = this.selectedConversationState !== null || this.activeSession !== null
        this.selectedConversationState?.viewportSource.dispose()
        this.activeSession?.invalidate()
        this.activeSession = null
        this.selectedConversationState = null
        this.promotionFlight = null
        if (changed && notify) this.notifyActiveConversationViewportSource()
        return changed
    }

    private async promoteWindowedConversation(
        state: WindowedSelectedConversationState,
        target: SelectedConversationTarget,
        reason: string,
    ): Promise<CompleteSelectedConversationState> {
        await this.dependencies.coordinator.flushPendingData(
            `complete-selected-conversation:${reason}`,
        )
        this.requireCurrentWindowedState(state, target)
        if (this.dependencies.coordinator.revision !== state.authority.storeRevision) {
            throw new SelectedConversationPromotionStaleError()
        }
        const persisted = await this.dependencies.store.readConversation(
            state.characterId,
            state.conversationId,
        )
        this.requireCurrentWindowedState(state, target)
        if (
            !persisted ||
            persisted.revision !== state.authority.storeRevision ||
            persisted.value.id !== state.conversationId
        ) throw new SelectedConversationPromotionStaleError()

        const resident = this.dependencies.getResidentCharacter?.(state.characterId)
        if (!resident) throw new SelectedConversationPromotionStaleError()
        const conversationIndex = resident.chats.findIndex(
            (conversation) => conversation === state.conversation,
        )
        if (conversationIndex < 0) throw new SelectedConversationPromotionStaleError()
        const conversation = persisted.value
        const nextCharacter = {
            ...resident,
            chats: resident.chats.map((candidate, index) =>
                index === conversationIndex ? conversation : candidate),
        } as CompleteCharacter
        const complete = this.createCompleteSelectedConversationState(
            state.characterId,
            conversation,
            state.authority.storeRevision,
            state.navigationGeneration,
        )
        const transition = this.dependencies.coordinator.runSelectedConversationTransition
        if (!transition) {
            complete.viewportSource.dispose()
            complete.session.invalidate()
            throw new Error('Selected conversation transition is unavailable')
        }
        let adopted = false
        try {
            adopted = transition.call(this.dependencies.coordinator, () => {
                this.selectedConversationState = complete
                this.activeSession = complete.session
                this.dependencies.publishConversation(
                    state.characterId,
                    conversation,
                    nextCharacter,
                )
                return this.dependencies.coordinator.adoptHydratedCharacter(
                    state.authority.storeRevision,
                    this.dependencies.coordinator.mutationGeneration,
                    nextCharacter,
                )
            })
            if (!adopted) {
                throw new Error('The complete selected conversation was not adopted')
            }
        } catch (error) {
            this.selectedConversationState = state
            this.activeSession = null
            complete.viewportSource.dispose()
            complete.session.invalidate()
            try {
                this.dependencies.publishConversation(
                    state.characterId,
                    state.conversation,
                    resident,
                )
            } catch {
                this.clearActiveConversationSession()
            }
            throw error
        }
        state.viewportSource.dispose()
        this.notifyActiveConversationViewportSource()
        return complete
    }

    private createCompleteSelectedConversationState(
        characterId: string,
        conversation: Chat,
        storeRevision: DataRevision,
        navigationGeneration = this.navigationGeneration,
    ): CompleteSelectedConversationState {
        const conversationId = conversation.id
        if (!conversationId) throw new Error('Selected conversation has no ID')
        const session = new ActiveConversationSession({
            characterId,
            conversationId,
            conversation,
            storeRevision,
            onMutation: this.dependencies.coordinator.recordActiveConversationMutation === undefined
                ? undefined
                : (event) => this.dependencies.coordinator.recordActiveConversationMutation!(event),
            onPinReleased: () => this.scheduleSelectedConversationDemotion(),
        })
        let complete!: CompleteSelectedConversationState
        const viewportSource = new SynchronousSessionConversationViewportSource({
            session,
            captureCurrent: () => {
                const resident = this.dependencies.getResidentCharacter?.(characterId)
                const currentConversation = resident?.chats.find(
                    (candidate) => candidate === conversation,
                )
                return this.selectedConversationState === complete &&
                    currentConversation === conversation
                    ? { character: resident!, conversation }
                    : null
            },
        })
        complete = {
            kind: 'complete',
            stateToken: Symbol('complete selected conversation'),
            navigationGeneration,
            characterId,
            conversationId,
            conversation,
            session,
            viewportSource,
        }
        return complete
    }

    private notifyActiveConversationViewportSource(): void {
        const source = this.activeConversationViewportSource
        for (const listener of [...this.viewportSourceListeners]) {
            try {
                listener(source)
            } catch (error) {
                console.error('Active conversation viewport source subscriber failed', error)
            }
        }
    }

    scheduleSelectedConversationDemotion(): void {
        if (this.demotionScheduled) return
        this.demotionScheduled = true
        queueMicrotask(() => {
            this.demotionScheduled = false
            this.tryDemoteSelectedConversation()
        })
    }

    private requireCurrentWindowedState(
        state: WindowedSelectedConversationState,
        target: SelectedConversationTarget,
    ): void {
        if (
            this.selectedConversationState !== state ||
            !this.matchesTarget(state, target) ||
            this.dependencies.getSelectedCharacterId() !== state.characterId ||
            this.dependencies.coordinator.revision !== state.authority.storeRevision
        ) throw new SelectedConversationPromotionStaleError()
    }

    private matchesTarget(
        state: SelectedConversationState,
        target: SelectedConversationTarget,
    ): boolean {
        return target.characterId === state.characterId &&
            target.conversationId === state.conversationId &&
            target.navigationGeneration === state.navigationGeneration &&
            state.navigationGeneration === this.navigationGeneration &&
            target.storeRevision === (
                state.kind === 'complete'
                    ? state.session.storeRevision
                    : state.authority.storeRevision
            ) &&
            target[selectedConversationTargetBrand] === state.stateToken
    }

    private async hydrateCharacter(
        id: string,
        revision: DataRevision,
        mutationGeneration: number,
        generation: number,
    ): Promise<CompleteCharacter | null> {
        const detail = await this.hydrateCharacterDetail(
            id,
            revision,
            mutationGeneration,
            generation,
        )
        if (!detail) return null

        const summaries: ConversationSummary[] = []
        const summaryPositions = new Map<string, number>()
        const chats: Chat[] = []
        const hydrateAll = this.dependencies.shouldHydrateFullCharacter?.() === true
        let selectedId: string | undefined
        let cursor: string | undefined
        do {
            const page = await this.dependencies.store.queryConversations({
                characterId: id,
                order: 'configured',
                limit: 100,
                cursor,
            })
            if (
                !this.isCurrent(generation, revision, mutationGeneration) ||
                page.revision !== revision
            ) return null
            for (const summary of page.items) {
                if (summary.characterId !== id) {
                    throw new Error(`Conversation ${summary.id} returned mismatched character ID`)
                }
            }
            for (const summary of page.items) {
                summaryPositions.set(summary.id, summaries.length)
                summaries.push(summary)
                chats.push(createConversationSummaryStub(summary))
            }
            const selectedSummary = page.items.find(
                (summary) => summary.configuredIndex === (detail.chatPage ?? 0),
            )
            if (selectedSummary) selectedId = selectedSummary.id
            const summariesToHydrate = hydrateAll
                ? page.items
                : selectedSummary ? [selectedSummary] : []
            for (
                let start = 0;
                start < summariesToHydrate.length;
                start += CONVERSATION_HYDRATION_CONCURRENCY
            ) {
                const chunk = summariesToHydrate.slice(
                    start,
                    start + CONVERSATION_HYDRATION_CONCURRENCY,
                )
                const conversations = await Promise.all(
                    chunk.map((summary) => this.dependencies.store.readConversation(id, summary.id)),
                )
                if (!this.isCurrent(generation, revision, mutationGeneration)) return null
                for (let index = 0; index < chunk.length; index++) {
                    const summary = chunk[index]
                    const conversation = conversations[index]
                    if (!conversation) {
                        throw new Error(`Conversation ${summary.id} was not found for ${id}`)
                    }
                    if (conversation.revision !== revision) return null
                    if (conversation.value.id !== summary.id) {
                        throw new Error(`Conversation ${summary.id} returned mismatched ID`)
                    }
                    chats[summaryPositions.get(summary.id)!] = conversation.value
                }
            }
            cursor = page.nextCursor
        } while (cursor !== undefined)

        if (!hydrateAll && !selectedId && summaries.length > 0) {
            const summary = summaries[0]
            const conversation = await this.dependencies.store.readConversation(id, summary.id)
            if (!this.isCurrent(generation, revision, mutationGeneration)) return null
            if (!conversation) {
                throw new Error(`Conversation ${summary.id} was not found for ${id}`)
            }
            if (conversation.revision !== revision) return null
            if (conversation.value.id !== summary.id) {
                throw new Error(`Conversation ${summary.id} returned mismatched ID`)
            }
            selectedId = summary.id
            chats[0] = conversation.value
        }

        const chatPage = selectedId
            ? chats.findIndex((conversation) => conversation.id === selectedId)
            : 0
        return {
            ...detail,
            chats,
            chatPage: chatPage < 0 ? 0 : chatPage,
        } as CompleteCharacter
    }

    private async hydrateCharacterDetail(
        id: string,
        revision: DataRevision,
        mutationGeneration: number,
        generation: number,
    ): Promise<CharacterDetail | null> {
        const detail = await this.dependencies.store.readCharacter(id)
        if (!this.isCurrent(generation, revision, mutationGeneration)) return null
        if (!detail) throw new MissingCharacterError(`Character ${id} was not found`)
        if (detail.revision !== revision) return null
        if (detail.value.chaId !== id) {
            throw new Error(`Character ${id} returned mismatched ID ${detail.value.chaId}`)
        }
        return detail.value
    }

    private isCurrent(
        generation: number,
        revision: DataRevision,
        mutationGeneration?: number,
    ): boolean {
        return (
            generation === this.navigationGeneration &&
            this.dependencies.canActivateWorkingSet?.() !== false &&
            revision === this.dependencies.coordinator.revision &&
            (
                mutationGeneration === undefined ||
                mutationGeneration === this.dependencies.coordinator.mutationGeneration
            )
        )
    }
}
