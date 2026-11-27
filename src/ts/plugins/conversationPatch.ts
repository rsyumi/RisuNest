import type { Message } from '../storage/database.svelte'
import type {
    ConversationMutation,
    DataRevision,
    PersistentRevisionReader,
    PersistentUnitMutation,
} from '../storage/persistentDataStore'
import { assertPinnedRevision } from '../storage/persistentRecordIterator'
import { responseEditReplacement } from '../responseVariants'
import { isPlainObject, isPluginMessageField, requiredId } from './pluginQueryInput'

export const CONVERSATION_PATCH_MAX_MESSAGES = 64
export const CONVERSATION_PATCH_MAX_CHAT_VARIABLES = 64
export const CONVERSATION_PATCH_MAX_MUTATION_ID_LENGTH = 128
export const CONVERSATION_PATCH_MAX_FIELD_BYTES = 1_048_576
export const CONVERSATION_PATCH_LEDGER_LIMIT = 128
export const CONVERSATION_PATCH_LEDGER_RETENTION_MS = 5 * 60_000

const SPAN_READ_LIMIT = 16

export type ChatVariableValue = string | number | boolean

export interface ConversationPatchMessage {
    index: number
    messageId: string | null
    set: Record<string, unknown>
}

export interface ConversationPatchChatVariable {
    key: string
    value: ChatVariableValue | null
}

export interface ConversationPatchRequest {
    characterId: string
    conversationId: string
    mutationId: string
    messages: ConversationPatchMessage[]
    chatVariables: ConversationPatchChatVariable[]
}

export interface ConversationPatchConflict {
    target: 'conversation' | 'message'
    reason: 'not-found'
    index?: number
}

export interface ConversationPatchResult {
    status: 'applied' | 'already-applied' | 'conflict' | 'busy'
    conflict?: ConversationPatchConflict
    revision: DataRevision
}

export interface ConversationPatchRun {
    start: number
    messages: Message[]
}

export interface PreparedConversationPatch {
    unitMutations: PersistentUnitMutation[]
    conversations: ConversationMutation[]
}

export type ConversationPatchPlan =
    | { kind: 'conflict'; conflict: ConversationPatchConflict }
    | { kind: 'apply'; runs: ConversationPatchRun[]; scriptstate: Record<string, unknown> | null }

function safeIndex(value: unknown, name: string): number {
    if (!Number.isSafeInteger(value) || (value as number) < 0) {
        throw new RangeError(`${name} must be a nonnegative safe integer`)
    }
    return value as number
}

function assertJsonValue(value: unknown, path: string, ancestors: unknown[] = []): void {
    if (value === null || typeof value === 'string' || typeof value === 'boolean') return
    if (typeof value === 'number') {
        if (!Number.isFinite(value)) throw new TypeError(`${path} must be a finite number`)
        return
    }
    if (typeof value !== 'object') throw new TypeError(`${path} is not JSON-serializable`)
    if (ancestors.includes(value)) throw new TypeError(`${path} contains a cycle`)
    const nested = [...ancestors, value]
    if (Array.isArray(value)) {
        for (let index = 0; index < value.length; index++) {
            if (!(index in value)) throw new TypeError(`${path}[${index}] is missing`)
            assertJsonValue(value[index], `${path}[${index}]`, nested)
        }
        return
    }
    if (!isPlainObject(value) || typeof (value as { toJSON?: unknown }).toJSON === 'function') {
        throw new TypeError(`${path} is not JSON-serializable`)
    }
    for (const [key, item] of Object.entries(value)) assertJsonValue(item, `${path}.${key}`, nested)
}

function serializedBytes(value: unknown): number {
    return new TextEncoder().encode(JSON.stringify(value)).byteLength
}

function cloneJson<T>(value: T): T {
    return JSON.parse(JSON.stringify(value)) as T
}

function chatVariableValue(value: unknown, path: string, allowNull: boolean): ChatVariableValue | null {
    if (value === null && allowNull) return null
    if (typeof value === 'string' || typeof value === 'boolean') return value
    if (typeof value === 'number' && Number.isFinite(value)) return value
    throw new TypeError(`${path} must be a string, a finite number, a boolean or null`)
}

function normalizeMessage(input: unknown, position: number): ConversationPatchMessage {
    const path = `messages[${position}]`
    if (!isPlainObject(input)) throw new TypeError(`${path} must be an object`)
    const index = safeIndex(input.index, `${path}.index`)
    if (input.messageId !== null && typeof input.messageId !== 'string') {
        throw new TypeError(`${path}.messageId must be a string or null`)
    }
    const messageId = (input.messageId as string | null) || null
    if (!isPlainObject(input.set)) throw new TypeError(`${path}.set must be an object`)
    const set: Record<string, unknown> = {}
    for (const [field, value] of Object.entries(input.set)) {
        if (field === 'data') {
            if (typeof value !== 'string') throw new TypeError(`${path}.set.data must be a string`)
        } else if (!isPluginMessageField(field)) {
            throw new TypeError(`${path}.set.${field} is not data or a plugin message field`)
        } else if (value !== undefined) {
            assertJsonValue(value, `${path}.set.${field}`)
            if (serializedBytes(value) > CONVERSATION_PATCH_MAX_FIELD_BYTES) {
                throw new RangeError(`${path}.set.${field} exceeds ${CONVERSATION_PATCH_MAX_FIELD_BYTES} bytes`)
            }
        }
        set[field] = value === undefined ? undefined : cloneJson(value)
    }
    return { index, messageId, set }
}

function normalizeChatVariable(input: unknown, position: number): ConversationPatchChatVariable {
    const path = `chatVariables[${position}]`
    if (!isPlainObject(input)) throw new TypeError(`${path} must be an object`)
    requiredId(input.key as string, `${path}.key`)
    return { key: input.key as string, value: chatVariableValue(input.value, `${path}.value`, true) }
}

/**
 * Validates plugin input before any read or commit; invalid input rejects the call.
 * Released plugins also send `baseRevision` and `expected`, which are not read.
 */
export function normalizeConversationPatchInput(input: unknown): ConversationPatchRequest {
    if (!isPlainObject(input)) throw new TypeError('Conversation patch input must be an object')
    requiredId(input.characterId as string, 'characterId')
    requiredId(input.conversationId as string, 'conversationId')
    const mutationId = input.mutationId
    if (typeof mutationId !== 'string' || mutationId.length === 0 ||
        mutationId.length > CONVERSATION_PATCH_MAX_MUTATION_ID_LENGTH) {
        throw new RangeError(`mutationId must be a string of 1 to ${CONVERSATION_PATCH_MAX_MUTATION_ID_LENGTH} characters`)
    }
    const messages: ConversationPatchMessage[] = []
    if (input.messages !== undefined) {
        if (!Array.isArray(input.messages)) throw new TypeError('messages must be an array')
        if (input.messages.length > CONVERSATION_PATCH_MAX_MESSAGES) {
            throw new RangeError(`messages accepts at most ${CONVERSATION_PATCH_MAX_MESSAGES} entries`)
        }
        const indices = new Set<number>()
        input.messages.forEach((entry, position) => {
            const message = normalizeMessage(entry, position)
            if (indices.has(message.index)) throw new RangeError(`messages repeats index ${message.index}`)
            indices.add(message.index)
            messages.push(message)
        })
    }
    const chatVariables: ConversationPatchChatVariable[] = []
    if (input.chatVariables !== undefined) {
        if (!Array.isArray(input.chatVariables)) throw new TypeError('chatVariables must be an array')
        if (input.chatVariables.length > CONVERSATION_PATCH_MAX_CHAT_VARIABLES) {
            throw new RangeError(`chatVariables accepts at most ${CONVERSATION_PATCH_MAX_CHAT_VARIABLES} entries`)
        }
        const keys = new Set<string>()
        input.chatVariables.forEach((entry, position) => {
            const variable = normalizeChatVariable(entry, position)
            if (keys.has(variable.key)) throw new RangeError(`chatVariables repeats key ${variable.key}`)
            keys.add(variable.key)
            chatVariables.push(variable)
        })
    }
    return {
        characterId: input.characterId as string,
        conversationId: input.conversationId as string,
        mutationId,
        messages,
        chatVariables,
    }
}

export function patchSetsMessageData(request: ConversationPatchRequest): boolean {
    return request.messages.some((entry) => Object.hasOwn(entry.set, 'data'))
}

function jsonEqual(left: unknown, right: unknown): boolean {
    if (left === right) return true
    if (left === null || right === null || typeof left !== 'object' || typeof right !== 'object') return false
    if (Array.isArray(left) !== Array.isArray(right)) return false
    if (Array.isArray(left)) {
        const other = right as unknown[]
        return left.length === other.length && left.every((item, index) => jsonEqual(item, other[index]))
    }
    const leftEntries = Object.entries(left).filter(([, value]) => value !== undefined)
    const rightRecord = right as Record<string, unknown>
    const rightCount = Object.values(rightRecord).filter((value) => value !== undefined).length
    return leftEntries.length === rightCount &&
        leftEntries.every(([key, value]) => Object.hasOwn(rightRecord, key) && jsonEqual(value, rightRecord[key]))
}

function isTurnStart(message: Message): boolean {
    return message.role === 'user' && !message.isComment
}

interface ConversationPatchSource {
    totalMessages: number
    message(index: number): Message
    /** Where the only message with this ID is; undefined when no message or several have it. */
    uniqueMessageIndex(messageId: string): number | undefined
    scriptstate: Record<string, unknown> | undefined
}

function holdsMessage(entry: ConversationPatchMessage, source: ConversationPatchSource): boolean {
    return entry.index < source.totalMessages && (source.message(entry.index).chatId || null) === entry.messageId
}

/** The message at `index` when it has the ID, else the only message with the ID; a null ID matches only at `index`. */
function locateMessage(entry: ConversationPatchMessage, source: ConversationPatchSource): number | undefined {
    if (holdsMessage(entry, source)) return entry.index
    return entry.messageId === null ? undefined : source.uniqueMessageIndex(entry.messageId)
}

function applySet(message: Message, set: Record<string, unknown>): Message {
    const updated = { ...message } as Record<string, unknown>
    for (const [field, value] of Object.entries(set)) {
        if (value === undefined) delete updated[field]
        else updated[field] = cloneJson(value)
    }
    return updated as unknown as Message
}

interface PatchTarget {
    index: number
    set: Record<string, unknown>
}

/** Each change takes the user edit's replacement, so a selected response variant keeps its snapshot in step. */
function planRuns(targets: PatchTarget[], source: ConversationPatchSource): ConversationPatchRun[] {
    const working = new Map<number, Message>()
    const current = (index: number) => working.get(index) ?? source.message(index)
    for (const target of [...targets].sort((left, right) => left.index - right.index)) {
        let end = target.index + 1
        while (end < source.totalMessages && !isTurnStart(current(end))) end++
        const span: Message[] = []
        for (let index = target.index; index < end; index++) span.push(current(index))
        const replacement = responseEditReplacement(span, 0, applySet(span[0], target.set))
        replacement.forEach((message, offset) => working.set(target.index + offset, message))
    }
    const changed = [...working]
        .filter(([index, message]) => JSON.stringify(message) !== JSON.stringify(source.message(index)))
        .sort(([left], [right]) => left - right)
    const runs: ConversationPatchRun[] = []
    for (const [index, message] of changed) {
        const last = runs.at(-1)
        if (last && last.start + last.messages.length === index) last.messages.push(message)
        else runs.push({ start: index, messages: [message] })
    }
    return runs
}

function planScriptstate(request: ConversationPatchRequest, current: Record<string, unknown> | undefined): Record<string, unknown> | null {
    if (!request.chatVariables.length) return null
    const next: Record<string, unknown> = { ...current }
    for (const variable of request.chatVariables) {
        if (variable.value === null) delete next[variable.key]
        else next[variable.key] = variable.value
    }
    return jsonEqual(next, current ?? {}) ? null : next
}

export function planConversationPatch(request: ConversationPatchRequest, source: ConversationPatchSource): ConversationPatchPlan {
    const targets: PatchTarget[] = []
    for (const entry of request.messages) {
        const index = locateMessage(entry, source)
        if (index === undefined) return { kind: 'conflict', conflict: { target: 'message', reason: 'not-found', index: entry.index } }
        targets.push({ index, set: entry.set })
    }
    return { kind: 'apply', runs: planRuns(targets, source), scriptstate: planScriptstate(request, source.scriptstate) }
}

/** Plans a patch against messages held in memory, such as the active session's. */
export function planLiveConversationPatch(
    request: ConversationPatchRequest,
    messages: readonly Message[],
    scriptstate: Record<string, unknown> | undefined,
): ConversationPatchPlan {
    return planConversationPatch(request, livePatchSource(request, messages, scriptstate))
}

export function livePatchSource(request: ConversationPatchRequest, messages: readonly Message[], scriptstate?: Record<string, unknown>): ConversationPatchSource {
    let unique: Map<string, number | undefined> | undefined
    return {
        totalMessages: messages.length,
        message: (index) => messages[index],
        uniqueMessageIndex(messageId) {
            if (!unique) {
                const addressed = new Set(request.messages.map((entry) => entry.messageId))
                const found = new Map<string, number | undefined>()
                messages.forEach((message, index) => {
                    if (message.chatId == null || !addressed.has(message.chatId)) return
                    found.set(message.chatId, found.has(message.chatId) ? undefined : index)
                })
                unique = found
            }
            return unique.get(messageId)
        },
        scriptstate,
    }
}

/** Plans a patch at the reader's pinned revision, reading only the addressed responses. */
export async function planStoredConversationPatch(
    reader: PersistentRevisionReader,
    request: ConversationPatchRequest,
): Promise<ConversationPatchPlan> {
    const { characterId, conversationId } = request
    const metadata = await reader.readConversationMetadata(characterId, conversationId)
    if (!metadata) return { kind: 'conflict', conflict: { target: 'conversation', reason: 'not-found' } }
    assertPinnedRevision(reader.revision, metadata.revision, 'Patch conversation')
    const source = await loadPatchMessages(reader, request, metadata.value.totalMessages)
    return planConversationPatch(request, { ...source, scriptstate: metadata.value.conversation.scriptstate })
}

/** Load only the addressed turns and ID lookups from one immutable projection. */
export async function loadPatchMessages(
    reader: Pick<PersistentRevisionReader, 'revision' | 'readConversationWindow'>,
    request: ConversationPatchRequest,
    totalMessages: number,
) {
    const { characterId, conversationId } = request
    const loaded = new Map<number, Message>()
    const unique = new Map<string, number | undefined>()
    const source: ConversationPatchSource = {
        totalMessages,
        message: (index) => loaded.get(index)!,
        uniqueMessageIndex: (messageId) => unique.get(messageId),
        scriptstate: undefined,
    }
    const load = async (startIndex: number) => {
        const window = await reader.readConversationWindow({ characterId, conversationId, startIndex, limit: SPAN_READ_LIMIT })
        if (!window) throw new Error('Patch conversation disappeared from its pinned revision')
        assertPinnedRevision(reader.revision, window.revision, 'Patch messages')
        window.value.messages.forEach((message, offset) => loaded.set(window.value.startIndex + offset, message))
    }
    const occurrence = async (messageId: string, anchorOccurrence: 'first' | 'last') => {
        const window = await reader.readConversationWindow({
            characterId, conversationId, anchorMessageId: messageId, anchorOccurrence, before: 0, after: 0,
        })
        if (window) assertPinnedRevision(reader.revision, window.revision, 'Patch message occurrence')
        return window?.value.startIndex
    }
    for (const entry of request.messages) {
        if (entry.index < totalMessages && !loaded.has(entry.index)) await load(entry.index)
        if (entry.messageId !== null && !holdsMessage(entry, source) && !unique.has(entry.messageId)) {
            const first = await occurrence(entry.messageId, 'first')
            unique.set(entry.messageId, first !== undefined && first === await occurrence(entry.messageId, 'last') ? first : undefined)
        }
        const target = locateMessage(entry, source)
        // The plan reports this entry as not found.
        if (target === undefined) break
        for (let index = target; index < totalMessages; index++) {
            if (!loaded.has(index)) await load(index)
            if (index > target && isTurnStart(loaded.get(index)!)) break
        }
    }
    return {
        ...source,
        // An indexed lookup and its response-variant span refer to positions at this
        // revision. Reuse them only if the live owner still addresses the same rows.
        matchesLive(live: ConversationPatchSource, replyIndex: number, replyId: string | undefined) {
            if (source.totalMessages !== live.totalMessages) return false
            for (const [index, row] of loaded) {
                const effective = live.message(index)
                if (!effective) return false
                if (index === replyIndex && replyId && effective.chatId === replyId && row.chatId === replyId) {
                    if (isTurnStart(effective) !== isTurnStart(row)) return false
                } else if (!jsonEqual(row, effective)) return false
            }
            for (const entry of request.messages) {
                const index = locateMessage(entry, source)
                if (index !== locateMessage(entry, live)) return false
                if (index === undefined) break
            }
            return true
        },
    }
}

/** The store writes for a planned patch: one count-preserving range per run and the `scriptstate` unit. */
export function prepareConversationPatchCommit(
    request: ConversationPatchRequest,
    planned: Extract<ConversationPatchPlan, { kind: 'apply' }>,
): PreparedConversationPatch {
    const { characterId, conversationId } = request
    return {
        unitMutations: planned.scriptstate === null ? [] : [{
            key: JSON.stringify(['conversation', characterId, conversationId, 'scriptstate']),
            type: 'set',
            value: planned.scriptstate,
        }],
        conversations: planned.runs.map((run) => ({
            type: 'replace-range',
            characterId,
            conversationId,
            start: run.start,
            deleteCount: run.messages.length,
            messages: run.messages,
        })),
    }
}

/** Patch outcomes kept per plugin, so a retried mutation ID returns its first outcome. */
export function createConversationPatchLedger(now: () => number = Date.now) {
    const entries = new Map<string, { at: number; outcome: Promise<ConversationPatchResult> }>()
    return {
        run(mutationId: string, execute: () => Promise<ConversationPatchResult>): Promise<ConversationPatchResult> {
            const time = now()
            for (const [id, entry] of entries) {
                if (time - entry.at <= CONVERSATION_PATCH_LEDGER_RETENTION_MS) break
                entries.delete(id)
            }
            const existing = entries.get(mutationId)
            if (existing) {
                return existing.outcome.then((result) =>
                    result.status === 'applied' ? { ...result, status: 'already-applied' } : result)
            }
            const outcome = execute()
            const entry = { at: time, outcome }
            entries.set(mutationId, entry)
            while (entries.size > CONVERSATION_PATCH_LEDGER_LIMIT) entries.delete(entries.keys().next().value!)
            const forget = () => {
                if (entries.get(mutationId) === entry) entries.delete(mutationId)
            }
            outcome.then((result) => {
                if (result.status === 'busy') forget()
            }, forget)
            return outcome
        },
    }
}
