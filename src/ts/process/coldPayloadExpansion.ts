import { normalizePocketColdPayload } from '../drive/pocketRisuFeatures'
import type { Database, character, groupChat } from '../storage/database.svelte'
import { coldStorageHeader, isColdStorageBackupData } from './coldstorageData'

type ColdCharacter = Database['characters'][number]

export type ColdPayloadReader = (key: string) => Promise<unknown>

export interface ColdPayloadExpansion {
    expandedKeys: string[]
    unavailableKeys: string[]
}

function chatColdKey(chat: unknown): string | null {
    const data = (chat as { message?: { data?: string }[] })?.message?.[0]?.data
    if (typeof data !== 'string' || !data.startsWith(coldStorageHeader)) return null
    return data.slice(coldStorageHeader.length)
}

function applyChatPayload(chat: Record<string, unknown>, payload: unknown): boolean {
    if (Array.isArray(payload)) {
        chat.message = payload
        return true
    }
    if (!payload || typeof payload !== 'object') return false
    const source = payload as Record<string, unknown>
    if (!Array.isArray(source.message)) return false
    chat.message = source.message
    if (Object.hasOwn(source, 'savedToggleValues')) chat.savedToggleValues = source.savedToggleValues
    if (Object.hasOwn(source, 'bindedPersona')) chat.bindedPersona = source.bindedPersona
    chat.hypaV2Data = source.hypaV2Data
    chat.hypaV3Data = source.hypaV3Data
    chat.scriptstate = source.scriptstate
    chat.localLore = source.localLore
    return true
}

function payloadCharacter(payload: unknown): character | groupChat | null {
    if (!payload || typeof payload !== 'object' || Array.isArray(payload)) return null
    const value = (payload as { character?: unknown }).character
    if (!value || typeof value !== 'object' || Array.isArray(value)) return null
    return value as character | groupChat
}

/**
 * Expands upstream cold storage references into the records that carry them.
 * Imported and received databases keep their bodies inline; no reference survives.
 */
export async function expandColdPayloads(
    db: Pick<Database, 'characters'>,
    read: ColdPayloadReader,
): Promise<ColdPayloadExpansion> {
    const expandedKeys: string[] = []
    const unavailableKeys: string[] = []
    const characters = db?.characters
    if (!Array.isArray(characters)) return { expandedKeys, unavailableKeys }

    const load = async (key: string): Promise<unknown | null> => {
        let value: unknown
        try {
            value = await read(key)
        } catch (error) {
            if ((error as { name?: string })?.name === 'AbortError') throw error
            console.error(`Failed to read the cold storage payload ${key}:`, error)
            unavailableKeys.push(key)
            return null
        }
        if (value === null || value === undefined || !isColdStorageBackupData(value)) {
            unavailableKeys.push(key)
            return null
        }
        try {
            return normalizePocketColdPayload(value)
        } catch (error) {
            console.error(`Cold storage payload ${key} has an unsupported shape:`, error)
            unavailableKeys.push(key)
            return null
        }
    }

    for (let index = 0; index < characters.length; index++) {
        let current = characters[index]
        if (!current) continue

        const characterKey = current.coldstorage
        if (characterKey) {
            const payload = await load(characterKey)
            const restored = payloadCharacter(payload)
            if (restored) {
                characters[index] = restored as ColdCharacter
                current = characters[index]
                expandedKeys.push(characterKey)
            }
        }

        for (const chat of current.chats ?? []) {
            const chatKey = chatColdKey(chat)
            if (!chatKey) continue
            const payload = await load(chatKey)
            if (payload === null) continue
            if (applyChatPayload(chat as unknown as Record<string, unknown>, payload)) {
                expandedKeys.push(chatKey)
            } else {
                unavailableKeys.push(chatKey)
            }
        }

        delete current.coldstorage
        delete current.coldStoragedChats
    }

    return { expandedKeys, unavailableKeys }
}
