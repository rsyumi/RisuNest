import { v4 } from 'uuid'
import type { Message } from '../storage/database.svelte'
import { snapshotResponse, type ResponseVariantSet } from '../responseVariants'
import { sanitizeToggleValues } from '../toggleBindings'

type RecordValue = Record<string, unknown>
const object = (value: unknown): value is RecordValue =>
    !!value && typeof value === 'object' && !Array.isArray(value)

/** Keeps a named toggle preset; values that are not a record become empty. */
function togglePreset(value: unknown): boolean {
    if (!object(value) || typeof value.name !== 'string') return false
    value.values = sanitizeToggleValues(value.values) ?? {}
    return true
}

export function normalizePocketMessage(value: unknown, fallbackId: string): void {
    if (!object(value) || !Array.isArray(value.swipes) || value.responseVariants !== undefined) return
    // Array holes are not candidates; a usable swipeId keeps pointing at its own text.
    const swipes: string[] = []
    let selected: number | undefined
    value.swipes.forEach((swipe: unknown, i: number) => {
        if (typeof swipe !== 'string') return
        if (i === value.swipeId) selected = swipes.length
        swipes.push(swipe)
    })
    if (typeof value.data !== 'string') value.data = swipes[selected ?? 0] ?? ''
    const body = value.data as string
    let index: number
    const warnings: string[] = []
    if (selected === undefined) {
        warnings.push('invalid-swipe-index')
        const matching = swipes.flatMap((swipe, i) => (swipe === body ? [i] : []))
        index = matching.length === 1 ? matching[0] : swipes.length
        if (index === swipes.length) swipes.push(body)
    } else if (swipes[selected] !== body) {
        warnings.push('swipe-body-mismatch')
        index = swipes.length
        swipes.push(body)
    } else index = selected
    const groupId = typeof value.chatId === 'string' && value.chatId ? value.chatId : fallbackId
    const snapshot = snapshotResponse([value as unknown as Message])[0] as unknown as RecordValue
    delete snapshot.swipes
    delete snapshot.swipeId
    const variants: ResponseVariantSet = {
        groupId,
        selectedId: `${groupId}:${index}`,
        candidates: swipes.map((data, i) => {
            const message: RecordValue = { ...snapshot, data }
            if (i !== index) {
                delete message.generationInfo
                delete message.promptInfo
                delete message.time
            }
            return { id: `${groupId}:${i}`, messages: [message as unknown as Message] }
        }),
    }
    value.responseVariants = variants
    value.chatId = groupId
    if (warnings.length) value.pocketRisuImportWarnings = warnings
    // Keep upstream fields as import provenance. Our envelope is authoritative.
}

export interface PocketNormalizeOptions {
    /** A RisuSave RisuNest wrote keeps a chat's preset binding fields as stored. */
    readonly risuNestSave?: boolean
}

export function normalizePocketChat(value: unknown, fallbackId: string, options: PocketNormalizeOptions = {}): void {
    if (!object(value)) return
    if (value.savedToggleValues !== undefined) {
        const values = sanitizeToggleValues(value.savedToggleValues)
        if (values) value.savedToggleValues = values
        else delete value.savedToggleValues
    }
    if (value.bindedPersona !== undefined && typeof value.bindedPersona !== 'string') delete value.bindedPersona
    if (!options.risuNestSave) {
        if (value.promptPresetId !== undefined && typeof value.promptPresetId !== 'string') delete value.promptPresetId
        // PocketRisu and RisuBard store a chat's preset binding as bindedBotPreset.
        if (Object.hasOwn(value, 'bindedBotPreset')) {
            if (value.promptPresetId === undefined && typeof value.bindedBotPreset === 'string' && value.bindedBotPreset)
                value.promptPresetId = value.bindedBotPreset
            delete value.bindedBotPreset
        }
    }
    if (Array.isArray(value.message))
        value.message.forEach((message, index) =>
            normalizePocketMessage(
                message,
                `${typeof value.id === 'string' ? value.id : fallbackId}:response:${index}`,
            ),
        )
}

export function normalizePocketCharacter(value: unknown, fallbackId = 'character', options: PocketNormalizeOptions = {}): void {
    if (object(value) && Array.isArray(value.chats))
        value.chats.forEach((chat, index) => normalizePocketChat(chat, `${fallbackId}:chat:${index}`, options))
}

/** Drops root values RisuNest cannot read, such as the null PocketRisu stores for a cleared toggle default. */
export function normalizePocketFeatures<T>(value: T, options: PocketNormalizeOptions = {}): T {
    if (!object(value)) return value
    const root: RecordValue = value
    if (root.disableToggleBinding !== undefined && typeof root.disableToggleBinding !== 'boolean')
        delete root.disableToggleBinding
    if (root.defaultToggleValues !== undefined) {
        const values = sanitizeToggleValues(root.defaultToggleValues)
        if (values) root.defaultToggleValues = values
        else delete root.defaultToggleValues
    }
    if (Array.isArray(root.togglePresets)) root.togglePresets = root.togglePresets.filter(togglePreset)
    else if (root.togglePresets !== undefined) delete root.togglePresets
    if (Array.isArray(root.personas)) {
        // Lookups take the first persona with an ID, so a later duplicate gets a new one.
        const ids = new Set<string>()
        for (const persona of root.personas) {
            if (!object(persona) || persona.id === undefined || persona.id === '') continue
            if (typeof persona.id !== 'string' || ids.has(persona.id)) persona.id = v4()
            ids.add(persona.id as string)
        }
    }
    if (Array.isArray(root.characters))
        root.characters.forEach((character, index) =>
            normalizePocketCharacter(character, `character:${index}`, options),
        )
    return value
}

export function normalizePocketColdPayload<T>(value: T): T {
    if (Array.isArray(value))
        value.forEach((message, index) => normalizePocketMessage(message, `cold:response:${index}`))
    else if (object(value)) {
        if (value.character) normalizePocketCharacter(value.character, 'cold')
        else normalizePocketChat(value, 'cold')
    }
    return value
}
