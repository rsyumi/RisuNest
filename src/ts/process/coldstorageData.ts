import { normalizePocketColdPayload } from '../drive/pocketRisuFeatures'
import { safeStructuredClone } from "../polyfill"
import type { Database, character, groupChat } from "../storage/database.svelte"
import { compress, decompress } from 'fflate'

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

function compressBytes(data: Uint8Array): Promise<Uint8Array> {
    return new Promise((resolve, reject) => {
        compress(data, (error, result) => error ? reject(error) : resolve(result))
    })
}

function decompressBytes(data: Uint8Array): Promise<Uint8Array> {
    return new Promise((resolve, reject) => {
        decompress(data, (error, result) => error ? reject(error) : resolve(result))
    })
}

export async function encodeColdStoragePayload(value: unknown): Promise<Uint8Array> {
    if (!isColdStorageBackupData(value)) {
        throw new TypeError('Cold storage payload has an unsupported value')
    }
    return compressBytes(new TextEncoder().encode(JSON.stringify(value)))
}

export async function decodeColdStoragePayload(data: Uint8Array): Promise<unknown> {
    let value: unknown
    try {
        value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(await decompressBytes(data)))
    } catch {
        throw new TypeError('Cold storage payload is not valid compressed JSON')
    }
    if (!isColdStorageBackupData(value)) {
        throw new TypeError('Cold storage payload has an unsupported value')
    }
    return normalizePocketColdPayload(value)
}

function replaceData(
    data: string | undefined,
    replacer: Readonly<Record<string, string>>,
) {
    if (!data) {
        return data
    }
    return replacer[data] ?? data
}

function addResource(resources: string[], value: string | undefined) {
    if (value) {
        resources.push(value)
    }
}

export function listDatabaseRootResources(
    root: Omit<Database, 'characters' | 'botPresets' | 'pluginCustomStorage'>,
): string[] {
    const resources: string[] = []
    addResource(resources, root.customBackground)
    addResource(resources, root.userIcon)
    for (const module of root.modules ?? []) {
        for (const asset of module.assets ?? []) {
            addResource(resources, asset[1])
        }
        addResource(resources, module.icon)
    }
    for (const persona of root.personas ?? []) {
        addResource(resources, persona.icon)
        for (const asset of persona.embeddedModule?.assets ?? []) {
            addResource(resources, asset[1])
        }
        addResource(resources, persona.embeddedModule?.icon)
    }
    for (const item of root.characterOrder ?? []) {
        if (typeof item === 'object') {
            addResource(resources, item.imgFile)
        }
    }
    return resources
}

export function listCharacterResources(value: character | groupChat): string[] {
    const resources: string[] = []
    addResource(resources, value.image)
    for (const emotion of value.emotionImages ?? []) {
        addResource(resources, emotion[1])
    }
    if (value.type !== 'group') {
        for (const asset of value.additionalAssets ?? []) {
            addResource(resources, asset[1])
        }
        for (const file of Object.values(value.vits?.files ?? {})) {
            addResource(resources, file)
        }
        for (const asset of value.ccAssets ?? []) {
            addResource(resources, asset.uri)
        }
    }
    return resources
}

export function replaceDatabaseRootResources<T extends Omit<Database, 'characters'>>(
    root: T,
    replacements: Readonly<Record<string, string>>,
): T {
    const cloned = safeStructuredClone(root)
    cloned.customBackground = replaceData(cloned.customBackground, replacements)
    cloned.userIcon = replaceData(cloned.userIcon, replacements)
    for (const module of cloned.modules ?? []) {
        for (const asset of module.assets ?? []) {
            asset[1] = replaceData(asset[1], replacements)
        }
        module.icon = replaceData(module.icon, replacements)
    }
    for (const persona of cloned.personas ?? []) {
        persona.icon = replaceData(persona.icon, replacements)
        for (const asset of persona.embeddedModule?.assets ?? []) {
            asset[1] = replaceData(asset[1], replacements)
        }
        if (persona.embeddedModule) {
            persona.embeddedModule.icon = replaceData(
                persona.embeddedModule.icon,
                replacements,
            )
        }
    }
    for (const item of cloned.characterOrder ?? []) {
        if (typeof item === 'object') {
            item.imgFile = replaceData(item.imgFile, replacements)
        }
    }
    return cloned
}

export function replaceCharacterResources<T extends character | groupChat>(
    value: T,
    replacements: Readonly<Record<string, string>>,
): T {
    const cha = safeStructuredClone(value)
    cha.image = replaceData(cha.image, replacements)

    if (cha.emotionImages) {
        for (let i = 0; i < cha.emotionImages.length; i++) {
            cha.emotionImages[i][1] = replaceData(cha.emotionImages[i][1], replacements)
        }
    }

    if (cha.type !== 'group') {
        for (const asset of cha.additionalAssets ?? []) {
            asset[1] = replaceData(asset[1], replacements)
        }
        for (const key of Object.keys(cha.vits?.files ?? {})) {
            cha.vits!.files[key] = replaceData(cha.vits!.files[key], replacements)
        }
        for (const asset of cha.ccAssets ?? []) {
            asset.uri = replaceData(asset.uri, replacements)
        }
    }
    return cha
}

export function replaceColdStoragePayloadResources(data: unknown, replacer: { [key: string]: string }): unknown {
    if (
        !data
        || typeof data !== 'object'
        || !('character' in data)
        || !data.character
        || typeof data.character !== 'object'
    ) {
        return data
    }

    const cloned = safeStructuredClone(data) as { character: character | groupChat }
    cloned.character = replaceCharacterResources(cloned.character, replacer)
    return cloned
}

export function listColdDataKeysFromCharacter(character: character | groupChat): string[] {
    const keys: string[] = []
    if (character.coldstorage) {
        keys.push(character.coldstorage)
    }
    keys.push(...(character.coldStoragedChats ?? []))
    for (const chat of character.chats ?? []) {
        const firstMessage = chat.message?.[0]
        if (firstMessage?.data?.startsWith(coldStorageHeader)) {
            keys.push(firstMessage.data.slice(coldStorageHeader.length))
        }
    }
    return keys
}

export function listColdDataKeysFromDb(db: Pick<Database, 'characters'> | null | undefined): string[] {
    const keys = new Set<string>()
    for (const character of db?.characters ?? []) {
        if (!character) {
            continue
        }
        for (const key of listColdDataKeysFromCharacter(character)) {
            keys.add(key)
        }
    }
    return Array.from(keys)
}

export function getColdStorageAffectedCharacters(
    db: Pick<Database, 'characters'> | null | undefined,
    unavailableKeys: Iterable<string>,
): {
    characterNames: string[]
    unresolvedKeys: string[]
} {
    const targetKeys = new Set(unavailableKeys)
    const resolvedKeys = new Set<string>()
    const characterNames: string[] = []

    for (const character of db?.characters ?? []) {
        if (!character) {
            continue
        }

        let isAffected = false
        for (const key of listColdDataKeysFromCharacter(character)) {
            if (targetKeys.has(key)) {
                resolvedKeys.add(key)
                isAffected = true
            }
        }

        if (isAffected) {
            characterNames.push(character.name?.trim() || character.chaId || 'Unknown character')
        }
    }

    return {
        characterNames,
        unresolvedKeys: Array.from(targetKeys).filter((key) => !resolvedKeys.has(key)),
    }
}
