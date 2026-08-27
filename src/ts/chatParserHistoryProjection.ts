import rfdc from 'rfdc'
import {
    classifyChatParserHistory,
    type ChatParserHistoryReason,
    type ChatParserUnsafeHistoryDependency,
} from './chatParserHistory'
import type { ProcessScriptCaptureContext } from './process/scripts'
import type { Message } from './storage/database.svelte'
import {
    CONVERSATION_RANGE_MAX_LIMIT,
    type ConversationWindowQuery,
    type DataRevision,
    type PersistentRevisionReader,
} from './storage/persistentDataStore'

const cloneProjectionData = rfdc()
const BACKWARD_SCAN_BATCH_SIZE = 128

export type ChatParserCompleteProjectionReason =
    | ChatParserHistoryReason
    | 'projection-budget'

export interface ChatParserHistoryProjectionReader {
    readonly revision: DataRevision
    readConversationWindow: PersistentRevisionReader['readConversationWindow']
}

export interface ChatParserCompleteProjectionRequest {
    readonly characterId: string
    readonly conversationId: string
    readonly revision: DataRevision
    readonly totalMessages: number
    readonly currentAbsoluteIndex: number
    readonly currentMessage: Message
    readonly reasons: readonly ChatParserCompleteProjectionReason[]
    readonly signal?: AbortSignal
}

export interface ChatParserCompleteProjectionLease {
    readonly characterId: string
    readonly conversationId: string
    readonly revision: DataRevision
    readonly totalMessages: number
    readonly context: ProcessScriptCaptureContext
    release(): void | Promise<void>
}

export interface ChatParserHistoryProjectionInput {
    characterId: string
    conversationId: string
    revision: DataRevision
    totalMessages: number
    currentAbsoluteIndex: number
    maxProjectionMessages: number
    parserSource: unknown
    parserIndirections?: Readonly<Record<string, unknown>>
    unsafeDependencies?: readonly ChatParserUnsafeHistoryDependency[]
    contextSeed: ProcessScriptCaptureContext
    reader: ChatParserHistoryProjectionReader
    signal?: AbortSignal
    isCurrent?: () => boolean
    acquireCompleteProjection?: (
        request: ChatParserCompleteProjectionRequest,
    ) => Promise<ChatParserCompleteProjectionLease>
}

interface ChatParserHistoryProjectionBase {
    readonly characterId: string
    readonly conversationId: string
    readonly revision: DataRevision
    readonly totalMessages: number
    readonly chatID: number
    readonly projectedChatID: number
    readonly historyOffset: number
}

export interface BoundedChatParserHistoryProjection extends ChatParserHistoryProjectionBase {
    readonly kind: 'bounded'
    readonly messages: readonly Message[]
    readonly context: ProcessScriptCaptureContext
}

export interface CompleteChatParserHistoryProjection extends ChatParserHistoryProjectionBase {
    readonly kind: 'complete'
    readonly reasons: readonly ChatParserCompleteProjectionReason[]
    /** Ownership transfers to the result consumer, which must release this lease exactly once. */
    readonly lease: ChatParserCompleteProjectionLease
}

export type ChatParserHistoryProjection =
    | BoundedChatParserHistoryProjection
    | CompleteChatParserHistoryProjection

export class ChatParserCompleteProjectionRequiredError extends Error {
    readonly reasons: readonly ChatParserCompleteProjectionReason[]

    constructor(reasons: readonly ChatParserCompleteProjectionReason[]) {
        super(`Complete chat parser history projection required: ${reasons.join(', ')}`)
        this.name = 'ChatParserCompleteProjectionRequiredError'
        this.reasons = reasons
    }
}

export class ChatParserHistoryProjectionStaleError extends Error {
    constructor() {
        super('Chat parser history projection became stale')
        this.name = 'ChatParserHistoryProjectionStaleError'
    }
}

export async function createChatParserHistoryProjection(
    input: ChatParserHistoryProjectionInput,
): Promise<ChatParserHistoryProjection> {
    validateProjectionInput(input)
    assertProjectionCurrent(input)
    validateContextIdentity(input.contextSeed, input, false)

    const [readCurrentMessage] = await readExactRange(
        input,
        input.currentAbsoluteIndex,
        input.currentAbsoluteIndex + 1,
    )
    const currentMessage = cloneProjectionData(readCurrentMessage)
    const classification = classifyChatParserHistory({
        source: [input.parserSource, currentMessage],
        indirections: input.parserIndirections,
        unsafeDependencies: input.unsafeDependencies,
    })
    let initialStart = input.currentAbsoluteIndex
    let initialEnd = input.currentAbsoluteIndex + 1
    for (const index of classification.absoluteMessageIndices) {
        if (index < 0 || index >= input.totalMessages) continue
        initialStart = Math.min(initialStart, index)
        initialEnd = Math.max(initialEnd, index + 1)
    }
    const reasons: ChatParserCompleteProjectionReason[] = [...classification.reasons]
    if (initialEnd - initialStart > input.maxProjectionMessages) {
        reasons.push('projection-budget')
    }
    if (reasons.length > 0) {
        return acquireCompleteProjection(input, currentMessage, uniqueReasons(reasons))
    }

    let prefixStart = initialStart
    const initialPrefix = prefixStart === input.currentAbsoluteIndex
        ? []
        : await readExactRange(input, prefixStart, input.currentAbsoluteIndex)
    const prefixSegmentsNewestFirst = initialPrefix.length > 0 ? [initialPrefix] : []
    const suffix = initialEnd === input.currentAbsoluteIndex + 1
        ? []
        : await readExactRange(input, input.currentAbsoluteIndex + 1, initialEnd)

    const backward = createBackwardRowScan(currentMessage, input.currentAbsoluteIndex)
    inspectBackwardRows(initialPrefix, prefixStart, backward)
    while (!backwardRowsSatisfied(backward) && prefixStart > 0) {
        const minimumAllowedStart = initialEnd - input.maxProjectionMessages
        const nextStart = Math.max(
            0,
            minimumAllowedStart,
            prefixStart - BACKWARD_SCAN_BATCH_SIZE,
        )
        if (nextStart === prefixStart) {
            return acquireCompleteProjection(input, currentMessage, ['projection-budget'])
        }
        const extension = await readExactRange(input, nextStart, prefixStart)
        prefixSegmentsNewestFirst.push(extension)
        prefixStart = nextStart
        inspectBackwardRows(extension, prefixStart, backward)
    }

    const historyOffset = Math.min(initialStart, backward.requiredStart)
    const prefix = [...prefixSegmentsNewestFirst].reverse().flat()
    const retainedPrefix = prefix.slice(historyOffset - prefixStart)
    const messages = [...retainedPrefix, currentMessage, ...suffix]
    if (messages.length > input.maxProjectionMessages) {
        return acquireCompleteProjection(input, currentMessage, ['projection-budget'])
    }
    const context = buildBoundedContext(input.contextSeed, input, messages, historyOffset)
    assertProjectionCurrent(input)
    return {
        kind: 'bounded',
        characterId: input.characterId,
        conversationId: input.conversationId,
        revision: input.revision,
        totalMessages: input.totalMessages,
        chatID: input.currentAbsoluteIndex,
        projectedChatID: input.currentAbsoluteIndex - historyOffset,
        historyOffset,
        messages,
        context,
    }
}

function validateProjectionInput(input: ChatParserHistoryProjectionInput): void {
    if (!input.characterId || !input.conversationId) {
        throw new Error('Chat parser history projection requires exact conversation identity')
    }
    if (!Number.isSafeInteger(input.revision) || input.revision < 0) {
        throw new RangeError('Chat parser history projection revision must be nonnegative')
    }
    if (!Number.isSafeInteger(input.totalMessages) || input.totalMessages <= 0) {
        throw new RangeError('Chat parser history projection message count must be positive')
    }
    if (
        !Number.isSafeInteger(input.currentAbsoluteIndex)
        || input.currentAbsoluteIndex < 0
        || input.currentAbsoluteIndex >= input.totalMessages
    ) {
        throw new RangeError('Chat parser history projection current row is out of range')
    }
    if (
        !Number.isSafeInteger(input.maxProjectionMessages)
        || input.maxProjectionMessages <= 0
    ) {
        throw new RangeError('Chat parser history projection budget must be positive')
    }
    if (input.reader.revision !== input.revision) {
        throw new Error('Chat parser history reader revision does not match the request')
    }
}

function assertProjectionCurrent(input: ChatParserHistoryProjectionInput): void {
    if (input.signal?.aborted) {
        throw new DOMException('Chat parser history projection was cancelled', 'AbortError')
    }
    if (input.reader.revision !== input.revision || input.isCurrent?.() === false) {
        throw new ChatParserHistoryProjectionStaleError()
    }
}

async function readExactRange(
    input: ChatParserHistoryProjectionInput,
    startIndex: number,
    endIndex: number,
): Promise<Message[]> {
    const messages: Message[] = []
    for (let cursor = startIndex; cursor < endIndex;) {
        assertProjectionCurrent(input)
        const limit = Math.min(CONVERSATION_RANGE_MAX_LIMIT, endIndex - cursor)
        const query: ConversationWindowQuery = {
            characterId: input.characterId,
            conversationId: input.conversationId,
            startIndex: cursor,
            limit,
        }
        const result = await input.reader.readConversationWindow(query)
        assertProjectionCurrent(input)
        if (!result) throw new Error('Chat parser history conversation window is missing')
        validateWindow(result, input, cursor, limit)
        messages.push(...result.value.messages)
        cursor += limit
    }
    return messages
}

function validateWindow(
    result: Awaited<ReturnType<PersistentRevisionReader['readConversationWindow']>>,
    input: ChatParserHistoryProjectionInput,
    startIndex: number,
    limit: number,
): void {
    if (!result || result.revision !== input.revision) {
        throw new Error('Chat parser history window revision does not match the request')
    }
    const window = result.value
    const endIndex = startIndex + limit
    if (
        window.characterId !== input.characterId
        || window.conversationId !== input.conversationId
        || window.totalMessages !== input.totalMessages
        || window.startIndex !== startIndex
        || window.endIndex !== endIndex
        || window.messages.length !== limit
        || window.hasMoreBefore !== (startIndex > 0)
        || window.hasMoreAfter !== (endIndex < input.totalMessages)
    ) {
        throw new Error('Chat parser history conversation window is not the exact requested range')
    }
    assertValidMessageRows(
        window.messages,
        'Chat parser history conversation window contains an invalid message row',
        'Chat parser history conversation window message rows must be dense',
    )
}

interface BackwardRowScan {
    readonly pendingRoles: Set<Message['role']>
    remainingUsers: number
    requiredStart: number
}

function createBackwardRowScan(
    currentMessage: Message,
    currentAbsoluteIndex: number,
): BackwardRowScan {
    return {
        pendingRoles: new Set<Message['role']>([currentMessage.role, 'char']),
        remainingUsers: 2,
        requiredStart: currentAbsoluteIndex,
    }
}

function inspectBackwardRows(
    prefix: readonly Message[],
    prefixStart: number,
    scan: BackwardRowScan,
): void {
    for (let offset = prefix.length - 1; offset >= 0; offset -= 1) {
        const message = prefix[offset]
        const absoluteIndex = prefixStart + offset
        const role = message.role
        if (scan.pendingRoles.delete(role)) {
            scan.requiredStart = Math.min(scan.requiredStart, absoluteIndex)
        }
        if (role === 'user' && scan.remainingUsers > 0) {
            scan.remainingUsers -= 1
            scan.requiredStart = Math.min(scan.requiredStart, absoluteIndex)
        }
    }
}

function backwardRowsSatisfied(scan: BackwardRowScan): boolean {
    return scan.pendingRoles.size === 0 && scan.remainingUsers === 0
}

async function acquireCompleteProjection(
    input: ChatParserHistoryProjectionInput,
    currentMessage: Message,
    reasons: readonly ChatParserCompleteProjectionReason[],
): Promise<CompleteChatParserHistoryProjection> {
    if (!input.acquireCompleteProjection) {
        throw new ChatParserCompleteProjectionRequiredError(reasons)
    }
    assertProjectionCurrent(input)
    const candidate: unknown = await input.acquireCompleteProjection({
        characterId: input.characterId,
        conversationId: input.conversationId,
        revision: input.revision,
        totalMessages: input.totalMessages,
        currentAbsoluteIndex: input.currentAbsoluteIndex,
        currentMessage: cloneProjectionData(currentMessage),
        reasons,
        signal: input.signal,
    })
    let lease: ChatParserCompleteProjectionLease
    try {
        assertProjectionCurrent(input)
        validateCompleteLease(candidate, input, currentMessage)
        lease = candidate
    } catch (error) {
        await releaseRejectedCompleteLease(candidate)
        throw error
    }
    return {
        kind: 'complete',
        characterId: input.characterId,
        conversationId: input.conversationId,
        revision: input.revision,
        totalMessages: input.totalMessages,
        chatID: input.currentAbsoluteIndex,
        projectedChatID: input.currentAbsoluteIndex,
        historyOffset: 0,
        reasons,
        lease,
    }
}

function validateCompleteLease(
    lease: unknown,
    input: ChatParserHistoryProjectionInput,
    currentMessage: Message,
): asserts lease is ChatParserCompleteProjectionLease {
    if (
        typeof lease !== 'object'
        || lease === null
        || typeof (lease as { release?: unknown }).release !== 'function'
    ) {
        throw new Error('Complete projection lease must provide a callable release')
    }
    const completeLease = lease as ChatParserCompleteProjectionLease
    if (
        completeLease.characterId !== input.characterId
        || completeLease.conversationId !== input.conversationId
        || completeLease.revision !== input.revision
        || completeLease.totalMessages !== input.totalMessages
    ) {
        throw new Error('Complete projection evidence does not match the requested conversation')
    }
    validateContextIdentity(completeLease.context, input, true, currentMessage)
}

async function releaseRejectedCompleteLease(lease: unknown): Promise<void> {
    if (typeof lease !== 'object' || lease === null) return
    const release = (lease as { release?: unknown }).release
    if (typeof release !== 'function') return
    try {
        await release.call(lease)
    } catch {
        // Preserve the validation, cancellation, or staleness error that rejected the lease.
    }
}

function validateContextIdentity(
    context: ProcessScriptCaptureContext,
    input: ChatParserHistoryProjectionInput,
    complete: boolean,
    currentMessage?: Message,
): void {
    const parser = context.parserContext
    const character = parser.character
    const databaseCharacter = parser.database.characters[parser.selectedCharID]
    const chat = character?.chats?.[character.chatPage]
    const databaseChat = databaseCharacter?.chats?.[databaseCharacter.chatPage]
    if (
        character?.chaId !== input.characterId
        || databaseCharacter?.chaId !== input.characterId
        || chat?.id !== input.conversationId
        || databaseChat?.id !== input.conversationId
    ) {
        throw new Error('Chat parser projection context does not match the requested conversation')
    }
    if (complete && (databaseCharacter !== character || databaseChat !== chat)) {
        throw new Error('Complete projection context must use one shared conversation authority')
    }
    const expectedCount = complete ? input.totalMessages : 0
    if (chat.message.length !== expectedCount || databaseChat.message.length !== expectedCount) {
        throw new Error(
            complete
                ? 'Complete projection context message count is not complete'
                : 'Bounded projection context seed must not contain conversation messages',
        )
    }
    if (complete) {
        assertDenseCompleteMessages(chat.message)
        if (!isSameProjectionValue(chat.message[input.currentAbsoluteIndex], currentMessage)) {
            throw new Error('Complete projection current row does not match the pinned PDS row')
        }
    }
    if (complete && (parser.historyOffset ?? 0) !== 0) {
        throw new Error('Complete projection context cannot have a history offset')
    }
}

function assertDenseCompleteMessages(messages: readonly Message[]): void {
    assertValidMessageRows(
        messages,
        'Complete projection contains an invalid message row',
        'Complete projection message history must be dense',
    )
}

function assertValidMessageRows(
    messages: readonly Message[],
    invalidMessage: string,
    sparseMessage: string,
): void {
    for (let index = 0; index < messages.length; index += 1) {
        if (!(index in messages)) throw new Error(`${sparseMessage} at index ${index}`)
        const message = messages[index]
        const role = message?.role
        if (
            typeof message !== 'object'
            || message === null
            || (role !== 'user' && role !== 'char')
            || typeof message.data !== 'string'
        ) {
            throw new Error(`${invalidMessage} at index ${index}`)
        }
    }
}

function isSameProjectionValue(left: unknown, right: unknown): boolean {
    if (Object.is(left, right)) return true
    if (typeof left !== 'object' || left === null || typeof right !== 'object' || right === null) {
        return false
    }
    if (Array.isArray(left) || Array.isArray(right)) {
        if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
            return false
        }
        for (let index = 0; index < left.length; index += 1) {
            if ((index in left) !== (index in right)) return false
            if (index in left && !isSameProjectionValue(left[index], right[index])) return false
        }
        return true
    }
    const leftRecord = left as Record<string, unknown>
    const rightRecord = right as Record<string, unknown>
    const leftKeys = Object.keys(leftRecord).sort()
    const rightKeys = Object.keys(rightRecord).sort()
    if (leftKeys.length !== rightKeys.length) return false
    for (let index = 0; index < leftKeys.length; index += 1) {
        const key = leftKeys[index]
        if (key !== rightKeys[index] || !isSameProjectionValue(leftRecord[key], rightRecord[key])) {
            return false
        }
    }
    return true
}

function buildBoundedContext(
    seed: ProcessScriptCaptureContext,
    input: ChatParserHistoryProjectionInput,
    messages: Message[],
    historyOffset: number,
): ProcessScriptCaptureContext {
    const context = cloneProjectionData(seed) as ProcessScriptCaptureContext
    const parser = context.parserContext
    const character = parser.character
    character.chats[character.chatPage].message = messages
    parser.database.characters[parser.selectedCharID] = character
    parser.historyOffset = historyOffset
    validateBoundedContext(context, input, messages.length)
    return context
}

function validateBoundedContext(
    context: ProcessScriptCaptureContext,
    input: ChatParserHistoryProjectionInput,
    expectedMessages: number,
): void {
    const parser = context.parserContext
    const character = parser.character
    const chat = character.chats[character.chatPage]
    if (
        character.chaId !== input.characterId
        || chat.id !== input.conversationId
        || chat.message.length !== expectedMessages
        || parser.database.characters[parser.selectedCharID] !== character
    ) {
        throw new Error('Bounded projection context could not preserve exact conversation identity')
    }
}

function uniqueReasons(
    reasons: readonly ChatParserCompleteProjectionReason[],
): ChatParserCompleteProjectionReason[] {
    return [...new Set(reasons)]
}
