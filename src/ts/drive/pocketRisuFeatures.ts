import type { Message } from '../storage/database.svelte'
import { snapshotResponse, type ResponseVariantSet } from '../responseVariants'

type RecordValue = Record<string, unknown>
const object = (value: unknown): value is RecordValue =>
    !!value && typeof value === 'object' && !Array.isArray(value)

function toggleValues(value: unknown): void {
    if (
        !object(value) ||
        Object.entries(value).some(([key, value]) => !key.startsWith('toggle_') || typeof value !== 'string')
    ) {
        throw new TypeError('PocketRisu toggle values must be a toggle_ string record')
    }
}

export function normalizePocketMessage(value: unknown, fallbackId: string): void {
    if (!object(value) || value.swipes === undefined) return
    if (!Array.isArray(value.swipes) || value.swipes.some((swipe) => typeof swipe !== 'string'))
        throw new TypeError('PocketRisu swipes must be strings')
    if (
        value.swipeId !== undefined &&
        (typeof value.swipeId !== 'number' || !Number.isInteger(value.swipeId))
    )
        throw new TypeError('PocketRisu swipeId must be an integer')
    if (typeof value.data !== 'string') throw new TypeError('PocketRisu response body must be a string')
    if (value.responseVariants !== undefined) return
    const swipes = [...value.swipes] as string[]
    let index = value.swipeId as number | undefined
    const warnings: string[] = []
    if (index === undefined || index < 0 || index >= swipes.length) {
        warnings.push('invalid-swipe-index')
        const matching = swipes.flatMap((swipe, i) => (swipe === value.data ? [i] : []))
        index = matching.length === 1 ? matching[0] : swipes.length
        if (index === swipes.length) swipes.push(value.data)
    } else if (swipes[index] !== value.data) {
        warnings.push('swipe-body-mismatch')
        index = swipes.length
        swipes.push(value.data)
    }
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

export function normalizePocketChat(value: unknown, fallbackId: string): void {
    if (!object(value)) return
    if (value.savedToggleValues !== undefined) toggleValues(value.savedToggleValues)
    if (value.bindedPersona !== undefined && typeof value.bindedPersona !== 'string')
        throw new TypeError('PocketRisu persona binding must be a string')
    if (Array.isArray(value.message))
        value.message.forEach((message, index) =>
            normalizePocketMessage(
                message,
                `${typeof value.id === 'string' ? value.id : fallbackId}:response:${index}`,
            ),
        )
}

export function normalizePocketCharacter(value: unknown, fallbackId = 'character'): void {
    if (object(value) && Array.isArray(value.chats))
        value.chats.forEach((chat, index) => normalizePocketChat(chat, `${fallbackId}:chat:${index}`))
}

export function normalizePocketFeatures<T>(value: T): T {
    if (!object(value)) return value
    if (value.disableToggleBinding !== undefined && typeof value.disableToggleBinding !== 'boolean')
        throw new TypeError('PocketRisu disableToggleBinding must be boolean')
    if (value.defaultToggleValues !== undefined) toggleValues(value.defaultToggleValues)
    if (value.togglePresets !== undefined) {
        if (!Array.isArray(value.togglePresets))
            throw new TypeError('PocketRisu toggle presets must be an array')
        for (const preset of value.togglePresets) {
            if (!object(preset) || typeof preset.name !== 'string')
                throw new TypeError('PocketRisu toggle preset must have a name')
            toggleValues(preset.values)
        }
    }
    if (Array.isArray(value.personas)) {
        const ids = new Set<string>()
        for (const persona of value.personas) {
            if (!object(persona) || persona.id === undefined) continue
            if (typeof persona.id !== 'string' || (persona.id && ids.has(persona.id)))
                throw new TypeError('PocketRisu persona IDs must be unique strings')
            if (persona.id) ids.add(persona.id)
        }
    }
    if (Array.isArray(value.characters))
        value.characters.forEach((character, index) =>
            normalizePocketCharacter(character, `character:${index}`),
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
