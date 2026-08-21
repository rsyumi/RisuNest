import type { Chat, Database, character, groupChat } from './database.svelte'
import type { DataRevision, PersistentDataStore, PersistentRevisionLease } from './persistentDataStore'

type CompleteCharacter = character | groupChat

export interface WorkingSetCoordinator {
    readonly revision: DataRevision
    initialize(revision: DataRevision): void
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

    async initializeActiveWorkingSet(_database: Database): Promise<void> {
        await this.dependencies.store.open()
        const root = await this.dependencies.store.readRoot()
        this.dependencies.coordinator.initialize(root.revision)
    }

    async activateCharacter(id: string): Promise<boolean> {
        const generation = ++this.navigationGeneration
        await this.dependencies.coordinator.flushPendingData('activate-character')
        if (generation !== this.navigationGeneration) return false
        const revision = this.dependencies.coordinator.revision
        const lease = await this.dependencies.store.acquireRevision(revision)
        try {
            if (lease.revision !== revision) return false
            const characterValue = await this.hydrateCharacter(lease, id)
            if (
                generation !== this.navigationGeneration ||
                this.dependencies.coordinator.revision !== revision
            ) {
                return false
            }
            if (!this.dependencies.coordinator.adoptHydratedCharacter(revision, characterValue)) {
                return false
            }
            this.dependencies.publishCharacter(characterValue)
            return true
        } finally {
            await lease.release()
        }
    }

    async activateConversation(id: string): Promise<boolean> {
        const generation = ++this.navigationGeneration
        const characterId = this.dependencies.getSelectedCharacterId()
        if (!characterId) throw new Error('No character is selected')
        await this.dependencies.coordinator.flushPendingData('activate-conversation')
        if (generation !== this.navigationGeneration) return false
        const revision = this.dependencies.coordinator.revision
        const lease = await this.dependencies.store.acquireRevision(revision)
        try {
            if (lease.revision !== revision) return false
            const conversation = await lease.readConversation(characterId, id)
            if (!conversation) throw new Error(`Conversation ${id} was not found for ${characterId}`)
            this.verifyRevision(lease, conversation.revision)
            if (conversation.value.id !== id) {
                throw new Error(`Conversation ${id} returned mismatched ID ${conversation.value.id ?? ''}`)
            }
            if (
                generation !== this.navigationGeneration ||
                this.dependencies.coordinator.revision !== revision
            ) {
                return false
            }
            this.dependencies.publishConversation(characterId, conversation.value)
            return true
        } finally {
            await lease.release()
        }
    }

    private async hydrateCharacter(
        lease: PersistentRevisionLease,
        id: string,
    ): Promise<CompleteCharacter> {
        const detail = await lease.readCharacter(id)
        if (!detail) throw new Error(`Character ${id} was not found`)
        this.verifyRevision(lease, detail.revision)
        if (detail.value.chaId !== id) {
            throw new Error(`Character ${id} returned mismatched ID ${detail.value.chaId}`)
        }

        const chats: Chat[] = []
        let cursor: string | undefined
        do {
            const page = await lease.queryConversations({
                characterId: id,
                order: 'configured',
                limit: 100,
                cursor,
            })
            for (const summary of page.items) {
                if (summary.characterId !== id) {
                    throw new Error(`Conversation ${summary.id} returned mismatched character ID`)
                }
                const conversation = await lease.readConversation(id, summary.id)
                if (!conversation) throw new Error(`Conversation ${summary.id} was not found for ${id}`)
                this.verifyRevision(lease, conversation.revision)
                if (conversation.value.id !== summary.id) {
                    throw new Error(`Conversation ${summary.id} returned mismatched ID`)
                }
                chats.push(conversation.value)
            }
            cursor = page.nextCursor
        } while (cursor !== undefined)

        return { ...detail.value, chats } as CompleteCharacter
    }

    private verifyRevision(lease: PersistentRevisionLease, revision: DataRevision): void {
        if (revision !== lease.revision) {
            throw new Error(`Persistent revision changed from ${lease.revision} to ${revision}`)
        }
    }
}
