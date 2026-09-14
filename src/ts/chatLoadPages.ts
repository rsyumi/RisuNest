export const DEFAULT_CHAT_LOAD_INITIAL_PAGES = 30
export const DEFAULT_CHAT_LOAD_ADDITIONAL_PAGES = 15

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
