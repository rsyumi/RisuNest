import type { Chat, Message } from './database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT, type DataRevision } from './persistentDataStore'
import { safeStructuredClone } from '../safeStructuredClone'
import { WindowedConversationOverlay } from './windowedConversationOverlay'
import { messageReplaceRange } from './saveCoordinatorHelpers'
import { replaceArrayRange } from '../arrayRange'
import {
    SegmentedConversationResidency,
    type ConversationDirtyMutation,
    type ConversationPersistenceAttempt,
    type ConversationResidentInterval,
} from './segmentedConversationResidency'

export type ActiveConversationPinReason =
    | 'viewport'
    | 'editor'
    | 'playing-media'
    | 'dirty'
    | 'pending-save'
    | 'streaming'
    | 'transaction'
    | 'prompt'
    | 'compatibility'

declare const conversationSessionTokenBrand: unique symbol
declare const messageLocatorTokenBrand: unique symbol
declare const conversationPositionTokenBrand: unique symbol

export type ConversationSessionToken = string & {
    readonly [conversationSessionTokenBrand]: true
}
export type MessageLocatorToken = string & {
    readonly [messageLocatorTokenBrand]: true
}
export type ConversationPositionToken = string & {
    readonly [conversationPositionTokenBrand]: true
}

export interface MessageLocator {
    conversationId: string
    absoluteIndex: number
    expectedMessageId?: string
    sessionVersion: number
    sessionToken: ConversationSessionToken
    locatorToken: MessageLocatorToken
}

export interface ConversationPosition {
    conversationId: string
    absoluteIndex: number
    sessionVersion: number
    sessionToken: ConversationSessionToken
    positionToken: ConversationPositionToken
}

export interface ActiveConversationWindow {
    characterId: string
    conversationId: string
    messages: Message[]
    locators: MessageLocator[]
    startIndex: number
    endIndex: number
    totalMessages: number
    storeRevision: DataRevision
    sessionVersion: number
}

export interface ActiveConversationMetadataWindow extends Omit<ActiveConversationWindow, 'messages'> {
    messages: Pick<Message, 'disabled'>[]
}

export interface ActiveConversationMessageTarget {
    absoluteIndex: number
    message: Message
    locator: MessageLocator
}

export interface ActiveConversationBranchSource {
    characterId: string
    conversationId: string
    messages: Message[]
    startIndex: 0
    endIndex: number
    totalMessages: number
    storeRevision: DataRevision
    sessionVersion: number
}

export interface BackwardConversationEntry {
    absoluteIndex: number
    message: Message
    locator: MessageLocator
}

export interface ActiveConversationBackwardScan {
    characterId: string
    conversationId: string
    entries: BackwardConversationEntry[]
    startIndexExclusive: number
    totalMessages: number
    storeRevision: DataRevision
    sessionVersion: number
}

export interface ActiveConversationMetadataBackwardScan extends Omit<ActiveConversationBackwardScan, 'entries'> {
    entries: Array<{ absoluteIndex: number; disabled: Message['disabled'] }>
}

export interface ActiveConversationVariableUpdate {
    /** Changed chat variable names, without the `$` storage prefix. */
    keys: readonly string[]
    origin?: 'display'
}

export interface ActiveConversationMutationEvent {
    /** Display-listener variable writes persist without recursively invalidating that display. */
    displayVariableUpdate?: boolean
    /** Set only for chat-variable commits, which leave every message unchanged. */
    variableUpdate?: ActiveConversationVariableUpdate
    characterId: string
    conversationId: string
    sessionToken: ConversationSessionToken
    previousVersion: number
    sessionVersion: number
    commands: readonly ActiveConversationCommandName[]
    mutations: readonly ActiveConversationMutationRange[]
    conversation: ConversationMetadata
}

export interface ActiveConversationMutationRange {
    start: number
    deleteCount: number
    messages: Message[]
    sessionVersion: number
    completeOwner?: boolean
}

export type ActiveConversationCommandName =
    | 'append'
    | 'edit'
    | 'delete'
    | 'truncate'
    | 'replace-range'
    | 'update-metadata'
    | 'replace-tail'
    | 'reroll'
    | 'bookmark'
    | 'replace-conversation'

export interface SetConversationBookmarkOptions {
    bookmarked: boolean
    messageId?: string
    name?: string
}

export type ConversationMetadata = Record<string, unknown>

export interface ActiveConversationOperationRange {
    position: ConversationPosition
    deleteCount: number
    expectedMessages?: readonly Message[]
    messages: readonly Message[]
}

export interface ActiveConversationOperationCommit {
    origin?: 'display'
    expectedVersion: number
    expectedMetadata: ConversationMetadata
    metadata: ConversationMetadata
    range?: ActiveConversationOperationRange
    ranges?: readonly ActiveConversationOperationRange[]
}

export interface ActiveConversationPin {
    readonly reason: ActiveConversationPinReason
    release(): void
}

export interface ActiveConversationSessionOptions {
    characterId: string
    conversationId: string
    conversation: Chat | null
    storeRevision: DataRevision
    measureMessage?(message: Message): number
    // Receives detached event data; observers must never mutate live session state.
    onMutation?(event: ActiveConversationMutationEvent): void
    onPinReleased?(): void
}

export class ActiveConversationCompatibilitySnapshot {
    private disposed = false
    private retainedMessages: Message[]

    constructor(
        messages: Message[],
        private readonly onDispose: () => void,
    ) {
        this.retainedMessages = messages
    }

    get messages(): Message[] {
        return this.retainedMessages
    }

    get residentMessageCount(): number {
        return this.retainedMessages.length
    }

    takeMessages(): Message[] {
        if (this.disposed) {
            throw new Error('Conversation compatibility snapshot is disposed')
        }
        const messages = this.retainedMessages
        this.retainedMessages = []
        this.disposed = true
        this.onDispose()
        return messages
    }

    dispose(): void {
        if (this.disposed) return
        this.disposed = true
        this.retainedMessages.splice(0)
        this.retainedMessages = []
        this.onDispose()
    }
}

interface MessageLocatorIdentity {
    type: 'message'
    absoluteIndex: number
    messages: readonly Message[]
    message: Message
}

interface ConversationPositionIdentity {
    type: 'position'
    absoluteIndex: number
    messages: readonly Message[]
    before?: Message
    after?: Message
}

type ConversationTokenIdentity = MessageLocatorIdentity | ConversationPositionIdentity

let nextConversationSessionToken = 0
let nextConversationLocatorToken = 0

/**
 * Tokens for one content version: a content commit replaces or clears the registry,
 * a variable-only commit keeps it, so a token stays valid until the content changes.
 */
class ConversationLocatorRegistry {
    readonly sessionToken: ConversationSessionToken
    private readonly identities = new Map<string, ConversationTokenIdentity>()
    private readonly messageTokens = new Map<number, MessageLocatorToken>()
    private readonly positionTokens = new Map<number, ConversationPositionToken>()

    constructor(sessionToken?: ConversationSessionToken) {
        this.sessionToken = sessionToken ?? createConversationSessionToken()
    }

    fork(): ConversationLocatorRegistry {
        return new ConversationLocatorRegistry(this.sessionToken)
    }

    registerMessage(
        messages: readonly Message[],
        absoluteIndex: number,
        message: Message,
    ): MessageLocatorToken {
        const currentToken = this.messageTokens.get(absoluteIndex)
        const currentIdentity = currentToken === undefined
            ? undefined
            : this.identities.get(currentToken)
        if (
            currentToken !== undefined &&
            currentIdentity?.type === 'message' &&
            currentIdentity.absoluteIndex === absoluteIndex &&
            currentIdentity.messages === messages &&
            currentIdentity.message === message
        ) return currentToken

        if (currentToken !== undefined) this.identities.delete(currentToken)
        const token = createMessageLocatorToken()
        this.messageTokens.set(absoluteIndex, token)
        this.identities.set(token, {
            type: 'message',
            absoluteIndex,
            messages,
            message,
        })
        return token
    }

    registerPosition(
        messages: readonly Message[],
        absoluteIndex: number,
    ): ConversationPositionToken {
        const currentToken = this.positionTokens.get(absoluteIndex)
        const currentIdentity = currentToken === undefined
            ? undefined
            : this.identities.get(currentToken)
        if (
            currentToken !== undefined &&
            currentIdentity?.type === 'position' &&
            currentIdentity.absoluteIndex === absoluteIndex &&
            currentIdentity.messages === messages &&
            currentIdentity.before === messages[absoluteIndex - 1] &&
            currentIdentity.after === messages[absoluteIndex]
        ) return currentToken

        if (currentToken !== undefined) this.identities.delete(currentToken)
        const token = createConversationPositionToken()
        this.positionTokens.set(absoluteIndex, token)
        this.identities.set(token, {
            type: 'position',
            absoluteIndex,
            messages,
            before: messages[absoluteIndex - 1],
            after: messages[absoluteIndex],
        })
        return token
    }

    matchesMessage(locator: MessageLocator, messages: readonly Message[]): boolean {
        if (locator.sessionToken !== this.sessionToken) return false
        const identity = this.identities.get(locator.locatorToken)
        return identity?.type === 'message' &&
            identity.absoluteIndex === locator.absoluteIndex &&
            identity.messages === messages &&
            identity.message === messages[locator.absoluteIndex]
    }

    matchesPosition(
        position: ConversationPosition,
        messages: readonly Message[],
    ): boolean {
        if (position.sessionToken !== this.sessionToken) return false
        const identity = this.identities.get(position.positionToken)
        return identity?.type === 'position' &&
            identity.absoluteIndex === position.absoluteIndex &&
            identity.messages === messages &&
            identity.before === messages[position.absoluteIndex - 1] &&
            identity.after === messages[position.absoluteIndex]
    }

    clear(): void {
        this.identities.clear()
        this.messageTokens.clear()
        this.positionTokens.clear()
    }
}

export function createConversationSessionToken(): ConversationSessionToken {
    nextConversationSessionToken += 1
    return `conversation-session-${nextConversationSessionToken}` as ConversationSessionToken
}

function createMessageLocatorToken(): MessageLocatorToken {
    nextConversationLocatorToken += 1
    return `message-locator-${nextConversationLocatorToken}` as MessageLocatorToken
}

function createConversationPositionToken(): ConversationPositionToken {
    nextConversationLocatorToken += 1
    return `conversation-position-${nextConversationLocatorToken}` as ConversationPositionToken
}

const finishConversationTransaction = Symbol('finishConversationTransaction')
const abortConversationTransaction = Symbol('abortConversationTransaction')

interface CompletedConversationTransaction {
    messages: Message[]
    bookmarks?: string[]
    bookmarkNames?: Record<string, string>
    bookmarkMetadataChanged: boolean
    version: number
    commands: readonly ActiveConversationCommandName[]
    mutations: readonly ActiveConversationMutationRange[]
    locatorRegistry: ConversationLocatorRegistry
}

export class ConversationNotFoundError extends Error {
    constructor(characterId: string, conversationId: string) {
        super(`Conversation ${conversationId} was not found for ${characterId}`)
        this.name = 'ConversationNotFoundError'
    }
}

export class ConversationSessionStaleError extends Error {
    constructor(expectedVersion: number, actualVersion: number) {
        super(`Expected conversation session version ${expectedVersion}, but current version is ${actualVersion}`)
        this.name = 'ConversationSessionStaleError'
    }
}

export class ConversationSessionInactiveError extends Error {
    constructor() {
        super('Conversation session is inactive')
        this.name = 'ConversationSessionInactiveError'
    }
}

export class MessageLocatorNotFoundError extends Error {
    constructor(absoluteIndex: number) {
        super(`Message locator index ${absoluteIndex} does not exist`)
        this.name = 'MessageLocatorNotFoundError'
    }
}

export class MessageLocatorMismatchError extends Error {
    constructor(message: string) {
        super(message)
        this.name = 'MessageLocatorMismatchError'
    }
}

function validateIndex(value: number, name: string): void {
    if (!Number.isSafeInteger(value) || value < 0) {
        throw new RangeError(`${name} must be a nonnegative safe integer`)
    }
}

function validateCount(value: number): void {
    if (!Number.isSafeInteger(value) || value <= 0) {
        throw new RangeError('Conversation range limit must be a positive safe integer')
    }
    if (value > CONVERSATION_RANGE_MAX_LIMIT) {
        throw new RangeError(
            `Conversation range limit cannot exceed ${CONVERSATION_RANGE_MAX_LIMIT}`,
        )
    }
}

function estimateMessageBytes(message: Message): number {
    return JSON.stringify(message).length * 2
}

function isThenable(value: unknown): value is PromiseLike<unknown> {
    return (
        (typeof value === 'object' && value !== null) ||
        typeof value === 'function'
    ) && typeof (value as PromiseLike<unknown>).then === 'function'
}

function restoreMessage(target: Message, snapshot: Message): void {
    for (const key of Object.keys(target) as Array<keyof Message>) {
        if (!(key in snapshot)) delete target[key]
    }
    Object.assign(target, safeStructuredClone(snapshot))
}

type MessageRollbackEntry = {
    message: Message
    snapshot: Message
}

function captureMessageRollback(messages: readonly Message[]): MessageRollbackEntry[] {
    return messages.map((message) => ({
        message,
        snapshot: safeStructuredClone(message),
    }))
}

function restoreMessageRollback(
    messages: Message[],
    rollback: readonly MessageRollbackEntry[],
): void {
    messages.length = rollback.length
    for (let index = 0; index < rollback.length; index++) {
        const entry = rollback[index]
        restoreMessage(entry.message, entry.snapshot)
        messages[index] = entry.message
    }
}

function valuesEqual(left: unknown, right: unknown): boolean {
    if (Object.is(left, right)) return true
    if (typeof left !== typeof right || left === null || right === null) return false
    if (Array.isArray(left) || Array.isArray(right)) {
        if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
            return false
        }
        return left.every((value, index) => valuesEqual(value, right[index]))
    }
    if (typeof left !== 'object') return false
    const leftRecord = left as Record<string, unknown>
    const rightRecord = right as Record<string, unknown>
    const leftKeys = Object.keys(leftRecord)
    const rightKeys = Object.keys(rightRecord)
    if (leftKeys.length !== rightKeys.length) return false
    return leftKeys.every((key) =>
        Object.prototype.hasOwnProperty.call(rightRecord, key) &&
        valuesEqual(leftRecord[key], rightRecord[key]),
    )
}

export function cloneConversationMetadata(chat: Chat): ConversationMetadata {
    const metadata: ConversationMetadata = {}
    for (const [key, value] of Object.entries(chat)) {
        if (key !== 'message') metadata[key] = value
    }
    return safeStructuredClone(metadata)
}

export function conversationMetadataEqual(
    left: ConversationMetadata,
    right: ConversationMetadata,
): boolean {
    return valuesEqual(left, right)
}

/** Compares metadata without the chat and global variable maps, which may change underneath a holder. */
export function conversationContentMetadataEqual(
    left: ConversationMetadata,
    right: ConversationMetadata,
): boolean {
    return valuesEqual(withoutVariableMaps(left), withoutVariableMaps(right))
}

export function replaceConversationMetadata(
    chat: Chat,
    metadata: ConversationMetadata,
): void {
    const chatRecord = chat as unknown as Record<string, unknown>
    for (const key of Object.keys(chatRecord)) {
        if (key !== 'message' && !Object.prototype.hasOwnProperty.call(metadata, key)) {
            delete chatRecord[key]
        }
    }
    Object.assign(chatRecord, safeStructuredClone(metadata))
}

const VARIABLE_METADATA_KEYS = ['scriptstate', 'GLGlobalVariables'] as const

interface VariableOnlyCommit {
    display: boolean
    update?: ActiveConversationVariableUpdate
}

function withoutVariableMaps(metadata: ConversationMetadata): ConversationMetadata {
    return { ...metadata, scriptstate: undefined, GLGlobalVariables: undefined }
}

function asVariableRecord(value: unknown): Record<string, unknown> {
    return value !== null && typeof value === 'object' && !Array.isArray(value)
        ? value as Record<string, unknown>
        : {}
}

/** Applies the keys a caller changed from `expected` to `next` onto the live map. */
function mergeVariableMap(live: unknown, expected: unknown, next: unknown): unknown {
    if (valuesEqual(expected, next)) return live
    if (next !== undefined && asVariableRecord(next) !== next) return next
    const expectedRecord = asVariableRecord(expected)
    const nextRecord = asVariableRecord(next)
    const merged: Record<string, unknown> = { ...asVariableRecord(live) }
    for (const [key, value] of Object.entries(nextRecord)) {
        if (
            !Object.prototype.hasOwnProperty.call(expectedRecord, key) ||
            !valuesEqual(expectedRecord[key], value)
        ) merged[key] = value
    }
    for (const key of Object.keys(expectedRecord)) {
        if (!Object.prototype.hasOwnProperty.call(nextRecord, key)) delete merged[key]
    }
    if (next === undefined && Object.keys(merged).length === 0) return undefined
    return merged
}

function mergeOperationVariables(
    live: ConversationMetadata,
    expected: ConversationMetadata,
    next: ConversationMetadata,
): ConversationMetadata {
    const merged: ConversationMetadata = { ...next }
    for (const key of VARIABLE_METADATA_KEYS) {
        const value = mergeVariableMap(live[key], expected[key], next[key])
        if (value === undefined) delete merged[key]
        else merged[key] = value
    }
    return merged
}

function createLocator(
    conversationId: string,
    messages: readonly Message[],
    absoluteIndex: number,
    sessionVersion: number,
    locatorRegistry: ConversationLocatorRegistry,
): MessageLocator {
    validateIndex(absoluteIndex, 'Message locator index')
    const message = messages[absoluteIndex]
    if (!message) throw new MessageLocatorNotFoundError(absoluteIndex)
    const locator: MessageLocator = {
        conversationId,
        absoluteIndex,
        ...(message.chatId === undefined ? {} : { expectedMessageId: message.chatId }),
        sessionVersion,
        sessionToken: locatorRegistry.sessionToken,
        locatorToken: locatorRegistry.registerMessage(
            messages,
            absoluteIndex,
            message,
        ),
    }
    return locator
}

function createPosition(
    conversationId: string,
    messages: readonly Message[],
    absoluteIndex: number,
    sessionVersion: number,
    locatorRegistry: ConversationLocatorRegistry,
): ConversationPosition {
    validateIndex(absoluteIndex, 'Conversation position index')
    if (absoluteIndex > messages.length) {
        throw new MessageLocatorNotFoundError(absoluteIndex)
    }
    const position: ConversationPosition = {
        conversationId,
        absoluteIndex,
        sessionVersion,
        sessionToken: locatorRegistry.sessionToken,
        positionToken: locatorRegistry.registerPosition(
            messages,
            absoluteIndex,
        ),
    }
    return position
}

/** Locators from any version since the last content change still address the same messages. */
interface AcceptedLocatorVersions {
    oldest: number
    current: number
}

function validateLocator(
    conversationId: string,
    messages: readonly Message[],
    versions: AcceptedLocatorVersions,
    locator: MessageLocator,
    locatorRegistry: ConversationLocatorRegistry,
    sourceMessages?: readonly Message[],
    sourceLocatorRegistry?: ConversationLocatorRegistry,
): Message {
    if (locator.conversationId !== conversationId) {
        throw new MessageLocatorMismatchError(
            `Message locator belongs to ${locator.conversationId}, not ${conversationId}`,
        )
    }
    if (
        locator.sessionVersion < versions.oldest ||
        locator.sessionVersion > versions.current
    ) {
        throw new ConversationSessionStaleError(locator.sessionVersion, versions.current)
    }
    validateIndex(locator.absoluteIndex, 'Message locator index')
    const message = messages[locator.absoluteIndex]
    if (!message) throw new MessageLocatorNotFoundError(locator.absoluteIndex)
    const matchesCurrent = locatorRegistry.matchesMessage(locator, messages)
    const matchesSource =
        sourceMessages !== undefined &&
        sourceLocatorRegistry?.matchesMessage(locator, sourceMessages) === true
    if (!matchesCurrent && !matchesSource) {
        throw new MessageLocatorMismatchError(
            `Message locator identity changed at index ${locator.absoluteIndex}`,
        )
    }
    if (
        locator.expectedMessageId !== undefined &&
        message.chatId !== locator.expectedMessageId
    ) {
        throw new MessageLocatorMismatchError(
            `Message locator expected ${locator.expectedMessageId} at index ${locator.absoluteIndex}`,
        )
    }
    return message
}

function validatePosition(
    conversationId: string,
    messages: readonly Message[],
    versions: AcceptedLocatorVersions,
    position: ConversationPosition,
    locatorRegistry: ConversationLocatorRegistry,
    sourceMessages?: readonly Message[],
    sourceLocatorRegistry?: ConversationLocatorRegistry,
): void {
    if (position.conversationId !== conversationId) {
        throw new MessageLocatorMismatchError(
            `Conversation position belongs to ${position.conversationId}, not ${conversationId}`,
        )
    }
    if (
        position.sessionVersion < versions.oldest ||
        position.sessionVersion > versions.current
    ) {
        throw new ConversationSessionStaleError(position.sessionVersion, versions.current)
    }
    validateIndex(position.absoluteIndex, 'Conversation position index')
    if (position.absoluteIndex > messages.length) {
        throw new MessageLocatorNotFoundError(position.absoluteIndex)
    }
    const matchesCurrent = locatorRegistry.matchesPosition(position, messages)
    const matchesSource =
        sourceMessages !== undefined &&
        sourceLocatorRegistry?.matchesPosition(position, sourceMessages) === true
    if (!matchesCurrent && !matchesSource) {
        throw new MessageLocatorMismatchError(
            `Conversation position identity changed at index ${position.absoluteIndex}`,
        )
    }
}

function readRange(
    characterId: string,
    conversationId: string,
    messages: readonly Message[],
    storeRevision: DataRevision,
    sessionVersion: number,
    locatorRegistry: ConversationLocatorRegistry,
    requestedStart: number,
    limit: number,
): ActiveConversationWindow {
    validateIndex(requestedStart, 'Conversation range startIndex')
    validateCount(limit)
    const startIndex = Math.min(messages.length, requestedStart)
    const endIndex = Math.min(messages.length, startIndex + limit)
    const selected = messages.slice(startIndex, endIndex)
    return {
        characterId,
        conversationId,
        messages: safeStructuredClone(selected),
        locators: selected.map((_message, offset) =>
            createLocator(
                conversationId,
                messages,
                startIndex + offset,
                sessionVersion,
                locatorRegistry,
            ),
        ),
        startIndex,
        endIndex,
        totalMessages: messages.length,
        storeRevision,
        sessionVersion,
    }
}

export class ActiveConversationTransaction {
    private currentMessages: Message[]
    private currentBookmarks?: string[]
    private currentBookmarkNames?: Record<string, string>
    private bookmarkMetadataChanged = false
    private currentVersion: number
    private oldestAcceptedVersion: number
    private readonly locatorRegistry: ConversationLocatorRegistry
    private readonly commandNames: ActiveConversationCommandName[] = []
    private readonly mutationRanges: ActiveConversationMutationRange[] = []
    private closed = false

    constructor(
        private readonly characterId: string,
        private readonly conversationId: string,
        sourceConversation: Chat,
        private readonly sourceMessages: readonly Message[],
        private readonly storeRevision: DataRevision,
        private readonly sourceLocatorRegistry: ConversationLocatorRegistry,
        sessionVersion: number,
        contentVersion: number,
    ) {
        this.currentMessages = [...sourceMessages]
        this.currentBookmarks = sourceConversation.bookmarks === undefined
            ? undefined
            : [...sourceConversation.bookmarks]
        this.currentBookmarkNames = sourceConversation.bookmarkNames === undefined
            ? undefined
            : { ...sourceConversation.bookmarkNames }
        this.currentVersion = sessionVersion
        this.oldestAcceptedVersion = contentVersion
        this.locatorRegistry = sourceLocatorRegistry.fork()
    }

    private get acceptedVersions(): AcceptedLocatorVersions {
        return { oldest: this.oldestAcceptedVersion, current: this.currentVersion }
    }

    get version(): number {
        this.assertOpen()
        return this.currentVersion
    }

    get totalMessages(): number {
        this.assertOpen()
        return this.currentMessages.length
    }

    locate(absoluteIndex: number): MessageLocator {
        this.assertOpen()
        return createLocator(
            this.conversationId,
            this.currentMessages,
            absoluteIndex,
            this.currentVersion,
            this.locatorRegistry,
        )
    }

    positionAt(absoluteIndex: number): ConversationPosition {
        this.assertOpen()
        return createPosition(
            this.conversationId,
            this.currentMessages,
            absoluteIndex,
            this.currentVersion,
            this.locatorRegistry,
        )
    }

    readRange(startIndex: number, limit: number): ActiveConversationWindow {
        this.assertOpen()
        return readRange(
            this.characterId,
            this.conversationId,
            this.currentMessages,
            this.storeRevision,
            this.currentVersion,
            this.locatorRegistry,
            startIndex,
            limit,
        )
    }

    ensureNullishMessageIds(createId: () => string): number {
        this.assertOpen()
        let assigned = 0
        let rangeStart = -1
        let replacements: Message[] = []
        const flushRange = () => {
            if (rangeStart === -1) return
            this.record('edit', rangeStart, replacements.length, replacements)
            rangeStart = -1
            replacements = []
        }

        for (let index = 0; index < this.currentMessages.length; index++) {
            const message = this.currentMessages[index]
            if (message.chatId !== undefined && message.chatId !== null) {
                flushRange()
                continue
            }
            const id = createId()
            if (!id) throw new Error('Message ID generator returned an empty ID')
            const replacement = {
                ...safeStructuredClone(message),
                chatId: id,
            }
            this.currentMessages[index] = replacement
            if (rangeStart === -1) rangeStart = index
            replacements.push(replacement)
            assigned += 1
        }
        flushRange()
        return assigned
    }

    append(message: Message): MessageLocator {
        this.assertOpen()
        const absoluteIndex = this.currentMessages.length
        const replacement = safeStructuredClone(message)
        this.currentMessages = [...this.currentMessages, replacement]
        this.record('append', absoluteIndex, 0, [replacement])
        return this.locate(absoluteIndex)
    }

    edit(locator: MessageLocator, message: Message): MessageLocator {
        this.assertOpen()
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        const nextMessages = this.currentMessages.slice()
        const replacement = safeStructuredClone(message)
        nextMessages[locator.absoluteIndex] = replacement
        this.currentMessages = nextMessages
        this.record('edit', locator.absoluteIndex, 1, [replacement])
        return this.locate(locator.absoluteIndex)
    }

    setBookmark(
        locator: MessageLocator,
        options: SetConversationBookmarkOptions,
    ): MessageLocator {
        this.assertOpen()
        const message = validateLocator(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        const messageId = options.bookmarked
            ? message.chatId || options.messageId
            : message.chatId
        if (!messageId) {
            if (!options.bookmarked) return this.locate(locator.absoluteIndex)
            throw new Error('A bookmark requires a message ID')
        }

        let replacement: Message | undefined
        if (options.bookmarked) {
            if (!message.chatId) {
                const nextMessages = this.currentMessages.slice()
                replacement = {
                    ...safeStructuredClone(message),
                    chatId: messageId,
                }
                nextMessages[locator.absoluteIndex] = replacement
                this.currentMessages = nextMessages
            }
            this.currentBookmarks ??= []
            this.currentBookmarkNames ??= {}
            if (!this.currentBookmarks.includes(messageId)) {
                this.currentBookmarks.push(messageId)
            }
            if (options.name !== undefined) {
                this.currentBookmarkNames[messageId] = options.name
            }
        } else {
            const bookmarkIndex = this.currentBookmarks?.indexOf(messageId) ?? -1
            if (bookmarkIndex >= 0) this.currentBookmarks!.splice(bookmarkIndex, 1)
            if (this.currentBookmarkNames) delete this.currentBookmarkNames[messageId]
        }

        this.bookmarkMetadataChanged = true
        this.record(
            'bookmark',
            replacement === undefined ? undefined : locator.absoluteIndex,
            replacement === undefined ? undefined : 1,
            replacement === undefined ? undefined : [replacement],
        )
        return this.locate(locator.absoluteIndex)
    }

    renameBookmark(locator: MessageLocator, name: string): MessageLocator {
        this.assertOpen()
        const message = validateLocator(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        if (!message.chatId || !this.currentBookmarks?.includes(message.chatId)) {
            throw new Error('The message is not bookmarked')
        }
        this.currentBookmarkNames ??= {}
        this.currentBookmarkNames[message.chatId] = name
        this.bookmarkMetadataChanged = true
        this.record('bookmark')
        return this.locate(locator.absoluteIndex)
    }

    delete(locator: MessageLocator): void {
        this.assertOpen()
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        this.currentMessages = [
            ...this.currentMessages.slice(0, locator.absoluteIndex),
            ...this.currentMessages.slice(locator.absoluteIndex + 1),
        ]
        this.record('delete', locator.absoluteIndex, 1, [])
    }

    truncate(locator: MessageLocator): void {
        this.assertOpen()
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        const deleteCount = this.currentMessages.length - locator.absoluteIndex
        this.currentMessages = this.currentMessages.slice(0, locator.absoluteIndex)
        this.record('truncate', locator.absoluteIndex, deleteCount, [])
    }

    replaceRange(
        position: ConversationPosition,
        deleteCount: number,
        messages: readonly Message[],
    ): void {
        this.assertOpen()
        validatePosition(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            position,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        validateIndex(deleteCount, 'Conversation replace-range deleteCount')
        if (deleteCount > this.currentMessages.length - position.absoluteIndex) {
            throw new RangeError('Conversation replace-range exceeds the current message count')
        }
        const replacement = safeStructuredClone(messages)
        this.currentMessages = [
            ...this.currentMessages.slice(0, position.absoluteIndex),
            ...replacement,
            ...this.currentMessages.slice(position.absoluteIndex + deleteCount),
        ]
        this.record('replace-range', position.absoluteIndex, deleteCount, replacement)
    }

    replaceTail(position: ConversationPosition, messages: readonly Message[]): void {
        this.assertOpen()
        this.replaceTailAs('replace-tail', position, messages)
    }

    reroll(position: ConversationPosition, messages: readonly Message[]): void {
        this.assertOpen()
        this.replaceTailAs('reroll', position, messages)
    }

    readBranchSource(locator: MessageLocator): ActiveConversationBranchSource {
        this.assertOpen()
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        const endIndex = locator.absoluteIndex + 1
        return {
            characterId: this.characterId,
            conversationId: this.conversationId,
            messages: safeStructuredClone(this.currentMessages.slice(0, endIndex)),
            startIndex: 0,
            endIndex,
            totalMessages: this.currentMessages.length,
            storeRevision: this.storeRevision,
            sessionVersion: this.currentVersion,
        }
    }

    get changed(): boolean {
        this.assertOpen()
        return this.commandNames.length > 0
    }

    get messages(): Message[] {
        this.assertOpen()
        return safeStructuredClone(this.currentMessages)
    }

    get commands(): readonly ActiveConversationCommandName[] {
        this.assertOpen()
        return this.commandNames.slice()
    }

    [finishConversationTransaction](): CompletedConversationTransaction {
        this.assertOpen()
        this.closed = true
        return {
            messages: this.currentMessages,
            bookmarks: this.currentBookmarks,
            bookmarkNames: this.currentBookmarkNames,
            bookmarkMetadataChanged: this.bookmarkMetadataChanged,
            version: this.currentVersion,
            commands: this.commandNames.slice(),
            mutations: safeStructuredClone(this.mutationRanges),
            locatorRegistry: this.locatorRegistry,
        }
    }

    [abortConversationTransaction](): void {
        this.closed = true
    }

    private replaceTailAs(
        command: 'replace-tail' | 'reroll',
        position: ConversationPosition,
        messages: readonly Message[],
    ): void {
        validatePosition(
            this.conversationId,
            this.currentMessages,
            this.acceptedVersions,
            position,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        const deleteCount = this.currentMessages.length - position.absoluteIndex
        const replacement = safeStructuredClone(messages)
        this.currentMessages = [
            ...this.currentMessages.slice(0, position.absoluteIndex),
            ...replacement,
        ]
        this.record(command, position.absoluteIndex, deleteCount, replacement)
    }

    private record(
        command: ActiveConversationCommandName,
        start?: number,
        deleteCount?: number,
        messages?: readonly Message[],
    ): void {
        this.currentVersion += 1
        this.oldestAcceptedVersion = this.currentVersion
        this.locatorRegistry.clear()
        this.commandNames.push(command)
        this.mutationRanges.push({
            start: start ?? this.currentMessages.length,
            deleteCount: deleteCount ?? 0,
            messages: safeStructuredClone([...(messages ?? [])]),
            sessionVersion: this.currentVersion,
        })
    }

    private assertOpen(): void {
        if (this.closed) throw new Error('Conversation session transaction is closed')
    }
}

export class ActiveConversationSession {
    readonly characterId: string
    readonly conversationId: string

    private conversation: Chat
    private readonly onMutation?: (event: ActiveConversationMutationEvent) => void
    private readonly onPinReleased?: () => void
    private readonly residency: SegmentedConversationResidency
    private readonly pins = new Map<ActiveConversationPinReason, number>()
    private readonly residencyRangePins = new Map<ActiveConversationPinReason, number>()
    private readonly subscribers = new Set<(
        event: ActiveConversationMutationEvent | null,
    ) => void>()
    private locatorRegistry = new ConversationLocatorRegistry()
    private sessionVersion = 0
    private lastMessageChangeVersion = 0
    private lastContentChangeVersion = 0
    private generationContinuationStart = 0
    private persistedSessionVersion = 0
    private currentStoreRevision: DataRevision
    private transactionActive = false
    private active = true
    private compatibilityFallback = false
    private compatibilityBaselineMessageCount: number | null = null
    private pinReleaseNotificationPending = false

    constructor(options: ActiveConversationSessionOptions) {
        if (!options.conversation) {
            throw new ConversationNotFoundError(options.characterId, options.conversationId)
        }
        this.characterId = options.characterId
        this.conversationId = options.conversationId
        this.conversation = options.conversation
        this.currentStoreRevision = options.storeRevision
        this.onMutation = options.onMutation
        this.onPinReleased = options.onPinReleased
        this.residency = new SegmentedConversationResidency({
            revision: options.storeRevision,
            totalMessages: options.conversation.message.length,
            maxResidentBytes: 0,
            measureMessage: options.measureMessage ?? estimateMessageBytes,
        })
    }

    get storeRevision(): DataRevision {
        return this.currentStoreRevision
    }

    /** Persisted metadata may advance edit locators without changing a request's input. */
    canContinueGenerationFrom(version: number): boolean {
        return (
            this.active &&
            version >= this.generationContinuationStart &&
            version <= this.sessionVersion
        )
    }

    get generationInvalidationVersion(): number {
        return this.generationContinuationStart
    }

    /** Advances with every commit except one that changes only chat or global variables. */
    get contentVersion(): number {
        return this.lastContentChangeVersion
    }

    /** Whether messages and non-variable metadata are as they were at `version`. */
    isContentUnchangedSince(version: number): boolean {
        return (
            this.active &&
            version >= this.lastContentChangeVersion &&
            version <= this.sessionVersion
        )
    }

    private get acceptedLocatorVersions(): AcceptedLocatorVersions {
        return { oldest: this.lastContentChangeVersion, current: this.sessionVersion }
    }

    /** Whether a commit of metadata and unknown message fields would be adopted now; it changes nothing. */
    get canAdoptPersistedMetadata(): boolean {
        return (
            this.active &&
            !this.transactionActive &&
            this.sessionVersion === this.persistedSessionVersion &&
            (this.compatibilityFallback || this.residency.canAdoptPersistedMetadata)
        )
    }

    adoptPersistedMetadata(replacement: Chat, revision: DataRevision): boolean {
        this.assertActive()
        if (
            this.transactionActive ||
            this.sessionVersion !== this.persistedSessionVersion ||
            revision < this.currentStoreRevision ||
            replacement.id !== this.conversationId ||
            replacement.message.length !== this.conversation.message.length
        )
            return false
        // These fields affect the prompt or identify its messages. Unknown plugin fields,
        // timing and generation diagnostics are metadata, and must survive later writes.
        const contentKeys = [
            'role',
            'data',
            'saying',
            'chatId',
            'name',
            'disabled',
            'isComment',
        ] as const
        if (
            !replacement.message.every((message, index) =>
                contentKeys.every((key) =>
                    Object.is(message[key], this.conversation.message[index][key]),
                ),
            )
        )
            return false
        const metadata = cloneConversationMetadata(replacement)
        const messages = replacement.message
        const version = this.sessionVersion + 1
        if (
            !this.compatibilityFallback &&
            !this.residency.adoptPersistedMetadata(revision, version, messages)
        )
            return false
        replaceConversationMetadata(this.conversation, metadata)
        this.conversation.message = messages
        this.locatorRegistry.clear()
        this.sessionVersion = version
        this.lastContentChangeVersion = version
        // Message metadata was replaced too; pending edits must not restore older fields.
        this.lastMessageChangeVersion = version
        this.persistedSessionVersion = version
        this.currentStoreRevision = revision
        this.notifySubscribers({
            characterId: this.characterId,
            conversationId: this.conversationId,
            sessionToken: this.locatorRegistry.sessionToken,
            previousVersion: version - 1,
            sessionVersion: version,
            commands: ['update-metadata'],
            mutations: [],
            conversation: metadata,
        })
        return true
    }

    /** Install a changed durable conversation underneath pending writes without replacing the live owner. */
    adoptGenerationHead(base: Chat, stored: Chat, revision: number, outputIndex: number, publishedPatch: boolean): number | null {
        this.assertActive()
        const live = this.conversation
        const storedMetadata = cloneConversationMetadata(stored)
        const baseMetadata = cloneConversationMetadata(base)
        const localMetadata = cloneConversationMetadata(live)
        const metadata = publishedPatch ? localMetadata : mergeOperationVariables(storedMetadata, baseMetadata, localMetadata)
        if (!publishedPatch) {
            for (const key of new Set([...Object.keys(baseMetadata), ...Object.keys(storedMetadata), ...Object.keys(localMetadata)])) {
                if (VARIABLE_METADATA_KEYS.some(field => field === key) || !valuesEqual(baseMetadata[key], localMetadata[key])) continue
                if (Object.hasOwn(storedMetadata, key)) metadata[key] = storedMetadata[key]
                else delete metadata[key]
            }
        }

        let mappedIndex: number | null = outputIndex >= 0 ? outputIndex : null
        let ranges: { start: number; deleteCount: number; messages: Message[] }[]
        if (publishedPatch || valuesEqual(base.message, stored.message)) {
            // Membership is known. Keep positional writes, including rows with old or duplicate IDs.
            ranges = []
            if (live.message.length === stored.message.length) {
                live.message.forEach((message, index) => {
                    if (!valuesEqual(message, stored.message[index])) ranges.push({ start: index, deleteCount: 1, messages: [message] })
                })
            } else ranges.push(messageReplaceRange(stored.message, live.message))
        } else {
            const overlay = new WindowedConversationOverlay(this.characterId, this.conversationId, this.storeRevision, base.message.length)
            const previous = base.message.slice()
            const replay = (range: { start: number; deleteCount: number; messages: Message[] }) => {
                const before = previous.slice(range.start, range.start + range.deleteCount)
                overlay.applyRange(range.start, before, range.messages, 0, { anchor: previous[range.start] })
                replaceArrayRange(previous, range.start, range.deleteCount, range.messages)
            }
            for (const range of this.pendingMutations) replay(range)
            // Direct compatibility edits need not have a session event.
            const remaining = messageReplaceRange(previous, live.message)
            if (remaining.messages.length || remaining.deleteCount) replay(remaining)
            const output = mappedIndex === null ? null : overlay.bind(mappedIndex, live.message[mappedIndex])
            const projected = overlay.projectCompleteRows(base.message, stored.message)
            mappedIndex = output && projected.locate(output)
            ranges = projected.mutations
        }

        const previousVersion = this.sessionVersion++
        const version = this.sessionVersion
        this.residency.adoptGenerationHead(revision, version, stored.message.length)
        this.compatibilityBaselineMessageCount = stored.message.length
        this.locatorRegistry.clear()
        this.currentStoreRevision = revision
        this.persistedSessionVersion = this.lastContentChangeVersion = this.lastMessageChangeVersion = this.generationContinuationStart = version
        const previousCount = live.message.length
        live.message = safeStructuredClone(stored.message)
        replaceConversationMetadata(live, storedMetadata)
        this.notifySubscribers({ characterId: this.characterId, conversationId: this.conversationId, sessionToken: this.sessionToken,
            previousVersion, sessionVersion: version, commands: ['replace-conversation'], conversation: storedMetadata,
            mutations: [{ start: 0, deleteCount: previousCount, messages: live.message, sessionVersion: version }] })

        // Only the residual local ranges and metadata are dirty on the new base.
        if (ranges.length === 0 && !valuesEqual(metadata, storedMetadata))
            this.applyOperation({ expectedVersion: version, expectedMetadata: storedMetadata, metadata })
        for (const range of ranges) {
            const expectedMetadata = cloneConversationMetadata(live)
            this.applyOperation({ expectedVersion: this.version, expectedMetadata, metadata,
                range: { position: this.positionAt(range.start), deleteCount: range.deleteCount, messages: range.messages } })
        }
        return mappedIndex
    }

    advanceStoreRevision(revision: DataRevision): boolean {
        this.assertActive()
        validateIndex(revision, 'Conversation storage-only data revision')
        if (revision < this.currentStoreRevision) {
            throw new RangeError('Conversation storage-only data revision moved backwards')
        }
        if (revision === this.currentStoreRevision) return false
        if (!this.compatibilityFallback) this.residency.advanceStoreRevision(revision)
        this.currentStoreRevision = revision
        return true
    }

    get version(): number {
        return this.sessionVersion
    }

    /** Advances with message replacements, including metadata; chat-only metadata commits keep it. */
    get messageVersion(): number {
        return this.lastMessageChangeVersion
    }

    /** Reads one chat variable of the live conversation without copying it. */
    readChatVariable(key: string): unknown {
        this.assertActive()
        return this.conversation.scriptstate?.['$' + key]
    }

    get persistedVersion(): number {
        return this.persistedSessionVersion
    }

    get sessionToken(): ConversationSessionToken {
        this.assertActive()
        return this.locatorRegistry.sessionToken
    }

    get isTransactionActive(): boolean {
        return this.transactionActive
    }

    get isActive(): boolean {
        return this.active
    }

    get totalMessages(): number {
        this.assertActive()
        return this.conversation.message.length
    }

    get residentBytes(): number {
        this.assertActive()
        return this.compatibilityFallback ? 0 : this.residency.residentBytes
    }

    get residentIntervals(): ConversationResidentInterval[] {
        this.assertActive()
        return this.compatibilityFallback ? [] : this.residency.residentIntervals
    }

    get pendingMutations(): ConversationDirtyMutation[] {
        this.assertActive()
        return this.compatibilityFallback ? [] : this.residency.pendingMutations
    }

    get residencyFallbackActive(): boolean {
        return this.compatibilityFallback
    }

    get activePinReasons(): ActiveConversationPinReason[] {
        return [...this.pins.keys()]
    }

    subscribe(
        listener: (event: ActiveConversationMutationEvent | null) => void,
    ): () => void {
        this.assertActive()
        this.subscribers.add(listener)
        let subscribed = true
        return () => {
            if (!subscribed) return
            subscribed = false
            this.subscribers.delete(listener)
        }
    }

    matchesConversation(characterId: string, conversation: Chat): boolean {
        return (
            this.active &&
            this.characterId === characterId &&
            this.conversationId === conversation.id &&
            this.conversation === conversation
        )
    }

    resolveMessage(locator: MessageLocator): Message {
        this.assertActive()
        return validateLocator(
            this.conversationId,
            this.conversation.message,
            this.acceptedLocatorVersions,
            locator,
            this.locatorRegistry,
        )
    }

    ensureMessageId(locator: MessageLocator, createId: () => string): Message {
        const message = this.resolveMessage(locator)
        if (message.chatId) return message
        const id = createId()
        if (!id) throw new Error('Message ID generator returned an empty ID')
        message.chatId = id
        locator.expectedMessageId = id
        return message
    }

    ensureNullishMessageIds(createId: () => string): number {
        this.assertActive()
        return this.transaction((transaction) =>
            transaction.ensureNullishMessageIds(createId),
        )
    }

    locate(absoluteIndex: number): MessageLocator {
        this.assertActive()
        return createLocator(
            this.conversationId,
            this.conversation.message,
            absoluteIndex,
            this.sessionVersion,
            this.locatorRegistry,
        )
    }

    resolveLocator(locator: MessageLocator): number {
        this.assertActive()
        validateLocator(
            this.conversationId,
            this.conversation.message,
            this.acceptedLocatorVersions,
            locator,
            this.locatorRegistry,
        )
        return locator.absoluteIndex
    }

    findMessageLocatorById(messageId: string): MessageLocator | null {
        this.assertActive()
        return this.findMessageTargetsByIds([messageId], 'first')[0]?.locator ?? null
    }

    findMessageTargetsByIds(
        messageIds: readonly string[],
        occurrence: 'first' | 'last' = 'first',
    ): ActiveConversationMessageTarget[] {
        this.assertActive()
        if (messageIds.length === 0) return []
        const requested = new Set(messageIds)
        const matches = new Map<string, number>()
        const messages = this.conversation.message
        const pageSize = 128
        for (let startIndex = 0; startIndex < messages.length; startIndex += pageSize) {
            const page = messages.slice(
                startIndex,
                Math.min(messages.length, startIndex + pageSize),
            )
            for (let offset = 0; offset < page.length; offset++) {
                const messageId = page[offset].chatId
                if (
                    messageId === undefined ||
                    !requested.has(messageId) ||
                    (occurrence === 'first' && matches.has(messageId))
                ) continue
                matches.set(messageId, startIndex + offset)
            }
            if (occurrence === 'first' && matches.size === requested.size) break
        }
        return messageIds.flatMap((messageId) => {
            const absoluteIndex = matches.get(messageId)
            if (absoluteIndex === undefined) return []
            return [{
                absoluteIndex,
                message: safeStructuredClone(messages[absoluteIndex]),
                locator: this.locate(absoluteIndex),
            }]
        })
    }

    readMessage(locator: MessageLocator): Message {
        this.assertActive()
        const message = validateLocator(
            this.conversationId,
            this.conversation.message,
            this.acceptedLocatorVersions,
            locator,
            this.locatorRegistry,
        )
        return safeStructuredClone(message)
    }

    ownsMessageLocator(locator: MessageLocator): boolean {
        if (!this.active) return false
        try {
            validateLocator(
                this.conversationId,
                this.conversation.message,
                this.acceptedLocatorVersions,
                locator,
                this.locatorRegistry,
            )
            return true
        } catch {
            return false
        }
    }

    positionAt(absoluteIndex: number): ConversationPosition {
        this.assertActive()
        return createPosition(
            this.conversationId,
            this.conversation.message,
            absoluteIndex,
            this.sessionVersion,
            this.locatorRegistry,
        )
    }

    readLatest(limit: number): ActiveConversationWindow {
        this.assertActive()
        validateCount(limit)
        return this.readRange(Math.max(0, this.totalMessages - limit), limit)
    }

    readRange(startIndex: number, limit: number): ActiveConversationWindow {
        this.assertActive()
        const window = readRange(
            this.characterId,
            this.conversationId,
            this.conversation.message,
            this.storeRevision,
            this.sessionVersion,
            this.locatorRegistry,
            startIndex,
            limit,
        )
        return window
    }

    readMetadataRange(requestedStart: number, limit: number): ActiveConversationMetadataWindow {
        this.assertActive()
        validateIndex(requestedStart, 'Conversation range startIndex')
        validateCount(limit)
        const startIndex = Math.min(this.totalMessages, requestedStart)
        const endIndex = Math.min(this.totalMessages, startIndex + limit)
        const messages: ActiveConversationMetadataWindow['messages'] = []
        const locators: MessageLocator[] = []
        for (let index = startIndex; index < endIndex; index++) {
            messages.push({ disabled: this.conversation.message[index].disabled })
            locators.push(this.locate(index))
        }
        return {
            characterId: this.characterId,
            conversationId: this.conversationId,
            messages,
            locators,
            startIndex,
            endIndex,
            totalMessages: this.totalMessages,
            storeRevision: this.storeRevision,
            sessionVersion: this.sessionVersion,
        }
    }

    scanMetadataBackward(
        startIndexExclusive = this.totalMessages,
        limit = CONVERSATION_RANGE_MAX_LIMIT,
    ): ActiveConversationMetadataBackwardScan {
        this.assertActive()
        validateIndex(startIndexExclusive, 'Backward scan startIndex')
        validateCount(limit)
        const start = Math.min(this.totalMessages, startIndexExclusive)
        const entries: ActiveConversationMetadataBackwardScan['entries'] = []
        for (let absoluteIndex = start - 1; absoluteIndex >= 0 && entries.length < limit; absoluteIndex--) {
            entries.push({
                absoluteIndex,
                disabled: this.conversation.message[absoluteIndex].disabled,
            })
        }
        return {
            characterId: this.characterId,
            conversationId: this.conversationId,
            entries,
            startIndexExclusive: start,
            totalMessages: this.totalMessages,
            storeRevision: this.storeRevision,
            sessionVersion: this.sessionVersion,
        }
    }

    scanBackward(
        startIndexExclusive = this.totalMessages,
        limit = CONVERSATION_RANGE_MAX_LIMIT,
    ): ActiveConversationBackwardScan {
        this.assertActive()
        validateIndex(startIndexExclusive, 'Backward scan startIndex')
        validateCount(limit)
        const start = Math.min(this.totalMessages, startIndexExclusive)
        const entries: BackwardConversationEntry[] = []
        for (let absoluteIndex = start - 1; absoluteIndex >= 0 && entries.length < limit; absoluteIndex--) {
            entries.push({
                absoluteIndex,
                message: safeStructuredClone(this.conversation.message[absoluteIndex]),
                locator: this.locate(absoluteIndex),
            })
        }
        return {
            characterId: this.characterId,
            conversationId: this.conversationId,
            entries,
            startIndexExclusive: start,
            totalMessages: this.totalMessages,
            storeRevision: this.storeRevision,
            sessionVersion: this.sessionVersion,
        }
    }

    append(message: Message): MessageLocator {
        this.assertActive()
        if (this.transactionActive) throw new Error('Nested conversation session transactions are not supported')
        this.transactionActive = true
        try {
            const previousVersion = this.sessionVersion
            const previousLocatorRegistry = this.locatorRegistry
            const nextLocatorRegistry = previousLocatorRegistry.fork()
            const previousMessages = this.conversation.message
            const absoluteIndex = previousMessages.length
            previousMessages.push(safeStructuredClone(message))
            this.sessionVersion += 1
            this.locatorRegistry = nextLocatorRegistry
            try {
                this.notifyMutation(previousVersion, ['append'], [{
                    start: absoluteIndex,
                    deleteCount: 0,
                    messages: [previousMessages[absoluteIndex]],
                    sessionVersion: this.sessionVersion,
                }])
                previousLocatorRegistry.clear()
            } catch (error) {
                previousMessages.pop()
                this.conversation.message = previousMessages
                this.sessionVersion = previousVersion
                nextLocatorRegistry.clear()
                this.locatorRegistry = previousLocatorRegistry
                throw error
            }
            return this.locate(absoluteIndex)
        } finally {
            this.transactionActive = false
        }
    }

    edit(locator: MessageLocator, message: Message): MessageLocator {
        this.assertActive()
        if (this.transactionActive) throw new Error('Nested conversation session transactions are not supported')
        this.transactionActive = true
        try {
            validateLocator(
                this.conversationId,
                this.conversation.message,
                this.acceptedLocatorVersions,
                locator,
                this.locatorRegistry,
            )
            const previousVersion = this.sessionVersion
            const previousLocatorRegistry = this.locatorRegistry
            const nextLocatorRegistry = previousLocatorRegistry.fork()
            const previousMessages = this.conversation.message
            const previousMessage = previousMessages[locator.absoluteIndex]
            previousMessages[locator.absoluteIndex] = safeStructuredClone(message)
            this.sessionVersion += 1
            this.locatorRegistry = nextLocatorRegistry
            try {
                this.notifyMutation(previousVersion, ['edit'], [{
                    start: locator.absoluteIndex,
                    deleteCount: 1,
                    messages: [previousMessages[locator.absoluteIndex]],
                    sessionVersion: this.sessionVersion,
                }])
                previousLocatorRegistry.clear()
            } catch (error) {
                previousMessages[locator.absoluteIndex] = previousMessage
                this.conversation.message = previousMessages
                this.sessionVersion = previousVersion
                nextLocatorRegistry.clear()
                this.locatorRegistry = previousLocatorRegistry
                throw error
            }
            return this.locate(locator.absoluteIndex)
        } finally {
            this.transactionActive = false
        }
    }

    setBookmark(
        locator: MessageLocator,
        options: SetConversationBookmarkOptions,
    ): MessageLocator {
        this.assertActive()
        return this.transaction((transaction) => transaction.setBookmark(locator, options))
    }

    renameBookmark(locator: MessageLocator, name: string): MessageLocator {
        this.assertActive()
        return this.transaction((transaction) => transaction.renameBookmark(locator, name))
    }

    delete(locator: MessageLocator): void {
        this.assertActive()
        this.transaction((transaction) => transaction.delete(locator))
    }

    truncate(locator: MessageLocator): void {
        this.assertActive()
        this.transaction((transaction) => transaction.truncate(locator))
    }

    replaceRange(
        position: ConversationPosition,
        deleteCount: number,
        messages: readonly Message[],
    ): void {
        this.assertActive()
        this.transaction((transaction) =>
            transaction.replaceRange(position, deleteCount, messages),
        )
    }

    applyOperation(commit: ActiveConversationOperationCommit): void {
        this.assertActive()
        if (this.transactionActive) {
            throw new Error('Nested conversation session transactions are not supported')
        }
        if (!this.isContentUnchangedSince(commit.expectedVersion)) {
            throw new ConversationSessionStaleError(
                commit.expectedVersion,
                this.sessionVersion,
            )
        }

        this.transactionActive = true
        try {
            // By reference: the baseline is only compared, and a rollback copies it back.
            const currentMetadata: ConversationMetadata = {}
            for (const [key, value] of Object.entries(this.conversation)) {
                if (key !== 'message') currentMetadata[key] = value
            }
            // Variables written since the caller read them are kept; the caller's own
            // variable changes are merged per key.
            if (!conversationMetadataEqual(
                withoutVariableMaps(commit.expectedMetadata),
                withoutVariableMaps(currentMetadata),
            )) {
                throw new MessageLocatorMismatchError(
                    'Conversation operation metadata baseline changed',
                )
            }

            if (commit.range !== undefined && commit.ranges !== undefined) {
                throw new TypeError('Conversation operation cannot include both range and ranges')
            }
            const ranges = commit.ranges ?? (commit.range === undefined ? [] : [commit.range])
            const nextMetadata = mergeOperationVariables(
                currentMetadata,
                commit.expectedMetadata,
                commit.metadata,
            )
            const metadataChanged = !conversationMetadataEqual(currentMetadata, nextMetadata)
            if (ranges.length === 0 && !metadataChanged) return
            const variableOnly = ranges.length === 0 && conversationMetadataEqual(
                withoutVariableMaps(currentMetadata),
                withoutVariableMaps(nextMetadata),
            )

            const previousVersion = this.sessionVersion
            const previousMessages = this.conversation.message
            const previousMetadata = currentMetadata
            const previousLocatorRegistry = this.locatorRegistry
            let nextMessages = previousMessages
            const commands: ActiveConversationCommandName[] = []
            const mutationRanges: ActiveConversationMutationRange[] = []

            let previousRangeEnd = 0
            const replacements = ranges.map((range, index) => {
                validatePosition(
                    this.conversationId,
                    previousMessages,
                    this.acceptedLocatorVersions,
                    range.position,
                    previousLocatorRegistry,
                )
                validateIndex(
                    range.deleteCount,
                    'Conversation replace-range deleteCount',
                )
                if (
                    range.deleteCount >
                    previousMessages.length - range.position.absoluteIndex
                ) {
                    throw new RangeError(
                        'Conversation replace-range exceeds the current message count',
                    )
                }
                if (index > 0 && range.position.absoluteIndex < previousRangeEnd) {
                    throw new RangeError('Conversation operation ranges must be ordered and disjoint')
                }
                if (ranges.length > 1 && range.messages.length !== range.deleteCount) {
                    throw new RangeError(
                        'Multi-range conversation operations must preserve message positions',
                    )
                }
                if (
                    range.expectedMessages !== undefined &&
                    (
                        range.expectedMessages.length !== range.deleteCount ||
                        !valuesEqual(
                            range.expectedMessages,
                            previousMessages.slice(
                                range.position.absoluteIndex,
                                range.position.absoluteIndex + range.deleteCount,
                            ),
                        )
                    )
                ) {
                    throw new MessageLocatorMismatchError(
                        'Conversation operation message baseline changed',
                    )
                }
                previousRangeEnd = range.position.absoluteIndex + range.deleteCount
                return safeStructuredClone([...range.messages])
            })
            if (ranges.length > 0) {
                nextMessages = previousMessages.slice()
                for (let index = ranges.length - 1; index >= 0; index--) {
                    const range = ranges[index]
                    replaceArrayRange(
                        nextMessages,
                        range.position.absoluteIndex,
                        range.deleteCount,
                        replacements[index],
                    )
                }
                for (let index = 0; index < ranges.length; index++) {
                    const range = ranges[index]
                    commands.push('replace-range')
                    mutationRanges.push({
                        start: range.position.absoluteIndex,
                        deleteCount: range.deleteCount,
                        messages: replacements[index],
                        sessionVersion: previousVersion + index + 1,
                    })
                }
            }
            if (metadataChanged) commands.push('update-metadata')

            // A variable-only commit leaves every message in place, so its locators stay valid.
            const nextLocatorRegistry = variableOnly
                ? previousLocatorRegistry
                : previousLocatorRegistry.fork()
            this.conversation.message = nextMessages
            if (metadataChanged) {
                replaceConversationMetadata(this.conversation, nextMetadata)
            }
            this.sessionVersion = previousVersion + Math.max(1, ranges.length)
            this.locatorRegistry = nextLocatorRegistry
            try {
                this.notifyMutation(
                    previousVersion,
                    commands,
                    mutationRanges.length > 0
                        ? mutationRanges
                        : [
                              {
                                  start: nextMessages.length,
                                  deleteCount: 0,
                                  messages: [],
                                  sessionVersion: this.sessionVersion,
                              },
                          ],
                    variableOnly ? { display: commit.origin === 'display' } : undefined,
                )

                if (!variableOnly) previousLocatorRegistry.clear()
            } catch (error) {
                this.conversation.message = previousMessages
                if (metadataChanged) {
                    replaceConversationMetadata(this.conversation, previousMetadata)
                }
                this.sessionVersion = previousVersion
                if (!variableOnly) nextLocatorRegistry.clear()
                this.locatorRegistry = previousLocatorRegistry
                throw error
            }
        } finally {
            this.transactionActive = false
        }
    }

    /**
     * Writes chat variables, last writer wins per key, regardless of what changed since
     * the caller read them. Returns the keys whose value changed.
     */
    updateChatVariables(
        changes: ReadonlyMap<string, string>,
        options: { origin?: 'display' } = {},
    ): readonly string[] {
        this.assertActive()
        if (this.transactionActive) {
            throw new Error('Nested conversation session transactions are not supported')
        }
        const previousScriptstate = this.conversation.scriptstate
        const changed = [...changes].filter(
            ([key, value]) => previousScriptstate?.['$' + key] !== value,
        )
        if (changed.length === 0) return []

        this.transactionActive = true
        try {
            const previousVersion = this.sessionVersion
            const previousValues = changed.map(([key]) => {
                const stateKey = '$' + key
                return [
                    stateKey,
                    previousScriptstate !== undefined &&
                        Object.prototype.hasOwnProperty.call(previousScriptstate, stateKey),
                    previousScriptstate?.[stateKey],
                ] as const
            })
            const scriptstate = (this.conversation.scriptstate ??= {})
            for (const [key, value] of changed) scriptstate['$' + key] = value
            this.sessionVersion = previousVersion + 1
            try {
                this.notifyMutation(
                    previousVersion,
                    ['update-metadata'],
                    [{
                        start: this.conversation.message.length,
                        deleteCount: 0,
                        messages: [],
                        sessionVersion: this.sessionVersion,
                    }],
                    {
                        display: options.origin === 'display',
                        update: {
                            keys: changed.map(([key]) => key),
                            ...(options.origin ? { origin: options.origin } : {}),
                        },
                    },
                )
            } catch (error) {
                if (previousScriptstate === undefined) delete this.conversation.scriptstate
                else {
                    for (const [stateKey, existed, value] of previousValues) {
                        if (existed) scriptstate[stateKey] = value
                        else delete scriptstate[stateKey]
                    }
                }
                this.sessionVersion = previousVersion
                throw error
            }
            return changed.map(([key]) => key)
        } finally {
            this.transactionActive = false
        }
    }

    replaceTail(position: ConversationPosition, messages: readonly Message[]): void {
        this.assertActive()
        this.transaction((transaction) => transaction.replaceTail(position, messages))
    }

    reroll(position: ConversationPosition, messages: readonly Message[]): void {
        this.assertActive()
        this.transaction((transaction) => transaction.reroll(position, messages))
    }

    readBranchSource(locator: MessageLocator): ActiveConversationBranchSource {
        this.assertActive()
        validateLocator(
            this.conversationId,
            this.conversation.message,
            this.acceptedLocatorVersions,
            locator,
            this.locatorRegistry,
        )
        const endIndex = locator.absoluteIndex + 1
        return {
            characterId: this.characterId,
            conversationId: this.conversationId,
            messages: safeStructuredClone(this.conversation.message.slice(0, endIndex)),
            startIndex: 0,
            endIndex,
            totalMessages: this.totalMessages,
            storeRevision: this.storeRevision,
            sessionVersion: this.sessionVersion,
        }
    }

    adoptConversationReplacement(
        expectedVersion: number,
        expectedMessages: readonly Message[],
        replacement: Chat,
    ): Chat {
        this.assertActive()
        if (this.transactionActive) {
            throw new Error('Nested conversation session transactions are not supported')
        }
        if (this.sessionVersion !== expectedVersion) {
            throw new ConversationSessionStaleError(expectedVersion, this.sessionVersion)
        }
        if (this.conversation.message !== expectedMessages) {
            throw new MessageLocatorMismatchError('Conversation message array identity changed')
        }

        const previousVersion = this.sessionVersion
        const previousState = { ...this.conversation } as Chat
        const previousLocatorRegistry = this.locatorRegistry
        const nextLocatorRegistry = previousLocatorRegistry.fork()
        const target = this.conversation as unknown as Record<string, unknown>
        const source = replacement as unknown as Record<string, unknown>

        this.transactionActive = true
        try {
            for (const key of Object.keys(target)) {
                if (!(key in source)) delete target[key]
            }
            for (const [key, value] of Object.entries(source)) {
                target[key] = value
            }
            this.sessionVersion = previousVersion + 1
            this.locatorRegistry = nextLocatorRegistry
            this.notifyMutation(previousVersion, ['replace-conversation'], [{
                start: 0,
                deleteCount: previousState.message.length,
                messages: replacement.message,
                sessionVersion: this.sessionVersion,
            }])
            previousLocatorRegistry.clear()
            return this.conversation
        } catch (error) {
            const previous = previousState as unknown as Record<string, unknown>
            for (const key of Object.keys(target)) {
                if (!(key in previous)) delete target[key]
            }
            for (const [key, value] of Object.entries(previous)) {
                target[key] = value
            }
            this.sessionVersion = previousVersion
            nextLocatorRegistry.clear()
            this.locatorRegistry = previousLocatorRegistry
            throw error
        } finally {
            this.transactionActive = false
        }
    }

    transaction<T>(run: (transaction: ActiveConversationTransaction) => T): T {
        this.assertActive()
        if (this.transactionActive) throw new Error('Nested conversation session transactions are not supported')
        this.transactionActive = true
        const previousVersion = this.sessionVersion
        const transaction = new ActiveConversationTransaction(
            this.characterId,
            this.conversationId,
            this.conversation,
            this.conversation.message,
            this.storeRevision,
            this.locatorRegistry,
            previousVersion,
            this.lastContentChangeVersion,
        )
        try {
            const result = run(transaction)
            if (isThenable(result)) {
                transaction[abortConversationTransaction]()
                void Promise.resolve(result).catch(() => undefined)
                throw new TypeError('Conversation session transactions must be synchronous')
            }
            const completed = transaction[finishConversationTransaction]()
            if (completed.commands.length > 0) {
                const previousMessages = this.conversation.message
                const rollbackMessages = this.onMutation
                    ? captureMessageRollback(previousMessages)
                    : null
                const previousBookmarks = this.conversation.bookmarks
                const previousBookmarkNames = this.conversation.bookmarkNames
                const previousLocatorRegistry = this.locatorRegistry
                this.conversation.message = completed.messages
                if (completed.bookmarkMetadataChanged) {
                    this.conversation.bookmarks = completed.bookmarks
                    this.conversation.bookmarkNames = completed.bookmarkNames
                }
                this.sessionVersion = completed.version
                this.locatorRegistry = completed.locatorRegistry
                try {
                    this.notifyMutation(
                        previousVersion,
                        completed.commands,
                        completed.mutations,
                    )
                    previousLocatorRegistry.clear()
                } catch (error) {
                    if (rollbackMessages) {
                        restoreMessageRollback(previousMessages, rollbackMessages)
                    }
                    this.conversation.message = previousMessages
                    if (completed.bookmarkMetadataChanged) {
                        this.conversation.bookmarks = previousBookmarks
                        this.conversation.bookmarkNames = previousBookmarkNames
                    }
                    this.sessionVersion = previousVersion
                    completed.locatorRegistry.clear()
                    this.locatorRegistry = previousLocatorRegistry
                    throw error
                }
            }
            return result
        } finally {
            transaction[abortConversationTransaction]()
            this.transactionActive = false
        }
    }

    acquirePin(reason: ActiveConversationPinReason): ActiveConversationPin {
        this.assertActive()
        if (this.totalMessages > 0) {
            return this.acquireRangePin(0, this.totalMessages, reason)
        }
        return this.trackPin(reason)
    }

    acquireRangePin(
        startIndex: number,
        endIndex: number,
        reason: ActiveConversationPinReason,
    ): ActiveConversationPin {
        this.assertActive()
        validateIndex(startIndex, 'Conversation pin startIndex')
        validateIndex(endIndex, 'Conversation pin endIndex')
        if (endIndex <= startIndex) {
            throw new RangeError('Conversation pin range must not be empty')
        }
        if (endIndex > this.totalMessages) {
            throw new RangeError('Conversation pin range exceeds the current message count')
        }
        if (this.compatibilityFallback) return this.trackPin(reason)
        const residencyPin = this.residency.pinRange(startIndex, endIndex, reason)
        this.residencyRangePins.set(
            reason,
            (this.residencyRangePins.get(reason) ?? 0) + 1,
        )
        const tracked = this.trackPin(reason)
        let released = false
        return {
            reason,
            release: () => {
                if (released) return
                released = true
                residencyPin.release()
                const rangeCount = this.residencyRangePins.get(reason) ?? 0
                if (rangeCount <= 1) this.residencyRangePins.delete(reason)
                else this.residencyRangePins.set(reason, rangeCount - 1)
                tracked.release()
            },
        }
    }

    private trackPin(reason: ActiveConversationPinReason): ActiveConversationPin {
        this.pins.set(reason, (this.pins.get(reason) ?? 0) + 1)
        let released = false
        return {
            reason,
            release: () => {
                if (released) return
                released = true
                const count = this.pins.get(reason) ?? 0
                if (count <= 1) this.pins.delete(reason)
                else this.pins.set(reason, count - 1)
                this.schedulePinReleaseNotification()
            },
        }
    }

    pinCount(reason: ActiveConversationPinReason): number {
        const explicit = this.pins.get(reason) ?? 0
        if (this.compatibilityFallback) return explicit
        const residencyExplicit = this.residencyRangePins.get(reason) ?? 0
        const automatic = Math.max(
            0,
            this.residency.pinCount(reason) - residencyExplicit,
        )
        return explicit + automatic
    }

    ownsSessionToken(sessionToken: ConversationSessionToken): boolean {
        return this.active && sessionToken === this.locatorRegistry.sessionToken
    }

    beginPersistence(sessionVersion: number): ConversationPersistenceAttempt {
        this.assertActive()
        if (this.compatibilityFallback) {
            validateIndex(sessionVersion, 'Conversation persistence session version')
            if (sessionVersion <= this.persistedSessionVersion) {
                throw new RangeError('Conversation persistence version is already acknowledged')
            }
            if (sessionVersion > this.sessionVersion) {
                throw new RangeError('Conversation persistence version exceeds the current session version')
            }
            const pin = this.trackPin('pending-save')
            let released = false
            return {
                sessionVersion,
                acknowledge: (revision) => {
                    if (released) return
                    try {
                        this.acknowledgePersisted(
                            this.locatorRegistry.sessionToken,
                            sessionVersion,
                            revision,
                        )
                    } finally {
                        released = true
                        pin.release()
                    }
                },
                release: () => {
                    if (released) return
                    released = true
                    pin.release()
                },
            }
        }
        const attempt = this.residency.beginPersistence(sessionVersion)
        let released = false
        return {
            sessionVersion,
            acknowledge: (revision) => {
                if (released) return
                try {
                    this.acknowledgePersisted(
                        this.locatorRegistry.sessionToken,
                        sessionVersion,
                        revision,
                    )
                } finally {
                    released = true
                    attempt.release()
                    this.schedulePinReleaseNotification()
                }
            },
            release: () => {
                if (released) return
                released = true
                attempt.release()
                this.schedulePinReleaseNotification()
            },
        }
    }

    acknowledgePersisted(
        sessionToken: ConversationSessionToken,
        sessionVersion: number,
        revision: DataRevision,
    ): boolean {
        this.assertActive()
        if (sessionToken !== this.locatorRegistry.sessionToken) {
            throw new MessageLocatorMismatchError('Conversation persistence belongs to another session')
        }
        validateIndex(sessionVersion, 'Conversation persisted session version')
        validateIndex(revision, 'Conversation persisted data revision')
        if (sessionVersion <= this.persistedSessionVersion) return false
        if (sessionVersion > this.sessionVersion) {
            throw new ConversationSessionStaleError(sessionVersion, this.sessionVersion)
        }
        if (
            revision < this.currentStoreRevision ||
            (revision === this.currentStoreRevision && this.persistedSessionVersion === 0)
        ) {
            throw new RangeError('Conversation persisted data revision did not advance')
        }
        if (!this.compatibilityFallback) {
            try {
                this.residency.acknowledgePersisted(sessionVersion, revision)
            } catch (error) {
                this.activateCompatibilityFallback(this.conversation.message.length, { error })
            }
        }
        this.persistedSessionVersion = sessionVersion
        this.currentStoreRevision = revision
        return true
    }

    acknowledgeFallbackPersisted(
        sessionToken: ConversationSessionToken,
        sessionVersion: number,
        revision: DataRevision,
    ): boolean {
        this.assertActive()
        if (sessionToken !== this.locatorRegistry.sessionToken) {
            throw new MessageLocatorMismatchError('Conversation persistence belongs to another session')
        }
        this.activateCompatibilityFallback(this.conversation.message.length)
        return this.acknowledgePersisted(sessionToken, sessionVersion, revision)
    }

    materializeCompatibilitySnapshot(): ActiveConversationCompatibilitySnapshot {
        this.assertActive()
        const pin = this.acquirePin('compatibility')
        try {
            return new ActiveConversationCompatibilitySnapshot(
                safeStructuredClone(this.conversation.message),
                () => pin.release(),
            )
        } catch (error) {
            pin.release()
            throw error
        }
    }

    materializeCompatibilityArray(): Message[] {
        this.assertActive()
        return this.conversation.message
    }

    invalidate(): void {
        if (!this.active) return
        this.active = false
        this.notifySubscribers(null)
        this.subscribers.clear()
        this.pins.clear()
        this.residencyRangePins.clear()
        this.locatorRegistry.clear()
        this.conversation = null as unknown as Chat
    }

    private assertActive(): void {
        if (!this.active) throw new ConversationSessionInactiveError()
    }

    private schedulePinReleaseNotification(): void {
        if (!this.onPinReleased || this.pinReleaseNotificationPending) return
        this.pinReleaseNotificationPending = true
        queueMicrotask(() => {
            this.pinReleaseNotificationPending = false
            if (this.active) this.onPinReleased?.()
        })
    }

    private notifyMutation(
        previousVersion: number,
        commands: readonly ActiveConversationCommandName[],
        mutations: readonly ActiveConversationMutationRange[] = [],
        variableCommit?: VariableOnlyCommit,
    ): void {
        let detachedMutations = safeStructuredClone(mutations)
        if (!this.compatibilityFallback && !this.canApplyResidentMutations(detachedMutations)) {
            this.activateCompatibilityFallback(this.residency.totalMessages)
        }
        if (this.compatibilityFallback) {
            detachedMutations = this.createCompatibilityFallbackMutations(previousVersion)
        }
        const event: ActiveConversationMutationEvent = {
            ...(variableCommit?.display ? { displayVariableUpdate: true } : {}),
            ...(variableCommit?.update ? { variableUpdate: variableCommit.update } : {}),
            characterId: this.characterId,
            conversationId: this.conversationId,
            sessionToken: this.locatorRegistry.sessionToken,
            previousVersion,
            sessionVersion: this.sessionVersion,
            commands: [...commands],
            mutations: detachedMutations,
            conversation: cloneConversationMetadata(this.conversation),
        }
        this.onMutation?.(event)
        if (commands.some((command) => command !== 'update-metadata')) {
            this.lastMessageChangeVersion = this.sessionVersion
        }
        // Requests, prompts and locators read messages and non-variable metadata only.
        if (!variableCommit) {
            this.lastContentChangeVersion = this.sessionVersion
            this.generationContinuationStart = this.sessionVersion
        }
        if (this.compatibilityFallback) {
            this.compatibilityBaselineMessageCount = this.conversation.message.length
        } else {
            try {
                for (const mutation of event.mutations) {
                    this.residency.recordReplaceRange(mutation)
                }
            } catch (error) {
                this.activateCompatibilityFallback(this.conversation.message.length, { error })
            }
        }
        this.notifySubscribers(event)
    }

    private notifySubscribers(event: ActiveConversationMutationEvent | null): void {
        for (const subscriber of [...this.subscribers]) {
            try {
                subscriber(event)
            } catch (error) {
                console.error('Active conversation subscriber failed', error)
            }
        }
    }

    private activateCompatibilityFallback(
        expectedMessageCount: number,
        failure?: { error: unknown },
    ): void {
        if (this.compatibilityFallback) return
        if (failure) {
            console.error(
                'Active conversation residency failed; using compatibility fallback',
                failure.error,
            )
        }
        this.compatibilityFallback = true
        this.compatibilityBaselineMessageCount = expectedMessageCount
        this.residency.discardResidentState()
    }

    private createCompatibilityFallbackMutations(
        previousVersion: number,
    ): ActiveConversationMutationRange[] {
        const messages = safeStructuredClone(this.conversation.message)
        const mutations: ActiveConversationMutationRange[] = [{
            start: 0,
            deleteCount: this.compatibilityBaselineMessageCount ?? 0,
            messages,
            sessionVersion: previousVersion + 1,
            completeOwner: true,
        }]
        for (
            let sessionVersion = previousVersion + 2;
            sessionVersion <= this.sessionVersion;
            sessionVersion++
        ) {
            mutations.push({
                start: messages.length,
                deleteCount: 0,
                messages: [],
                sessionVersion,
            })
        }
        return mutations
    }

    private canApplyResidentMutations(
        mutations: readonly ActiveConversationMutationRange[],
    ): boolean {
        let messageCount = this.residency.totalMessages
        let version = this.residency.sessionVersion
        for (const mutation of mutations) {
            if (
                mutation.sessionVersion !== version + 1 ||
                mutation.start > messageCount ||
                mutation.deleteCount > messageCount - mutation.start
            ) return false
            messageCount += mutation.messages.length - mutation.deleteCount
            version = mutation.sessionVersion
        }
        return messageCount === this.conversation.message.length
    }
}

export function requireCurrentConversationSession(
    expected: ActiveConversationSession,
    current: ActiveConversationSession | null,
): ActiveConversationSession {
    if (!expected.isActive || current !== expected) {
        throw new ConversationSessionInactiveError()
    }
    return expected
}
