export type ChatParserUnsafeHistoryDependency =
    | 'lua'
    | 'plugin-v2'
    | 'display-trigger'
    | 'inject'

export type ChatParserHistoryReason =
    | ChatParserUnsafeHistoryDependency
    | 'full-history-cbs'
    | 'dynamic-previous-chat-log'
    | 'ambiguous-risu-style'

export interface ChatParserHistoryClassification {
    readonly requiresFullHistory: boolean
    readonly absoluteMessageIndices: readonly number[]
    readonly reasons: readonly ChatParserHistoryReason[]
}

export interface ChatParserHistoryClassificationInput {
    readonly source: unknown
    readonly unsafeDependencies?: readonly ChatParserUnsafeHistoryDependency[]
    readonly indirections?: Readonly<Record<string, unknown>>
}

const FULL_HISTORY_CBS_NAMES = new Set([
    'userhistory',
    'usermessages',
    'charhistory',
    'charmessages',
    'history',
    'messages',
    'messageunixtimearray',
    'idleduration',
    'lastmessage',
    'lastmessageid',
    'lastmessageindex',
    'pick',
    'rollp',
    'rollpick',
])

const PREVIOUS_CHAT_LOG_CBS_NAME = 'previouschatlog'

export function classifyChatParserHistory(
    input: ChatParserHistoryClassificationInput,
): ChatParserHistoryClassification {
    const reasons = new Set<ChatParserHistoryReason>(input.unsafeDependencies ?? [])
    const absoluteMessageIndices = new Set<number>()
    const texts = collectParserInputText(input.source)
    const indirections = new Map(
        Object.entries(input.indirections ?? {}).map(([name, value]) => [
            normalizeCbsName(name),
            value,
        ]),
    )
    const followedIndirections = new Set<string>()

    for (const text of texts) {
        classifyText(
            text,
            reasons,
            absoluteMessageIndices,
            true,
            indirections,
            followedIndirections,
        )
    }

    return {
        requiresFullHistory: reasons.size > 0,
        absoluteMessageIndices: [...absoluteMessageIndices].sort((a, b) => a - b),
        reasons: [...reasons],
    }
}

export function extendChatParserHistoryBounds(
    classification: ChatParserHistoryClassification,
    bounds: Readonly<{ start: number; end: number; totalMessages: number }>,
): Readonly<{ start: number; end: number }> {
    if (classification.requiresFullHistory) {
        return { start: 0, end: bounds.totalMessages }
    }

    let start = bounds.start
    let end = bounds.end
    for (const requestedIndex of classification.absoluteMessageIndices) {
        if (requestedIndex < 0 || requestedIndex >= bounds.totalMessages) continue
        start = Math.min(start, requestedIndex)
        end = Math.max(end, requestedIndex + 1)
    }
    return { start, end }
}

function classifyText(
    text: string,
    reasons: Set<ChatParserHistoryReason>,
    absoluteMessageIndices: Set<number>,
    decodeStyles: boolean,
    indirections: ReadonlyMap<string, unknown>,
    followedIndirections: Set<string>,
): void {
    for (const expression of extractCbsExpressions(text)) {
        const parsed = parseCbsExpression(expression)
        if (FULL_HISTORY_CBS_NAMES.has(parsed.name)) {
            reasons.add('full-history-cbs')
            continue
        }
        if (parsed.name === PREVIOUS_CHAT_LOG_CBS_NAME) {
            if (!parsed.firstArgument || !/^\d+$/.test(parsed.firstArgument)) {
                reasons.add('dynamic-previous-chat-log')
            } else {
                absoluteMessageIndices.add(Number(parsed.firstArgument))
            }
        }
        if (
            !indirections.has(parsed.name) ||
            followedIndirections.has(parsed.name)
        ) continue
        followedIndirections.add(parsed.name)
        for (const indirectText of collectParserInputText(indirections.get(parsed.name))) {
            classifyText(
                indirectText,
                reasons,
                absoluteMessageIndices,
                true,
                indirections,
                followedIndirections,
            )
        }
    }

    if (!decodeStyles || !text.toLocaleLowerCase().includes('<risu-style')) return

    const stylePattern = /<risu-style(?:\s[^>]*)?>([\s\S]*?)<\/risu-style\s*>/gi
    let textWithoutMatchedStyles = text
    for (const match of text.matchAll(stylePattern)) {
        const decoded = decodeHexUtf8(match[1])
        if (decoded === null) {
            reasons.add('ambiguous-risu-style')
        } else {
            classifyText(
                decoded,
                reasons,
                absoluteMessageIndices,
                false,
                indirections,
                followedIndirections,
            )
        }
    }
    textWithoutMatchedStyles = textWithoutMatchedStyles.replace(stylePattern, '')
    if (/<risu-style\b/i.test(textWithoutMatchedStyles)) {
        reasons.add('ambiguous-risu-style')
    }
}

function extractCbsExpressions(text: string): string[] {
    const starts: number[] = []
    const expressions: string[] = []
    for (let index = 0; index < text.length - 1; index += 1) {
        if (text[index] === '{' && text[index + 1] === '{') {
            starts.push(index + 2)
            index += 1
            continue
        }
        if (text[index] !== '}' || text[index + 1] !== '}' || starts.length === 0) continue
        expressions.push(text.slice(starts.pop(), index))
        index += 1
    }
    return expressions
}

function parseCbsExpression(expression: string): Readonly<{
    name: string
    firstArgument: string | null
}> {
    const colonIndex = expression.indexOf(':')
    const rawName = colonIndex === -1 ? expression : expression.slice(0, colonIndex)
    const name = normalizeCbsName(rawName)
    if (colonIndex === -1) return { name, firstArgument: null }

    const doubleColon = expression[colonIndex + 1] === ':'
    const argumentStart = colonIndex + (doubleColon ? 2 : 1)
    const argumentText = expression.slice(argumentStart)
    const nextSeparator = argumentText.indexOf(doubleColon ? '::' : ':')
    return {
        name,
        firstArgument: (nextSeparator === -1
            ? argumentText
            : argumentText.slice(0, nextSeparator)
        ).trim(),
    }
}

function normalizeCbsName(name: string): string {
    return name.toLocaleLowerCase().replace(/[\s_-]/g, '')
}

function decodeHexUtf8(value: string): string | null {
    if (!/^(?:[0-9a-fA-F]{2})*$/.test(value)) return null
    const bytes = new Uint8Array(value.length / 2)
    for (let index = 0; index < bytes.length; index += 1) {
        bytes[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16)
    }
    try {
        return new TextDecoder('utf-8', { fatal: true }).decode(bytes)
    } catch {
        return null
    }
}

function collectParserInputText(value: unknown, seen = new WeakSet<object>()): string[] {
    if (typeof value === 'string') return [value]
    if (!value || typeof value !== 'object' || seen.has(value)) return []
    seen.add(value)
    const output: string[] = []
    for (const child of Object.values(value)) {
        output.push(...collectParserInputText(child, seen))
    }
    return output
}
