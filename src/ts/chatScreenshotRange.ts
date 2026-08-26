import type { Message } from './storage/database.svelte'

export type ScreenshotRange = Readonly<{ start: number; end: number }>

export type ScreenshotRangeValidation =
    | Readonly<{ ok: true; start: number; end: number }>
    | Readonly<{
          ok: false
          reason: 'empty' | 'integer' | 'bounds' | 'order'
      }>

type DeepReadonly<T> = T extends (...args: never[]) => unknown
    ? T
    : T extends readonly (infer U)[]
      ? readonly DeepReadonly<U>[]
      : T extends object
        ? { readonly [K in keyof T]: DeepReadonly<T[K]> }
        : T

export interface ChatScreenshotJob {
    readonly characterId: string
    readonly chatId: string
    readonly totalTurns: number
    readonly start: number
    readonly end: number
    readonly messages: readonly DeepReadonly<Message>[]
}

export function validateScreenshotRange(
    totalTurns: number,
    start: number,
    end: number,
): ScreenshotRangeValidation {
    if (totalTurns === 0) return { ok: false, reason: 'empty' }
    if (!Number.isInteger(start) || !Number.isInteger(end)) {
        return { ok: false, reason: 'integer' }
    }
    if (start < 1 || end < 1 || start > totalTurns || end > totalTurns) {
        return { ok: false, reason: 'bounds' }
    }
    if (start > end) return { ok: false, reason: 'order' }
    return { ok: true, start, end }
}

export function recentScreenshotRange(totalTurns: number): ScreenshotRange {
    return Object.freeze({ start: Math.max(1, totalTurns - 49), end: totalTurns })
}

export function fullScreenshotRange(totalTurns: number): ScreenshotRange {
    return Object.freeze({ start: 1, end: totalTurns })
}

function deepFreeze<T>(value: T): DeepReadonly<T> {
    if (value && typeof value === 'object' && !Object.isFrozen(value)) {
        Object.freeze(value)
        for (const child of Object.values(value)) deepFreeze(child)
    }
    return value as DeepReadonly<T>
}

export function createChatScreenshotJob(input: {
    characterId: string
    chatId: string
    messages: Message[]
    start: number
    end: number
}): ChatScreenshotJob {
    const validation = validateScreenshotRange(input.messages.length, input.start, input.end)
    if (!validation.ok) throw new Error(`Invalid screenshot range: ${validation.reason}`)

    const selectedMessages = structuredClone(
        input.messages.slice(validation.start - 1, validation.end),
    )
    return deepFreeze({
        characterId: input.characterId,
        chatId: input.chatId,
        totalTurns: input.messages.length,
        start: validation.start,
        end: validation.end,
        messages: selectedMessages,
    })
}
