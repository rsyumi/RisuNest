import { ChatRenderIdentityRegistry } from './chatRenderIdentity'
import { safeStructuredClone } from './polyfill'
import type {
    CapturedChatMessageTarget,
    CurrentChatMessageTarget,
} from './chatMessageUi'
import {
    type ActiveConversationMutationEvent,
    type ActiveConversationPin,
    type ActiveConversationSession,
} from './storage/activeConversationSession'
import type { Message } from './storage/database.svelte'
import type {
    ConversationWindow,
    ConversationWindowQuery,
    DataRevision,
    Versioned,
} from './storage/persistentDataStore'
import { validateConversationWindowQuery } from './storage/persistentDataStore'

declare const conversationViewportKeyBrand: unique symbol

export type ConversationViewportKey = string & {
    readonly [conversationViewportKeyBrand]: true
}

export interface ConversationViewportRow {
    readonly key: ConversationViewportKey
    readonly absoluteIndex: number
    readonly message: Readonly<Message>
    readonly sourceVersion: number
}

export interface ConversationViewportSnapshot {
    readonly sourceToken: string
    /** Render-input revision; display-owned variable writes do not advance it. */
    readonly version: number
    readonly storeRevision: DataRevision
    readonly totalMessages: number
    keyAt(absoluteIndex: number): ConversationViewportKey | undefined
    indexOfKey(key: ConversationViewportKey): number
    rowAt(absoluteIndex: number): ConversationViewportRow | undefined
}

export type ConversationViewportLoadReason = 'viewport' | 'jump' | 'streaming'
export type ConversationViewportPinReason =
    | 'viewport'
    | 'editor'
    | 'playing-media'
    | 'streaming'

export interface ConversationViewportRangeRequest {
    startIndex: number
    limit: number
    reason: ConversationViewportLoadReason
    signal?: AbortSignal
}

export interface ConversationViewportPin {
    release(): void
}

export interface ConversationViewportSource {
    snapshot(): ConversationViewportSnapshot
    ensureRange(input: ConversationViewportRangeRequest): Promise<void>
    acquireRangePin(
        startIndex: number,
        endIndex: number,
        reason: ConversationViewportPinReason,
    ): ConversationViewportPin
    subscribe(listener: () => void): () => void
    captureMessageTarget(key: ConversationViewportKey): CapturedChatMessageTarget | null
    dispose(): void
}

export interface SynchronousSessionConversationViewportSourceOptions {
    session: ActiveConversationSession
    captureCurrent(): CurrentChatMessageTarget | null
}

export interface PersistentConversationWindowReader {
    readConversationWindow(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationWindow> | null>
}

export interface PersistentConversationViewportSourceOptions {
    reader: PersistentConversationWindowReader
    characterId: string
    conversationId: string
    revision: DataRevision
    totalMessages: number
    rowBudget: number
}

let nextSourceToken = 0

function createSourceToken(): string {
    nextSourceToken += 1
    return `conversation-viewport-source-${nextSourceToken}`
}

export class SynchronousSessionConversationViewportSource
implements ConversationViewportSource {
    readonly sourceToken = createSourceToken()

    private readonly session: ActiveConversationSession
    private captureCurrent: (() => CurrentChatMessageTarget | null) | null
    private readonly identityRegistry = new ChatRenderIdentityRegistry()
    private readonly listeners = new Set<() => void>()
    private readonly pins = new Set<ActiveConversationPin>()
    private readonly unsubscribeSession: () => void
    private keys: ConversationViewportKey[]
    private keyIndices = new Map<ConversationViewportKey, number>()
    private rows = new Map<number, ConversationViewportRow>()
    private currentVersion: number
    private lastSessionVersion: number
    private nextInsertedKey = 0
    private disposed = false

    constructor(options: SynchronousSessionConversationViewportSourceOptions) {
        this.session = options.session
        this.captureCurrent = options.captureCurrent
        this.currentVersion = this.session.version
        this.lastSessionVersion = this.session.version
        this.keys = this.createInitialKeys()
        this.rebuildKeyIndices()
        this.unsubscribeSession = this.session.subscribe((event) => {
            this.handleSessionChange(event)
        })
    }

    snapshot(): ConversationViewportSnapshot {
        const keys = this.keys
        const keyIndices = this.keyIndices
        const rows = this.rows
        return {
            sourceToken: this.sourceToken,
            version: this.currentVersion,
            storeRevision: this.session.storeRevision,
            totalMessages: keys.length,
            keyAt: (absoluteIndex) => keys[absoluteIndex],
            indexOfKey: (key) => keyIndices.get(key) ?? -1,
            rowAt: (absoluteIndex) => rows.get(absoluteIndex),
        }
    }

    async ensureRange(input: ConversationViewportRangeRequest): Promise<void> {
        this.assertUsable()
        if (input.signal?.aborted) return
        const sourceVersion = this.currentVersion
        const sessionVersion = this.session.version
        const window = this.session.readRange(input.startIndex, input.limit)
        void input.reason

        await Promise.resolve()
        if (
            input.signal?.aborted ||
            this.disposed ||
            sourceVersion !== this.currentVersion ||
            window.sessionVersion !== sessionVersion ||
            !this.session.isActive
        ) return

        const nextRows = new Map(this.rows)
        for (let offset = 0; offset < window.messages.length; offset++) {
            const absoluteIndex = window.startIndex + offset
            const key = this.keys[absoluteIndex]
            if (key === undefined) continue
            nextRows.set(absoluteIndex, {
                key,
                absoluteIndex,
                message: window.messages[offset],
                sourceVersion,
            })
        }
        this.rows = nextRows
        this.notifyListeners()
    }

    acquireRangePin(
        startIndex: number,
        endIndex: number,
        reason: ConversationViewportPinReason,
    ): ConversationViewportPin {
        this.assertUsable()
        const sessionPin = this.session.acquireRangePin(startIndex, endIndex, reason)
        this.pins.add(sessionPin)
        let released = false
        return {
            release: () => {
                if (released) return
                released = true
                this.pins.delete(sessionPin)
                sessionPin.release()
            },
        }
    }

    subscribe(listener: () => void): () => void {
        this.assertUsable()
        this.listeners.add(listener)
        let subscribed = true
        return () => {
            if (!subscribed) return
            subscribed = false
            this.listeners.delete(listener)
        }
    }

    captureMessageTarget(key: ConversationViewportKey): CapturedChatMessageTarget | null {
        if (this.disposed || !this.session.isActive) return null
        const absoluteIndex = this.keyIndices.get(key)
        if (absoluteIndex === undefined) return null
        const version = this.currentVersion
        const current = this.captureCurrent?.() ?? null
        if (!current || !this.session.matchesConversation(current.character.chaId, current.conversation)) {
            return null
        }
        try {
            const locator = this.session.locate(absoluteIndex)
            const message = this.session.readMessage(locator)
            if (
                version !== this.currentVersion ||
                this.keys[absoluteIndex] !== key ||
                !this.session.isActive
            ) return null
            return {
                kind: 'session',
                absoluteIndex,
                ...current,
                message,
                session: this.session,
                locator,
            }
        } catch {
            return null
        }
    }

    dispose(): void {
        if (this.disposed) return
        this.disposed = true
        this.unsubscribeSession()
        for (const pin of [...this.pins]) pin.release()
        this.pins.clear()
        this.keys = []
        this.keyIndices = new Map()
        this.rows = new Map()
        this.captureCurrent = null
        this.identityRegistry.clearRegistration()
        this.notifyListeners()
        this.listeners.clear()
    }

    private handleSessionChange(event: ActiveConversationMutationEvent | null): void {
        if (!event || !this.session.isActive) {
            this.dispose()
            return
        }
        // Lua display listeners may store per-row scratch variables. Restarting
        // all rows for those writes creates a render -> write -> abort loop.
        // Keep render rows valid, while session locators and persistence advance.
        if (
            event.displayVariableUpdate &&
            event.previousVersion === this.lastSessionVersion
        ) {
            this.lastSessionVersion = event.sessionVersion
            return
        }
        const previousKeys = this.keys
        this.reconcileKeys(event)
        this.lastSessionVersion = this.session.version
        this.currentVersion = this.session.version
        this.rows = new Map()
        if (this.keys !== previousKeys) this.rebuildKeyIndices()
        this.notifyListeners()
    }

    private createInitialKeys(): ConversationViewportKey[] {
        return this.identityRegistry
            .register(this.sourceToken, this.session.materializeCompatibilityArray())
            .toArray() as ConversationViewportKey[]
    }

    private reconcileKeys(event: ActiveConversationMutationEvent): void {
        if (event.previousVersion !== this.lastSessionVersion) {
            this.keys = this.createInitialKeys()
            return
        }

        // Streaming replaces text without moving rows. Keep the immutable key
        // array and index map instead of copying/reindexing the entire history.
        if (
            this.keys.length === this.session.totalMessages &&
            event.mutations.every(
                (mutation) =>
                    mutation.deleteCount === mutation.messages.length &&
                    mutation.start >= 0 &&
                    mutation.start + mutation.deleteCount <= this.keys.length,
            )
        )
            return

        const nextKeys = [...this.keys]
        for (const mutation of event.mutations) {
            if (
                mutation.start > nextKeys.length ||
                mutation.deleteCount > nextKeys.length - mutation.start
            ) {
                this.keys = this.createInitialKeys()
                return
            }
            const retainedCount = Math.min(mutation.deleteCount, mutation.messages.length)
            const replacements = nextKeys.slice(
                mutation.start,
                mutation.start + retainedCount,
            )
            while (replacements.length < mutation.messages.length) {
                replacements.push(this.createInsertedKey())
            }
            nextKeys.splice(mutation.start, mutation.deleteCount, ...replacements)
        }
        this.keys = nextKeys.length === this.session.totalMessages
            ? nextKeys
            : this.createInitialKeys()
    }

    private rebuildKeyIndices(): void {
        this.keyIndices = new Map(this.keys.map((key, index) => [key, index]))
    }

    private createInsertedKey(): ConversationViewportKey {
        this.nextInsertedKey += 1
        return `${this.sourceToken}|inserted:${this.nextInsertedKey}` as ConversationViewportKey
    }

    private notifyListeners(): void {
        for (const listener of [...this.listeners]) {
            try {
                listener()
            } catch (error) {
                console.error('Conversation viewport subscriber failed', error)
            }
        }
    }

    private assertUsable(): void {
        if (this.disposed || !this.session.isActive) {
            throw new Error('Conversation viewport source is disposed')
        }
    }
}

interface PersistentCachedRow {
    row: ConversationViewportRow
    lastUsed: number
}

interface PersistentRangePinState {
    startIndex: number
    endIndex: number
    reason: ConversationViewportPinReason
}

export class PersistentConversationViewportSource
implements ConversationViewportSource {
    readonly sourceToken = createSourceToken()

    private readonly reader: PersistentConversationWindowReader
    private readonly characterId: string
    private readonly conversationId: string
    private readonly rowBudget: number
    private readonly listeners = new Set<() => void>()
    private readonly pins = new Map<number, PersistentRangePinState>()
    private rows = new Map<number, PersistentCachedRow>()
    private optimisticRows = new Set<number>()
    private currentRevision: DataRevision
    private persistedTotalMessages: number
    private totalMessages: number
    private epoch = 0
    private accessClock = 0
    private nextPinId = 0
    private disposed = false

    constructor(options: PersistentConversationViewportSourceOptions) {
        if (typeof options.characterId !== 'string' || options.characterId.length === 0) {
            throw new RangeError('Character ID must be a nonempty string')
        }
        if (typeof options.conversationId !== 'string' || options.conversationId.length === 0) {
            throw new RangeError('Conversation ID must be a nonempty string')
        }
        this.validateRevision(options.revision)
        this.validateMessageCount(options.totalMessages)
        if (!Number.isSafeInteger(options.rowBudget) || options.rowBudget <= 0) {
            throw new RangeError('Conversation viewport row budget must be a positive safe integer')
        }
        this.reader = options.reader
        this.characterId = options.characterId
        this.conversationId = options.conversationId
        this.currentRevision = options.revision
        this.persistedTotalMessages = options.totalMessages
        this.totalMessages = options.totalMessages
        this.rowBudget = options.rowBudget
    }

    snapshot(): ConversationViewportSnapshot {
        const epoch = this.epoch
        const totalMessages = this.disposed ? 0 : this.totalMessages
        return {
            sourceToken: this.sourceToken,
            version: epoch,
            storeRevision: this.currentRevision,
            totalMessages,
            keyAt: (absoluteIndex) => this.keyAt(epoch, totalMessages, absoluteIndex),
            indexOfKey: (key) => this.indexOfKey(epoch, totalMessages, key),
            rowAt: (absoluteIndex) => this.rowAt(epoch, absoluteIndex),
        }
    }

    async ensureRange(input: ConversationViewportRangeRequest): Promise<void> {
        this.assertUsable()
        if (input.signal?.aborted) return
        const query = {
            characterId: this.characterId,
            conversationId: this.conversationId,
            startIndex: input.startIndex,
            limit: input.limit,
        }
        validateConversationWindowQuery(query)
        const epoch = this.epoch
        const revision = this.currentRevision
        const result = await this.reader.readConversationWindow(query)
        if (
            input.signal?.aborted ||
            this.disposed ||
            epoch !== this.epoch ||
            revision !== this.currentRevision
        ) return
        if (!result) throw new Error(`Conversation ${this.conversationId} was not found`)
        if (result.revision !== revision) return
        const window = result.value
        const expectedStartIndex = Math.min(this.persistedTotalMessages, input.startIndex)
        const expectedEndIndex = Math.min(
            this.persistedTotalMessages,
            expectedStartIndex + input.limit,
        )
        if (
            window.characterId !== this.characterId ||
            window.conversationId !== this.conversationId ||
            window.startIndex !== expectedStartIndex ||
            window.endIndex !== expectedEndIndex ||
            window.totalMessages !== this.persistedTotalMessages ||
            window.messages.length !== expectedEndIndex - expectedStartIndex
        ) {
            throw new Error(`Conversation ${this.conversationId} returned a mismatched window`)
        }
        for (let offset = 0; offset < window.messages.length; offset++) {
            const absoluteIndex = window.startIndex + offset
            if (this.optimisticRows.has(absoluteIndex)) continue
            const key = this.keyAt(epoch, this.totalMessages, absoluteIndex)
            if (key === undefined) continue
            this.rows.set(absoluteIndex, {
                row: {
                    key,
                    absoluteIndex,
                    message: window.messages[offset],
                    sourceVersion: epoch,
                },
                lastUsed: ++this.accessClock,
            })
        }
        this.evictUnpinnedRows()
        this.notifyListeners()
    }

    applyOptimisticRange(
        startIndex: number,
        deleteCount: number,
        messages: readonly Message[],
    ): () => void {
        this.assertUsable()
        if (
            !Number.isSafeInteger(startIndex)
            || startIndex < 0
            || !Number.isSafeInteger(deleteCount)
            || deleteCount < 0
            || startIndex + deleteCount > this.totalMessages
            || (messages.length !== deleteCount
                && !(startIndex === this.totalMessages && deleteCount === 0))
        ) throw new RangeError('Optimistic conversation range is unsupported')
        const previousRows = new Map(this.rows)
        const previousOptimisticRows = new Set(this.optimisticRows)
        const previousTotalMessages = this.totalMessages
        this.totalMessages = this.totalMessages - deleteCount + messages.length
        for (let offset = 0; offset < Math.max(deleteCount, messages.length); offset += 1) {
            const absoluteIndex = startIndex + offset
            if (offset >= messages.length) {
                this.rows.delete(absoluteIndex)
                this.optimisticRows.delete(absoluteIndex)
                continue
            }
            const key = this.keyAt(this.epoch, this.totalMessages, absoluteIndex)
            if (key === undefined) continue
            this.rows.set(absoluteIndex, {
                row: {
                    key,
                    absoluteIndex,
                    message: safeStructuredClone(messages[offset]),
                    sourceVersion: this.epoch,
                },
                lastUsed: ++this.accessClock,
            })
            this.optimisticRows.add(absoluteIndex)
        }
        this.evictUnpinnedRows()
        this.notifyListeners()
        let restored = false
        return () => {
            if (restored || this.disposed) return
            restored = true
            this.rows = previousRows
            this.optimisticRows = previousOptimisticRows
            this.totalMessages = previousTotalMessages
            this.notifyListeners()
        }
    }

    advanceRevision(revision: DataRevision, totalMessages: number): void {
        this.assertUsable()
        this.validateRevision(revision)
        this.validateMessageCount(totalMessages)
        if (revision <= this.currentRevision) {
            throw new RangeError('Persistent conversation revision must advance')
        }
        this.currentRevision = revision
        this.persistedTotalMessages = totalMessages
        this.totalMessages = totalMessages
        this.epoch += 1
        this.pins.clear()
        this.rows = new Map()
        this.optimisticRows = new Set()
        this.notifyListeners()
    }

    acquireRangePin(
        startIndex: number,
        endIndex: number,
        reason: ConversationViewportPinReason,
    ): ConversationViewportPin {
        this.assertUsable()
        this.validatePinRange(startIndex, endIndex)
        const pinId = ++this.nextPinId
        this.pins.set(pinId, { startIndex, endIndex, reason })
        let released = false
        return {
            release: () => {
                if (released) return
                released = true
                this.pins.delete(pinId)
                if (!this.disposed && this.evictUnpinnedRows()) {
                    this.notifyListeners()
                }
            },
        }
    }

    subscribe(listener: () => void): () => void {
        this.assertUsable()
        this.listeners.add(listener)
        let subscribed = true
        return () => {
            if (!subscribed) return
            subscribed = false
            this.listeners.delete(listener)
        }
    }

    captureMessageTarget(_key: ConversationViewportKey): CapturedChatMessageTarget | null {
        return null
    }

    dispose(): void {
        if (this.disposed) return
        this.disposed = true
        this.pins.clear()
        this.rows.clear()
        this.notifyListeners()
        this.listeners.clear()
    }

    private keyAt(
        epoch: number,
        totalMessages: number,
        absoluteIndex: number,
    ): ConversationViewportKey | undefined {
        if (
            this.disposed ||
            epoch !== this.epoch ||
            !Number.isSafeInteger(absoluteIndex) ||
            absoluteIndex < 0 ||
            absoluteIndex >= totalMessages
        ) return undefined
        return `${this.sourceToken}|${epoch}|${absoluteIndex}` as ConversationViewportKey
    }

    private indexOfKey(
        epoch: number,
        totalMessages: number,
        key: ConversationViewportKey,
    ): number {
        if (this.disposed || epoch !== this.epoch || typeof key !== 'string') return -1
        const prefix = `${this.sourceToken}|${epoch}|`
        if (!key.startsWith(prefix)) return -1
        const encodedIndex = key.slice(prefix.length)
        if (!/^(?:0|[1-9]\d*)$/.test(encodedIndex)) return -1
        const absoluteIndex = Number(encodedIndex)
        if (!Number.isSafeInteger(absoluteIndex) || absoluteIndex >= totalMessages) return -1
        return absoluteIndex
    }

    private rowAt(epoch: number, absoluteIndex: number): ConversationViewportRow | undefined {
        if (this.disposed || epoch !== this.epoch) return undefined
        const cached = this.rows.get(absoluteIndex)
        if (!cached) return undefined
        cached.lastUsed = ++this.accessClock
        return cached.row
    }

    private notifyListeners(): void {
        for (const listener of [...this.listeners]) {
            try {
                listener()
            } catch (error) {
                console.error('Conversation viewport subscriber failed', error)
            }
        }
    }

    private evictUnpinnedRows(): boolean {
        let evicted = false
        while (this.rows.size > this.rowBudget) {
            let oldestIndex: number | undefined
            let oldestAccess = Number.POSITIVE_INFINITY
            for (const [absoluteIndex, cached] of this.rows) {
                if (this.isPinned(absoluteIndex) || cached.lastUsed >= oldestAccess) continue
                oldestIndex = absoluteIndex
                oldestAccess = cached.lastUsed
            }
            if (oldestIndex === undefined) return evicted
            this.rows.delete(oldestIndex)
            evicted = true
        }
        return evicted
    }

    private isPinned(absoluteIndex: number): boolean {
        for (const pin of this.pins.values()) {
            if (absoluteIndex >= pin.startIndex && absoluteIndex < pin.endIndex) return true
        }
        return false
    }

    private validatePinRange(startIndex: number, endIndex: number): void {
        if (!Number.isSafeInteger(startIndex) || startIndex < 0) {
            throw new RangeError('Conversation pin startIndex must be a nonnegative safe integer')
        }
        if (!Number.isSafeInteger(endIndex) || endIndex < 0) {
            throw new RangeError('Conversation pin endIndex must be a nonnegative safe integer')
        }
        if (endIndex <= startIndex) {
            throw new RangeError('Conversation pin range must not be empty')
        }
        if (endIndex > this.totalMessages) {
            throw new RangeError('Conversation pin range exceeds the current message count')
        }
    }

    private validateRevision(revision: DataRevision): void {
        if (!Number.isSafeInteger(revision) || revision < 0) {
            throw new RangeError('Persistent conversation revision must be a nonnegative safe integer')
        }
    }

    private validateMessageCount(totalMessages: number): void {
        if (!Number.isSafeInteger(totalMessages) || totalMessages < 0) {
            throw new RangeError('Persistent conversation message count must be a nonnegative safe integer')
        }
    }

    private assertUsable(): void {
        if (this.disposed) throw new Error('Conversation viewport source is disposed')
    }
}
