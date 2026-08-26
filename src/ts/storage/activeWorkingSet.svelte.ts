import type { Chat, Database, character, groupChat } from './database.svelte'
import { ActiveConversationSession } from './activeConversationSession'
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

type CompleteCharacter = character | groupChat

const CONVERSATION_HYDRATION_CONCURRENCY = 8
const RELATED_CHARACTER_HYDRATION_CONCURRENCY = 4
const DEFAULT_GROUP_TALKNESS = 1 / 6 * 4

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
}

export class ActiveWorkingSet {
    private navigationGeneration = 0
    private activeIds = new Set<string>()
    private readonly conversationFlights = new Map<
        string,
        { generation: number; promise: Promise<boolean> }
    >()
    private activeSession: ActiveConversationSession | null = null

    constructor(private readonly dependencies: ActiveWorkingSetDependencies) {}

    get navigationGenerationToken(): number {
        return this.navigationGeneration
    }

    get activeCharacterIds(): ReadonlySet<string> {
        return new Set(this.activeIds)
    }

    get activeConversationSession(): ActiveConversationSession | null {
        return this.activeSession
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
        this.clearActiveConversationSession()
        if (selected && conversation) {
            this.publishActiveConversationSession(selected.chaId, conversation, revision)
        }
    }

    async activateCharacter(
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
            const retainedIndices = groupValue.characters
                .map((memberId, index) => ({ memberId, index }))
                .filter(({ memberId }) => !missingRelatedIds.has(memberId))
            characterValue = {
                ...groupValue,
                characters: retainedIndices.map(({ memberId }) => memberId),
                characterTalks: retainedIndices.map(
                    ({ index }) => groupValue.characterTalks?.[index] ?? DEFAULT_GROUP_TALKNESS,
                ),
                characterActive: retainedIndices.map(
                    ({ index }) => groupValue.characterActive?.[index] ?? true,
                ),
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
                nextCharacter,
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
        this.clearActiveConversationSession()
        if (!conversationId) return
        this.activeSession = new ActiveConversationSession({
            characterId,
            conversationId,
            conversation,
            storeRevision,
        })
    }

    private clearActiveConversationSession(): void {
        this.activeSession?.invalidate()
        this.activeSession = null
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
