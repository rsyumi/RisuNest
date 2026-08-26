import type { Chat, Message, character, customscript, groupChat } from './storage/database.svelte'
import type { simpleCharacterArgument } from './parser/parser.svelte'
import type { ProcessScriptCaptureContext } from './process/scripts'
import rfdc from 'rfdc'

const cloneScreenshotData = rfdc()

export type ScreenshotRange = Readonly<{ start: number; end: number }>

export type ScreenshotRangeValidation =
    | Readonly<{ ok: true; start: number; end: number }>
    | Readonly<{
          ok: false
          reason: 'empty' | 'integer' | 'bounds' | 'order'
      }>

export type DeepReadonly<T> = T extends (...args: never[]) => unknown
    ? T
    : T extends readonly unknown[]
      ? { readonly [K in keyof T]: DeepReadonly<T[K]> }
      : T extends object
        ? { readonly [K in keyof T]: DeepReadonly<T[K]> }
        : T

export interface ChatScreenshotRenderSettings {
    autoTranslate: boolean
    autoTranslateCachedOnly: boolean
    translatorType: string
    translateBeforeHTMLFormatting: boolean
    legacyTranslation: boolean
    showTranslationLoading: boolean
    newImageHandlingBeta: boolean
    assetWidth: number
    hideAllImages: boolean
    iconSize: number
    zoomSize: number
    lineHeight: number
    dynamicAssets: boolean
    dynamicAssetsEditDisplay: boolean
    legacyMediaFindings: boolean
    assetMaxDifference: number
    theme?: string
    guiHTML?: string
    roundIcons?: boolean
    hideIcons?: boolean
    proseInvert?: boolean
    requestInfoInsideChat?: boolean
    aiLawApplies?: boolean
    translator?: string
    swipe?: boolean
    showFirstMessagePages?: boolean
    memoryLimitThickness?: number
    customQuotes?: boolean
    customQuotesData?: [string, string, string, string]
    unformatQuotes?: boolean
    blockquoteStyling?: boolean
    returnCSSError?: boolean
}

export interface ChatScreenshotRenderContext {
    character: simpleCharacterArgument | null
    characterName: string
    characterImageSource: string
    characterLargePortrait: boolean
    userName: string
    userImageSource: string
    userLargePortrait: boolean
    moduleAssets: [string, string, string][]
    presetRegex: customscript[]
    moduleRegexScripts: customscript[]
    assetStyle: string
    parserContext: ProcessScriptCaptureContext['parserContext']
    totalTurns?: number
    selectionStart?: number
    historyStartIndex?: number
    firstParserMessageIndex?: number
    settings: ChatScreenshotRenderSettings
}

export type FrozenChatScreenshotRenderContext = DeepReadonly<ChatScreenshotRenderContext>

export interface ChatScreenshotJob {
    readonly characterId: string
    readonly chatId: string
    readonly totalTurns: number
    readonly start: number
    readonly end: number
    readonly messages: readonly DeepReadonly<Message>[]
    readonly renderContext: FrozenChatScreenshotRenderContext
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

export function snapshotChatScreenshotCharacter(
    source: character | groupChat,
    chat: Chat,
): character | groupChat {
    const captureChat: Chat = {
        message: [],
        note: chat.note ?? '',
        name: chat.name ?? '',
        localLore: chat.localLore ?? [],
        scriptstate: chat.scriptstate ?? {},
        modules: chat.modules ?? [],
        id: chat.id,
        bindedPersona: chat.bindedPersona,
        fmIndex: chat.fmIndex ?? -1,
        bookmarks: chat.bookmarks ?? [],
        bookmarkNames: chat.bookmarkNames ?? {},
        useLocallySetGlobalVariables: chat.useLocallySetGlobalVariables,
        GLGlobalVariables: chat.GLGlobalVariables ?? {},
    }
    const shared = {
        type: source.type,
        name: source.name,
        nickname: source.nickname,
        chaId: source.chaId,
        firstMessage: source.firstMessage ?? '',
        alternateGreetings: source.alternateGreetings ?? [],
        chats: [captureChat],
        chatPage: 0,
        customscript: source.customscript ?? [],
        virtualscript: source.virtualscript,
        globalLore: source.globalLore ?? [],
        defaultVariables: source.defaultVariables ?? '',
        additionalAssets: source.additionalAssets ?? [],
        emotionImages: source.emotionImages ?? [],
        prebuiltAssetStyle: source.prebuiltAssetStyle ?? '',
        prebuiltAssetCommand: source.prebuiltAssetCommand ?? false,
        prebuiltAssetExclude: source.prebuiltAssetExclude ?? [],
    }
    if (source.type === 'group') {
        return {
            ...shared,
            type: 'group',
            characters: source.characters ?? [],
            characterTalks: source.characterTalks ?? [],
            characterActive: source.characterActive ?? [],
        } as groupChat
    }
    return {
        ...shared,
        type: 'character',
        desc: source.desc ?? '',
        personality: source.personality ?? '',
        scenario: source.scenario ?? '',
        exampleMessage: source.exampleMessage ?? '',
        systemPrompt: source.systemPrompt ?? '',
        postHistoryInstructions: source.postHistoryInstructions ?? '',
        translatorNote: source.translatorNote ?? '',
        triggerscript: source.triggerscript ?? [],
    } as character
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
    renderContext: ChatScreenshotRenderContext
}): ChatScreenshotJob {
    const validation = validateScreenshotRange(input.messages.length, input.start, input.end)
    if (validation.ok === false) throw new Error(`Invalid screenshot range: ${validation.reason}`)

    const selectedMessages = cloneScreenshotData(
        input.messages.slice(validation.start - 1, validation.end),
    )
    const renderContext = cloneScreenshotData(input.renderContext)
    const historyBounds = deriveParserHistoryBounds(
        input.messages,
        validation.start - 1,
        validation.end,
        renderContext,
    )
    const historyMessages = cloneScreenshotData(
        input.messages.slice(historyBounds.start, validation.start - 1),
    )
    const trailingMessages = cloneScreenshotData(
        input.messages.slice(validation.end, historyBounds.end),
    )
    const parserMessages = [...historyMessages, ...selectedMessages, ...trailingMessages]
    const parserCharacter = renderContext.parserContext.character
    parserCharacter.chats[parserCharacter.chatPage].message = parserMessages
    renderContext.parserContext.database.characters[renderContext.parserContext.selectedCharID] = parserCharacter
    renderContext.parserContext.historyOffset = historyBounds.start
    renderContext.totalTurns = input.messages.length
    renderContext.selectionStart = validation.start
    renderContext.historyStartIndex = historyBounds.start
    renderContext.firstParserMessageIndex = validation.start - 1 - historyBounds.start
    return deepFreeze({
        characterId: input.characterId,
        chatId: input.chatId,
        totalTurns: input.messages.length,
        start: validation.start,
        end: validation.end,
        messages: selectedMessages,
        renderContext,
    })
}

function deriveParserHistoryBounds(
    messages: Message[],
    selectionStartIndex: number,
    selectionEndExclusive: number,
    renderContext: ChatScreenshotRenderContext,
) {
    let start = selectionStartIndex
    let end = selectionEndExclusive

    const selectedRoles = new Set(
        messages.slice(selectionStartIndex, selectionEndExclusive).map((message) => message.role),
    )
    selectedRoles.add('char')

    for (const role of selectedRoles) {
        const previousIndex = findPreviousRoleIndex(messages, selectionStartIndex, role)
        if (previousIndex !== -1) start = Math.min(start, previousIndex)
    }

    let previousUserIndex = selectionStartIndex
    for (let count = 0; count < 2; count += 1) {
        previousUserIndex = findPreviousRoleIndex(messages, previousUserIndex, 'user')
        if (previousUserIndex === -1) break
        start = Math.min(start, previousUserIndex)
    }

    const captureText = collectCaptureText([
        messages.slice(selectionStartIndex, selectionEndExclusive),
        renderContext,
    ]).join('\n')
    if (/{{\s*(?:userhistory|usermessages|user_history|charhistory|charmessages|char_history|history|messages|messageunixtimearray|idleduration|idle_duration)(?=\s*(?:::|}}))/i.test(captureText)) {
        start = 0
        end = messages.length
    }

    for (const match of captureText.matchAll(/{{\s*(?:previouschatlog|previous_chat_log)\s*::\s*(\d+)/gi)) {
        const requestedIndex = Number(match[1])
        if (!Number.isInteger(requestedIndex) || requestedIndex < 0 || requestedIndex >= messages.length) {
            continue
        }
        start = Math.min(start, requestedIndex)
        end = Math.max(end, requestedIndex + 1)
    }

    return { start, end }
}

function findPreviousRoleIndex(messages: Message[], beforeIndex: number, role: Message['role']) {
    for (let index = beforeIndex - 1; index >= 0; index -= 1) {
        if (messages[index].role === role) return index
    }
    return -1
}

function collectCaptureText(value: unknown, seen = new WeakSet<object>()): string[] {
    if (typeof value === 'string') return [value]
    if (!value || typeof value !== 'object' || seen.has(value)) return []
    seen.add(value)
    const output: string[] = []
    for (const child of Object.values(value)) output.push(...collectCaptureText(child, seen))
    return output
}
