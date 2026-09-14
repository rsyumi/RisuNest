import type { Database, character, groupChat } from './database.svelte'
import { isConversationSummaryStub } from './conversationResidency'
import {
    createCatalogCharacterStub,
    getCatalogCharacterMetadata,
    getCatalogConversationCount,
    isCatalogCharacterStub,
} from './workingSetCatalog'

type CompleteCharacter = character | groupChat

export { createConversationSummaryStub, isConversationSummaryStub } from './conversationResidency'

export class WorkingSetResidencyRegistry {
    private readonly releasedCharacterIds = new Set<string>()
    private readonly releasedConversationIds = new Map<string, Set<string>>()
    private readonly selectedConversationIds = new Map<string, string>()
    private evictionAllowed = true

    get allowsEviction(): boolean {
        return this.evictionAllowed
    }

    setEvictionAllowed(allowed: boolean): void {
        this.evictionAllowed = allowed
    }

    canReleaseConversation(
        character: CompleteCharacter,
        conversationId: string,
        nextConversationId?: string,
    ): boolean {
        if (!this.evictionAllowed) return false
        const pinnedId = nextConversationId ?? this.selectedConversationIds.get(character.chaId)
        if (pinnedId === conversationId) return false
        const conversation = character.chats.find((candidate) => candidate.id === conversationId)
        return Boolean(conversation && !conversation.isStreaming)
    }

    pinSelectedConversation(characterId: string, conversationId: string): void {
        this.selectedConversationIds.set(characterId, conversationId)
        this.markConversationHydrated(characterId, conversationId)
    }

    markConversationReleased(characterId: string, conversationId: string): void {
        const released = this.releasedConversationIds.get(characterId) ?? new Set<string>()
        released.add(conversationId)
        this.releasedConversationIds.set(characterId, released)
    }

    markConversationHydrated(characterId: string, conversationId: string): void {
        const released = this.releasedConversationIds.get(characterId)
        released?.delete(conversationId)
        if (released?.size === 0) this.releasedConversationIds.delete(characterId)
    }

    isConversationReleased(characterId: string, conversationId: string): boolean {
        return this.releasedConversationIds.get(characterId)?.has(conversationId) === true
    }

    hasReleasedConversations(characterId: string): boolean {
        return (this.releasedConversationIds.get(characterId)?.size ?? 0) > 0
    }

    reconcileConversationResidency(character: CompleteCharacter): void {
        this.releasedConversationIds.delete(character.chaId)
        for (const conversation of character.chats) {
            if (conversation.id && isConversationSummaryStub(conversation)) {
                this.markConversationReleased(character.chaId, conversation.id)
            }
        }
        const selectedId = character.chats[character.chatPage ?? 0]?.id
        if (selectedId) this.pinSelectedConversation(character.chaId, selectedId)
        else this.selectedConversationIds.delete(character.chaId)
    }

    canReleaseCharacterToCatalog(database: Database, id: string): boolean {
        const character = database.characters.find((candidate) => candidate.chaId === id)
        return Boolean(
            character &&
            (isCatalogCharacterStub(character) || (
                this.evictionAllowed &&
                !character.chats.some((chat) => chat.isStreaming)
            )),
        )
    }

    releaseCharacterToCatalog(database: Database, id: string): boolean {
        const index = database.characters.findIndex((character) => character.chaId === id)
        if (index < 0) return false
        const character = database.characters[index]
        if (isCatalogCharacterStub(character)) {
            this.markCharacterReleased(id)
            return true
        }
        if (!this.canReleaseCharacterToCatalog(database, id)) return false
        const metadata = getCatalogCharacterMetadata(character)
        database.characters[index] = createCatalogCharacterStub({
            id: character.chaId,
            name: character.name,
            image: character.image,
            configuredIndex: metadata?.configuredIndex ?? index,
            recentAt: character.lastInteraction ?? 0,
            trashed: character.trashTime !== undefined,
            conversationCount: getCatalogConversationCount(character),
            type: character.type,
            creatorNotes: character.creatorNotes ?? '',
            trashTime: character.trashTime,
        })
        this.markCharacterReleased(id)
        return true
    }

    markCharacterReleased(id: string): void {
        this.releasedCharacterIds.add(id)
        this.releasedConversationIds.delete(id)
        this.selectedConversationIds.delete(id)
    }

    markCharacterHydrated(id: string): void {
        this.releasedCharacterIds.delete(id)
    }

    forgetCharacter(id: string): void {
        this.releasedCharacterIds.delete(id)
        this.releasedConversationIds.delete(id)
        this.selectedConversationIds.delete(id)
    }

    isCharacterReleased(id: string): boolean {
        return this.releasedCharacterIds.has(id)
    }

    clear(): void {
        this.releasedCharacterIds.clear()
        this.releasedConversationIds.clear()
        this.selectedConversationIds.clear()
    }
}

export const workingSetResidency = new WorkingSetResidencyRegistry()
