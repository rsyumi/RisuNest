import { projectConversationWindows } from './projectedConversationWindowReader'
import { ChatRenderIdentityRegistry } from './chatRenderIdentity'
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
    ConversationMessageMetadataWindow,
    DataRevision,
    Versioned,
} from './storage/persistentDataStore'
import { CONVERSATION_RANGE_MAX_LIMIT, validateConversationWindowQuery } from './storage/persistentDataStore'
import { replaceArrayRange } from './arrayRange'
import { yieldToMainThread } from './ui/yieldToUi'
import type { WindowedConversationOverlay } from './storage/windowedConversationOverlay'
import type { CbsHistoryChange } from './cbsHistoryAccess'

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
    readonly historyVersion?: number
    historyChangesSince?(version: number): readonly CbsHistoryChange[] | null
    readonly sourceToken: string
    /** Render-input revision; display-owned variable writes do not advance it. */
    readonly version: number
    readonly storeRevision: DataRevision
    readonly totalMessages: number
    keyAt(absoluteIndex: number): ConversationViewportKey | undefined
    positionKeyAt?(absoluteIndex: number): string | undefined
    indexOfKey(key: ConversationViewportKey): number
    rowAt(absoluteIndex: number): ConversationViewportRow | undefined
}

export type ConversationViewportLoadReason = 'viewport' | 'jump' | 'streaming'
export type ConversationViewportPinReason =
    | 'viewport'
    | 'editor'
    | 'playing-media'
    | 'streaming'

/** Stored rows a commit replaced one for one, from `start`. */
export interface ConversationRowsRewrite {
    start: number
    messages: readonly Message[]
}

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
    projectedLocalRow?(index: number): Readonly<Message> | undefined
    captureProjectedReader?(reader: PersistentConversationWindowReader): PersistentConversationWindowReader
    captureResidentRows?(): readonly ConversationViewportRow[]
    ensureRange(input: ConversationViewportRangeRequest): Promise<void>
    findLatestMessagePair?(signal: AbortSignal): Promise<number[]>
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
    readConversationMessageMetadataWindow?(
        input: ConversationWindowQuery,
    ): Promise<Versioned<ConversationMessageMetadataWindow> | null>
}

export interface PersistentConversationViewportSourceOptions {
    overlay?: WindowedConversationOverlay
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

async function scanLatestMessagePair(
    totalMessages: number,
    readPage: (start: number, limit: number, found: ReadonlyMap<Message['role'], number>) => Promise<readonly (Readonly<Message> | undefined)[]>,
    signal: AbortSignal,
): Promise<number[]> {
    const found = new Map<Message['role'], number>()
    for (let end = totalMessages; end > 0 && found.size < 2; end = Math.max(0, end - 64)) {
        signal.throwIfAborted()
        const start = Math.max(0, end - 64)
        const messages = await readPage(start, end - start, found)
        signal.throwIfAborted()
        for (let offset = messages.length - 1; offset >= 0; offset--) {
            const message = messages[offset]
            if (message && !message.disabled && !message.isComment &&
                (message.role === 'user' || message.role === 'char') && !found.has(message.role)) {
                found.set(message.role, start + offset)
            }
        }
        if (found.size < 2 && start > 0) await yieldToMainThread()
    }
    return [...found.values()].sort((left, right) => left - right)
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
    private positionVersions = new Map<ConversationViewportKey, number>()
    private positionEpoch = 0
    private rows = new Map<number, ConversationViewportRow>()
    private currentVersion: number
    private lastSessionVersion: number
    private nextInsertedKey = 0
    private disposed = false
    private latestPair: Promise<number[]> | null = null

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
        const positionVersions = this.positionVersions
        const positionEpoch = this.positionEpoch
        const rows = this.rows
        return {
            sourceToken: this.sourceToken,
            version: this.currentVersion,
            storeRevision: this.session.storeRevision,
            totalMessages: keys.length,
            keyAt: (absoluteIndex) => keys[absoluteIndex],
            positionKeyAt: (index) => {
                const key = keys[index]
                return key === undefined ? undefined : JSON.stringify([key, positionEpoch, positionVersions.get(key) ?? 0])
            },
            indexOfKey: (key) => keyIndices.get(key) ?? -1,
            rowAt: (absoluteIndex) => rows.get(absoluteIndex),
        }
    }

    captureResidentRows(): readonly ConversationViewportRow[] {
        return [...this.rows.values()]
    }

    findLatestMessagePair(signal: AbortSignal): Promise<number[]> {
        this.assertUsable()
        if (this.latestPair) return this.latestPair
        const lookup = scanLatestMessagePair(
            this.keys.length,
            async (start, limit) => this.session.readRange(start, limit).messages,
            signal,
        ).then((indices) => {
            if (this.disposed || this.latestPair !== lookup) throw new DOMException('Conversation changed during pair lookup', 'AbortError')
            return indices
        }).catch((error) => { if (this.latestPair === lookup) this.latestPair = null; throw error })
        return this.latestPair = lookup
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
        // Other variable commits leave messages unchanged too; the rows that read
        // a changed variable are refreshed by the script display refresh instead.
        if (
            (event.displayVariableUpdate || event.variableUpdate) &&
            event.previousVersion === this.lastSessionVersion
        ) {
            this.lastSessionVersion = event.sessionVersion
            return
        }
        this.latestPair = null
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
        if (event.previousVersion !== this.lastSessionVersion || event.commands.includes('replace-conversation')) {
            this.resetKeyProofs()
            return
        }

        let versions: Map<ConversationViewportKey, number> | undefined
        // A splice may reuse a render key for a different legacy message. Retire only
        // the affected proofs; retained snapshots keep the pre-splice map.
        const updateProofs = (keys: readonly ConversationViewportKey[], mutation: ActiveConversationMutationEvent['mutations'][number]) => {
            const retained = Math.min(mutation.deleteCount, mutation.messages.length)
            const ambiguous = retained > 0 && (mutation.deleteCount !== 1 || mutation.messages.length !== 1)
            if (ambiguous) {
                versions ??= new Map(this.positionVersions)
                for (let i = 0; i < retained; i++) versions.set(keys[mutation.start + i], event.sessionVersion)
            }
            for (let i = retained; i < mutation.deleteCount && (versions ?? this.positionVersions).size > 0; i++) {
                versions ??= new Map(this.positionVersions)
                versions.delete(keys[mutation.start + i])
            }
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
        ) {
            for (const mutation of event.mutations) updateProofs(this.keys, mutation)
            if (versions) this.positionVersions = versions
            return
        }

        const nextKeys = [...this.keys]
        for (const mutation of event.mutations) {
            if (
                mutation.start > nextKeys.length ||
                mutation.deleteCount > nextKeys.length - mutation.start
            ) {
                this.resetKeyProofs()
                return
            }
            updateProofs(nextKeys, mutation)
            const retainedCount = Math.min(mutation.deleteCount, mutation.messages.length)
            const replacements = nextKeys.slice(
                mutation.start,
                mutation.start + retainedCount,
            )
            while (replacements.length < mutation.messages.length) {
                replacements.push(this.createInsertedKey())
            }
            replaceArrayRange(nextKeys, mutation.start, mutation.deleteCount, replacements)
        }
        if (nextKeys.length === this.session.totalMessages) {
            this.keys = nextKeys
            if (versions) this.positionVersions = versions
        } else this.resetKeyProofs()
    }

    private resetKeyProofs(): void {
        this.keys = this.createInitialKeys()
        this.positionVersions = new Map()
        this.positionEpoch++
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

interface ProjectionChange {
    start: number
    end: number
    structureChanged: boolean
    pairChanged?: boolean
    previousCount?: number
    nextCount?: number
}

interface PositionProofBoundary { start: number; epoch: number }

function positionProofEpoch(boundaries: readonly PositionProofBoundary[], index: number): number {
    let low = 0
    let high = boundaries.length
    while (low < high) {
        const middle = Math.floor((low + high) / 2)
        if (boundaries[middle].start <= index) low = middle + 1
        else high = middle
    }
    return boundaries[low - 1]?.epoch ?? 0
}

interface PersistentRangePinState {
    startIndex: number
    endIndex: number
    reason: ConversationViewportPinReason
}

export class PersistentConversationViewportSource
implements ConversationViewportSource {
    readonly sourceToken = createSourceToken()

    private readonly overlay?: WindowedConversationOverlay
    private readonly reader: PersistentConversationWindowReader
    private readonly characterId: string
    private readonly conversationId: string
    private readonly rowBudget: number
    private readonly listeners = new Set<() => void>()
    private readonly pins = new Map<number, PersistentRangePinState>()
    private rows = new Map<number, PersistentCachedRow>()
    private historyVersion = 0
    private readonly historyChanges: (CbsHistoryChange & { version: number })[] = []
    private currentRevision: DataRevision
    private persistedTotalMessages: number
    private totalMessages: number
    private epoch = 0
    private positionProofs: PositionProofBoundary[] = []
    private accessClock = 0
    private nextPinId = 0
    private disposed = false
    private latestPair: Promise<number[]> | null = null

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
        this.overlay = options.overlay
        this.reader = options.reader
        this.characterId = options.characterId
        this.conversationId = options.conversationId
        this.currentRevision = options.revision
        this.persistedTotalMessages = options.totalMessages
        this.totalMessages = options.overlay?.count ?? options.totalMessages
        this.rowBudget = options.rowBudget
    }

    snapshot(): ConversationViewportSnapshot {
        const epoch = this.epoch
        const proofs = this.positionProofs
        const historyVersion = this.historyVersion
        const totalMessages = this.disposed ? 0 : this.totalMessages
        return {
            sourceToken: this.sourceToken,
            version: epoch,
            historyVersion,
            historyChangesSince: (version) => {
                if (version < this.historyVersion - this.historyChanges.length) return null
                return this.historyChanges.filter((change) => change.version > version && change.version <= historyVersion)
            },
            storeRevision: this.currentRevision,
            totalMessages,
            keyAt: (absoluteIndex) => this.keyAt(epoch, totalMessages, absoluteIndex),
            positionKeyAt: index => Number.isSafeInteger(index) && index >= 0 && index < totalMessages
                ? JSON.stringify([this.sourceToken, positionProofEpoch(proofs, index), index]) : undefined,
            indexOfKey: (key) => this.indexOfKey(epoch, totalMessages, key),
            rowAt: (absoluteIndex) => this.rowAt(epoch, absoluteIndex),
        }
    }

    captureResidentRows(): readonly ConversationViewportRow[] {
        this.assertUsable()
        return [...this.rows.values()].map(({ row }) => row)
    }

    projectedLocalRow(index: number): Readonly<Message> | undefined {
        const row = this.overlay?.row(index)
        return row && 'message' in row ? row.message : undefined
    }

    captureProjectedReader(reader: PersistentConversationWindowReader, sameRevision?: (revision: number) => boolean): PersistentConversationWindowReader {
        const projection = this.overlay?.captureProjection()
        if (!projection) return reader
        return projectConversationWindows(reader, projection, this.characterId, this.conversationId, this.persistedTotalMessages, sameRevision)
    }

    async readProjectedRange(startIndex: number, limit: number): Promise<Message[]> {
        this.assertUsable()
        if (!Number.isSafeInteger(startIndex) || startIndex < 0 || !Number.isSafeInteger(limit) || limit < 0)
            throw new RangeError('Invalid projected conversation range')
        const historyVersion = this.historyVersion
        const revision = this.currentRevision
        const reader = this.captureProjectedReader(this.reader)
        const messages: Message[] = []
        const end = Math.min(this.totalMessages, startIndex + limit)
        for (let cursor = startIndex; cursor < end; cursor += CONVERSATION_RANGE_MAX_LIMIT) {
            const result = await reader.readConversationWindow({
                characterId: this.characterId, conversationId: this.conversationId,
                startIndex: cursor, limit: Math.min(end - cursor, CONVERSATION_RANGE_MAX_LIMIT),
            })
            if (!result) throw new Error(`Conversation ${this.conversationId} was not found`)
            if (result.revision !== revision || this.currentRevision !== revision || this.historyVersion !== historyVersion || this.disposed)
                throw new DOMException('Conversation changed during window read', 'AbortError')
            messages.push(...result.value.messages)
        }
        return messages
    }

    refreshProjection(change: ProjectionChange): void {
        this.refreshProjections([change])
    }

    refreshProjections(changes: readonly ProjectionChange[]): void {
        this.assertUsable()
        if (!changes.length) return
        const overlay = this.overlay
        if (!overlay) throw new Error('Conversation overlay is missing')
        let previousCount = this.totalMessages
        this.totalMessages = overlay.count
        if (changes.some(change => change.structureChanged || change.pairChanged !== false)) this.latestPair = null
        if (changes.some(change => change.structureChanged)) this.resetPositions()
        for (const change of changes) {
            const before = change.previousCount ?? previousCount
            const after = change.nextCount ?? this.totalMessages
            if (change.structureChanged && change.start < before) {
                // A known append leaves all earlier slots proven. A middle splice
                // can displace the suffix; a reorder affects only its own range.
                this.invalidatePositionProofs(change.start, before === after ? change.end : undefined)
            }
            for (const interval of overlay.intervals(change.start, Math.max(0, Math.min(change.end, this.totalMessages) - change.start))) {
                if ('messages' in interval) {
                    interval.messages.forEach((message, offset) => this.cacheRow(interval.start + offset, message))
                } else {
                    for (let index = interval.start; index < interval.start + interval.length; index++) this.rows.delete(index)
                }
            }
            this.recordHistoryChange(change.start, before !== after ? Math.max(before, after) : change.end, before !== after)
            previousCount = after
        }
        this.evictUnpinnedRows()
        this.notifyListeners()
    }

    private invalidatePositionProofs(start: number, end?: number): void {
        const previous = this.positionProofs
        const next = previous.filter(boundary => boundary.start < start)
        next.push({ start, epoch: this.epoch })
        if (end !== undefined) {
            next.push({ start: end, epoch: positionProofEpoch(previous, end) })
            next.push(...previous.filter(boundary => boundary.start > end))
        }
        this.positionProofs = next
    }

    installProjection(revision: DataRevision, storedCount: number, structureChanged: boolean, rewritten: readonly ConversationRowsRewrite[] = []): void {
        this.assertUsable()
        this.validateRevision(revision)
        this.validateMessageCount(storedCount)
        if (revision < this.currentRevision) throw new RangeError('Persistent conversation revision moved backwards')
        const previousCount = this.totalMessages
        const previousStoredCount = this.persistedTotalMessages
        this.currentRevision = revision
        this.persistedTotalMessages = storedCount
        this.totalMessages = this.overlay?.count ?? storedCount
        if (!structureChanged && previousCount === this.totalMessages && previousStoredCount === storedCount && !rewritten.length) return
        this.latestPair = null
        if (structureChanged) {
            this.resetPositions()
            this.invalidatePositionProofs(0)
            this.recordHistoryChange(0, Math.max(previousCount, this.totalMessages), previousCount !== this.totalMessages)
        } else for (const { start, messages } of rewritten) {
            messages.forEach((message, offset) => {
                const index = this.overlay ? this.overlay.storedToVisible(start + offset) : start + offset
                if (index === null) return
                const dirty = this.overlay?.row(index)
                if (this.rows.has(index)) this.cacheRow(index, dirty && 'message' in dirty ? dirty.message : message)
                this.recordHistoryChange(index, index + 1, false)
            })
        }
        this.notifyListeners()
    }

    private resetPositions(): void {
        this.epoch++
        this.pins.clear()
        this.rows.clear()
    }

    private cacheRow(index: number, message: Message): void {
        const key = this.keyAt(this.epoch, this.totalMessages, index)
        if (key) this.rows.set(index, {
            row: { key, absoluteIndex: index, message, sourceVersion: this.epoch },
            lastUsed: ++this.accessClock,
        })
    }

    findLatestMessagePair(signal: AbortSignal): Promise<number[]> {
        this.assertUsable()
        if (this.latestPair) return this.latestPair
        const resident = new Map<Message['role'], number>()
        let index = this.totalMessages - 1
        for (; index >= 0 && resident.size < 2; index--) {
            const message = this.rows.get(index)?.row.message
            if (!message) break
            if (!message.disabled && !message.isComment && (message.role === 'char' || message.role === 'user') && !resident.has(message.role)) {
                resident.set(message.role, index)
            }
        }
        if (resident.size === 2 || index < 0) {
            const lookup = Promise.resolve().then(() => {
                signal.throwIfAborted()
                if (this.disposed || this.latestPair !== lookup) throw new DOMException('Conversation changed during pair lookup', 'AbortError')
                return [...resident.values()].sort((left, right) => left - right)
            }).catch(error => {
                if (this.latestPair === lookup) this.latestPair = null
                throw error
            })
            return this.latestPair = lookup
        }
        const revision = this.currentRevision
        const epoch = this.epoch
        // A later revision that stores a different count also changes the windows this lookup checks.
        const expectedTotal = this.overlay ? this.totalMessages : this.persistedTotalMessages
        const persistedTotalMessages = this.persistedTotalMessages
        const sameConversationRevision = (value: number | undefined) => !this.disposed && epoch === this.epoch &&
            persistedTotalMessages === this.persistedTotalMessages &&
            value !== undefined && value >= revision && value <= this.currentRevision
        const reader = this.captureProjectedReader(this.reader, sameConversationRevision)
        const lookup = scanLatestMessagePair(this.totalMessages, async (start, limit, found) => {
            const query = { characterId: this.characterId, conversationId: this.conversationId, startIndex: start, limit }
            if (reader.readConversationMessageMetadataWindow) {
                const metadata = await reader.readConversationMessageMetadataWindow(query)
                if (!sameConversationRevision(metadata?.revision) || !metadata) {
                    throw new DOMException('Conversation changed during pair lookup', 'AbortError')
                }
                if (metadata.value.characterId !== this.characterId || metadata.value.conversationId !== this.conversationId ||
                    metadata.value.startIndex !== Math.min(start, expectedTotal) ||
                    metadata.value.endIndex !== Math.min(start + limit, expectedTotal) ||
                    metadata.value.messages.length !== metadata.value.endIndex - metadata.value.startIndex ||
                    metadata.value.totalMessages !== expectedTotal) {
                    throw new Error('Conversation pair lookup returned mismatched metadata')
                }
                const messages: (Readonly<Message> | undefined)[] = new Array(limit)
                const roles = new Set(found.keys())
                for (let offset = limit - 1; offset >= 0 && roles.size < 2; offset--) {
                    signal.throwIfAborted()
                    const index = start + offset
                    const summary = metadata.value.messages[offset]
                    if (!summary || summary.disabled || (summary.role !== 'char' && summary.role !== 'user') || roles.has(summary.role)) continue
                    let message = this.rows.get(index)?.row.message
                    if (!message) {
                        const body = await reader.readConversationWindow({ ...query, startIndex: index, limit: 1 })
                        if (!sameConversationRevision(body?.revision) || !body || body.value.startIndex !== index ||
                            body.value.endIndex !== index + 1 || body.value.messages.length !== 1 ||
                            body.value.characterId !== this.characterId || body.value.conversationId !== this.conversationId ||
                            body.value.totalMessages !== expectedTotal) {
                            throw new DOMException('Conversation changed during pair lookup', 'AbortError')
                        }
                        message = body.value.messages[0]
                    }
                    if (message && !message.isComment && !message.disabled && message.role === summary.role) {
                        messages[offset] = message
                        roles.add(message.role)
                    }
                }
                return messages
            }
            const result = await reader.readConversationWindow(query)
            if (!sameConversationRevision(result?.revision) || !result) {
                throw new DOMException('Conversation changed during pair lookup', 'AbortError')
            }
            if (result.value.characterId !== this.characterId || result.value.conversationId !== this.conversationId ||
                result.value.startIndex !== Math.min(start, expectedTotal) ||
                result.value.endIndex !== Math.min(start + limit, expectedTotal) ||
                result.value.messages.length !== result.value.endIndex - result.value.startIndex ||
                result.value.totalMessages !== expectedTotal) {
                throw new Error('Conversation pair lookup returned a mismatched window')
            }
            return result.value.messages
        }, signal).then((indices) => {
            if (this.disposed || this.latestPair !== lookup) throw new DOMException('Conversation changed during pair lookup', 'AbortError')
            return indices
        }).catch((error) => { if (this.latestPair === lookup) this.latestPair = null; throw error })
        return this.latestPair = lookup
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
        const historyVersion = this.historyVersion
        const expectedTotal = this.overlay ? this.totalMessages : this.persistedTotalMessages
        let result: Versioned<ConversationWindow> | null
        try {
            result = await this.captureProjectedReader(this.reader).readConversationWindow(query)
        } catch (error) {
            if (error instanceof DOMException && error.name === 'AbortError') return
            throw error
        }
        if (
            input.signal?.aborted ||
            this.disposed ||
            epoch !== this.epoch ||
            (historyVersion !== this.historyVersion && this.rangeChangedSince(historyVersion, input.startIndex, input.limit)) ||
            revision !== this.currentRevision
        ) return
        if (!result) throw new Error(`Conversation ${this.conversationId} was not found`)
        if (result.revision !== revision) return
        const window = result.value
        const expectedStartIndex = Math.min(expectedTotal, input.startIndex)
        const expectedEndIndex = Math.min(
            expectedTotal,
            expectedStartIndex + input.limit,
        )
        if (
            window.characterId !== this.characterId ||
            window.conversationId !== this.conversationId ||
            window.startIndex !== expectedStartIndex ||
            window.endIndex !== expectedEndIndex ||
            window.totalMessages !== expectedTotal ||
            window.messages.length !== expectedEndIndex - expectedStartIndex
        ) {
            throw new Error(`Conversation ${this.conversationId} returned a mismatched window`)
        }
        for (let offset = 0; offset < window.messages.length; offset++) {
            const absoluteIndex = window.startIndex + offset
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

    private rangeChangedSince(version: number, start: number, limit: number): boolean {
        return version < this.historyVersion - this.historyChanges.length ||
            this.historyChanges.some(change => change.version > version && change.start < start + limit && change.end > start)
    }

    private recordHistoryChange(start: number, end: number, countChanged: boolean): void {
        this.historyChanges.push({ version: ++this.historyVersion, start, end, countChanged })
        if (this.historyChanges.length > 64) this.historyChanges.shift()
    }

    advanceRevision(revision: DataRevision, totalMessages: number): void {
        this.assertUsable()
        this.validateRevision(revision)
        this.validateMessageCount(totalMessages)
        if (revision <= this.currentRevision) {
            throw new RangeError('Persistent conversation revision must advance')
        }
        this.currentRevision = revision
        this.latestPair = null
        this.persistedTotalMessages = totalMessages
        this.totalMessages = totalMessages
        this.epoch += 1
        this.invalidatePositionProofs(0)
        this.pins.clear()
        this.rows = new Map()
        this.notifyListeners()
    }

    advanceUnchangedRevision(revision: DataRevision): void {
        this.installProjection(revision, this.persistedTotalMessages, false)
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
        const excess = this.rows.size - this.rowBudget
        if (excess <= 0) return false
        const unpinned: [absoluteIndex: number, lastUsed: number][] = []
        for (const [absoluteIndex, cached] of this.rows) {
            if (!this.isPinned(absoluteIndex)) unpinned.push([absoluteIndex, cached.lastUsed])
        }
        if (unpinned.length === 0) return false
        unpinned.sort((left, right) => left[1] - right[1])
        for (let index = 0; index < Math.min(excess, unpinned.length); index += 1) {
            this.rows.delete(unpinned[index][0])
        }
        return true
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
