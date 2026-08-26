import { safeStructuredClone } from '../polyfill'
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

export interface SegmentedConversationRangeInput {
    revision: DataRevision
    startIndex: number
    totalMessages: number
    messages: readonly Message[]
}

export interface ConversationResidentInterval {
    startIndex: number
    endIndex: number
    byteSize: number
    messages: Message[]
}

export interface ConversationMissingRange {
    startIndex: number
    endIndex: number
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
    acknowledge(): void
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
}

interface StreamingOverlay {
    absoluteIndex: number
    message: Message
    byteSize: number
    sessionVersion: number
}

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

export class SegmentedConversationResidency {
    readonly revision: DataRevision
    readonly maxResidentBytes: number

    private readonly entries = new Map<number, ResidentEntry>()
    private readonly pins = new Set<PinRecord>()
    private readonly dirtyRecords: DirtyRecord[] = []
    private readonly pendingSaveAttempts = new Set<object>()
    private readonly measureMessage: (message: Message) => number
    private streamingOverlay: StreamingOverlay | null = null
    private accessClock = 0
    private currentSessionVersion = 0
    private acknowledgedVersion = 0
    private messageCount: number

    constructor(options: SegmentedConversationResidencyOptions) {
        validateIndex(options.totalMessages, 'Conversation totalMessages')
        validateIndex(options.maxResidentBytes, 'Conversation resident byte budget')
        this.revision = options.revision
        this.messageCount = options.totalMessages
        this.maxResidentBytes = options.maxResidentBytes
        this.measureMessage = options.measureMessage
    }

    get totalMessages(): number {
        return this.messageCount
    }

    get sessionVersion(): number {
        return this.currentSessionVersion
    }

    get persistedVersion(): number {
        return this.acknowledgedVersion
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
        if (this.streamingOverlay) {
            add(this.streamingOverlay.message, this.streamingOverlay.byteSize)
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

    storeRange(input: SegmentedConversationRangeInput): void {
        if (input.revision !== this.revision) {
            throw new Error(
                `Conversation range revision ${input.revision} does not match ${this.revision}`,
            )
        }
        if (input.totalMessages !== this.messageCount) {
            throw new Error(
                `Conversation range total ${input.totalMessages} does not match ${this.messageCount}`,
            )
        }
        validateIndex(input.startIndex, 'Conversation range startIndex')
        if (input.startIndex + input.messages.length > this.messageCount) {
            throw new RangeError('Conversation range exceeds the current message count')
        }
        for (let offset = 0; offset < input.messages.length; offset++) {
            const absoluteIndex = input.startIndex + offset
            if (this.isDirtyIndex(absoluteIndex) || this.streamingOverlay?.absoluteIndex === absoluteIndex) {
                continue
            }
            const message = safeStructuredClone(input.messages[offset])
            this.entries.set(absoluteIndex, {
                message,
                byteSize: this.measure(message),
                lastAccess: this.tick(),
            })
        }
        this.evictToBudget()
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

    missingRanges(startIndex: number, limit: number): ConversationMissingRange[] {
        validateIndex(startIndex, 'Conversation range startIndex')
        validateLimit(limit)
        const start = Math.min(startIndex, this.messageCount)
        const end = Math.min(this.messageCount, start + limit)
        const missing: ConversationMissingRange[] = []
        for (let absoluteIndex = start; absoluteIndex < end; absoluteIndex++) {
            if (this.messageAt(absoluteIndex)) continue
            const current = missing.at(-1)
            if (current?.endIndex === absoluteIndex) current.endIndex += 1
            else missing.push({ startIndex: absoluteIndex, endIndex: absoluteIndex + 1 })
        }
        return missing
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
        if (reason === 'streaming' && this.streamingOverlay) count += 1
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
        if (
            this.streamingOverlay &&
            input.start <= this.streamingOverlay.absoluteIndex
        ) {
            throw new Error('Conversation replace-range cannot shift an active streaming overlay')
        }

        const replacement = safeStructuredClone([...input.messages])
        const removedEnd = input.start + input.deleteCount
        const delta = replacement.length - input.deleteCount
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
        const replacementByteSizes = replacement.map((message) => this.measure(message))
        const dirty: DirtyRecord = {
            start: input.start,
            deleteCount: input.deleteCount,
            messages: replacement,
            sessionVersion: input.sessionVersion,
            startIndex: input.start,
            endIndex: input.start + replacement.length,
            messageByteSizes: replacementByteSizes,
        }
        this.dirtyRecords.push(dirty)
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
            acknowledge: () => {
                if (released) return
                this.acknowledgePersisted(sessionVersion)
                released = true
                this.pendingSaveAttempts.delete(attempt)
                this.evictToBudget()
            },
            release: () => {
                if (released) return
                released = true
                this.pendingSaveAttempts.delete(attempt)
                this.evictToBudget()
            },
        }
    }

    acknowledgePersisted(sessionVersion: number): void {
        validateIndex(sessionVersion, 'Conversation persisted session version')
        if (sessionVersion < this.acknowledgedVersion) {
            throw new RangeError('Conversation persisted version cannot move backwards')
        }
        if (sessionVersion > this.currentSessionVersion) {
            throw new RangeError('Conversation persisted version exceeds the current session version')
        }
        this.acknowledgedVersion = sessionVersion
        for (let index = this.dirtyRecords.length - 1; index >= 0; index--) {
            if (this.dirtyRecords[index].sessionVersion <= sessionVersion) {
                this.dirtyRecords.splice(index, 1)
            }
        }
        this.evictToBudget()
    }

    setStreamingOverlay(
        absoluteIndex: number,
        message: Message,
        sessionVersion: number,
    ): void {
        validateIndex(absoluteIndex, 'Conversation streaming index')
        validateIndex(sessionVersion, 'Conversation streaming session version')
        if (absoluteIndex >= this.messageCount) {
            throw new RangeError('Conversation streaming index does not exist')
        }
        if (sessionVersion < this.currentSessionVersion) {
            throw new RangeError('Conversation streaming overlay is stale')
        }
        const cloned = safeStructuredClone(message)
        this.entries.delete(absoluteIndex)
        this.streamingOverlay = {
            absoluteIndex,
            message: cloned,
            byteSize: this.measure(cloned),
            sessionVersion,
        }
        this.evictToBudget()
    }

    clearStreamingOverlay(sessionVersion: number): void {
        validateIndex(sessionVersion, 'Conversation streaming session version')
        if (!this.streamingOverlay) return
        if (this.streamingOverlay.sessionVersion !== sessionVersion) {
            throw new RangeError('Conversation streaming overlay version does not match')
        }
        this.streamingOverlay = null
        this.evictToBudget()
    }

    evictToBudget(): number {
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
        if (this.streamingOverlay?.absoluteIndex === absoluteIndex) return true
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
        if (this.streamingOverlay) indices.add(this.streamingOverlay.absoluteIndex)
        return [...indices].sort((left, right) => left - right)
    }

    private messageAt(absoluteIndex: number): Message | undefined {
        if (this.streamingOverlay?.absoluteIndex === absoluteIndex) {
            return this.streamingOverlay.message
        }
        return this.entries.get(absoluteIndex)?.message
    }

    private byteSizeAt(absoluteIndex: number): number {
        if (this.streamingOverlay?.absoluteIndex === absoluteIndex) {
            return this.streamingOverlay.byteSize
        }
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
