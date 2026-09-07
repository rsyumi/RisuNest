import { safeStructuredClone } from '../polyfill'
import type { Database } from './database.svelte'
import { coldStorageHeader } from '../process/coldstorageData'
import { hasIncompletePersistentWorkingSet } from './workingSetCatalog'
import { workingSetResidency } from './workingSetResidency'
import { canonicalJson } from './saveCoordinatorHelpers'

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

type DatabaseCharacter = Database['characters'][number]

function isColdCharacter(character: DatabaseCharacter, now: number, coldTime: number): boolean {
    if (character.coldstorage) {
        return false
    }
    return (character.lastInteraction ?? now) < coldTime
}

function isColdChat(character: DatabaseCharacter, chat: any, coldTime: number): boolean {
    if (character.coldstorage) {
        return false
    }
    if ((chat.message?.length ?? 0) < 4) {
        return false
    }
    if (chat.message?.[0]?.data?.startsWith(coldStorageHeader)) {
        return false
    }
    return latestChatTime(chat) < coldTime
}

function hasCompactionCandidates(database: Database, now: number, coldTime: number): boolean {
    return database.characters.some((character) => (
        isColdCharacter(character, now, coldTime)
        || character.chats.some((chat) => isColdChat(character, chat, coldTime))
    ))
}

function isVerifiedPayload(value: unknown, expected: unknown): boolean {
    // The cold payload codec persists JSON, including omission of undefined fields.
    return canonicalJson(value) === canonicalJson(JSON.parse(JSON.stringify(expected)))
}

export async function compactColdStorageDatabase(
    database: Database,
    dependencies: ColdStorageCompactionDependencies,
): Promise<boolean> {
    if (hasIncompletePersistentWorkingSet(database, workingSetResidency)) {
        return false
    }
    if (!database.coldstorage) {
        return false
    }

    const coldTime = dependencies.now - tenDays
    if (!hasCompactionCandidates(database, dependencies.now, coldTime)) {
        return false
    }

    const candidate = safeStructuredClone(database)
    const sourceSnapshot = JSON.stringify(database)
    let changed = false

    const characterTasks = candidate.characters.map((_character, index) => async () => {
        const character = candidate.characters[index]
        if (!isColdCharacter(character, dependencies.now, coldTime)) {
            return
        }

        const key = dependencies.createId()
        const payload = { character: safeStructuredClone(character) }
        if (!await dependencies.write(key, payload)) {
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
        if (!isVerifiedPayload(verifiedPayload, payload)) {
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
                if (!isColdChat(currentCharacter, chat, coldTime)) {
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
                if (!isVerifiedPayload(verifiedPayload, payload)) {
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
        if (JSON.stringify(database) !== sourceSnapshot) {
            throw new Error('Database changed during cold storage compaction')
        }
        await dependencies.replaceDatabase(candidate, 'cold-storage-compaction')
    }
    return changed
}
