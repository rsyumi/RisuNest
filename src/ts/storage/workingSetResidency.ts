import type { Database, character, groupChat } from './database.svelte'
import {
    createCatalogCharacterStub,
    getCatalogCharacterMetadata,
    getCatalogConversationCount,
    isCatalogCharacterStub,
} from './workingSetCatalog'

type CompleteCharacter = character | groupChat

export class WorkingSetResidencyRegistry {
    private readonly releasedCharacterIds = new Set<string>()
    private evictionAllowed = true

    get allowsEviction(): boolean {
        return this.evictionAllowed
    }

    setEvictionAllowed(allowed: boolean): void {
        this.evictionAllowed = allowed
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

    releaseCharacterMessages(character: CompleteCharacter): boolean {
        if (!this.evictionAllowed) return false
        if (character.chats.some((chat) => chat.isStreaming)) return false
        let released = false
        for (const chat of character.chats) {
            if (!Array.isArray(chat.message)) continue
            chat.message = []
            released = true
        }
        if (released) this.markCharacterReleased(character.chaId)
        return released
    }

    markCharacterReleased(id: string): void {
        this.releasedCharacterIds.add(id)
    }

    markCharacterHydrated(id: string): void {
        this.releasedCharacterIds.delete(id)
    }

    forgetCharacter(id: string): void {
        this.releasedCharacterIds.delete(id)
    }

    isCharacterReleased(id: string): boolean {
        return this.releasedCharacterIds.has(id)
    }

    clear(): void {
        this.releasedCharacterIds.clear()
    }
}

export const workingSetResidency = new WorkingSetResidencyRegistry()
