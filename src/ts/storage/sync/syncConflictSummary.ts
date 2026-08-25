import type { Database } from '../database.svelte'
import type { PersistentRevisionReader } from '../persistentDataStore'
import {
    assertPinnedRevision,
    iteratePinnedCharacterSummaries,
} from '../persistentRecordIterator'

type CharacterLike = Database['characters'][number]

export interface SyncConflictSummary {
    localOnlyNames: string[]
    remoteOnlyNames: string[]
    changedNames: string[]
}

function characterName(character: CharacterLike): string {
    const name = typeof character.name === 'string' ? character.name.trim() : ''
    return name || character.chaId
}

function contentSignature(character: CharacterLike): string {
    const chats = Array.isArray(character.chats) ? character.chats : []
    const messages = chats.reduce(
        (total, chat) => total + (Array.isArray(chat?.message) ? chat.message.length : 0),
        0,
    )
    return `${character.name ?? ''}\u0000${chats.length}\u0000${messages}`
}

export function summarizeSyncConflict(local: Database, remote: Database): SyncConflictSummary {
    const remoteById = new Map(remote.characters.map((character) => [character.chaId, character]))
    const localOnlyNames: string[] = []
    const changedNames: string[] = []
    const matchedIds = new Set<string>()
    for (const character of local.characters) {
        const other = remoteById.get(character.chaId)
        if (!other) {
            localOnlyNames.push(characterName(character))
            continue
        }
        matchedIds.add(character.chaId)
        if (contentSignature(character) !== contentSignature(other)) {
            changedNames.push(characterName(character))
        }
    }
    const remoteOnlyNames = remote.characters
        .filter((character) => !matchedIds.has(character.chaId))
        .map(characterName)
    return { localOnlyNames, remoteOnlyNames, changedNames }
}

export async function summarizePinnedSyncConflict(
    reader: PersistentRevisionReader,
    remote: Database,
): Promise<SyncConflictSummary> {
    const remoteById = new Map(remote.characters.map((character) => [character.chaId, character]))
    const localOnlyNames: string[] = []
    const changedNames: string[] = []
    const matchedIds = new Set<string>()
    for await (const character of iteratePinnedCharacterSummaries(reader)) {
        const other = remoteById.get(character.id)
        if (!other) {
            const name = character.name.trim()
            localOnlyNames.push(name || character.id)
            continue
        }
        matchedIds.add(character.id)
        let conversationCount = 0
        let messageCount = 0
        let cursor: string | undefined
        do {
            const page = await reader.queryConversations({
                characterId: character.id,
                order: 'configured',
                limit: 128,
                cursor,
            })
            assertPinnedRevision(
                reader.revision,
                page.revision,
                `Conversation page for ${character.id}`,
            )
            conversationCount += page.items.length
            for (const conversation of page.items) messageCount += conversation.messageCount
            cursor = page.nextCursor
        } while (cursor !== undefined)
        const localSignature = [character.name, conversationCount, messageCount].join('\0')
        if (localSignature !== contentSignature(other)) {
            const name = character.name.trim()
            changedNames.push(name || character.id)
        }
    }
    const remoteOnlyNames = remote.characters
        .filter((character) => !matchedIds.has(character.chaId))
        .map(characterName)
    return { localOnlyNames, remoteOnlyNames, changedNames }
}

export function formatNameList(names: readonly string[], limit = 3): string {
    if (names.length <= limit) return names.join(', ')
    return `${names.slice(0, limit).join(', ')} +${names.length - limit}`
}
