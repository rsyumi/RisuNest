import type { character, groupChat } from './database.svelte'
import { getCatalogCharacterMetadata, isArchivedCharacter } from './workingSetCatalog'

export function characterIsArchived(value: character | groupChat): boolean {
    return isArchivedCharacter(value)
}

export function archivedConversationCount(value: character | groupChat): number {
    return getCatalogCharacterMetadata(value)?.conversationCount ?? 0
}

export function archivedAt(value: character | groupChat): number | undefined {
    return getCatalogCharacterMetadata(value)?.archivedAt
}

export function countBlockedGroupMembers(
    group: groupChat,
    characters: readonly (character | groupChat)[],
): number {
    let blocked = 0
    for (const memberId of group.characters ?? []) {
        const member = characters.find((candidate) => candidate.chaId === memberId)
        if (member && isArchivedCharacter(member)) blocked += 1
    }
    return blocked
}

export function formatArchivedAt(value: number | undefined): string {
    if (value === undefined) return ''
    return new Date(value).toLocaleDateString()
}

export function countArchivedCharacters(
    characters: readonly (character | groupChat)[],
): number {
    let archived = 0
    for (const character of characters) {
        if (isArchivedCharacter(character)) archived += 1
    }
    return archived
}
