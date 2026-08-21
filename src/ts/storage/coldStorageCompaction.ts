import { safeStructuredClone } from '../polyfill'
import type { Database } from './database.svelte'
import { coldStorageHeader } from '../process/coldstorageData'

type ColdStorageCompactionDependencies = {
    now: number
    createId: () => string
    write: (key: string, value: unknown) => Promise<boolean>
    read: (key: string) => Promise<any>
    replaceDatabase: (database: Database, reason: string) => Promise<void>
}

const tenDays = 10 * 24 * 60 * 60 * 1000

function latestChatTime(chat: any): number {
    let latest = chat.lastDate ?? 0
    for (const message of chat.message ?? []) {
        latest = Math.max(latest, message.time ?? 0)
    }
    return latest
}

function isVerifiedCharacterPayload(value: any): boolean {
    return Boolean(value && (Array.isArray(value) || value.character))
}

function isVerifiedChatPayload(value: any): boolean {
    return Boolean(value && (Array.isArray(value) || value.message))
}

export async function compactColdStorageDatabase(
    database: Database,
    dependencies: ColdStorageCompactionDependencies,
): Promise<boolean> {
    if (!database.coldstorage) {
        return false
    }

    const candidate = safeStructuredClone(database)
    const coldTime = dependencies.now - tenDays
    let changed = false

    for (let index = 0; index < candidate.characters.length; index += 1) {
        const character = candidate.characters[index]
        const lastInteraction = character.lastInteraction ?? dependencies.now
        if (lastInteraction >= coldTime || character.coldstorage) {
            continue
        }

        const key = dependencies.createId()
        if (!await dependencies.write(key, { character: safeStructuredClone(character) })) {
            continue
        }
        if (!isVerifiedCharacterPayload(await dependencies.read(key))) {
            continue
        }

        const coldStoragedChats = character.chats
            .map((chat) => chat.message?.[0]?.data)
            .filter((data): data is string => data?.startsWith(coldStorageHeader) ?? false)
            .map((data) => data.slice(coldStorageHeader.length))

        candidate.characters[index] = {
            type: 'character',
            image: character.image,
            name: character.name,
            chats: [{
                id: character.chats[0]?.id,
                message: [{ time: dependencies.now, data: '', role: 'char' }],
                note: '',
                name: '',
                localLore: [],
            }],
            chatPage: 0,
            chaId: character.chaId,
            firstMsgIndex: 0,
            coldstorage: key,
            coldStoragedChats,
        } as any
        changed = true
    }

    for (const character of candidate.characters) {
        if (character.coldstorage) {
            continue
        }
        for (const chat of character.chats) {
            if ((chat.message?.length ?? 0) < 4) {
                continue
            }
            if (chat.message?.[0]?.data?.startsWith(coldStorageHeader)) {
                continue
            }
            if (latestChatTime(chat) >= coldTime) {
                continue
            }

            const key = dependencies.createId()
            const payload = {
                message: safeStructuredClone(chat.message),
                hypaV2Data: safeStructuredClone(chat.hypaV2Data),
                hypaV3Data: safeStructuredClone(chat.hypaV3Data),
                scriptstate: safeStructuredClone(chat.scriptstate),
                localLore: safeStructuredClone(chat.localLore),
            }
            if (!await dependencies.write(key, payload)) {
                continue
            }
            if (!isVerifiedChatPayload(await dependencies.read(key))) {
                continue
            }

            chat.message = [{
                time: dependencies.now,
                data: coldStorageHeader + key,
                role: 'char',
            }]
            chat.hypaV2Data = { chunks: [], mainChunks: [], lastMainChunkID: 0 }
            chat.hypaV3Data = { summaries: [] }
            chat.scriptstate = {}
            chat.localLore = []
            changed = true
        }
    }

    if (changed) {
        await dependencies.replaceDatabase(candidate, 'cold-storage-compaction')
    }
    return changed
}
