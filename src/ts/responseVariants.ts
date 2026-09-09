import type { Chat, Message } from './storage/database.svelte'
import { safeStructuredClone } from './polyfill'

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
    let start = end - 1
    const speakers = new Set([carrier.saying])
    while (start > 0) {
        const previous = messages[start - 1]
        if (
            previous.role !== 'char' ||
            previous.isComment ||
            previous.disabled ||
            speakers.has(previous.saying)
        )
            break
        speakers.add(previous.saying)
        start--
    }
    return { start, end }
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
}

export function trackRerollOutput(chat: Chat, message: Message): void {
    if (chat.rerollRecovery?.phase === 'generating' && message.chatId) {
        chat.rerollRecovery.outputs[message.chatId] = safeStructuredClone(message)
    }
}

export function recoverRerollMessages(chat: Chat): Message[] {
    const recovery = chat.rerollRecovery
    if (!recovery || recovery.phase === 'prepared') return chat.message
    const anchor = recovery.anchorId
        ? chat.message.findIndex((message) => message.chatId === recovery.anchorId)
        : -1
    const start = anchor >= 0 ? anchor + 1 : Math.min(recovery.startIndex, chat.message.length)
    const retained = chat.message.slice(start).filter((message) => {
        const owned = message.chatId ? recovery.outputs[message.chatId] : undefined
        return !owned || JSON.stringify(message) !== JSON.stringify(owned)
    })
    return [...chat.message.slice(0, start), ...safeStructuredClone(recovery.original), ...retained]
}

/** Synchronize the selected group's carrier when an earlier group member is edited. */
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
