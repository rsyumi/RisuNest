import type { Chat, Database, character, groupChat } from './database.svelte'
import type { DataRevision, PersistentDataStore } from './persistentDataStore'

type CompleteCharacter = character | groupChat

export interface WorkingSetCoordinator {
    readonly revision: DataRevision
    initialize(revision: DataRevision, database: Database): void
    flushPendingData(reason: string): Promise<void>
    adoptHydratedCharacter(revision: DataRevision, character: CompleteCharacter): boolean
}

export interface ActiveWorkingSetDependencies {
    store: PersistentDataStore
    coordinator: WorkingSetCoordinator
    getSelectedCharacterId(): string | null | undefined
    publishCharacter(character: CompleteCharacter): void
    publishConversation(characterId: string, conversation: Chat): void
}

export class ActiveWorkingSet {
    private navigationGeneration = 0

    constructor(private readonly dependencies: ActiveWorkingSetDependencies) {}

    async initializeActiveWorkingSet(database: Database): Promise<void> {
        await this.dependencies.store.open()
        const root = await this.dependencies.store.readRoot()
        this.dependencies.coordinator.initialize(root.revision, database)
    }

    async activateCharacter(id: string): Promise<boolean> {
        const generation = ++this.navigationGeneration
        await this.dependencies.coordinator.flushPendingData('activate-character')
        if (generation !== this.navigationGeneration) return false
        const revision = this.dependencies.coordinator.revision
        const characterValue = await this.hydrateCharacter(id, revision, generation)
        if (!characterValue) return false
        if (!this.dependencies.coordinator.adoptHydratedCharacter(revision, characterValue)) {
            return false
        }
        this.dependencies.publishCharacter(characterValue)
        return true
    }

    async activateConversation(id: string): Promise<boolean> {
        const generation = ++this.navigationGeneration
        const characterId = this.dependencies.getSelectedCharacterId()
        if (!characterId) throw new Error('No character is selected')
        await this.dependencies.coordinator.flushPendingData('activate-conversation')
        if (generation !== this.navigationGeneration) return false
        const revision = this.dependencies.coordinator.revision
        const conversation = await this.dependencies.store.readConversation(characterId, id)
        if (!this.isCurrent(generation, revision)) return false
        if (!conversation) throw new Error(`Conversation ${id} was not found for ${characterId}`)
        if (conversation.revision !== revision) return false
        if (conversation.value.id !== id) {
            throw new Error(`Conversation ${id} returned mismatched ID ${conversation.value.id ?? ''}`)
        }
        this.dependencies.publishConversation(characterId, conversation.value)
        return true
    }

    private async hydrateCharacter(
        id: string,
        revision: DataRevision,
        generation: number,
    ): Promise<CompleteCharacter | null> {
        const detail = await this.dependencies.store.readCharacter(id)
        if (!this.isCurrent(generation, revision)) return null
        if (!detail) throw new Error(`Character ${id} was not found`)
        if (detail.revision !== revision) return null
        if (detail.value.chaId !== id) {
            throw new Error(`Character ${id} returned mismatched ID ${detail.value.chaId}`)
        }

        const chats: Chat[] = []
        let cursor: string | undefined
        do {
            const page = await this.dependencies.store.queryConversations({
                characterId: id,
                order: 'configured',
                limit: 100,
                cursor,
            })
            if (!this.isCurrent(generation, revision) || page.revision !== revision) return null
            for (const summary of page.items) {
                if (summary.characterId !== id) {
                    throw new Error(`Conversation ${summary.id} returned mismatched character ID`)
                }
                const conversation = await this.dependencies.store.readConversation(id, summary.id)
                if (!this.isCurrent(generation, revision)) return null
                if (!conversation) throw new Error(`Conversation ${summary.id} was not found for ${id}`)
                if (conversation.revision !== revision) return null
                if (conversation.value.id !== summary.id) {
                    throw new Error(`Conversation ${summary.id} returned mismatched ID`)
                }
                chats.push(conversation.value)
            }
            cursor = page.nextCursor
        } while (cursor !== undefined)

        return { ...detail.value, chats } as CompleteCharacter
    }

    private isCurrent(generation: number, revision: DataRevision): boolean {
        return (
            generation === this.navigationGeneration &&
            revision === this.dependencies.coordinator.revision
        )
    }
}
