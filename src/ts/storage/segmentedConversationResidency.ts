import { safeStructuredClone } from '../safeStructuredClone'
import type { Message } from './database.svelte'
import type { ActiveConversationPinReason } from './activeConversationSession'
import { CONVERSATION_RANGE_MAX_LIMIT, type DataRevision } from './persistentDataStore'

export type SegmentedConversationPinReason =
    | ActiveConversationPinReason
    | 'viewport'
    | 'editor'
    | 'background'

export interface SegmentedConversationResidencyOptions {
    revision: DataRevision
    totalMessages: number
    maxResidentBytes: number
    measureMessage(message: Message): number
}

export interface ConversationResidentInterval {
    startIndex: number
    endIndex: number
    byteSize: number
    messages: Message[]
}

export interface ConversationRangePin {
    readonly reason: SegmentedConversationPinReason
    release(): void
}

export interface ConversationDirtyMutation {
    start: number
    deleteCount: number
    messages: Message[]
    sessionVersion: number
}

export interface RecordConversationReplaceRangeInput {
    start: number
    deleteCount: number
    messages: readonly Message[]
    sessionVersion: number
}

export interface ConversationPersistenceAttempt {
    readonly sessionVersion: number
    acknowledge(revision: DataRevision): void
    release(): void
}

interface ResidentEntry {
    message: Message
    byteSize: number
    lastAccess: number
}

interface RangeRecord {
    startIndex: number
    endIndex: number
}

interface PinRecord extends RangeRecord {
    reason: SegmentedConversationPinReason
}

interface DirtyRecord extends ConversationDirtyMutation, RangeRecord {
    messageByteSizes: number[]
    /** The version that recorded this range; later folded versions only rewrite its messages. */
    firstSessionVersion: number
}

interface PersistentSequenceSpan {
    kind: 'persistent'
    persistentStartIndex: number
    length: number
}

interface LocalSequenceSpan {
    kind: 'local'
    length: number
}

type SequenceSpan = PersistentSequenceSpan | LocalSequenceSpan

function validateIndex(value: number, name: string): void {
    if (!Number.isSafeInteger(value) || value < 0) {
        throw new RangeError(`${name} must be a nonnegative safe integer`)
    }
}

function validateLimit(value: number): void {
    if (!Number.isSafeInteger(value) || value <= 0) {
        throw new RangeError('Conversation range limit must be a positive safe integer')
    }
    if (value > CONVERSATION_RANGE_MAX_LIMIT) {
        throw new RangeError(
            `Conversation range limit cannot exceed ${CONVERSATION_RANGE_MAX_LIMIT}`,
        )
    }
}

function transformRange(
    range: RangeRecord,
    start: number,
    deleteCount: number,
    insertCount: number,
): void {
    const removedEnd = start + deleteCount
    const delta = insertCount - deleteCount
    if (range.endIndex <= start) return
    if (range.startIndex >= removedEnd) {
        range.startIndex += delta
        range.endIndex += delta
        return
    }
    const nextStart = range.startIndex < start ? range.startIndex : start
    const nextEnd = range.endIndex >= removedEnd
        ? range.endIndex + delta
        : start + insertCount
    range.startIndex = nextStart
    range.endIndex = Math.max(nextStart, nextEnd)
}

function sequenceLength(spans: readonly SequenceSpan[]): number {
    return spans.reduce((total, span) => total + span.length, 0)
}

function sliceSequenceSpan(
    span: SequenceSpan,
    startOffset: number,
    endOffset: number,
): SequenceSpan {
    const length = endOffset - startOffset
    if (span.kind === 'local') return { kind: 'local', length }
    return {
        kind: 'persistent',
        persistentStartIndex: span.persistentStartIndex + startOffset,
        length,
    }
}

function splitSequenceSpans(
    spans: readonly SequenceSpan[],
    absoluteIndex: number,
): [SequenceSpan[], SequenceSpan[]] {
    const before: SequenceSpan[] = []
    const after: SequenceSpan[] = []
    let cursor = 0
    for (const span of spans) {
        const spanEnd = cursor + span.length
        if (spanEnd <= absoluteIndex) before.push({ ...span })
        else if (cursor >= absoluteIndex) after.push({ ...span })
        else {
            const splitOffset = absoluteIndex - cursor
            before.push(sliceSequenceSpan(span, 0, splitOffset))
            after.push(sliceSequenceSpan(span, splitOffset, span.length))
        }
        cursor = spanEnd
    }
    return [before, after]
}

function mergeSequenceSpans(spans: readonly SequenceSpan[]): SequenceSpan[] {
    const merged: SequenceSpan[] = []
    for (const span of spans) {
        if (span.length === 0) continue
        const previous = merged.at(-1)
        if (previous?.kind === 'local' && span.kind === 'local') {
            previous.length += span.length
        } else if (
            previous?.kind === 'persistent' &&
            span.kind === 'persistent' &&
            previous.persistentStartIndex + previous.length === span.persistentStartIndex
        ) {
            previous.length += span.length
        } else {
            merged.push({ ...span })
        }
    }
    return merged
}

function replaceSequenceRange(
    spans: readonly SequenceSpan[],
    start: number,
    deleteCount: number,
    insertCount: number,
): SequenceSpan[] {
    const [before, fromStart] = splitSequenceSpans(spans, start)
    const [, after] = splitSequenceSpans(fromStart, deleteCount)
    return mergeSequenceSpans([
        ...before,
        ...(insertCount === 0 ? [] : [{ kind: 'local' as const, length: insertCount }]),
        ...after,
    ])
}

export class SegmentedConversationResidency {
    readonly maxResidentBytes: number

    private readonly entries = new Map<number, ResidentEntry>()
    private readonly pins = new Set<PinRecord>()
    private readonly dirtyRecords: DirtyRecord[] = []
    private readonly pendingSaveAttempts = new Set<object>()
    private readonly measureMessage: (message: Message) => number
    private accessClock = 0
    private currentSessionVersion = 0
    private acknowledgedVersion = 0
    private messageCount: number
    private baseRevision: DataRevision
    private baseMessageCount: number
    private sequenceSpans: SequenceSpan[]

    constructor(options: SegmentedConversationResidencyOptions) {
        validateIndex(options.totalMessages, 'Conversation totalMessages')
        validateIndex(options.maxResidentBytes, 'Conversation resident byte budget')
        this.baseRevision = options.revision
        this.baseMessageCount = options.totalMessages
        this.messageCount = options.totalMessages
        this.sequenceSpans = options.totalMessages === 0
            ? []
            : [{ kind: 'persistent', persistentStartIndex: 0, length: options.totalMessages }]
        this.maxResidentBytes = options.maxResidentBytes
        this.measureMessage = options.measureMessage
    }

    get totalMessages(): number {
        return this.messageCount
    }

    get revision(): DataRevision {
        return this.baseRevision
    }

    get persistentTotalMessages(): number {
        return this.baseMessageCount
    }

    get sessionVersion(): number {
        return this.currentSessionVersion
    }

    get persistedVersion(): number {
        return this.acknowledgedVersion
    }

    advanceStoreRevision(revision: DataRevision): boolean {
        validateIndex(revision, 'Conversation storage-only data revision')
        if (revision < this.baseRevision) {
            throw new RangeError('Conversation storage-only data revision moved backwards')
        }
        if (revision === this.baseRevision) return false
        this.baseRevision = revision
        return true
    }

    get canAdoptPersistedMetadata(): boolean {
        return this.dirtyRecords.length === 0 && this.pendingSaveAttempts.size === 0
    }

    adoptPersistedMetadata(
        revision: DataRevision,
        version: number,
        messages: readonly Message[],
    ): boolean {
        if (
            revision < this.baseRevision ||
            version !== this.currentSessionVersion + 1 ||
            messages.length !== this.messageCount ||
            this.dirtyRecords.length > 0 ||
            this.pendingSaveAttempts.size > 0
        )
            return false
        const entries = [...this.entries].map(([index, entry]) => {
            const message = safeStructuredClone(messages[index])
            return [index, { ...entry, message, byteSize: this.measure(message) }] as const
        })
        for (const [index, entry] of entries) this.entries.set(index, entry)
        this.baseRevision = revision
        this.currentSessionVersion = version
        this.acknowledgedVersion = version
        this.evictToBudget()
        return true
    }

    get residentBytes(): number {
        let bytes = 0
        const counted = new Set<Message>()
        const add = (message: Message, byteSize: number) => {
            if (counted.has(message)) return
            counted.add(message)
            bytes += byteSize
        }
        for (const entry of this.entries.values()) add(entry.message, entry.byteSize)
        for (const dirty of this.dirtyRecords) {
            dirty.messages.forEach((message, index) => {
                add(message, dirty.messageByteSizes[index])
            })
        }
        return bytes
    }

    get residentIntervals(): ConversationResidentInterval[] {
        const indices = this.residentIndices()
        const intervals: ConversationResidentInterval[] = []
        for (const index of indices) {
            const message = this.messageAt(index)
            if (!message) continue
            const byteSize = this.byteSizeAt(index)
            const current = intervals.at(-1)
            if (current && current.endIndex === index) {
                current.endIndex += 1
                current.byteSize += byteSize
                current.messages.push(safeStructuredClone(message))
            } else {
                intervals.push({
                    startIndex: index,
                    endIndex: index + 1,
                    byteSize,
                    messages: [safeStructuredClone(message)],
                })
            }
        }
        return intervals
    }

    get pendingMutations(): ConversationDirtyMutation[] {
        return this.dirtyRecords.map((record) => ({
            start: record.start,
            deleteCount: record.deleteCount,
            messages: safeStructuredClone(record.messages),
            sessionVersion: record.sessionVersion,
        }))
    }

    discardResidentState(): void {
        this.entries.clear()
        this.pins.clear()
        this.dirtyRecords.splice(0)
        this.pendingSaveAttempts.clear()
    }

    readRange(startIndex: number, limit: number): Message[] | null {
        validateIndex(startIndex, 'Conversation range startIndex')
        validateLimit(limit)
        const start = Math.min(startIndex, this.messageCount)
        const end = Math.min(this.messageCount, start + limit)
        const messages: Message[] = []
        for (let absoluteIndex = start; absoluteIndex < end; absoluteIndex++) {
            const message = this.messageAt(absoluteIndex)
            if (!message) return null
            const entry = this.entries.get(absoluteIndex)
            if (entry) entry.lastAccess = this.tick()
            messages.push(safeStructuredClone(message))
        }
        return messages
    }

    pinRange(
        startIndex: number,
        endIndex: number,
        reason: SegmentedConversationPinReason,
    ): ConversationRangePin {
        validateIndex(startIndex, 'Conversation pin startIndex')
        validateIndex(endIndex, 'Conversation pin endIndex')
        if (endIndex <= startIndex) {
            throw new RangeError('Conversation pin range must not be empty')
        }
        if (endIndex > this.messageCount) {
            throw new RangeError('Conversation pin range exceeds the current message count')
        }
        const record: PinRecord = { startIndex, endIndex, reason }
        this.pins.add(record)
        let released = false
        return {
            reason,
            release: () => {
                if (released) return
                released = true
                this.pins.delete(record)
                this.evictToBudget()
            },
        }
    }

    pinCount(reason: SegmentedConversationPinReason): number {
        let count = 0
        for (const pin of this.pins) {
            if (pin.reason === reason) count += 1
        }
        if (reason === 'dirty') count += this.dirtyRecords.length
        if (reason === 'pending-save') count += this.pendingSaveAttempts.size
        return count
    }

    recordReplaceRange(input: RecordConversationReplaceRangeInput): void {
        validateIndex(input.start, 'Conversation replace-range start')
        validateIndex(input.deleteCount, 'Conversation replace-range deleteCount')
        validateIndex(input.sessionVersion, 'Conversation session version')
        if (input.sessionVersion !== this.currentSessionVersion + 1) {
            throw new RangeError('Conversation mutation must use the next session version')
        }
        if (input.start > this.messageCount) {
            throw new RangeError('Conversation replace-range start exceeds the current message count')
        }
        if (input.deleteCount > this.messageCount - input.start) {
            throw new RangeError('Conversation replace-range deleteCount exceeds the current message count')
        }
        const replacement = safeStructuredClone([...input.messages])
        const replacementByteSizes = replacement.map((message) => this.measure(message))
        const removedEnd = input.start + input.deleteCount
        const delta = replacement.length - input.deleteCount
        this.sequenceSpans = replaceSequenceRange(
            this.sequenceSpans,
            input.start,
            input.deleteCount,
            replacement.length,
        )
        const nextEntries = new Map<number, ResidentEntry>()
        for (const [absoluteIndex, entry] of this.entries) {
            if (absoluteIndex < input.start) nextEntries.set(absoluteIndex, entry)
            else if (absoluteIndex >= removedEnd) nextEntries.set(absoluteIndex + delta, entry)
        }
        this.entries.clear()
        for (const [absoluteIndex, entry] of nextEntries) {
            this.entries.set(absoluteIndex, entry)
        }
        for (const pin of this.pins) {
            transformRange(pin, input.start, input.deleteCount, replacement.length)
        }
        for (const dirty of this.dirtyRecords) {
            transformRange(dirty, input.start, input.deleteCount, replacement.length)
        }

        this.messageCount += delta
        this.currentSessionVersion = input.sessionVersion
        const last = this.dirtyRecords.at(-1)
        const lastOffset = last ? input.start - last.start : -1
        if (
            last &&
            delta === 0 &&
            lastOffset >= 0 &&
            lastOffset + replacement.length <= last.messages.length &&
            last.startIndex === last.start &&
            last.endIndex === last.start + last.messages.length
        ) {
            // A count-preserving edit inside the newest unsaved range rewrites that range.
            for (let offset = 0; offset < replacement.length; offset++) {
                last.messages[lastOffset + offset] = replacement[offset]
                last.messageByteSizes[lastOffset + offset] = replacementByteSizes[offset]
            }
            last.sessionVersion = input.sessionVersion
        } else {
            this.dirtyRecords.push({
                start: input.start,
                deleteCount: input.deleteCount,
                messages: replacement,
                sessionVersion: input.sessionVersion,
                firstSessionVersion: input.sessionVersion,
                startIndex: input.start,
                endIndex: input.start + replacement.length,
                messageByteSizes: replacementByteSizes,
            })
        }
        for (let offset = 0; offset < replacement.length; offset++) {
            const message = replacement[offset]
            this.entries.set(input.start + offset, {
                message,
                byteSize: replacementByteSizes[offset],
                lastAccess: this.tick(),
            })
        }
        this.evictToBudget()
    }

    beginPersistence(sessionVersion: number): ConversationPersistenceAttempt {
        validateIndex(sessionVersion, 'Conversation persistence session version')
        if (sessionVersion <= this.acknowledgedVersion) {
            throw new RangeError('Conversation persistence version is already acknowledged')
        }
        if (sessionVersion > this.currentSessionVersion) {
            throw new RangeError('Conversation persistence version exceeds the current session version')
        }
        const attempt = {}
        this.pendingSaveAttempts.add(attempt)
        let released = false
        return {
            sessionVersion,
            acknowledge: (revision) => {
                if (released) return
                try {
                    this.acknowledgePersisted(sessionVersion, revision)
                } finally {
                    released = true
                    this.pendingSaveAttempts.delete(attempt)
                    this.evictToBudget()
                }
            },
            release: () => {
                if (released) return
                released = true
                this.pendingSaveAttempts.delete(attempt)
                this.evictToBudget()
            },
        }
    }

    acknowledgePersisted(sessionVersion: number, revision: DataRevision): boolean {
        validateIndex(sessionVersion, 'Conversation persisted session version')
        validateIndex(revision, 'Conversation persisted data revision')
        if (sessionVersion <= this.acknowledgedVersion) return false
        if (sessionVersion > this.currentSessionVersion) {
            throw new RangeError('Conversation persisted version exceeds the current session version')
        }
        if (
            revision < this.baseRevision ||
            (revision === this.baseRevision && this.acknowledgedVersion === 0)
        ) {
            throw new RangeError('Conversation persisted data revision did not advance')
        }
        let nextBaseMessageCount = this.baseMessageCount
        // A folded record whose versions straddle the acknowledged one is stored in an earlier
        // state, so its count change is persisted and the rest rewrites its messages in place.
        let partial: DirtyRecord | undefined
        for (const dirty of this.dirtyRecords) {
            if (dirty.firstSessionVersion > sessionVersion) continue
            nextBaseMessageCount += dirty.messages.length - dirty.deleteCount
            if (dirty.sessionVersion > sessionVersion) partial = dirty
        }
        const remainingDirty = this.dirtyRecords.filter(
            (dirty) => dirty.sessionVersion > sessionVersion,
        )
        let nextSequence: SequenceSpan[] = nextBaseMessageCount === 0
            ? []
            : [{
                kind: 'persistent',
                persistentStartIndex: 0,
                length: nextBaseMessageCount,
            }]
        for (const dirty of remainingDirty) {
            nextSequence = replaceSequenceRange(
                nextSequence,
                dirty.start,
                dirty === partial ? dirty.messages.length : dirty.deleteCount,
                dirty.messages.length,
            )
        }
        if (sequenceLength(nextSequence) !== this.messageCount) {
            throw new Error('Conversation persistence rebase does not match current message count')
        }

        this.baseRevision = revision
        this.baseMessageCount = nextBaseMessageCount
        this.sequenceSpans = nextSequence
        this.acknowledgedVersion = sessionVersion
        if (partial) {
            partial.deleteCount = partial.messages.length
            partial.firstSessionVersion = sessionVersion + 1
        }
        for (let index = this.dirtyRecords.length - 1; index >= 0; index--) {
            if (this.dirtyRecords[index].sessionVersion <= sessionVersion) {
                this.dirtyRecords.splice(index, 1)
            }
        }
        this.evictToBudget()
        return true
    }

    evictToBudget(): number {
        if (
            this.dirtyRecords.length > 0 ||
            this.pendingSaveAttempts.size > 0
        ) return 0
        if (this.residentBytes <= this.maxResidentBytes) return 0
        const candidates = [...this.entries.entries()]
            .filter(([absoluteIndex]) => !this.isProtected(absoluteIndex))
            .sort((left, right) => left[1].lastAccess - right[1].lastAccess)
        let evicted = 0
        for (const [absoluteIndex] of candidates) {
            if (this.residentBytes <= this.maxResidentBytes) break
            if (this.entries.delete(absoluteIndex)) evicted += 1
        }
        return evicted
    }

    private isProtected(absoluteIndex: number): boolean {
        if (this.isDirtyIndex(absoluteIndex)) return true
        for (const pin of this.pins) {
            if (absoluteIndex >= pin.startIndex && absoluteIndex < pin.endIndex) return true
        }
        return false
    }

    private isDirtyIndex(absoluteIndex: number): boolean {
        return this.dirtyRecords.some(
            (dirty) => absoluteIndex >= dirty.startIndex && absoluteIndex < dirty.endIndex,
        )
    }

    private residentIndices(): number[] {
        const indices = new Set(this.entries.keys())
        return [...indices].sort((left, right) => left - right)
    }

    private messageAt(absoluteIndex: number): Message | undefined {
        return this.entries.get(absoluteIndex)?.message
    }

    private byteSizeAt(absoluteIndex: number): number {
        return this.entries.get(absoluteIndex)?.byteSize ?? 0
    }

    private measure(message: Message): number {
        const byteSize = this.measureMessage(message)
        if (!Number.isSafeInteger(byteSize) || byteSize < 0) {
            throw new RangeError('Conversation resident message size must be a nonnegative safe integer')
        }
        return byteSize
    }

    private tick(): number {
        this.accessClock += 1
        return this.accessClock
    }
}
