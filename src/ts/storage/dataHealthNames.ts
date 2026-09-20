import { getDatabase } from './database.svelte'
import type { DataHealthFinding } from './dataHealth'
import {
    dataHealthOwnerKey,
    type DataHealthResolvedNames,
} from './dataHealthPresentation'
import { getPersistentDataStore } from './persistentDataStoreFactory'

export async function resolveDataHealthFindingNames(
    items: readonly DataHealthFinding[],
): Promise<Map<string, DataHealthResolvedNames>> {
    const database = getDatabase()
    const result = new Map<string, DataHealthResolvedNames>()
    const modules = new Map(
        (database.modules ?? []).map((module) => [module.id, module.name] as const),
    )
    const characters = new Map(
        (database.characters ?? []).map((character) => [character.chaId, character] as const),
    )
    const conversations = new Map<string, string>(
        (database.characters ?? []).flatMap((character) =>
            (character.chats ?? []).flatMap((conversation) =>
                conversation.id
                    ? [[`${character.chaId}/${conversation.id}`, conversation.name ?? ''] as const]
                    : [],
            ),
        ),
    )
    const unresolved: Array<Promise<void>> = []
    for (const item of items) {
        const key = dataHealthOwnerKey(item.owner.kind, item.owner.id)
        if (result.has(key)) continue
        if (item.owner.kind === 'module') {
            const ownerName = modules.get(item.owner.id)
            result.set(key, ownerName ? { ownerName } : {})
            continue
        }
        if (item.owner.kind === 'character' || item.owner.kind === 'group') {
            const ownerName = characters.get(item.owner.id)?.name
            result.set(key, ownerName ? { ownerName, characterName: ownerName } : {})
            continue
        }
        if (item.owner.kind !== 'conversation') continue
        const separator = item.owner.id.indexOf('/')
        if (separator < 0) continue
        const characterId = item.owner.id.slice(0, separator)
        const conversationId = item.owner.id.slice(separator + 1)
        const localCharacter = characters.get(characterId)
        const localConversation = conversations.get(item.owner.id)
        const names: DataHealthResolvedNames = {
            ...(localCharacter?.name ? { characterName: localCharacter.name } : {}),
            ...(localConversation ? { conversationName: localConversation } : {}),
        }
        if (names.characterName || names.conversationName) {
            names.ownerName = [names.characterName, names.conversationName]
                .filter(Boolean)
                .join(' / ')
        }
        result.set(key, names)
        if (names.characterName && names.conversationName) continue
        unresolved.push((async () => {
            try {
                const store = getPersistentDataStore()
                const [character, conversation] = await Promise.all([
                    names.characterName ? null : store.readCharacterSummary(characterId),
                    names.conversationName
                        ? null
                        : store.readConversationMetadata(characterId, conversationId),
                ])
                if (character?.name) names.characterName = character.name
                const conversationName = conversation?.value.conversation.name
                if (conversationName) names.conversationName = conversationName
                if (names.characterName || names.conversationName) {
                    names.ownerName = [names.characterName, names.conversationName]
                        .filter(Boolean)
                        .join(' / ')
                }
            } catch {
                // The recovery shell can show the diagnosis before the ordinary store is open.
            }
        })())
    }
    await Promise.all(unresolved)
    return result
}
