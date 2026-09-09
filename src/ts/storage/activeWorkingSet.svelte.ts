import type { Chat, Database, character, groupChat } from './database.svelte'
import { safeStructuredClone } from '../polyfill'
import {
    ActiveConversationSession,
    createConversationSessionToken,
    type ActiveConversationMutationEvent,
} from './activeConversationSession'
import isEqual from 'lodash/isEqual'
import type {
    CharacterDetail,
    ConversationSummary,
    DataRevision,
    PersistentConversationMetadata,
    PersistentDataStore,
} from './persistentDataStore'
import {
    createConversationSummaryFromMetadata,
    createConversationSummaryStub,
    createConversationSummaryStubFromChat,
    getConversationSummaryStub,
} from './conversationResidency'
import type {
    PersistedConversationMutationEvent,
    WindowedConversationActivationChange,
} from './saveCoordinator'
import type { WindowedConversationPersistenceAuthority } from './saveCoordinator'
import {
    PersistentConversationViewportSource,
    SynchronousSessionConversationViewportSource,
    type ConversationViewportSource,
} from '../conversationViewportSource'
import {
    cloneConversationMetadata,
    createMetadataOnlySelectedConversation,
    isMetadataOnlySelectedConversation,
} from './selectedConversationLifecycle'
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
    markPersistentDataDirty(estimatedBytes: number): void
    adoptWindowedSelectedConversation?(
        revision: DataRevision,
        mutationGeneration: number,
        character: CompleteCharacter,
        authority: WindowedConversationPersistenceAuthority,
        activationChange?: WindowedConversationActivationChange,
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
    normalize?(character: CompleteCharacter): CompleteCharacter
}

export interface ConversationPublicationOptions {
    /** Promotion and eviction preserve the selected conversation's content. */
    representationOnly?: boolean
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
        options?: ConversationPublicationOptions,
    ): void
    captureActivationRollback?(characterIds: readonly string[]): () => void
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
    summary: ConversationSummary
    viewportSource: PersistentConversationViewportSource
}

type SelectedConversationState =
    | CompleteSelectedConversationState
    | WindowedSelectedConversationState

interface WindowedCharacterHydration {
    character: CompleteCharacter
    persistedDetail: CharacterDetail
    selectedMetadata: PersistentConversationMetadata
    selectedSummary: ConversationSummary
}

type WindowedCharacterActivationResult = boolean | 'complete-fallback'
type WindowedCharacterHydrationResult =
    WindowedCharacterHydration | 'complete-fallback' | null

function captureCharacterDetail(character: CompleteCharacter): CharacterDetail {
    const detail = {} as CharacterDetail
    for (const key of Object.keys(character) as Array<
        keyof CompleteCharacter
    >) {
        if (key === 'chats') continue
        Object.defineProperty(detail, key, {
            configurable: true,
            enumerable: true,
            value: safeStructuredClone(character[key]),
            writable: true,
        })
    }
    return detail
}

function estimateActivationChangeBytes(
    change: WindowedConversationActivationChange,
): number {
    const serialized = JSON.stringify(change)
    return Math.max(
        1,
        Math.min(1_048_576, new TextEncoder().encode(serialized).byteLength),
    )
}

function matchesPublishedCharacter(
    expected: CompleteCharacter,
    published: CompleteCharacter,
    selectedConversationId: string,
    includeSelectedMessages: boolean,
): boolean {
    if (
        !isEqual(
            captureCharacterDetail(expected),
            captureCharacterDetail(published),
        ) ||
        expected.chats.length !== published.chats.length
    )
        return false
    for (let index = 0; index < expected.chats.length; index++) {
        const expectedConversation = expected.chats[index]
        const publishedConversation = published.chats[index]
        if (
            !publishedConversation ||
            !isEqual(
                cloneConversationMetadata(expectedConversation),
                cloneConversationMetadata(publishedConversation),
            )
        )
            return false
        if (
            includeSelectedMessages &&
            expectedConversation.id === selectedConversationId &&
            !isEqual(
                expectedConversation.message,
                publishedConversation.message,
            )
        )
            return false
    }
    return true
}

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
            summary: ConversationSummary
            viewportSource: PersistentConversationViewportSource
            windowedState: WindowedSelectedConversationState
        }
        try {
            const summary = createConversationSummaryFromMetadata(
                state.characterId,
                cloneConversationMetadata(state.conversation),
                conversationIndex,
                state.session.totalMessages,
                state.conversation.lastDate ??
                    state.conversation.message.at(-1)?.time ??
                    0,
            )
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
                summary,
                viewportSource,
                windowedState: {
                    kind: 'windowed',
                    stateToken: Symbol('windowed selected conversation'),
                    navigationGeneration: state.navigationGeneration,
                    characterId: state.characterId,
                    conversationId: state.conversationId,
                    conversation: shell,
                    authority,
                    summary,
                    viewportSource,
                },
            }
        } catch {
            return false
        }
        const { shell, nextCharacter, authority, viewportSource, windowedState } = prepared
        try {
            transition.call(this.dependencies.coordinator, () => {
                this.selectedConversationState = windowedState
                this.activeSession = null
                this.dependencies.publishConversation(
                    state.characterId,
                    shell,
                    nextCharacter,
                    { representationOnly: true },
                )
                const publishedCharacter =
                    this.dependencies.getResidentCharacter?.(state.characterId)
                const publishedConversation =
                    publishedCharacter?.chats[publishedCharacter.chatPage ?? 0]
                if (
                    !publishedCharacter ||
                    publishedCharacter.chaId !== state.characterId ||
                    !matchesPublishedCharacter(
                        nextCharacter,
                        publishedCharacter,
                        state.conversationId,
                        false,
                    ) ||
                    !publishedConversation ||
                    publishedConversation.id !== state.conversationId ||
                    !isMetadataOnlySelectedConversation(publishedConversation)
                ) {
                    throw new Error(
                        'Windowed selected conversation publication diverged',
                    )
                }
                windowedState.conversation = publishedConversation
                const adopted = this.dependencies.coordinator
                    .adoptWindowedSelectedConversation!(
                    authority.storeRevision,
                    this.dependencies.coordinator.mutationGeneration,
                    publishedCharacter,
                    authority,
                )
                if (!adopted) {
                    throw new Error(
                        'Windowed selected conversation was not adopted',
                    )
                }
            })
        } catch {
            this.selectedConversationState = state
            this.activeSession = state.session
            try {
                this.dependencies.publishConversation(
                    state.characterId,
                    state.conversation,
                    resident,
                    { representationOnly: true },
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

    fenceNavigation(): number {
        this.navigationGeneration++
        const state = this.selectedConversationState
        if (state) {
            state.navigationGeneration = this.navigationGeneration
            state.stateToken = Symbol('fenced selected conversation')
        }
        this.promotionFlight = null
        return this.navigationGeneration
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

        if (
            expectedSession.adoptPersistedMetadata(
                conversation,
                this.dependencies.coordinator.revision,
            )
        ) {
            resident!.chats[resident!.chatPage ?? 0] = state.conversation
            return true
        }

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
        if (this.canDirectlyActivateWindowed(options)) {
            const expectedFallbackGeneration = this.navigationGeneration + 1
            const direct = await this.activateWindowedCharacter(id, options)
            if (direct !== 'complete-fallback') return direct
            if (this.navigationGeneration !== expectedFallbackGeneration)
                return false
        }
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
        const generation = this.fenceNavigation()
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
        const persistedCharacterValue = safeStructuredClone(characterValue)
        if (characterValue.type === 'group' && missingRelatedIds.size > 0) {
            const groupValue = characterValue
            characterValue = {
                ...groupValue,
                ...removeGroupMemberReferences(groupValue, missingRelatedIds),
            }
            relatedIds = relatedIds.filter((memberId) => !missingRelatedIds.has(memberId))
        }
        characterValue = this.normalizeCharacterCandidate(
            characterValue,
            options.normalize,
            true,
        )
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
        if (!isEqual(persistedCharacterValue, characterValue)) {
            this.dependencies.coordinator.markPersistentDataDirty(
                Math.max(
                    1,
                    Math.min(
                        1_048_576,
                        new TextEncoder().encode(JSON.stringify(characterValue))
                            .byteLength,
                    ),
                ),
            )
        }
        return true
    }

    private async activateWindowedCharacter(
        id: string,
        options: CharacterActivationOptions,
    ): Promise<WindowedCharacterActivationResult> {
        if (!this.canDirectlyActivateWindowed(options))
            return 'complete-fallback'
        const generation = this.fenceNavigation()
        const previousCharacterId = this.dependencies.getSelectedCharacterId()
        const previousActiveIds =
            this.activeIds.size > 0
                ? new Set(this.activeIds)
                : new Set(previousCharacterId ? [previousCharacterId] : [])
        await this.dependencies.coordinator.flushPendingData(
            'activate-character',
        )
        if (
            generation !== this.navigationGeneration ||
            this.dependencies.canActivateWorkingSet?.() === false
        )
            return false
        if (!this.canDirectlyActivateWindowed(options))
            return 'complete-fallback'
        if (this.dependencies.getSelectedCharacterId() !== previousCharacterId)
            return false
        const previousState = this.selectedConversationState
        const revision = this.dependencies.coordinator.revision
        const mutationGeneration =
            this.dependencies.coordinator.mutationGeneration
        const hydrated = await this.hydrateWindowedCharacter(
            id,
            revision,
            mutationGeneration,
            generation,
            options,
        )
        if (!hydrated) return false
        if (hydrated === 'complete-fallback') return hydrated
        if (this.dependencies.getSelectedCharacterId() !== previousCharacterId)
            return false

        let relatedIds =
            hydrated.character.type === 'group'
                ? [...new Set(hydrated.character.characters)].filter(
                      (memberId) => memberId !== id,
                  )
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
            const values = await Promise.all(
                chunk.map(async (memberId) => {
                    try {
                        return await this.hydrateCharacterDetail(
                            memberId,
                            revision,
                            mutationGeneration,
                            generation,
                        )
                    } catch (error) {
                        if (error instanceof MissingCharacterError)
                            return undefined
                        throw error
                    }
                }),
            )
            relatedValues.push(...values)
            if (!this.isCurrent(generation, revision, mutationGeneration))
                return false
            if (!this.canDirectlyActivateWindowed(options))
                return 'complete-fallback'
            if (values.some((value) => value === null)) return false
        }
        if (relatedValues.some((value) => value === undefined)) {
            return 'complete-fallback'
        }

        const normalized = this.normalizeCharacterCandidate(
            hydrated.character,
            options.normalize,
        )
        if (
            this.dependencies.getSelectedCharacterId() !==
                previousCharacterId ||
            !this.isCurrent(generation, revision, mutationGeneration)
        )
            return false
        if (!this.canDirectlyActivateWindowed(options))
            return 'complete-fallback'
        const selectedConversation = normalized.chats[normalized.chatPage ?? 0]
        if (
            !selectedConversation ||
            selectedConversation.id !==
                hydrated.selectedMetadata.conversationId ||
            !isMetadataOnlySelectedConversation(selectedConversation)
        ) {
            throw new Error(
                'Windowed normalization changed selected conversation ownership',
            )
        }
        const beforeDetail = hydrated.persistedDetail
        const afterDetail = captureCharacterDetail(normalized)
        const beforeMetadata = hydrated.selectedMetadata.conversation
        const afterMetadata = cloneConversationMetadata(selectedConversation)
        const activationChange: WindowedConversationActivationChange = {}
        if (!isEqual(beforeDetail, afterDetail)) {
            activationChange.character = {
                before: beforeDetail,
                after: afterDetail,
            }
        }
        if (!isEqual(beforeMetadata, afterMetadata)) {
            activationChange.conversation = {
                before: beforeMetadata,
                after: afterMetadata,
            }
        }
        const activation =
            activationChange.character || activationChange.conversation
                ? activationChange
                : undefined
        const completeRelated = relatedValues.filter(
            (value): value is CharacterDetail =>
                value !== null && value !== undefined,
        )
        return this.publishWindowedSelection({
            character: normalized,
            metadata: hydrated.selectedMetadata,
            summary: hydrated.selectedSummary,
            revision,
            mutationGeneration,
            generation,
            previousState,
            previousActiveIds,
            nextActiveIds: new Set([id, ...relatedIds]),
            related: completeRelated,
            publishAsConversation: false,
            activation,
        })
    }

    activateConversation(id: string): Promise<boolean> {
        const selectedState = this.selectedConversationState
        if (
            selectedState?.kind === 'windowed' &&
            selectedState?.conversationId === id &&
            selectedState.characterId ===
                this.dependencies.getSelectedCharacterId()
        )
            return Promise.resolve(true)
        if (this.canDirectlyActivateWindowed()) {
            return this.startWindowedConversationActivation(id)
        }
        const preparation = this.prepareCompleteNavigation(
            `activate-conversation:${id}`,
        )
        if (!preparation) return this.startCompleteConversationActivation(id)
        return this.activateConversationAfterPreparation(id, preparation)
    }

    private async activateConversationAfterPreparation(
        id: string,
        preparation: Promise<CompleteConversationLease>,
    ): Promise<boolean> {
        let lease: CompleteConversationLease | null = null
        try {
            lease = await preparation
            return await this.startCompleteConversationActivation(id)
        } catch (error) {
            if (error instanceof SelectedConversationPromotionStaleError) return false
            throw error
        } finally {
            lease?.release()
        }
    }

    private startCompleteConversationActivation(id: string): Promise<boolean> {
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

    private startWindowedConversationActivation(id: string): Promise<boolean> {
        if (this.dependencies.canActivateWorkingSet?.() === false)
            return Promise.resolve(false)
        const characterId = this.dependencies.getSelectedCharacterId()
        if (!characterId)
            return Promise.reject(new Error('No character is selected'))
        const key = `${characterId}\u0000${id}`
        const existing = this.conversationFlights.get(key)
        if (existing?.generation === this.navigationGeneration)
            return existing.promise
        const generation = this.fenceNavigation()
        const pending = this.activateWindowedConversationOnce(
            characterId,
            id,
            generation,
        ).finally(() => {
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

    private async activateWindowedConversationOnce(
        characterId: string,
        id: string,
        generation: number,
    ): Promise<boolean> {
        const previousActiveIds = new Set(this.activeIds)
        await this.dependencies.coordinator.flushPendingData(
            'activate-conversation',
        )
        if (
            characterId !== this.dependencies.getSelectedCharacterId() ||
            !this.isCurrentWindowedActivation(generation)
        )
            return false
        const previousState = this.selectedConversationState
        const revision = this.dependencies.coordinator.revision
        const mutationGeneration =
            this.dependencies.coordinator.mutationGeneration
        const versioned =
            await this.dependencies.store.readConversationMetadata(
                characterId,
                id,
            )
        if (
            characterId !== this.dependencies.getSelectedCharacterId() ||
            !this.isCurrentWindowedActivation(
                generation,
                {},
                revision,
                mutationGeneration,
            )
        )
            return false
        if (!versioned)
            throw new Error(
                `Conversation ${id} was not found for ${characterId}`,
            )
        const resident = this.dependencies.getResidentCharacter?.(characterId)
        if (!resident) return false
        const conversationIndex = resident.chats.findIndex(
            (candidate) => candidate.id === id,
        )
        if (conversationIndex < 0) {
            throw new Error(
                `Conversation ${id} was not present in the active character`,
            )
        }
        const currentTarget = resident.chats[conversationIndex]
        const retainedCatalogSummary = getConversationSummaryStub(currentTarget)
        const metadata = this.requireValidConversationMetadata(
            versioned,
            characterId,
            id,
            revision,
            retainedCatalogSummary ?? undefined,
        )
        const retainedSummary =
            retainedCatalogSummary ??
            (previousState?.kind === 'windowed' &&
            previousState.conversation === currentTarget
                ? createConversationSummaryFromMetadata(
                      characterId,
                      cloneConversationMetadata(currentTarget),
                      conversationIndex,
                      previousState.authority.totalMessages,
                      previousState.summary.recentAt,
                      previousState.summary,
                  )
                : createConversationSummaryFromMetadata(
                      characterId,
                      cloneConversationMetadata(currentTarget),
                      conversationIndex,
                      currentTarget.message.length,
                      currentTarget.lastDate ??
                          currentTarget.message.at(-1)?.time ??
                          0,
                  ))
        const shell = createMetadataOnlySelectedConversation(
            metadata.conversation,
        )
        const chats = [...resident.chats]
        const previousIndex = resident.chatPage ?? 0
        const previousConversation = chats[previousIndex]
        if (
            previousConversation &&
            previousIndex !== conversationIndex &&
            this.dependencies.canReleaseConversation?.(
                resident,
                previousConversation.id ?? '',
                id,
            ) === true
        ) {
            if (
                previousState?.kind === 'windowed' &&
                previousState.characterId === characterId &&
                previousState.conversationId === previousConversation.id
            ) {
                chats[previousIndex] = createConversationSummaryStub(
                    createConversationSummaryFromMetadata(
                        characterId,
                        cloneConversationMetadata(previousState.conversation),
                        previousIndex,
                        previousState.authority.totalMessages,
                        previousState.summary.recentAt,
                        previousState.summary,
                    ),
                )
            } else {
                chats[previousIndex] = createConversationSummaryStubFromChat(
                    characterId,
                    previousConversation,
                    previousIndex,
                )
            }
        }
        chats[conversationIndex] = shell
        const nextCharacter = {
            ...resident,
            chats,
            chatPage: conversationIndex,
        } as CompleteCharacter
        const beforeDetail = captureCharacterDetail(resident)
        const afterDetail = captureCharacterDetail(nextCharacter)
        const activation: WindowedConversationActivationChange | undefined =
            isEqual(beforeDetail, afterDetail)
                ? undefined
                : { character: { before: beforeDetail, after: afterDetail } }
        return this.publishWindowedSelection({
            character: nextCharacter,
            metadata,
            summary: retainedSummary,
            revision,
            mutationGeneration,
            generation,
            previousState,
            previousActiveIds,
            nextActiveIds: new Set(
                previousActiveIds.size > 0 ? previousActiveIds : [characterId],
            ),
            related: [],
            publishAsConversation: true,
            activation,
        })
    }

    private canDirectlyActivateWindowed(
        options: CharacterActivationOptions = {},
    ): boolean {
        return (
            options.prepare === undefined &&
            this.dependencies.canActivateWorkingSet?.() !== false &&
            this.dependencies.canUseWindowedSelectedConversation?.() === true &&
            this.dependencies.isMaximumCompatibilityMode?.() !== true &&
            this.dependencies.isConversationOperationActive?.() !== true &&
            this.dependencies.coordinator.runSelectedConversationTransition !==
                undefined &&
            this.dependencies.coordinator.adoptWindowedSelectedConversation !==
                undefined &&
            this.dependencies.captureActivationRollback !== undefined
        )
    }

    private isCurrentWindowedActivation(
        generation: number,
        options: CharacterActivationOptions = {},
        revision?: DataRevision,
        mutationGeneration?: number,
    ): boolean {
        return (
            generation === this.navigationGeneration &&
            this.canDirectlyActivateWindowed(options) &&
            this.dependencies.canActivateWorkingSet?.() !== false &&
            (revision === undefined ||
                revision === this.dependencies.coordinator.revision) &&
            (mutationGeneration === undefined ||
                mutationGeneration ===
                    this.dependencies.coordinator.mutationGeneration)
        )
    }

    private normalizeCharacterCandidate(
        character: CompleteCharacter,
        normalize?: (character: CompleteCharacter) => CompleteCharacter,
        allowFirstConversation = false,
    ): CompleteCharacter {
        if (!normalize) return character
        const characterId = character.chaId
        const conversations = [...character.chats]
        const conversationIds = conversations.map(
            (conversation) => conversation.id,
        )
        const metadataOnly = conversations.map(
            isMetadataOnlySelectedConversation,
        )
        const messageOwners = conversations.map((conversation, index) =>
            metadataOnly[index] ? null : conversation.message,
        )
        const normalized = normalize(character)
        if (
            !normalized ||
            normalized.chaId !== characterId ||
            !Array.isArray(normalized.chats)
        ) {
            throw new Error(
                'Character normalization changed character ownership',
            )
        }
        if (
            allowFirstConversation &&
            conversations.length === 0 &&
            normalized.chats.length === 1
        )
            return normalized
        if (normalized.chats.length !== conversations.length) {
            throw new Error(
                'Character normalization changed conversation membership',
            )
        }
        for (let index = 0; index < conversations.length; index++) {
            const before = conversations[index]
            const after = normalized.chats[index]
            if (!after || after.id !== conversationIds[index]) {
                throw new Error(
                    'Character normalization changed conversation order',
                )
            }
            if (metadataOnly[index]) {
                if (!isMetadataOnlySelectedConversation(after)) {
                    throw new Error(
                        'Character normalization replaced metadata-only ownership',
                    )
                }
            } else if (after.message !== messageOwners[index]) {
                throw new Error(
                    'Character normalization changed message ownership',
                )
            }
        }
        return normalized
    }

    private requireValidConversationMetadata(
        versioned: {
            revision: DataRevision
            value: PersistentConversationMetadata
        },
        characterId: string,
        conversationId: string,
        revision: DataRevision,
        summary?: ConversationSummary,
    ): PersistentConversationMetadata {
        const value = versioned.value
        if (
            versioned.revision !== revision ||
            value.characterId !== characterId ||
            value.conversationId !== conversationId ||
            value.conversation.id !== conversationId ||
            Object.hasOwn(value.conversation, 'message') ||
            !Number.isSafeInteger(value.totalMessages) ||
            value.totalMessages < 0 ||
            (summary !== undefined &&
                summary.messageCount !== value.totalMessages)
        ) {
            throw new Error(
                `Conversation ${conversationId} returned invalid metadata`,
            )
        }
        return value
    }

    private async hydrateWindowedCharacter(
        id: string,
        revision: DataRevision,
        mutationGeneration: number,
        generation: number,
        options: CharacterActivationOptions,
    ): Promise<WindowedCharacterHydrationResult> {
        const detail = await this.dependencies.store.readCharacter(id)
        if (!this.isCurrent(generation, revision, mutationGeneration))
            return null
        if (!this.canDirectlyActivateWindowed(options))
            return 'complete-fallback'
        if (!detail)
            throw new MissingCharacterError(`Character ${id} was not found`)
        if (detail.revision !== revision) return null
        if (detail.value.chaId !== id) {
            throw new Error(
                `Character ${id} returned mismatched ID ${detail.value.chaId}`,
            )
        }

        const summaries: ConversationSummary[] = []
        const seenConversationIds = new Set<string>()
        let selectedSummary: ConversationSummary | undefined
        let cursor: string | undefined
        do {
            const page = await this.dependencies.store.queryConversations({
                characterId: id,
                order: 'configured',
                limit: 100,
                cursor,
            })
            if (!this.isCurrent(generation, revision, mutationGeneration))
                return null
            if (!this.canDirectlyActivateWindowed(options))
                return 'complete-fallback'
            if (page.revision !== revision) return null
            for (const summary of page.items) {
                if (
                    summary.characterId !== id ||
                    !Number.isSafeInteger(summary.messageCount) ||
                    summary.messageCount < 0 ||
                    seenConversationIds.has(summary.id)
                ) {
                    throw new Error(
                        `Conversation ${summary.id} returned invalid summary`,
                    )
                }
                seenConversationIds.add(summary.id)
                summaries.push(summary)
                if (summary.configuredIndex === (detail.value.chatPage ?? 0)) {
                    selectedSummary ??= summary
                }
            }
            cursor = page.nextCursor
        } while (cursor !== undefined)
        if (summaries.length === 0) return 'complete-fallback'
        selectedSummary ??= summaries[0]
        const selectedIndex = summaries.indexOf(selectedSummary)
        const metadataVersion =
            await this.dependencies.store.readConversationMetadata(
                id,
                selectedSummary.id,
            )
        if (!this.isCurrent(generation, revision, mutationGeneration))
            return null
        if (!this.canDirectlyActivateWindowed(options))
            return 'complete-fallback'
        if (!metadataVersion) {
            throw new Error(
                `Conversation ${selectedSummary.id} was not found for ${id}`,
            )
        }
        const selectedMetadata = this.requireValidConversationMetadata(
            metadataVersion,
            id,
            selectedSummary.id,
            revision,
            selectedSummary,
        )
        const chats = summaries.map(createConversationSummaryStub)
        chats[selectedIndex] = createMetadataOnlySelectedConversation(
            selectedMetadata.conversation,
        )
        return {
            character: {
                ...safeStructuredClone(detail.value),
                chats,
                chatPage: selectedIndex,
            } as CompleteCharacter,
            persistedDetail: safeStructuredClone(detail.value),
            selectedMetadata,
            selectedSummary: safeStructuredClone(selectedSummary),
        }
    }

    private publishWindowedSelection(input: {
        character: CompleteCharacter
        metadata: PersistentConversationMetadata
        summary: ConversationSummary
        revision: DataRevision
        mutationGeneration: number
        generation: number
        previousState: SelectedConversationState | null
        previousActiveIds: ReadonlySet<string>
        nextActiveIds: Set<string>
        related: CharacterDetail[]
        publishAsConversation: boolean
        activation?: WindowedConversationActivationChange
    }): boolean {
        if (
            !this.isCurrentWindowedActivation(
                input.generation,
                {},
                input.revision,
                input.mutationGeneration,
            )
        )
            return false
        const conversation =
            input.character.chats[input.character.chatPage ?? 0]
        if (
            !conversation ||
            conversation.id !== input.metadata.conversationId ||
            !isMetadataOnlySelectedConversation(conversation)
        )
            return false
        const authority: WindowedConversationPersistenceAuthority = {
            kind: 'windowed',
            characterId: input.character.chaId,
            conversationId: input.metadata.conversationId,
            sessionToken: createConversationSessionToken(),
            storeRevision: input.revision,
            persistedSessionVersion: 0,
            sessionVersion: 0,
            totalMessages: input.metadata.totalMessages,
        }
        const rollbackIds = [
            ...new Set([
                ...input.previousActiveIds,
                ...input.nextActiveIds,
                input.character.chaId,
                ...input.related.map((detail) => detail.chaId),
            ]),
        ]
        const restorePublication =
            this.dependencies.captureActivationRollback?.(rollbackIds)
        const transition =
            this.dependencies.coordinator.runSelectedConversationTransition
        const adopt =
            this.dependencies.coordinator.adoptWindowedSelectedConversation
        if (!restorePublication || !transition || !adopt) return false
        const viewportSource = new PersistentConversationViewportSource({
            reader: this.dependencies.store,
            characterId: authority.characterId,
            conversationId: authority.conversationId,
            revision: authority.storeRevision,
            totalMessages: authority.totalMessages,
            rowBudget: this.dependencies.conversationViewportRowBudget ?? 64,
        })
        const windowedState: WindowedSelectedConversationState = {
            kind: 'windowed',
            stateToken: Symbol('direct windowed selected conversation'),
            navigationGeneration: input.generation,
            characterId: authority.characterId,
            conversationId: authority.conversationId,
            conversation,
            authority,
            summary: safeStructuredClone(input.summary),
            viewportSource,
        }
        try {
            transition.call(this.dependencies.coordinator, () => {
                this.selectedConversationState = windowedState
                this.activeSession = null
                if (input.publishAsConversation) {
                    this.dependencies.publishConversation(
                        input.character.chaId,
                        conversation,
                        input.character,
                    )
                } else if (input.related.length > 0) {
                    this.dependencies.publishCharacterSet(
                        input.character,
                        input.related,
                    )
                } else {
                    this.dependencies.publishCharacter(input.character)
                }
                const publishedCharacter =
                    this.dependencies.getResidentCharacter?.(
                        input.character.chaId,
                    )
                const publishedConversation =
                    publishedCharacter?.chats[publishedCharacter.chatPage ?? 0]
                if (
                    !publishedCharacter ||
                    publishedCharacter.chaId !== input.character.chaId ||
                    !matchesPublishedCharacter(
                        input.character,
                        publishedCharacter,
                        authority.conversationId,
                        false,
                    ) ||
                    !publishedConversation ||
                    publishedConversation.id !== authority.conversationId ||
                    !isMetadataOnlySelectedConversation(publishedConversation)
                ) {
                    throw new Error(
                        'Windowed selected conversation publication diverged',
                    )
                }
                windowedState.conversation = publishedConversation
                const adopted = adopt.call(
                    this.dependencies.coordinator,
                    input.revision,
                    input.mutationGeneration,
                    publishedCharacter,
                    authority,
                    input.activation,
                )
                if (!adopted) {
                    throw new Error(
                        'Windowed selected conversation was not adopted',
                    )
                }
            })
        } catch (error) {
            this.selectedConversationState = input.previousState
            this.activeSession =
                input.previousState?.kind === 'complete'
                    ? input.previousState.session
                    : null
            viewportSource.dispose()
            try {
                restorePublication()
            } catch (rollbackError) {
                input.previousState?.viewportSource.dispose()
                if (input.previousState?.kind === 'complete') {
                    input.previousState.session.invalidate()
                }
                this.selectedConversationState = null
                this.activeSession = null
                this.promotionFlight = null
                this.notifyActiveConversationViewportSource()
                throw rollbackError
            }
            return false
        }

        if (input.previousState && input.previousState !== windowedState) {
            input.previousState.viewportSource.dispose()
            if (input.previousState.kind === 'complete')
                input.previousState.session.invalidate()
        }
        this.activeIds = input.nextActiveIds
        for (const previousId of input.previousActiveIds) {
            if (!input.nextActiveIds.has(previousId)) {
                this.dependencies.releaseInactiveCharacter?.(previousId)
            }
        }
        this.notifyActiveConversationViewportSource()
        if (input.activation) {
            this.dependencies.coordinator.markPersistentDataDirty(
                estimateActivationChangeBytes(input.activation),
            )
        }
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
        initialState: WindowedSelectedConversationState,
        initialTarget: SelectedConversationTarget,
        reason: string,
    ): Promise<CompleteSelectedConversationState> {
        await this.dependencies.coordinator.flushPendingData(
            `complete-selected-conversation:${reason}`,
        )
        let state = initialState
        let target = initialTarget
        if (this.selectedConversationState !== initialState) {
            const current = this.selectedConversationState
            if (
                current?.kind !== 'windowed' ||
                current.characterId !== initialState.characterId ||
                current.conversationId !== initialState.conversationId ||
                current.navigationGeneration !==
                    initialTarget.navigationGeneration ||
                current.authority.sessionToken !==
                    initialState.authority.sessionToken ||
                current.authority.sessionVersion !==
                    initialState.authority.sessionVersion ||
                current.authority.persistedSessionVersion !==
                    initialState.authority.persistedSessionVersion
            )
                throw new SelectedConversationPromotionStaleError()
            const currentTarget = this.captureSelectedConversationTarget()
            if (!currentTarget)
                throw new SelectedConversationPromotionStaleError()
            state = current
            target = currentTarget
        }
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
        const transition = this.dependencies.coordinator.runSelectedConversationTransition
        if (!transition) {
            throw new Error('Selected conversation transition is unavailable')
        }
        let complete: CompleteSelectedConversationState | null = null
        try {
            transition.call(this.dependencies.coordinator, () => {
                this.selectedConversationState = null
                this.activeSession = null
                this.dependencies.publishConversation(
                    state.characterId,
                    conversation,
                    nextCharacter,
                    { representationOnly: true },
                )
                const publishedCharacter =
                    this.dependencies.getResidentCharacter?.(state.characterId)
                const publishedConversation =
                    publishedCharacter?.chats[publishedCharacter.chatPage ?? 0]
                if (
                    !publishedCharacter ||
                    publishedCharacter.chaId !== state.characterId ||
                    !matchesPublishedCharacter(
                        nextCharacter,
                        publishedCharacter,
                        state.conversationId,
                        true,
                    ) ||
                    !publishedConversation ||
                    publishedConversation.id !== state.conversationId ||
                    isMetadataOnlySelectedConversation(publishedConversation)
                ) {
                    throw new Error(
                        'Complete selected conversation publication diverged',
                    )
                }
                complete = this.createCompleteSelectedConversationState(
                    state.characterId,
                    publishedConversation,
                    state.authority.storeRevision,
                    state.navigationGeneration,
                )
                this.selectedConversationState = complete
                this.activeSession = complete.session
                const adopted = this.dependencies.coordinator.adoptHydratedCharacter(
                    state.authority.storeRevision,
                    this.dependencies.coordinator.mutationGeneration,
                    publishedCharacter,
                )
                if (!adopted) {
                    throw new Error(
                        'The complete selected conversation was not adopted',
                    )
                }
            })
        } catch (error) {
            this.selectedConversationState = state
            this.activeSession = null
            complete?.viewportSource.dispose()
            complete?.session.invalidate()
            try {
                this.dependencies.publishConversation(
                    state.characterId,
                    state.conversation,
                    resident,
                    { representationOnly: true },
                )
            } catch {
                this.clearActiveConversationSession()
            }
            throw error
        }
        if (!complete)
            throw new Error('Complete selected conversation was not published')
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
