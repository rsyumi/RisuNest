import { safeStructuredClone } from '../polyfill'
import type { Database } from './database.svelte'
import { coldStorageHeader } from '../process/coldstorageData'

type ColdStorageCompactionDependencies = {
    now: number
    createId: () => string
    write: (key: string, value: unknown) => Promise<boolean>
    read: (key: string) => Promise<any>
    replaceDatabase: (database: Database, reason: string) => Promise<void>
    onProgress?: (phase: 'character' | 'chat', remaining: number) => void
    onFailure?: (failure: ColdStorageCompactionFailure) => void
}

export type ColdStorageCompactionFailure = {
    kind: 'write' | 'read' | 'verify'
    target: 'character' | 'chat'
    characterIndex: number
    chatIndex?: number
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

    const characterTasks = candidate.characters.map((_character, index) => async () => {
        const character = candidate.characters[index]
        const lastInteraction = character.lastInteraction ?? dependencies.now
        if (lastInteraction >= coldTime || character.coldstorage) {
            return
        }

        const key = dependencies.createId()
        if (!await dependencies.write(key, { character: safeStructuredClone(character) })) {
            dependencies.onFailure?.({ kind: 'write', target: 'character', characterIndex: index })
            return
        }
        let verifiedPayload: unknown
        try {
            verifiedPayload = await dependencies.read(key)
        } catch {
            dependencies.onFailure?.({ kind: 'read', target: 'character', characterIndex: index })
            return
        }
        if (!isVerifiedCharacterPayload(verifiedPayload)) {
            dependencies.onFailure?.({ kind: 'verify', target: 'character', characterIndex: index })
            return
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
    })

    while (characterTasks.length > 0) {
        const batch = characterTasks.splice(0, 5)
        dependencies.onProgress?.('character', characterTasks.length)
        await Promise.all(batch.map((task) => task()))
    }

    const chatTasks: Array<() => Promise<void>> = []
    candidate.characters.forEach((character, characterIndex) => {
        character.chats.forEach((_chat, chatIndex) => {
            chatTasks.push(async () => {
                const currentCharacter = candidate.characters[characterIndex]
                const chat = currentCharacter.chats[chatIndex]
                if (currentCharacter.coldstorage) {
                    return
                }
                if ((chat.message?.length ?? 0) < 4) {
                    return
                }
                if (chat.message?.[0]?.data?.startsWith(coldStorageHeader)) {
                    return
                }
                if (latestChatTime(chat) >= coldTime) {
                    return
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
                    dependencies.onFailure?.({
                        kind: 'write',
                        target: 'chat',
                        characterIndex,
                        chatIndex,
                    })
                    return
                }
                let verifiedPayload: unknown
                try {
                    verifiedPayload = await dependencies.read(key)
                } catch {
                    dependencies.onFailure?.({
                        kind: 'read',
                        target: 'chat',
                        characterIndex,
                        chatIndex,
                    })
                    return
                }
                if (!isVerifiedChatPayload(verifiedPayload)) {
                    dependencies.onFailure?.({
                        kind: 'verify',
                        target: 'chat',
                        characterIndex,
                        chatIndex,
                    })
                    return
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
            })
        })
    })

    while (chatTasks.length > 0) {
        const batch = chatTasks.splice(0, 5)
        dependencies.onProgress?.('chat', chatTasks.length)
        await Promise.all(batch.map((task) => task()))
    }

    if (changed) {
        await dependencies.replaceDatabase(candidate, 'cold-storage-compaction')
    }
    return changed
}
