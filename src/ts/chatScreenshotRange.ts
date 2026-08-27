import type { Chat, Message, character, customscript, groupChat } from './storage/database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT, type DataRevision } from './storage/persistentDataStore'
import type { simpleCharacterArgument } from './parser/parser.svelte'
import type { ProcessScriptCaptureContext } from './process/scripts'
import {
    classifyChatParserHistory,
    extendChatParserHistoryBounds,
} from './chatParserHistory'
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

export interface ChatScreenshotDialogSnapshot {
    readonly characterId: string
    readonly chatId: string
    readonly revision: DataRevision
    readonly sessionVersion: number
    readonly totalTurns: number
    readonly renderContext: FrozenChatScreenshotRenderContext
}

export interface ChatScreenshotRangeReader {
    readonly characterId: string
    readonly chatId: string
    readonly revision: DataRevision
    readonly totalTurns: number
    readRange(startIndex: number, limit: number, signal?: AbortSignal): Promise<Message[]>
    readCharacter?(characterId: string, signal?: AbortSignal): Promise<character | groupChat | null>
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

export function createChatScreenshotDialogSnapshot(input: {
    characterId: string
    chatId: string
    revision: DataRevision
    sessionVersion: number
    totalTurns: number
    renderContext: ChatScreenshotRenderContext
}): ChatScreenshotDialogSnapshot {
    const renderContext = cloneScreenshotData(input.renderContext)
    renderContext.totalTurns = input.totalTurns
    return deepFreeze({
        characterId: input.characterId,
        chatId: input.chatId,
        revision: input.revision,
        sessionVersion: input.sessionVersion,
        totalTurns: input.totalTurns,
        renderContext,
    })
}

export async function createChatScreenshotJobFromDialogSnapshot(
    snapshot: ChatScreenshotDialogSnapshot,
    reader: ChatScreenshotRangeReader,
    start: number,
    end: number,
    signal?: AbortSignal,
): Promise<ChatScreenshotJob> {
    assertScreenshotReaderMatches(snapshot, reader)
    const validation = validateScreenshotRange(snapshot.totalTurns, start, end)
    if (validation.ok === false) throw new Error(`Invalid screenshot range: ${validation.reason}`)
    const selectionStartIndex = validation.start - 1
    const selectionEndExclusive = validation.end
    const selectedMessages = await readScreenshotRange(
        reader,
        selectionStartIndex,
        selectionEndExclusive,
        signal,
    )
    const renderContext = cloneScreenshotData(
        snapshot.renderContext,
    ) as ChatScreenshotRenderContext
    const historyBounds = await deriveParserHistoryBoundsFromReader(
        reader,
        selectedMessages,
        selectionStartIndex,
        selectionEndExclusive,
        renderContext,
        signal,
    )
    const historyMessages = historyBounds.start === selectionStartIndex
        ? []
        : await readScreenshotRange(
            reader,
            historyBounds.start,
            selectionStartIndex,
            signal,
        )
    const trailingMessages = historyBounds.end === selectionEndExclusive
        ? []
        : await readScreenshotRange(
            reader,
            selectionEndExclusive,
            historyBounds.end,
            signal,
        )
    const parserMessages = [...historyMessages, ...selectedMessages, ...trailingMessages]
    const selectedOffset = selectionStartIndex - historyBounds.start
    const parserCharacter = renderContext.parserContext.character
    parserCharacter.chats[parserCharacter.chatPage].message = parserMessages
    renderContext.parserContext.database.characters[renderContext.parserContext.selectedCharID] = parserCharacter
    await hydrateScreenshotSpeakerCharacters(reader, parserMessages, renderContext, signal)
    renderContext.parserContext.historyOffset = historyBounds.start
    renderContext.totalTurns = snapshot.totalTurns
    renderContext.selectionStart = validation.start
    renderContext.historyStartIndex = historyBounds.start
    renderContext.firstParserMessageIndex = selectedOffset
    return deepFreeze({
        characterId: snapshot.characterId,
        chatId: snapshot.chatId,
        totalTurns: snapshot.totalTurns,
        start: validation.start,
        end: validation.end,
        messages: selectedMessages,
        renderContext,
    })
}

async function hydrateScreenshotSpeakerCharacters(
    reader: ChatScreenshotRangeReader,
    parserMessages: Message[],
    renderContext: ChatScreenshotRenderContext,
    signal?: AbortSignal,
): Promise<void> {
    if (!reader.readCharacter) return
    const characters = renderContext.parserContext.database.characters
    const knownIds = new Set(characters.map((candidate) => candidate.chaId))
    const missingIds = new Set(
        parserMessages
            .map((message) => message.saying)
            .filter(
                (id): id is string =>
                    typeof id === 'string' && id.length > 0 && !knownIds.has(id),
            ),
    )
    for (const characterId of missingIds) {
        assertScreenshotNotAborted(signal)
        const character = await reader.readCharacter(characterId, signal)
        assertScreenshotNotAborted(signal)
        if (character && !knownIds.has(character.chaId)) {
            characters.push(character)
            knownIds.add(character.chaId)
        }
    }
}

const SCREENSHOT_HISTORY_SCAN_BATCH_SIZE = 128

function assertScreenshotNotAborted(signal?: AbortSignal): void {
    if (signal?.aborted) {
        throw new DOMException('Screenshot capture was cancelled', 'AbortError')
    }
}

function assertScreenshotReaderMatches(
    snapshot: ChatScreenshotDialogSnapshot,
    reader: ChatScreenshotRangeReader,
): void {
    if (
        reader.characterId !== snapshot.characterId
        || reader.chatId !== snapshot.chatId
        || reader.revision !== snapshot.revision
        || reader.totalTurns !== snapshot.totalTurns
    ) {
        throw new Error('Screenshot conversation reader does not match the dialog snapshot')
    }
}

async function readScreenshotRange(
    reader: ChatScreenshotRangeReader,
    startIndex: number,
    endIndex: number,
    signal?: AbortSignal,
): Promise<Message[]> {
    const messages: Message[] = []
    for (let cursor = startIndex; cursor < endIndex;) {
        assertScreenshotNotAborted(signal)
        const limit = Math.min(CONVERSATION_RANGE_MAX_LIMIT, endIndex - cursor)
        const page = await reader.readRange(cursor, limit, signal)
        assertScreenshotNotAborted(signal)
        if (page.length !== limit) {
            throw new Error(
                `Screenshot conversation range ${cursor}:${cursor + limit} returned ${page.length} messages`,
            )
        }
        messages.push(...page)
        cursor += limit
    }
    return messages
}

async function deriveParserHistoryBoundsFromReader(
    reader: ChatScreenshotRangeReader,
    selectedMessages: Message[],
    selectionStartIndex: number,
    selectionEndExclusive: number,
    renderContext: ChatScreenshotRenderContext,
    signal?: AbortSignal,
) {
    let start = selectionStartIndex
    let end = selectionEndExclusive
    const classification = classifyChatParserHistory({
        source: [selectedMessages, renderContext],
    })
    const classifiedBounds = extendChatParserHistoryBounds(classification, {
        start,
        end,
        totalMessages: reader.totalTurns,
    })
    start = classifiedBounds.start
    end = classifiedBounds.end

    if (classification.requiresFullHistory) return { start, end }

    if (start === 0 || selectionStartIndex === 0) return { start, end }

    const pendingRoles = new Set(selectedMessages.map((message) => message.role))
    pendingRoles.add('char')
    let remainingPreviousUsers = 2
    let cursor = selectionStartIndex
    while (cursor > 0 && (pendingRoles.size > 0 || remainingPreviousUsers > 0)) {
        const batchStart = Math.max(0, cursor - SCREENSHOT_HISTORY_SCAN_BATCH_SIZE)
        const page = await readScreenshotRange(reader, batchStart, cursor, signal)
        for (let offset = page.length - 1; offset >= 0; offset -= 1) {
            const role = page[offset].role
            const absoluteIndex = batchStart + offset
            if (pendingRoles.delete(role)) start = Math.min(start, absoluteIndex)
            if (role === 'user' && remainingPreviousUsers > 0) {
                remainingPreviousUsers -= 1
                start = Math.min(start, absoluteIndex)
            }
        }
        cursor = batchStart
    }
    return { start, end }
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

    const classification = classifyChatParserHistory({
        source: [
            messages.slice(selectionStartIndex, selectionEndExclusive),
            renderContext,
        ],
    })
    const classifiedBounds = extendChatParserHistoryBounds(classification, {
        start,
        end,
        totalMessages: messages.length,
    })
    start = classifiedBounds.start
    end = classifiedBounds.end

    return { start, end }
}

function findPreviousRoleIndex(messages: Message[], beforeIndex: number, role: Message['role']) {
    for (let index = beforeIndex - 1; index >= 0; index -= 1) {
        if (messages[index].role === role) return index
    }
    return -1
}
