import type { Chat, Message } from './storage/database.svelte'
import { safeStructuredClone } from './safeStructuredClone'
import { toAbsoluteIndex, toWindowIndex } from './process/historyWindowIndex'

export type MessageSnapshot = Omit<Message, 'responseVariants'>
export interface ResponseVariantSet {
    groupId: string
    selectedId: string
    candidates: { id: string; messages: MessageSnapshot[] }[]
}

export function snapshotResponse(messages: readonly Message[]): MessageSnapshot[] {
    return messages.map((message) => {
        const { responseVariants: _variants, ...snapshot } = message
        return safeStructuredClone(snapshot)
    })
}

export function responseRange(messages: readonly Message[]): { start: number; end: number } | null {
    let end = messages.length
    while (end && (messages[end - 1].isComment || messages[end - 1].disabled)) end--
    if (!end || messages[end - 1].role !== 'char') return null
    const carrier = messages[end - 1]
    const selected = carrier.responseVariants?.candidates.find(
        (candidate) => candidate.id === carrier.responseVariants?.selectedId,
    )
    if (selected) return { start: Math.max(0, end - selected.messages.length), end }
    return { start: end - 1, end }
}

export function captureResponseVariants(
    messages: readonly Message[],
    createId: () => string,
): ResponseVariantSet {
    const carrier = messages.at(-1)!
    const current = snapshotResponse(messages)
    if (carrier.responseVariants) {
        const variants = safeStructuredClone(carrier.responseVariants)
        const selected = variants.candidates.find((candidate) => candidate.id === variants.selectedId)
        if (!selected) throw new Error('Response candidate selection is invalid')
        selected.messages = current
        return variants
    }
    const id = createId()
    return { groupId: carrier.chatId || createId(), selectedId: id, candidates: [{ id, messages: current }] }
}

export function projectResponseVariant(variants: ResponseVariantSet, selectedId: string): Message[] {
    const candidate = variants.candidates.find((candidate) => candidate.id === selectedId)
    if (!candidate?.messages.length) throw new Error('Response candidate is empty or unavailable')
    const messages: Message[] = snapshotResponse(candidate.messages)
    messages[messages.length - 1].chatId = variants.groupId
    messages[messages.length - 1].responseVariants = { ...variants, selectedId }
    return messages
}

export function precomputedResponseVariants(message: Message, values: readonly string[]): Message {
    if (values.length < 2) return message
    const groupId = message.chatId ?? message.generationInfo?.generationId ?? 'response'
    const candidates = values.map((data, index) => ({
        id: `${groupId}:${index}`,
        messages: snapshotResponse([{ ...message, data }]),
    }))
    return { ...message, responseVariants: { groupId, selectedId: candidates[0].id, candidates } }
}

export interface RerollRecovery {
    attemptId: string
    phase: 'prepared' | 'generating'
    startIndex: number
    anchorId?: string
    original: Message[]
    responseCount: number
    // Latest owned writes let recovery retain independently edited plugin output.
    outputs: Record<string, Message>
    firstOutput?: { id: string; offset: number }
    /** Independent rows that a plugin inserted after the response began. */
    followingIds?: string[]
}

export function trackRerollOutput(chat: Chat, message: Message, index?: number): void {
    const recovery = chat.rerollRecovery
    if (recovery?.phase === 'generating' && message.chatId) {
        if (!recovery.firstOutput && Object.keys(recovery.outputs).length === 0) {
            const position = index ?? toAbsoluteIndex(chat, chat.message.length)
            recovery.firstOutput = { id: message.chatId, offset: Math.max(0, position - recovery.startIndex) }
        }
        if (index !== undefined && recovery.firstOutput?.id === message.chatId) {
            recovery.startIndex = Math.max(0, index - recovery.firstOutput.offset)
        }
        recovery.outputs[message.chatId] = safeStructuredClone(message)
    }
}

function uniqueMessageIndex(messages: readonly Message[], id: string | undefined): number {
    if (!id) return -1
    const first = messages.findIndex(message => message.chatId === id)
    return first >= 0 && messages.findLastIndex(message => message.chatId === id) === first ? first : -1
}

export function rerollStart(chat: Chat, recovery: RerollRecovery): number {
    const first = recovery.firstOutput
    if (first) {
        const position = uniqueMessageIndex(chat.message, first.id)
        if (position >= 0) return Math.max(0, position - first.offset)
        return -1
    }
    const anchor = uniqueMessageIndex(chat.message, recovery.anchorId)
    if (anchor >= 0) return anchor + 1
    return Math.min(Math.max(toWindowIndex(chat, recovery.startIndex), 0), chat.message.length)
}

export function rerollProtectedIndexes(messages: readonly Message[], recovery: RerollRecovery, replyIndex: number): number[] {
    const first = recovery.firstOutput
    const firstIndex = uniqueMessageIndex(messages, first?.id)
    const start = firstIndex >= 0 ? firstIndex - first.offset
        : replyIndex >= 0 ? Math.min(recovery.startIndex, replyIndex) : recovery.startIndex
    const following = new Set(recovery.followingIds)
    const indexes: number[] = []
    for (let index = Math.max(0, start); index < messages.length; index++) {
        if (!following.has(messages[index].chatId)) indexes.push(index)
    }
    return indexes
}

/** Setter ranges are in application order. The protected response stays contiguous across each edit. */
export function rerollAfterPluginRanges(
    recovery: RerollRecovery,
    ranges: readonly { start: number; before: readonly Message[]; messages: readonly Message[] }[],
): RerollRecovery {
    if (!ranges.length) return recovery
    let start = recovery.startIndex
    const following = new Set(recovery.followingIds)
    for (const range of ranges) {
        if (range.before.length === range.messages.length &&
            range.before.every((row, index) => row.chatId === range.messages[index].chatId)) continue
        const previous = new Set<string>()
        for (const row of range.before) {
            if (row.chatId && following.delete(row.chatId)) previous.add(row.chatId)
        }
        if (range.start + range.before.length <= start) {
            start += range.messages.length - range.before.length
        } else if (range.start >= start) {
            const existing = new Set(range.before.map(row => row.chatId))
            for (const row of range.messages) {
                if (row.chatId && (!existing.has(row.chatId) || previous.has(row.chatId))) following.add(row.chatId)
            }
        }
    }
    if (start === recovery.startIndex && following.size === (recovery.followingIds?.length ?? 0)
        && (recovery.followingIds ?? []).every(id => following.has(id))) return recovery
    return { ...recovery, startIndex: start, followingIds: [...following] }
}

export function recoverRerollMessages(chat: Chat): Message[] {
    const recovery = chat.rerollRecovery
    if (!recovery || recovery.phase === 'prepared') return chat.message
    const start = rerollStart(chat, recovery)
    if (start < 0) return chat.message
    const retained = chat.message.slice(start).filter((message) => {
        const owned = message.chatId ? recovery.outputs[message.chatId] : undefined
        return !owned || JSON.stringify(message) !== JSON.stringify(owned)
    })
    return [...chat.message.slice(0, start), ...safeStructuredClone(recovery.original), ...retained]
}

/** Synchronize the selected response when an earlier message is edited. */
export function responseEditReplacement(
    messages: readonly Message[],
    index: number,
    updated: Message,
): Message[] {
    for (let end = index; end < messages.length; end++) {
        if (end > index && messages[end].role === 'user' && !messages[end].isComment) break
        const source = end === index ? updated : messages[end]
        const variants = source.responseVariants
        if (!variants) continue
        const selected = variants.candidates.find((candidate) => candidate.id === variants.selectedId)
        if (!selected || index < end - selected.messages.length + 1) continue
        const replacement = safeStructuredClone(messages.slice(index, end + 1))
        replacement[0] = safeStructuredClone(updated)
        const carrier = replacement.at(-1)!
        carrier.responseVariants = safeStructuredClone(variants)
        const snapshot = carrier.responseVariants.candidates.find(
            (candidate) => candidate.id === variants.selectedId,
        )!
        snapshot.messages[index - (end - selected.messages.length + 1)] = snapshotResponse([updated])[0]
        return replacement
    }
    return [updated]
}
