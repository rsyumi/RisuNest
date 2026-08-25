import type { PersistentRoot } from './persistentDataStore'

function containsCharacterId(root: PersistentRoot, characterId: string): boolean {
    return (root.characterOrder ?? []).some((entry) =>
        typeof entry === 'string'
            ? entry === characterId
            : entry.data.includes(characterId),
    )
}

export function removeCharacterIdFromOrder(
    root: PersistentRoot,
    characterId: string,
): void {
    const order: PersistentRoot['characterOrder'] = []
    for (const entry of root.characterOrder ?? []) {
        if (typeof entry === 'string') {
            if (entry !== characterId) order.push(entry)
            continue
        }
        order.push({
            ...entry,
            data: entry.data.filter((id) => id !== characterId),
        })
    }
    root.characterOrder = order
}

export function appendCharacterIdToOrder(
    root: PersistentRoot,
    characterId: string,
): void {
    if (containsCharacterId(root, characterId)) return
    root.characterOrder = [...(root.characterOrder ?? []), characterId]
}
