import type { Chat, Message } from './database.svelte'
import { CONVERSATION_RANGE_MAX_LIMIT, type DataRevision } from './persistentDataStore'

export type ActiveConversationPinReason =
    | 'dirty'
    | 'pending-save'
    | 'streaming'
    | 'transaction'
    | 'compatibility'

export interface MessageLocator {
    conversationId: string
    absoluteIndex: number
    expectedMessageId?: string
    sessionVersion: number
}

export interface ConversationPosition {
    conversationId: string
    absoluteIndex: number
    sessionVersion: number
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

function createLocator(
    conversationId: string,
    messages: readonly Message[],
    absoluteIndex: number,
    sessionVersion: number,
): MessageLocator {
    validateIndex(absoluteIndex, 'Message locator index')
    const message = messages[absoluteIndex]
    if (!message) throw new MessageLocatorNotFoundError(absoluteIndex)
    return {
        conversationId,
        absoluteIndex,
        ...(message.chatId === undefined ? {} : { expectedMessageId: message.chatId }),
        sessionVersion,
    }
}

function validateLocator(
    conversationId: string,
    messages: readonly Message[],
    sessionVersion: number,
    locator: MessageLocator,
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
}

function readRange(
    characterId: string,
    conversationId: string,
    messages: readonly Message[],
    storeRevision: DataRevision,
    sessionVersion: number,
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
        messages: selected,
        locators: selected.map((_message, offset) =>
            createLocator(conversationId, messages, startIndex + offset, sessionVersion),
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
    private currentVersion: number
    private readonly commandNames: ActiveConversationCommandName[] = []

    constructor(
        private readonly characterId: string,
        private readonly conversationId: string,
        messages: readonly Message[],
        private readonly storeRevision: DataRevision,
        sessionVersion: number,
    ) {
        this.currentMessages = messages.slice()
        this.currentVersion = sessionVersion
    }

    get version(): number {
        return this.currentVersion
    }

    get totalMessages(): number {
        return this.currentMessages.length
    }

    locate(absoluteIndex: number): MessageLocator {
        return createLocator(
            this.conversationId,
            this.currentMessages,
            absoluteIndex,
            this.currentVersion,
        )
    }

    positionAt(absoluteIndex: number): ConversationPosition {
        validateIndex(absoluteIndex, 'Conversation position index')
        if (absoluteIndex > this.currentMessages.length) {
            throw new MessageLocatorNotFoundError(absoluteIndex)
        }
        return {
            conversationId: this.conversationId,
            absoluteIndex,
            sessionVersion: this.currentVersion,
        }
    }

    readRange(startIndex: number, limit: number): ActiveConversationWindow {
        return readRange(
            this.characterId,
            this.conversationId,
            this.currentMessages,
            this.storeRevision,
            this.currentVersion,
            startIndex,
            limit,
        )
    }

    append(message: Message): MessageLocator {
        const absoluteIndex = this.currentMessages.length
        this.currentMessages = [...this.currentMessages, message]
        this.record('append')
        return this.locate(absoluteIndex)
    }

    edit(locator: MessageLocator, message: Message): MessageLocator {
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.currentVersion,
            locator,
        )
        const nextMessages = this.currentMessages.slice()
        nextMessages[locator.absoluteIndex] = message
        this.currentMessages = nextMessages
        this.record('edit')
        return this.locate(locator.absoluteIndex)
    }

    delete(locator: MessageLocator): void {
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.currentVersion,
            locator,
        )
        this.currentMessages = [
            ...this.currentMessages.slice(0, locator.absoluteIndex),
            ...this.currentMessages.slice(locator.absoluteIndex + 1),
        ]
        this.record('delete')
    }

    truncate(locator: MessageLocator): void {
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.currentVersion,
            locator,
        )
        this.currentMessages = this.currentMessages.slice(0, locator.absoluteIndex)
        this.record('truncate')
    }

    replaceTail(position: ConversationPosition, messages: readonly Message[]): void {
        this.replaceTailAs('replace-tail', position, messages)
    }

    reroll(position: ConversationPosition, messages: readonly Message[]): void {
        this.replaceTailAs('reroll', position, messages)
    }

    readBranchSource(locator: MessageLocator): ActiveConversationBranchSource {
        validateLocator(
            this.conversationId,
            this.currentMessages,
            this.currentVersion,
            locator,
        )
        const endIndex = locator.absoluteIndex + 1
        return {
            characterId: this.characterId,
            conversationId: this.conversationId,
            messages: this.currentMessages.slice(0, endIndex),
            startIndex: 0,
            endIndex,
            totalMessages: this.currentMessages.length,
            storeRevision: this.storeRevision,
            sessionVersion: this.currentVersion,
        }
    }

    get changed(): boolean {
        return this.commandNames.length > 0
    }

    get messages(): Message[] {
        return this.currentMessages
    }

    get commands(): readonly ActiveConversationCommandName[] {
        return this.commandNames
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
        )
        this.currentMessages = [
            ...this.currentMessages.slice(0, position.absoluteIndex),
            ...messages,
        ]
        this.record(command)
    }

    private record(command: ActiveConversationCommandName): void {
        this.currentVersion += 1
        this.commandNames.push(command)
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
    private sessionVersion = 0
    private transactionActive = false

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

    get totalMessages(): number {
        return this.conversation.message.length
    }

    get activePinReasons(): ActiveConversationPinReason[] {
        return [...this.pins.keys()]
    }

    locate(absoluteIndex: number): MessageLocator {
        return createLocator(
            this.conversationId,
            this.conversation.message,
            absoluteIndex,
            this.sessionVersion,
        )
    }

    positionAt(absoluteIndex: number): ConversationPosition {
        validateIndex(absoluteIndex, 'Conversation position index')
        if (absoluteIndex > this.totalMessages) {
            throw new MessageLocatorNotFoundError(absoluteIndex)
        }
        return {
            conversationId: this.conversationId,
            absoluteIndex,
            sessionVersion: this.sessionVersion,
        }
    }

    readLatest(limit: number): ActiveConversationWindow {
        validateCount(limit)
        return this.readRange(Math.max(0, this.totalMessages - limit), limit)
    }

    readRange(startIndex: number, limit: number): ActiveConversationWindow {
        return readRange(
            this.characterId,
            this.conversationId,
            this.conversation.message,
            this.storeRevision,
            this.sessionVersion,
            startIndex,
            limit,
        )
    }

    scanBackward(
        startIndexExclusive = this.totalMessages,
        limit = CONVERSATION_RANGE_MAX_LIMIT,
    ): ActiveConversationBackwardScan {
        validateIndex(startIndexExclusive, 'Backward scan startIndex')
        validateCount(limit)
        const start = Math.min(this.totalMessages, startIndexExclusive)
        const entries: BackwardConversationEntry[] = []
        for (let absoluteIndex = start - 1; absoluteIndex >= 0 && entries.length < limit; absoluteIndex--) {
            entries.push({
                absoluteIndex,
                message: this.conversation.message[absoluteIndex],
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
        return this.transaction((transaction) => transaction.append(message))
    }

    edit(locator: MessageLocator, message: Message): MessageLocator {
        return this.transaction((transaction) => transaction.edit(locator, message))
    }

    delete(locator: MessageLocator): void {
        this.transaction((transaction) => transaction.delete(locator))
    }

    truncate(locator: MessageLocator): void {
        this.transaction((transaction) => transaction.truncate(locator))
    }

    replaceTail(position: ConversationPosition, messages: readonly Message[]): void {
        this.transaction((transaction) => transaction.replaceTail(position, messages))
    }

    reroll(position: ConversationPosition, messages: readonly Message[]): void {
        this.transaction((transaction) => transaction.reroll(position, messages))
    }

    readBranchSource(locator: MessageLocator): ActiveConversationBranchSource {
        validateLocator(
            this.conversationId,
            this.conversation.message,
            this.sessionVersion,
            locator,
        )
        const endIndex = locator.absoluteIndex + 1
        return {
            characterId: this.characterId,
            conversationId: this.conversationId,
            messages: this.conversation.message.slice(0, endIndex),
            startIndex: 0,
            endIndex,
            totalMessages: this.totalMessages,
            storeRevision: this.storeRevision,
            sessionVersion: this.sessionVersion,
        }
    }

    transaction<T>(run: (transaction: ActiveConversationTransaction) => T): T {
        if (this.transactionActive) throw new Error('Nested conversation session transactions are not supported')
        this.transactionActive = true
        const previousVersion = this.sessionVersion
        const transaction = new ActiveConversationTransaction(
            this.characterId,
            this.conversationId,
            this.conversation.message,
            this.storeRevision,
            previousVersion,
        )
        try {
            const result = run(transaction)
            if (transaction.changed) {
                this.conversation.message = transaction.messages
                this.sessionVersion = transaction.version
                this.onMutation?.({
                    characterId: this.characterId,
                    conversationId: this.conversationId,
                    previousVersion,
                    sessionVersion: this.sessionVersion,
                    commands: transaction.commands,
                })
            }
            return result
        } finally {
            this.transactionActive = false
        }
    }

    acquirePin(reason: ActiveConversationPinReason): ActiveConversationPin {
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
        return this.conversation.message
    }
}
