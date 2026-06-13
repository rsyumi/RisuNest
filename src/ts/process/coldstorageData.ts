import type { Database } from "../storage/database.svelte"

export const coldStorageHeader = '\uEF01COLDSTORAGE\uEF01'

export function getColdStorageBackupKey(name: string): string | null {
    const match = name.match(/^(?:coldstorage[/_])?([0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})\.json$/)
    return match?.[1] ?? null
}

export function getColdStorageBackupName(key: string): string {
    return `coldstorage_${key}.json`
}

export function isColdStorageBackupData(data: unknown): boolean {
    if (Array.isArray(data)) {
        return true
    }

    return !!data
        && typeof data === 'object'
        && ('character' in data || 'message' in data)
}

export function listColdDataKeysFromDb(db: Pick<Database, 'characters'> | null | undefined): string[] {
    const keys = new Set<string>()
    for (const character of db?.characters ?? []) {
        if (!character) {
            continue
        }
        if (character.coldstorage) {
            keys.add(character.coldstorage)
            for (const key of character.coldStoragedChats ?? []) {
                keys.add(key)
            }
        }
        for (const chat of character.chats ?? []) {
            const firstMessage = chat.message?.[0]
            if (firstMessage?.data?.startsWith(coldStorageHeader)) {
                keys.add(firstMessage.data.slice(coldStorageHeader.length))
            }
        }
    }
    return Array.from(keys)
}
