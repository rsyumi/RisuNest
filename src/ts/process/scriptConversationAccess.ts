import {
    ConversationSessionStaleError,
    MessageLocatorMismatchError,
    requireCurrentConversationSession,
    type ActiveConversationOperationRange,
    type ActiveConversationSession,
    type ConversationMetadata,
} from '../storage/activeConversationSession'
import type { Chat, Database, Message } from '../storage/database.svelte'
import { peekActiveConversationSession } from '../storage/persistentDataRuntime.svelte'
import { safeStructuredClone } from '../safeStructuredClone'
import { parseKeyValue } from '../util'
import { getHistoryMessageAt } from './historyWindowIndex'
import { noteChatVariableRead } from './scriptVariableReads'
import {
    createConversationDatabaseView,
    createConversationOperationContext,
    valuesEqual,
    type ConversationCommitObserver,
    type ConversationOperationContext,
} from './conversationOperationContext'

export interface ScriptConversationAccessOptions {
    session: ActiveConversationSession
    /** The live conversation object the session owns. */
    chat: Chat
    /** Resolves character and template default variables. */
    database: Database
    characterId: string
    onCommitted?: ConversationCommitObserver
    /** Display listeners commit with display origin, which keeps rendered rows valid. */
    origin?: 'display'
    /**
     * Called with the changed keys after each commit that changes chat variables.
     * Rendered rows keep their output across those commits unless a message they show changed.
     */
    onVariablesCommitted?: (keys: readonly string[]) => void
}

/** Message field edits and appends, kept apart from the live conversation until the commit. */
interface MessageEditSet {
    readonly messageVersion: number
    /** The live messages, with copies in place of the edited ones and the appended ones after them. */
    readonly chat: Chat
    readonly baseLength: number
    readonly edited: Set<number>
}

/**
 * One script invocation's view of the selected conversation. Chat variables are read
 * from the live conversation and buffered on write; messages are read live and copied
 * only when the script first edits them: a field edit copies that message, any other
 * edit copies the conversation.
 */
export class ScriptConversationAccess {
    private readonly pendingVariables = new Map<string, string>()
    private operation: ConversationOperationContext | null = null
    private edits: MessageEditSet | null = null
    private firstReadMessageVersion: number | null = null
    private failure: { error: unknown } | null = null
    private defaultVariables: Map<string, string> | null | undefined
    private readonly precedingGenerationVersion: number

    constructor(private readonly options: ScriptConversationAccessOptions) {
        this.precedingGenerationVersion = options.session.generationInvalidationVersion
    }

    get session(): ActiveConversationSession {
        return this.options.session
    }

    get characterId(): string {
        return this.options.characterId
    }

    /** The live conversation object the session owns. */
    get liveChat(): Chat {
        return this.options.chat
    }

    get origin(): 'display' | undefined {
        return this.options.origin
    }

    get conversationOperation(): ConversationOperationContext | null {
        return this.operation
    }

    get hasPendingVariables(): boolean {
        return this.pendingVariables.size > 0
    }

    getVariable(key: string): string {
        noteChatVariableRead(key)
        const pending = this.pendingVariables.get(key)
        if (pending !== undefined) return pending
        const defaults = this.readDefaultVariables()
        if (defaults === null) return 'null'
        this.requireCurrent()
        const state = this.options.session.readChatVariable(key)
        if (state === undefined || state === null) return defaults.get(key) ?? 'null'
        return String(state)
    }

    /** Buffers a variable write. Returns whether it changes the stored value. */
    setVariable(key: string, value: string): boolean {
        const current = this.pendingVariables.has(key)
            ? this.pendingVariables.get(key)
            : this.options.session.readChatVariable(key)
        if (current === value) return false
        this.pendingVariables.set(key, value)
        return true
    }

    /** The conversation to read: live until the script edits it, then with its edits. */
    readChat(): Chat {
        if (this.operation) return this.operation.chat
        if (this.edits) return this.edits.chat
        this.requireCurrent()
        this.firstReadMessageVersion ??= this.options.session.messageVersion
        return this.options.chat
    }

    /** The conversation this access currently exposes, without recording a read. */
    currentChat(): Chat {
        return this.operation?.chat ?? this.edits?.chat ?? this.options.chat
    }

    /** Opens the message operation for an edit, unless messages changed since the first read. */
    writeOperation(): ConversationOperationContext {
        if (this.operation) return this.operation
        this.requireCurrent()
        this.requireMessagesSinceRead()
        const edits = this.edits
        if (edits && this.options.chat.message.length !== edits.baseLength) {
            this.fail(new MessageLocatorMismatchError(
                'Conversation messages changed without a session command',
            ))
        }
        const operation = createConversationOperationContext(
            this.options.session,
            this.options.chat,
            this.options.onCommitted,
        )
        if (edits) {
            const messages = operation.chat.message
            for (const index of edits.edited) messages[index] = edits.chat.message[index]
            messages.push(...edits.chat.message.slice(edits.baseLength))
        }
        this.edits = null
        this.operation = operation
        return operation
    }

    writeChat(): Chat {
        return this.writeOperation().chat
    }

    /** Edits one message, `Array.prototype.at` semantics; an index outside the conversation is ignored. */
    editMessage(index: number, edit: (message: Message) => void): void {
        if (this.operation) {
            const message = getHistoryMessageAt(this.operation.chat, index)
            if (message) edit(message)
            return
        }
        const edits = this.openEdits()
        const messages = edits.chat.message
        const truncated = Math.trunc(Number(index))
        const integer = Number.isNaN(truncated) ? 0 : truncated
        const position = integer < 0 ? messages.length + integer : integer
        if (position < 0 || position >= messages.length) return
        if (position < edits.baseLength && !edits.edited.has(position)) {
            messages[position] = safeStructuredClone(messages[position])
            edits.edited.add(position)
        }
        edit(messages[position])
    }

    appendMessage(message: Message): void {
        if (this.operation) {
            this.operation.chat.message.push(message)
            return
        }
        this.openEdits().chat.message.push(message)
    }

    databaseView(database: Database): Database {
        if (this.operation) return this.operation.createDatabaseView(database)
        if (!this.edits) return database
        const session = this.options.session
        return createConversationDatabaseView(
            database,
            session.characterId,
            session.conversationId,
            this.edits.chat,
        )
    }

    /** Commits buffered variables now, last writer wins. Returns the changed keys. */
    flushVariables(): readonly string[] {
        if (this.pendingVariables.size === 0) return []
        this.requireCurrent()
        const changes = new Map(this.pendingVariables)
        this.pendingVariables.clear()
        const session = this.options.session
        const baseVersion = session.version
        const keys = session.updateChatVariables(changes, {
            ...(this.options.origin ? { origin: this.options.origin } : {}),
        })
        if (keys.length === 0) return keys
        this.reportCommit(baseVersion)
        this.options.onVariablesCommitted?.(keys)
        return keys
    }

    /**
     * Commits everything the invocation changed. Variables and message edits commit
     * together. If the edits are stale, or the script hit a stale or closed conversation,
     * the variables still commit and the error is rethrown.
     */
    commit(): void {
        this.finish(true)
    }

    /** Publishes the segment read before a nested request, including a read-only baseline check. */
    checkpoint(): void {
        this.requireCurrent()
        this.requireMessagesSinceRead()
        this.commit()
    }

    discard(): void {
        this.operation?.release()
        this.operation = null
        this.edits = null
        this.pendingVariables.clear()
        this.firstReadMessageVersion = null
        this.failure = null
    }

    /** Commits like `commit`, but with nothing to commit it finishes even if the conversation closed. */
    commitIfChanged(): void {
        this.finish(true, false)
    }

    /** Commits variables and conversation metadata, dropping message edits. */
    commitWithoutMessages(): void {
        this.finish(false)
    }

    private finish(withMessages: boolean, requireOpen = true): void {
        const operation = this.operation
        const edits = withMessages ? this.edits : null
        const failure = this.failure
        this.operation = null
        this.edits = null
        this.failure = null
        this.firstReadMessageVersion = null
        if (failure) {
            operation?.release()
            this.commitVariablesAfterFailure()
            throw failure.error
        }
        if (!operation && !edits) {
            if (requireOpen) this.requireCurrent()
            this.flushVariables()
            return
        }
        try {
            const session = this.options.session
            const changed = [...this.pendingVariables]
                .filter(([key, value]) => session.readChatVariable(key) !== value)
                .map(([key]) => key)
            if (operation) {
                const current = peekActiveConversationSession()
                const variables = new Map(this.pendingVariables)
                if (withMessages) {
                    operation.commit(current, {
                        ...(this.options.origin ? { origin: this.options.origin } : {}),
                        variables,
                    })
                } else {
                    operation.commitMetadata(current, variables)
                }
            } else {
                this.commitEdits(edits!)
            }
            this.pendingVariables.clear()
            if (changed.length > 0) this.options.onVariablesCommitted?.(changed)
        } catch (error) {
            operation?.release()
            this.commitVariablesAfterFailure()
            throw error
        }
    }

    private openEdits(): MessageEditSet {
        if (this.edits) return this.edits
        this.requireCurrent()
        this.requireMessagesSinceRead()
        const session = this.options.session
        const live = this.options.chat
        const messages = live.message.slice()
        this.firstReadMessageVersion ??= session.messageVersion
        this.edits = {
            messageVersion: session.messageVersion,
            chat: { ...live, message: messages },
            baseLength: messages.length,
            edited: new Set(),
        }
        return this.edits
    }

    /** Applies the edit set and buffered variables as one session command, positioned now. */
    private commitEdits(edits: MessageEditSet): void {
        this.requireCurrent()
        const session = this.options.session
        if (session.messageVersion !== edits.messageVersion) {
            throw new ConversationSessionStaleError(edits.messageVersion, session.messageVersion)
        }
        const live = this.options.chat
        const liveMessages = session.materializeCompatibilityArray()
        if (liveMessages !== live.message || liveMessages.length !== edits.baseLength) {
            throw new MessageLocatorMismatchError(
                'Conversation messages changed without a session command',
            )
        }
        const messages = edits.chat.message
        const edited = [...edits.edited]
            .filter((index) => !valuesEqual(liveMessages[index], messages[index]))
            .sort((left, right) => left - right)
        const ranges: ActiveConversationOperationRange[] = []
        if (messages.length > edits.baseLength) {
            // A multi-range command keeps positions, so appends take one range from the first edit.
            const start = edited[0] ?? edits.baseLength
            ranges.push({
                position: session.positionAt(start),
                deleteCount: edits.baseLength - start,
                messages: messages.slice(start),
            })
        } else {
            for (let first = 0; first < edited.length;) {
                let end = first + 1
                while (end < edited.length && edited[end] === edited[end - 1] + 1) end += 1
                const start = edited[first]
                ranges.push({
                    position: session.positionAt(start),
                    deleteCount: end - first,
                    messages: messages.slice(start, start + end - first),
                })
                first = end
            }
        }

        const currentMetadata: ConversationMetadata = {}
        for (const [key, value] of Object.entries(live)) {
            if (key !== 'message') currentMetadata[key] = value
        }
        let metadata = currentMetadata
        if (this.pendingVariables.size > 0) {
            const scriptstate: Record<string, unknown> = {
                ...(currentMetadata.scriptstate as Record<string, unknown> | undefined),
            }
            for (const [key, value] of this.pendingVariables) scriptstate['$' + key] = value
            metadata = { ...currentMetadata, scriptstate }
        }
        const baseVersion = session.version
        session.applyOperation({
            ...(this.options.origin ? { origin: this.options.origin } : {}),
            expectedVersion: baseVersion,
            expectedMetadata: currentMetadata,
            metadata,
            ranges,
        })
        if (session.version !== baseVersion) this.reportCommit(baseVersion)
    }

    /**
     * The receipt an operation reports, so a generation waiting on this conversation
     * adopts the commit instead of treating it as a foreign edit.
     */
    private reportCommit(baseVersion: number): void {
        const session = this.options.session
        const version = session.version
        const chat = this.options.chat
        this.options.onCommitted?.({
            follows: (current, previousVersion, target) =>
                current === session &&
                previousVersion !== null &&
                previousVersion >= this.precedingGenerationVersion &&
                previousVersion <= baseVersion &&
                target === chat &&
                session.isActive &&
                session.version === version &&
                session.materializeCompatibilityArray() === target.message,
        })
    }

    private commitVariablesAfterFailure(): void {
        try {
            this.flushVariables()
        } catch {
            this.pendingVariables.clear()
        }
    }

    private requireMessagesSinceRead(): void {
        const session = this.options.session
        if (
            this.firstReadMessageVersion !== null &&
            session.messageVersion !== this.firstReadMessageVersion
        ) {
            this.fail(new ConversationSessionStaleError(
                this.firstReadMessageVersion,
                session.messageVersion,
            ))
        }
    }

    /** Errors the script sees are swallowed by the script host, so the commit rethrows them. */
    private fail(error: unknown): never {
        this.failure ??= { error }
        throw error
    }

    private requireCurrent(): void {
        try {
            requireCurrentConversationSession(this.options.session, peekActiveConversationSession())
        } catch (error) {
            this.fail(error)
        }
    }

    private readDefaultVariables(): Map<string, string> | null {
        if (this.defaultVariables !== undefined) return this.defaultVariables
        const database = this.options.database
        const character = database.characters?.find(
            (candidate) => candidate.chaId === this.options.characterId,
        )
        if (!character) return (this.defaultVariables = null)
        const defaults = new Map<string, string>()
        for (const [key, value] of parseKeyValue(character.defaultVariables).concat(
            parseKeyValue(database.templateDefaultVariables),
        )) {
            if (!defaults.has(key)) defaults.set(key, value)
        }
        return (this.defaultVariables = defaults)
    }
}

/** An access for the selected conversation, or null when the session does not own `chat`. */
export function createScriptConversationAccess(
    chat: Chat | null | undefined,
    database: Database,
    options: Pick<
        ScriptConversationAccessOptions,
        'onCommitted' | 'origin' | 'onVariablesCommitted'
    > = {},
): ScriptConversationAccess | null {
    const session = peekActiveConversationSession()
    if (!chat || !session?.isActive || session.materializeCompatibilityArray() !== chat.message) {
        return null
    }
    return new ScriptConversationAccess({
        session,
        chat,
        database,
        characterId: session.characterId,
        ...options,
    })
}
