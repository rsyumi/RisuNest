import type { Chat, Message } from './database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT, type DataRevision } from './persistentDataStore'
import { safeStructuredClone } from '../polyfill'

export type ActiveConversationPinReason =
    | 'dirty'
    | 'pending-save'
    | 'streaming'
    | 'transaction'
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

export interface ActiveConversationMutationEvent {
    characterId: string
    conversationId: string
    previousVersion: number
    sessionVersion: number
    commands: readonly ActiveConversationCommandName[]
}

export type ActiveConversationCommandName =
    | 'append'
    | 'edit'
    | 'delete'
    | 'truncate'
    | 'replace-tail'
    | 'reroll'
    | 'bookmark'

export interface SetConversationBookmarkOptions {
    bookmarked: boolean
    messageId?: string
    name?: string
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
    onMutation?(event: ActiveConversationMutationEvent): void
}

interface MessageLocatorIdentity {
    type: 'message'
    absoluteIndex: number
    sessionVersion: number
    messages: readonly Message[]
    message: Message
}

interface ConversationPositionIdentity {
    type: 'position'
    absoluteIndex: number
    sessionVersion: number
    messages: readonly Message[]
    before?: Message
    after?: Message
}

type ConversationTokenIdentity = MessageLocatorIdentity | ConversationPositionIdentity

let nextConversationSessionToken = 0
let nextConversationLocatorToken = 0

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
        sessionVersion: number,
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
            currentIdentity.sessionVersion === sessionVersion &&
            currentIdentity.messages === messages &&
            currentIdentity.message === message
        ) return currentToken

        if (currentToken !== undefined) this.identities.delete(currentToken)
        const token = createMessageLocatorToken()
        this.messageTokens.set(absoluteIndex, token)
        this.identities.set(token, {
            type: 'message',
            absoluteIndex,
            sessionVersion,
            messages,
            message,
        })
        return token
    }

    registerPosition(
        messages: readonly Message[],
        absoluteIndex: number,
        sessionVersion: number,
    ): ConversationPositionToken {
        const currentToken = this.positionTokens.get(absoluteIndex)
        const currentIdentity = currentToken === undefined
            ? undefined
            : this.identities.get(currentToken)
        if (
            currentToken !== undefined &&
            currentIdentity?.type === 'position' &&
            currentIdentity.absoluteIndex === absoluteIndex &&
            currentIdentity.sessionVersion === sessionVersion &&
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
            sessionVersion,
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
            identity.sessionVersion === locator.sessionVersion &&
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
            identity.sessionVersion === position.sessionVersion &&
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

function createConversationSessionToken(): ConversationSessionToken {
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
            sessionVersion,
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
            sessionVersion,
        ),
    }
    return position
}

function validateLocator(
    conversationId: string,
    messages: readonly Message[],
    sessionVersion: number,
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
    if (locator.sessionVersion !== sessionVersion) {
        throw new ConversationSessionStaleError(locator.sessionVersion, sessionVersion)
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
    sessionVersion: number,
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
    if (position.sessionVersion !== sessionVersion) {
        throw new ConversationSessionStaleError(position.sessionVersion, sessionVersion)
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
    private readonly locatorRegistry: ConversationLocatorRegistry
    private readonly commandNames: ActiveConversationCommandName[] = []
    private closed = false

    constructor(
        private readonly characterId: string,
        private readonly conversationId: string,
        sourceConversation: Chat,
        private readonly sourceMessages: readonly Message[],
        private readonly storeRevision: DataRevision,
        private readonly sourceLocatorRegistry: ConversationLocatorRegistry,
        sessionVersion: number,
    ) {
        this.currentMessages = [...sourceMessages]
        this.currentBookmarks = sourceConversation.bookmarks === undefined
            ? undefined
            : [...sourceConversation.bookmarks]
        this.currentBookmarkNames = sourceConversation.bookmarkNames === undefined
            ? undefined
            : { ...sourceConversation.bookmarkNames }
        this.currentVersion = sessionVersion
        this.locatorRegistry = sourceLocatorRegistry.fork()
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

    append(message: Message): MessageLocator {
        this.assertOpen()
        const absoluteIndex = this.currentMessages.length
        this.currentMessages = [...this.currentMessages, safeStructuredClone(message)]
        this.record('append')
        return this.locate(absoluteIndex)
    }

    edit(locator: MessageLocator, message: Message): MessageLocator {
        this.assertOpen()
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.currentVersion,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        const nextMessages = this.currentMessages.slice()
        nextMessages[locator.absoluteIndex] = safeStructuredClone(message)
        this.currentMessages = nextMessages
        this.record('edit')
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
            this.currentVersion,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        const messageId = options.bookmarked
            ? message.chatId ?? options.messageId
            : message.chatId
        if (!messageId) {
            if (!options.bookmarked) return this.locate(locator.absoluteIndex)
            throw new Error('A bookmark requires a message ID')
        }

        if (options.bookmarked) {
            if (message.chatId === undefined) {
                const nextMessages = this.currentMessages.slice()
                nextMessages[locator.absoluteIndex] = {
                    ...safeStructuredClone(message),
                    chatId: messageId,
                }
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
        this.record('bookmark')
        return this.locate(locator.absoluteIndex)
    }

    renameBookmark(locator: MessageLocator, name: string): MessageLocator {
        this.assertOpen()
        const message = validateLocator(
            this.conversationId,
            this.currentMessages,
            this.currentVersion,
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
            this.currentVersion,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        this.currentMessages = [
            ...this.currentMessages.slice(0, locator.absoluteIndex),
            ...this.currentMessages.slice(locator.absoluteIndex + 1),
        ]
        this.record('delete')
    }

    truncate(locator: MessageLocator): void {
        this.assertOpen()
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.currentVersion,
            locator,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        this.currentMessages = this.currentMessages.slice(0, locator.absoluteIndex)
        this.record('truncate')
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
            this.currentVersion,
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
            this.currentVersion,
            position,
            this.locatorRegistry,
            this.sourceMessages,
            this.sourceLocatorRegistry,
        )
        this.currentMessages = [
            ...this.currentMessages.slice(0, position.absoluteIndex),
            ...safeStructuredClone(messages),
        ]
        this.record(command)
    }

    private record(command: ActiveConversationCommandName): void {
        this.currentVersion += 1
        this.locatorRegistry.clear()
        this.commandNames.push(command)
    }

    private assertOpen(): void {
        if (this.closed) throw new Error('Conversation session transaction is closed')
    }
}

export class ActiveConversationSession {
    readonly characterId: string
    readonly conversationId: string
    readonly storeRevision: DataRevision
    readonly evictionEnabled = false

    private readonly conversation: Chat
    private readonly onMutation?: (event: ActiveConversationMutationEvent) => void
    private readonly pins = new Map<ActiveConversationPinReason, number>()
    private locatorRegistry = new ConversationLocatorRegistry()
    private sessionVersion = 0
    private transactionActive = false
    private active = true

    constructor(options: ActiveConversationSessionOptions) {
        if (!options.conversation) {
            throw new ConversationNotFoundError(options.characterId, options.conversationId)
        }
        this.characterId = options.characterId
        this.conversationId = options.conversationId
        this.conversation = options.conversation
        this.storeRevision = options.storeRevision
        this.onMutation = options.onMutation
    }

    get version(): number {
        return this.sessionVersion
    }

    get isActive(): boolean {
        return this.active
    }

    get totalMessages(): number {
        this.assertActive()
        return this.conversation.message.length
    }

    get activePinReasons(): ActiveConversationPinReason[] {
        return [...this.pins.keys()]
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
            this.sessionVersion,
            locator,
            this.locatorRegistry,
        )
        return locator.absoluteIndex
    }

    findMessageLocatorById(messageId: string): MessageLocator | null {
        this.assertActive()
        const absoluteIndex = this.conversation.message.findIndex(
            (message) => message.chatId === messageId,
        )
        return absoluteIndex === -1 ? null : this.locate(absoluteIndex)
    }

    readMessage(locator: MessageLocator): Message {
        this.assertActive()
        const message = validateLocator(
            this.conversationId,
            this.conversation.message,
            this.sessionVersion,
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
                this.sessionVersion,
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
        return readRange(
            this.characterId,
            this.conversationId,
            this.conversation.message,
            this.storeRevision,
            this.sessionVersion,
            this.locatorRegistry,
            startIndex,
            limit,
        )
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
        const previousVersion = this.sessionVersion
        const previousLocatorRegistry = this.locatorRegistry
        const nextLocatorRegistry = previousLocatorRegistry.fork()
        const absoluteIndex = this.conversation.message.length
        this.conversation.message.push(safeStructuredClone(message))
        this.sessionVersion += 1
        this.locatorRegistry = nextLocatorRegistry
        try {
            this.notifyMutation(previousVersion, ['append'])
            previousLocatorRegistry.clear()
        } catch (error) {
            this.conversation.message.pop()
            this.sessionVersion = previousVersion
            nextLocatorRegistry.clear()
            this.locatorRegistry = previousLocatorRegistry
            throw error
        }
        return this.locate(absoluteIndex)
    }

    edit(locator: MessageLocator, message: Message): MessageLocator {
        this.assertActive()
        if (this.transactionActive) throw new Error('Nested conversation session transactions are not supported')
        validateLocator(
            this.conversationId,
            this.conversation.message,
            this.sessionVersion,
            locator,
            this.locatorRegistry,
        )
        const previousVersion = this.sessionVersion
        const previousLocatorRegistry = this.locatorRegistry
        const nextLocatorRegistry = previousLocatorRegistry.fork()
        const previousMessage = this.conversation.message[locator.absoluteIndex]
        this.conversation.message[locator.absoluteIndex] = safeStructuredClone(message)
        this.sessionVersion += 1
        this.locatorRegistry = nextLocatorRegistry
        try {
            this.notifyMutation(previousVersion, ['edit'])
            previousLocatorRegistry.clear()
        } catch (error) {
            this.conversation.message[locator.absoluteIndex] = previousMessage
            this.sessionVersion = previousVersion
            nextLocatorRegistry.clear()
            this.locatorRegistry = previousLocatorRegistry
            throw error
        }
        return this.locate(locator.absoluteIndex)
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
            this.sessionVersion,
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
                    ? previousMessages.map((message) => ({
                        message,
                        snapshot: safeStructuredClone(message),
                    }))
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
                    this.onMutation?.({
                        characterId: this.characterId,
                        conversationId: this.conversationId,
                        previousVersion,
                        sessionVersion: this.sessionVersion,
                        commands: completed.commands,
                    })
                    previousLocatorRegistry.clear()
                } catch (error) {
                    if (rollbackMessages) {
                        previousMessages.length = rollbackMessages.length
                        for (let index = 0; index < rollbackMessages.length; index++) {
                            const rollback = rollbackMessages[index]
                            restoreMessage(rollback.message, rollback.snapshot)
                            previousMessages[index] = rollback.message
                        }
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
            },
        }
    }

    pinCount(reason: ActiveConversationPinReason): number {
        return this.pins.get(reason) ?? 0
    }

    materializeCompatibilityArray(): Message[] {
        this.assertActive()
        return this.conversation.message
    }

    invalidate(): void {
        if (!this.active) return
        this.active = false
        this.pins.clear()
        this.locatorRegistry.clear()
    }

    private assertActive(): void {
        if (!this.active) throw new ConversationSessionInactiveError()
    }

    private notifyMutation(
        previousVersion: number,
        commands: readonly ActiveConversationCommandName[],
    ): void {
        this.onMutation?.({
            characterId: this.characterId,
            conversationId: this.conversationId,
            previousVersion,
            sessionVersion: this.sessionVersion,
            commands,
        })
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
