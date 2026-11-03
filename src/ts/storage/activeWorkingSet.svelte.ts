import type { Chat, Database, Message, character } from './database.svelte'
import type { CommittedApplyOutcome } from './persistentDataRuntime'
import { safeStructuredClone } from '../safeStructuredClone'
import { replaceArrayRange } from '../arrayRange'
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
    PersistentRevisionReader,
} from './persistentDataStore'
import { readCurrentRevision } from './persistentDataStore'
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
import { isConversationStreaming } from './streamingConversationRegistry'

type CompleteCharacter = character

const CONVERSATION_HYDRATION_CONCURRENCY = 8
const WINDOWED_PUBLISH_SETTLE_ATTEMPTS = 8
const WINDOWED_CHARACTER_ACTIVATION_RETRIES = 3

class MissingCharacterError extends Error {}

export interface WorkingSetCoordinator {
    readonly revision: DataRevision
    readonly mutationGeneration: number
    initialize(revision: DataRevision, database?: Database): void
    flushPendingData(reason: string): Promise<void>
    retireWindowedSelectedConversation?(): void
    replacePersistentDatabase(database: Database, reason: string): Promise<CommittedApplyOutcome>
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
    recordWindowedChatListChange?(
        authority: WindowedConversationPersistenceAuthority,
        before: CompleteCharacter,
        after: CompleteCharacter,
    ): boolean
}

export type WindowedChatListEditResult =
    | { kind: 'applied'; nextId: string | null }
    | { kind: 'refused' }
    | { kind: 'unsupported' }

export interface CharacterActivationOptions {
    prepare?(): Promise<{ database: Database; reason: string } | null>
    normalize?(character: CompleteCharacter): CompleteCharacter
    onRetryExhausted?(navigationGeneration: number): void
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
    isConversationOperationActive?(): boolean
    subscribeConversationOperationActive?(listener: (active: boolean) => void): () => void
    conversationViewportRowBudget?: number
    conversationPageSize?: number
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

export interface WindowedConversationMutationController {
    readonly chat: Chat
    readonly absoluteStartIndex: number
    isCurrent(): boolean
    applyRange(
        localStart: number,
        deleteCount: number,
        messages: readonly Message[],
        command: 'append' | 'edit' | 'replace-range' | 'update-metadata',
    ): boolean
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

type WindowedCharacterActivationResult = boolean | 'complete-fallback' | 'retry'
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
    private readonly windowedControllers = new Set<WindowedConversationMutationController>()
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

    async prepareWindowedMetadataAdoption(reader: PersistentRevisionReader): Promise<() => void> {
        const state = this.selectedConversationState
        if (state?.kind !== 'windowed' ||
            state.authority.sessionVersion !== state.authority.persistedSessionVersion ||
            reader.revision < state.authority.storeRevision) return () => {}
        const authority = state.authority
        const adoptions: Array<{ controller: WindowedConversationMutationController; messages: Message[] }> = []
        for (const controller of this.windowedControllers) {
            if (!controller.isCurrent()) {
                this.windowedControllers.delete(controller)
                continue
            }
            const messages: Message[] = []
            const length = controller.chat.message.length
            for (let offset = 0; offset < length;) {
                const startIndex = controller.absoluteStartIndex + offset
                const page = await reader.readConversationWindow({
                    characterId: state.characterId, conversationId: state.conversationId,
                    startIndex, limit: Math.min(100, length - offset),
                })
                if (!page || page.revision !== reader.revision || page.value.startIndex !== startIndex ||
                    page.value.characterId !== state.characterId || page.value.conversationId !== state.conversationId ||
                    page.value.totalMessages !== authority.totalMessages || page.value.messages.length === 0) {
                    controller.release()
                    break
                }
                messages.push(...page.value.messages)
                offset += page.value.messages.length
            }
            const contentKeys = ['role', 'data', 'saying', 'chatId', 'name', 'disabled', 'isComment'] as const
            if (messages.length !== length || !messages.every((message, index) =>
                contentKeys.every((key) => Object.is(message[key], controller.chat.message[index]?.[key])))) {
                controller.release()
                continue
            }
            adoptions.push({ controller, messages })
        }
        return () => {
            const current = this.selectedConversationState
            if (current?.kind !== 'windowed' || current.authority.sessionToken !== authority.sessionToken ||
                current.authority.sessionVersion !== authority.sessionVersion ||
                current.authority.storeRevision !== reader.revision) return
            for (const { controller, messages } of adoptions) {
                if (controller.isCurrent()) replaceArrayRange(controller.chat.message, 0, messages.length, safeStructuredClone(messages))
            }
        }
    }

    captureWindowedConversationMutationController(
        target: SelectedConversationTarget,
        chat: Chat,
        absoluteStartIndex: number,
        countPreserving = false,
    ): WindowedConversationMutationController | null {
        const initialState = this.selectedConversationState
        if (
            initialState?.kind !== 'windowed'
            || !this.matchesTarget(initialState, target)
            || !Number.isSafeInteger(absoluteStartIndex)
            || absoluteStartIndex < 0
            || (countPreserving
                ? absoluteStartIndex + chat.message.length > initialState.authority.totalMessages
                : absoluteStartIndex + chat.message.length !== initialState.authority.totalMessages)
        ) return null
        let released = false
        const navigationGeneration = initialState.navigationGeneration
        let expectedSessionVersion = initialState.authority.sessionVersion
        const requireState = (): WindowedSelectedConversationState | null => {
            const state = this.selectedConversationState
            return !released
                && state?.kind === 'windowed'
                && state.navigationGeneration === navigationGeneration
                && state.characterId === initialState.characterId
                && state.conversationId === initialState.conversationId
                && state.authority.sessionToken === initialState.authority.sessionToken
                && state.authority.sessionVersion === expectedSessionVersion
                && (countPreserving
                    ? state.authority.totalMessages === initialState.authority.totalMessages
                    : state.authority.totalMessages === absoluteStartIndex + chat.message.length)
                ? state
                : null
        }
        const syncMetadata = (state: WindowedSelectedConversationState) => {
            const metadata = cloneConversationMetadata(chat) as Record<string, unknown>
            const shell = state.conversation as unknown as Record<string, unknown>
            for (const key of Object.keys(shell)) {
                if (key !== 'message' && !Object.hasOwn(metadata, key)) delete shell[key]
            }
            for (const [key, value] of Object.entries(metadata)) {
                if (key !== 'message') shell[key] = safeStructuredClone(value)
            }
            return metadata
        }
        const restoreObject = (
            target: Record<string, unknown>,
            snapshot: Record<string, unknown>,
        ) => {
            for (const key of Object.keys(target)) {
                if (!Object.hasOwn(snapshot, key)) delete target[key]
            }
            for (const [key, value] of Object.entries(snapshot)) {
                target[key] = safeStructuredClone(value)
            }
        }
        const controller: WindowedConversationMutationController = {
            chat,
            absoluteStartIndex,
            isCurrent: () => requireState() !== null,
            applyRange: (localStart, deleteCount, messages, command) => {
                const state = requireState()
                if (
                    !state
                    || !this.dependencies.coordinator.recordActiveConversationMutation
                    || !Number.isSafeInteger(localStart)
                    || localStart < 0
                    || !Number.isSafeInteger(deleteCount)
                    || deleteCount < 0
                    || localStart + deleteCount > chat.message.length
                    || (countPreserving && deleteCount !== messages.length)
                ) return false
                const replacedMessages = chat.message.slice(localStart, localStart + deleteCount)
                const shellSnapshot = safeStructuredClone(
                    state.conversation,
                ) as unknown as Record<string, unknown>
                const authoritySnapshot = state.authority
                const summarySnapshot = state.summary
                const previousVersion = state.authority.sessionVersion
                const sessionVersion = previousVersion + 1
                const detachedMessages = safeStructuredClone([...messages])
                let rollbackViewport = () => {}
                try {
                    replaceArrayRange(
                        chat.message,
                        localStart,
                        deleteCount,
                        safeStructuredClone(detachedMessages),
                    )
                    const metadata = syncMetadata(state)
                    state.authority = {
                        ...state.authority,
                        sessionVersion,
                        totalMessages:
                            state.authority.totalMessages - deleteCount + detachedMessages.length,
                    }
                    state.summary = {
                        ...state.summary,
                        messageCount: state.authority.totalMessages,
                        recentAt:
                            chat.lastDate
                            ?? detachedMessages.at(-1)?.time
                            ?? state.summary.recentAt,
                    }
                    rollbackViewport = state.viewportSource.applyOptimisticRange(
                        absoluteStartIndex + localStart,
                        deleteCount,
                        detachedMessages,
                    )
                    this.dependencies.coordinator.recordActiveConversationMutation({
                        characterId: state.characterId,
                        conversationId: state.conversationId,
                        sessionToken: state.authority.sessionToken,
                        previousVersion,
                        sessionVersion,
                        commands: [command],
                        mutations: [{
                            start: absoluteStartIndex + localStart,
                            deleteCount,
                            messages: detachedMessages,
                            sessionVersion,
                        }],
                        conversation: metadata,
                    })
                    expectedSessionVersion = sessionVersion
                    return true
                } catch (error) {
                    rollbackViewport()
                    replaceArrayRange(chat.message, localStart, detachedMessages.length, replacedMessages)
                    restoreObject(
                        state.conversation as unknown as Record<string, unknown>,
                        shellSnapshot,
                    )
                    state.authority = authoritySnapshot
                    state.summary = summarySnapshot
                    throw error
                }
            },
            release: () => {
                released = true
                this.windowedControllers.delete(controller)
            },
        }
        for (const previous of this.windowedControllers) {
            if (!previous.isCurrent()) this.windowedControllers.delete(previous)
        }
        this.windowedControllers.add(controller)
        return controller
    }

    async captureWindowedMessageMutation(
        target: SelectedConversationTarget,
        absoluteIndex: number,
        evidence: Readonly<Message>,
    ): Promise<WindowedConversationMutationController | null> {
        const state = this.selectedConversationState
        if (state?.kind !== 'windowed' || !this.matchesTarget(state, target) ||
            state.authority.sessionVersion !== state.authority.persistedSessionVersion ||
            !Number.isSafeInteger(absoluteIndex) || absoluteIndex < 0 || absoluteIndex >= state.authority.totalMessages) return null
        const version = state.authority.sessionVersion
        const lease = await this.dependencies.store.acquireRevision(target.storeRevision)
        try {
            const row = await lease.readConversationWindow({
                characterId: target.characterId, conversationId: target.conversationId, startIndex: absoluteIndex, limit: 1,
            })
            if (!row || row.revision !== target.storeRevision || row.value.characterId !== target.characterId ||
                row.value.conversationId !== target.conversationId || row.value.totalMessages !== state.authority.totalMessages ||
                row.value.startIndex !== absoluteIndex || row.value.endIndex !== absoluteIndex + 1 ||
                row.value.messages.length !== 1 || !isEqual(row.value.messages[0], evidence)) return null
            // A later response carrier can own this row; keep its multi-row edit on the complete path.
            let cursor = absoluteIndex
            let stopped = false
            while (cursor < state.authority.totalMessages && !stopped) {
                const page = await lease.readConversationWindow({
                    characterId: target.characterId, conversationId: target.conversationId, startIndex: cursor, limit: 64,
                })
                if (!page || page.revision !== target.storeRevision || page.value.characterId !== target.characterId ||
                    page.value.conversationId !== target.conversationId || page.value.totalMessages !== state.authority.totalMessages ||
                    page.value.startIndex !== cursor || page.value.messages.length === 0 || page.value.messages.length > 64 ||
                    page.value.endIndex !== cursor + page.value.messages.length || page.value.endIndex > state.authority.totalMessages) return null
                for (let index = 0; index < page.value.messages.length; index++) {
                    const message = page.value.messages[index]
                    const end = cursor + index
                    if (end > absoluteIndex && message.role === 'user' && !message.isComment) { stopped = true; break }
                    const variants = message.responseVariants
                    const selected = variants?.candidates.find((candidate) => candidate.id === variants.selectedId)
                    if (selected && absoluteIndex >= end - selected.messages.length + 1 && end !== absoluteIndex) return null
                }
                cursor += page.value.messages.length
            }
            if (this.selectedConversationState !== state || !this.matchesTarget(state, target) || state.authority.sessionVersion !== version) return null
            const chat = { ...cloneConversationMetadata(state.conversation), message: safeStructuredClone(row.value.messages) } as Chat
            return this.captureWindowedConversationMutationController(target, chat, absoluteIndex, true)
        } finally {
            await lease.release()
        }
    }

    /**
     * Applies a chat-list edit while the selected conversation stays windowed. `edit`
     * runs on a draft, so an 'unsupported' result leaves the working set untouched
     * and the caller can repeat the edit on the complete path.
     */
    editWindowedChatList(
        target: SelectedConversationTarget,
        edit: (character: CompleteCharacter) => string | null | false,
    ): WindowedChatListEditResult {
        const state = this.selectedConversationState
        const coordinator = this.dependencies.coordinator
        if (
            state?.kind !== 'windowed' ||
            !this.matchesTarget(state, target) ||
            this.promotionFlight !== null ||
            // A running generation writes the selected chat's metadata from its own copy.
            this.dependencies.isConversationOperationActive?.() === true ||
            !coordinator.recordWindowedChatListChange
        ) return { kind: 'unsupported' }
        const resident = this.dependencies.getResidentCharacter?.(state.characterId)
        if (
            !resident ||
            resident.chats.findIndex((conversation) => conversation === state.conversation) !==
                (resident.chatPage ?? 0)
        ) return { kind: 'unsupported' }

        const bodies = new Map<string | undefined, { message: Message[]; length: number }>()
        for (const conversation of resident.chats) {
            if (conversation === state.conversation) continue
            bodies.set(conversation.id, {
                message: conversation.message,
                length: conversation.message.length,
            })
        }
        const draft = {
            ...captureCharacterDetail(resident),
            chats: resident.chats.map((conversation) => ({ ...conversation })),
        } as CompleteCharacter
        const nextId = edit(draft)
        if (nextId === false) return { kind: 'refused' }
        const selectedIndex = draft.chats.findIndex(
            (conversation) => conversation.id === state.conversationId,
        )
        if (selectedIndex < 0 || Object.hasOwn(draft.chats[selectedIndex], 'message')) {
            return { kind: 'unsupported' }
        }
        draft.chatPage = selectedIndex
        for (const conversation of draft.chats) {
            if (conversation.id === state.conversationId) continue
            const body = bodies.get(conversation.id)
            if (
                body &&
                (conversation.message !== body.message ||
                    body.message.length !== body.length)
            ) return { kind: 'unsupported' }
        }
        if (!coordinator.recordWindowedChatListChange(
            { ...state.authority },
            resident,
            draft,
        )) return { kind: 'unsupported' }

        const originals = new Map(
            resident.chats.map((conversation) => [conversation.id, conversation]),
        )
        const syncOwnProperties = (
            target: Record<string, unknown>,
            source: Record<string, unknown>,
            excludedKey: string,
        ) => {
            for (const key of Object.keys(target)) {
                if (key !== excludedKey && !Object.hasOwn(source, key)) delete target[key]
            }
            for (const key of Object.keys(source)) {
                if (key !== excludedKey && !isEqual(target[key], source[key])) {
                    target[key] = source[key]
                }
            }
        }
        syncOwnProperties(
            resident as unknown as Record<string, unknown>,
            draft as unknown as Record<string, unknown>,
            'chats',
        )
        // Keep the existing chat objects, including the selected windowed shell.
        resident.chats = draft.chats.map((conversation) => {
            const original = originals.get(conversation.id)
            if (!original) return conversation
            syncOwnProperties(
                original as unknown as Record<string, unknown>,
                conversation as unknown as Record<string, unknown>,
                'message',
            )
            return original
        })
        return { kind: 'applied', nextId }
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
            this.dependencies.isConversationOperationActive?.() === true ||
            this.dependencies.coordinator.hasPendingPersistenceWork !== false ||
            !transition ||
            !this.dependencies.coordinator.adoptWindowedSelectedConversation ||
            state.session.version !== state.session.persistedVersion ||
            state.session.storeRevision !== this.dependencies.coordinator.revision ||
            state.session.isTransactionActive ||
            state.session.activePinReasons.some((reason) => reason !== 'viewport') ||
            state.conversation.isStreaming === true
            || isConversationStreaming(state.conversationId)
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

    advanceStoreRevision(revision: DataRevision, totalMessages?: number, preserveRows = false): void {
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
        const authority = { ...state.authority, storeRevision: revision, totalMessages: totalMessages ?? state.authority.totalMessages }
        if (preserveRows && authority.totalMessages === state.authority.totalMessages) {
            const previousAuthority = state.authority
            state.authority = authority
            if (!advance.call(this.dependencies.coordinator, revision, authority)) {
                state.authority = previousAuthority
                throw new Error('Windowed selected conversation revision was not adopted')
            }
            state.viewportSource.advanceUnchangedRevision(revision)
            return
        }
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
            summary: authority.totalMessages === state.authority.totalMessages
                ? state.summary : { ...state.summary, messageCount: authority.totalMessages },
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
        const windowed = this.selectedConversationState
        if (
            windowed?.kind === 'windowed'
            && windowed.characterId === event.characterId
            && windowed.conversationId === event.conversationId
            && windowed.authority.sessionToken === event.sessionToken
            && event.sessionVersion > windowed.authority.persistedSessionVersion
            && event.sessionVersion <= windowed.authority.sessionVersion
        ) {
            windowed.authority = {
                ...windowed.authority,
                persistedSessionVersion: event.sessionVersion,
            }
            return true
        }
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

    acknowledgeConversationMutationFallbackPersisted(
        event: PersistedConversationMutationEvent,
    ): boolean {
        const session = this.activeSession
        if (
            !session ||
            session.characterId !== event.characterId ||
            session.conversationId !== event.conversationId ||
            !session.ownsSessionToken(event.sessionToken)
        ) return false
        const acknowledged = session.acknowledgeFallbackPersisted(
            event.sessionToken,
            event.sessionVersion,
            event.revision,
        )
        if (acknowledged) this.notifyActiveConversationViewportSource()
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
        this.activeIds = new Set([selected.chaId])
        return this.activeCharacterIds
    }

    invalidateNavigation(): void {
        this.navigationGeneration++
        this.clearActiveConversationSession()
    }

    fenceNavigation(): number {
        this.navigationGeneration++
        this.refreshSelectedConversationNavigation(this.navigationGeneration)
        return this.navigationGeneration
    }

    private refreshSelectedConversationNavigation(generation: number): void {
        if (generation !== this.navigationGeneration) return
        const state = this.selectedConversationState
        if (state) {
            state.navigationGeneration = generation
            state.stateToken = Symbol('fenced selected conversation')
        }
        this.promotionFlight = null
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
        const generation = this.fenceNavigation()
        const activeIds = this.activeIds.size > 0
            ? new Set(this.activeIds)
            : new Set(
                this.dependencies.getSelectedCharacterId()
                    ? [this.dependencies.getSelectedCharacterId()!]
                    : [],
            )
        await this.dependencies.coordinator.flushPendingData('deactivate-working-set')
        if (generation !== this.navigationGeneration) return false
        // Another save can queue while the awaited flush is completing.
        for (
            let attempt = 0;
            attempt < WINDOWED_PUBLISH_SETTLE_ATTEMPTS &&
            this.dependencies.coordinator.hasPendingPersistenceWork === true;
            attempt++
        ) {
            await this.dependencies.coordinator.flushPendingData('deactivate-working-set')
            if (generation !== this.navigationGeneration) return false
        }
        if (this.dependencies.coordinator.hasPendingPersistenceWork === true) return false
        if (this.dependencies.canDeactivateWorkingSet?.() === false) return false
        if ([...activeIds].some(
            (id) => this.dependencies.canDeactivateCharacter?.(id) === false,
        )) return false
        this.dependencies.coordinator.retireWindowedSelectedConversation?.()
        this.activeIds = new Set()
        this.clearActiveConversationSession()
        for (const id of activeIds) this.dependencies.releaseInactiveCharacter?.(id)
        return true
    }

    async initializeActiveWorkingSet(database: Database): Promise<void> {
        await this.dependencies.store.open()
        this.installCommittedWorkingSet(database, await readCurrentRevision(this.dependencies.store))
    }

    installCommittedWorkingSet(database: Database, revision: DataRevision, metadata?: PersistentConversationMetadata): void {
        const selectedId = this.dependencies.getSelectedCharacterId()
        this.activeIds = selectedId ? new Set([selectedId]) : new Set()
        const selected = selectedId
            ? database.characters.find((character) => character.chaId === selectedId)
            : undefined
        const conversation = selected?.chats[selected.chatPage ?? 0]
        if (selected && conversation && isMetadataOnlySelectedConversation(conversation)) {
            if (!metadata || metadata.characterId !== selected.chaId || metadata.conversationId !== conversation.id) {
                throw new Error('Windowed installation requires current conversation metadata')
            }
            const authority: WindowedConversationPersistenceAuthority = {
                kind: 'windowed', characterId: selected.chaId, conversationId: metadata.conversationId,
                sessionToken: createConversationSessionToken(), storeRevision: revision,
                persistedSessionVersion: 0, sessionVersion: 0, totalMessages: metadata.totalMessages,
            }
            const viewportSource = new PersistentConversationViewportSource({
                reader: this.dependencies.store, characterId: selected.chaId,
                conversationId: metadata.conversationId, revision, totalMessages: metadata.totalMessages,
                rowBudget: this.dependencies.conversationViewportRowBudget ?? 64,
            })
            this.clearActiveConversationSession()
            this.selectedConversationState = {
                kind: 'windowed', stateToken: Symbol('committed windowed selected conversation'),
                navigationGeneration: this.navigationGeneration, characterId: selected.chaId,
                conversationId: metadata.conversationId, conversation, authority, viewportSource,
                summary: createConversationSummaryFromMetadata(selected.chaId, metadata.conversation,
                    selected.chatPage ?? 0, metadata.totalMessages, conversation.lastDate ?? 0),
            }
            try {
                this.dependencies.coordinator.initialize(revision)
            } catch (error) {
                this.clearActiveConversationSession()
                throw error
            }
            this.notifyActiveConversationViewportSource()
            return
        }
        this.dependencies.coordinator.initialize(revision, database)
        if (selected && conversation) {
            this.publishActiveConversationSession(selected.chaId, conversation, revision)
        } else this.clearActiveConversationSession()
    }

    async activateCharacter(
        id: string,
        options: CharacterActivationOptions = {},
    ): Promise<boolean> {
        if (this.canDirectlyActivateWindowed(options)) {
            for (let attempt = 0; ; attempt++) {
                const expectedFallbackGeneration = this.navigationGeneration + 1
                const direct = await this.activateWindowedCharacter(id, options)
                if (direct === 'retry') {
                    // Background persistence moved the revision under this
                    // activation; read again unless a newer navigation started.
                    if (
                        attempt < WINDOWED_CHARACTER_ACTIVATION_RETRIES &&
                        this.navigationGeneration === expectedFallbackGeneration
                    )
                        continue
                    if (this.navigationGeneration === expectedFallbackGeneration) {
                        options.onRetryExhausted?.(this.navigationGeneration)
                    }
                    return false
                }
                if (direct !== 'complete-fallback') return direct
                if (this.navigationGeneration !== expectedFallbackGeneration)
                    return false
                break
            }
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
            const outcome = await this.dependencies.coordinator.replacePersistentDatabase(
                prepared.database,
                prepared.reason,
            )
            if (
                outcome.projection === 'refresh-required' ||
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
        const persistedCharacterValue = safeStructuredClone(characterValue)

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
        this.dependencies.publishCharacter(characterValue)
        const selectedConversation = characterValue.chats[characterValue.chatPage ?? 0]
        if (selectedConversation) {
            this.publishActiveConversationSession(id, selectedConversation, revision)
        } else this.clearActiveConversationSession()
        const nextActiveIds = new Set([id])
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

        const settled = await this.settleBeforeWindowedPublish(
            generation,
            revision,
            mutationGeneration,
        )
        if (settled !== true) return settled

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
        if (!isEqual({ ...beforeDetail, chatFolders: beforeDetail.chatFolders ?? [] }, afterDetail)) {
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
        return this.publishWindowedSelection({
            character: normalized,
            metadata: hydrated.selectedMetadata,
            summary: hydrated.selectedSummary,
            revision,
            mutationGeneration,
            generation,
            previousState,
            previousActiveIds,
            nextActiveIds: new Set([id]),
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
        const pending = this.activateConversationOnce(characterId, id, generation)
            .then(
                (activated) => {
                    if (!activated) {
                        this.refreshSelectedConversationNavigation(generation)
                    }
                    return activated
                },
                (error) => {
                    this.refreshSelectedConversationNavigation(generation)
                    throw error
                },
            )
            .finally(() => {
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
            (await this.settleBeforeWindowedPublish(
                generation,
                revision,
                mutationGeneration,
            )) !== true
        )
            return false
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
            this.dependencies.isConversationOperationActive?.() !== true &&
            this.dependencies.coordinator.runSelectedConversationTransition !==
                undefined &&
            this.dependencies.coordinator.adoptWindowedSelectedConversation !==
                undefined &&
            this.dependencies.captureActivationRollback !== undefined
        )
    }

    // A sync receive or a scheduled flush can queue coordinator work while an
    // activation reads, and the selection transition refuses pending work.
    // Waits for that work, then reports whether the reads are still current.
    // A newer navigation ends the wait; work still pending after it is
    // reported like a moved revision.
    private async settleBeforeWindowedPublish(
        generation: number,
        revision: DataRevision,
        mutationGeneration: number,
    ): Promise<boolean | 'retry'> {
        for (
            let attempt = 0;
            attempt < WINDOWED_PUBLISH_SETTLE_ATTEMPTS &&
            this.dependencies.coordinator.hasPendingPersistenceWork === true;
            attempt++
        ) {
            await this.dependencies.coordinator.flushPendingData(
                'activate-selection',
            )
            if (generation !== this.navigationGeneration) return false
        }
        if (
            generation !== this.navigationGeneration ||
            this.dependencies.canActivateWorkingSet?.() === false
        )
            return false
        return this.dependencies.coordinator.hasPendingPersistenceWork !== true &&
            revision === this.dependencies.coordinator.revision &&
            mutationGeneration ===
                this.dependencies.coordinator.mutationGeneration
            ? true
            : 'retry'
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

        if (detail.value.chatFolders !== undefined && !Array.isArray(detail.value.chatFolders)) {
            throw new Error(`Character ${id} returned invalid conversation folders`)
        }
        const summaries: ConversationSummary[] = []
        const seenConversationIds = new Set<string>()
        let selectedSummary: ConversationSummary | undefined
        let cursor: string | undefined
        do {
            const page = await this.dependencies.store.queryConversations({
                characterId: id,
                order: 'configured',
                limit: this.dependencies.conversationPageSize ?? 100,
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
                chatFolders: safeStructuredClone(detail.value.chatFolders ?? []),
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
        publishAsConversation: boolean
        activation?: WindowedConversationActivationChange
    }): boolean {
        if (
            this.dependencies.coordinator.hasPendingPersistenceWork === true ||
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
        const supersededAdoption = new Error('Windowed selected conversation was not adopted')
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
                    throw supersededAdoption
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
            if (error === supersededAdoption) return false
            throw error
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
        for (const controller of this.windowedControllers) controller.release()
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
        const flushReason = `complete-selected-conversation:${reason}`
        await this.dependencies.coordinator.flushPendingData(flushReason)
        let owner = this.followPromotedConversation(initialState, initialTarget)
        if ('complete' in owner) return owner.complete
        let { state, target } = owner
        let persisted: Awaited<ReturnType<PersistentDataStore['readConversation']>>
        for (let attempt = 0; ; attempt++) {
            this.requireCurrentWindowedState(state, target)
            const readRevision = state.authority.storeRevision
            persisted = await this.dependencies.store.readConversation(
                state.characterId,
                state.conversationId,
            )
            // A sync receive or a scheduled save can queue coordinator work while
            // the conversation reads, and the transition refuses pending work.
            // Waits for that work, then reads again if it moved the revision.
            for (
                let settle = 0;
                settle < WINDOWED_PUBLISH_SETTLE_ATTEMPTS &&
                this.dependencies.coordinator.hasPendingPersistenceWork === true;
                settle++
            ) {
                await this.dependencies.coordinator.flushPendingData(flushReason)
            }
            owner = this.followPromotedConversation(state, target)
            if ('complete' in owner) return owner.complete
            ;({ state, target } = owner)
            if (
                state.authority.storeRevision === readRevision ||
                attempt >= WINDOWED_CHARACTER_ACTIVATION_RETRIES
            )
                break
        }
        this.requireCurrentWindowedState(state, target)
        if (
            this.dependencies.coordinator.revision !== state.authority.storeRevision ||
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

    /**
     * Publishes the selected windowed conversation with the given complete messages and its live
     * metadata. The coordinator calls this inside a save and keeps its persisted baseline, so
     * nothing here adopts the published character.
     */
    completeWindowedConversationForSave(
        authority: WindowedConversationPersistenceAuthority,
        messages: Message[],
    ): CompleteCharacter | null {
        const state = this.selectedConversationState
        if (
            state?.kind !== 'windowed' ||
            state.characterId !== authority.characterId ||
            state.conversationId !== authority.conversationId ||
            state.authority.sessionToken !== authority.sessionToken ||
            state.authority.sessionVersion !== authority.sessionVersion ||
            state.authority.totalMessages !== authority.totalMessages ||
            state.authority.storeRevision !== authority.storeRevision
        ) return null
        const resident = this.dependencies.getResidentCharacter?.(state.characterId)
        const conversationIndex = resident?.chats.findIndex(
            (conversation) => conversation === state.conversation,
        ) ?? -1
        if (!resident || conversationIndex < 0) return null
        const conversation = {
            ...cloneConversationMetadata(state.conversation),
            message: messages,
        } as Chat
        const nextCharacter = {
            ...resident,
            chats: resident.chats.map((candidate, index) =>
                index === conversationIndex ? conversation : candidate),
        } as CompleteCharacter
        let complete: CompleteSelectedConversationState | null = null
        let publishedCharacter: CompleteCharacter | undefined
        try {
            this.selectedConversationState = null
            this.activeSession = null
            this.dependencies.publishConversation(
                state.characterId,
                conversation,
                nextCharacter,
                { representationOnly: true },
            )
            publishedCharacter = this.dependencies.getResidentCharacter?.(state.characterId)
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
                throw new Error('Complete selected conversation publication diverged')
            }
            complete = this.createCompleteSelectedConversationState(
                state.characterId,
                publishedConversation,
                state.authority.storeRevision,
                state.navigationGeneration,
            )
            this.selectedConversationState = complete
            this.activeSession = complete.session
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
        state.viewportSource.dispose()
        this.notifyActiveConversationViewportSource()
        return publishedCharacter
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

    // Writes made while a promotion waits replace or advance the selected state
    // and keep its session. Any other change ends the promotion.
    private followPromotedConversation(
        state: WindowedSelectedConversationState,
        target: SelectedConversationTarget,
    ):
        | { complete: CompleteSelectedConversationState }
        | { state: WindowedSelectedConversationState; target: SelectedConversationTarget } {
        const current = this.selectedConversationState
        // A save completes the conversation itself when its edits have to be written whole.
        if (
            current?.kind === 'complete' &&
            current.characterId === state.characterId &&
            current.conversationId === state.conversationId &&
            current.navigationGeneration === target.navigationGeneration
        ) return { complete: current }
        if (
            current?.kind !== 'windowed' ||
            current.characterId !== state.characterId ||
            current.conversationId !== state.conversationId ||
            current.navigationGeneration !== target.navigationGeneration ||
            current.authority.sessionToken !== state.authority.sessionToken ||
            current.authority.sessionVersion !== state.authority.sessionVersion ||
            current.authority.persistedSessionVersion !==
                state.authority.persistedSessionVersion
        )
            throw new SelectedConversationPromotionStaleError()
        if (current === state && this.matchesTarget(state, target))
            return { state, target }
        const currentTarget = this.captureSelectedConversationTarget()
        if (!currentTarget) throw new SelectedConversationPromotionStaleError()
        return { state: current, target: currentTarget }
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
                limit: this.dependencies.conversationPageSize ?? 100,
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
