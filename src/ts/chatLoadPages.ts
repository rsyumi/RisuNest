export const DEFAULT_CHAT_LOAD_INITIAL_PAGES = 30
export const DEFAULT_CHAT_LOAD_ADDITIONAL_PAGES = 15

const liveChatMarkup = /<\s*(?:button|audio|video|iframe)\b|\brisu-(?:trigger|ctrl|btn)\b|\bdata-risu-live\b|\brisu-inlay-image\b|\{\{\s*(?:inlay(?:ed|eddata)?|audio|video(?:-img)?|bgm|asset|button)::/i

export function shouldContainChatMessage(input: {
    index: number
    totalLength: number
    isStreaming: boolean
    isComment: boolean
    data: string
    captureAll: boolean
}): boolean {
    return !input.captureAll
        && !input.isStreaming
        && !input.isComment
        && input.index < input.totalLength - 1
        && !liveChatMarkup.test(input.data)
}

export function normalizeChatLoadPages(value: unknown, fallback: number): number {
    const fallbackValue = Number.isFinite(fallback) && fallback >= 1
        ? Math.floor(fallback)
        : 1
    const numberValue = typeof value === 'number' ? value : Number(value)

    if (!Number.isFinite(numberValue) || numberValue < 1) {
        return fallbackValue
    }

    return Math.floor(numberValue)
}

export function getInitialChatLoadPages(db: { chatLoadInitialPages?: number }): number {
    return normalizeChatLoadPages(db.chatLoadInitialPages, DEFAULT_CHAT_LOAD_INITIAL_PAGES)
}

export function getAdditionalChatLoadPages(db: { chatLoadAdditionalPages?: number }): number {
    return normalizeChatLoadPages(db.chatLoadAdditionalPages, DEFAULT_CHAT_LOAD_ADDITIONAL_PAGES)
}
