import { writable, get } from 'svelte/store'
export const activeRerollConversations = writable<string[]>([])
import type { ActiveConversationSession } from './storage/activeConversationSession'
import { cloneConversationMetadata } from './storage/selectedConversationLifecycle'
import type { Chat, Message } from './storage/database.svelte'
import { safeStructuredClone } from './polyfill'
import {
    captureResponseVariants,
    projectResponseVariant,
    recoverRerollMessages,
    responseRange,
    snapshotResponse,
    type RerollRecovery,
} from './responseVariants'

export function replaceResponseTail(
    chat: Chat,
    session: ActiveConversationSession | null,
    start: number,
    messages: Message[],
    recovery: RerollRecovery | null | undefined = chat.rerollRecovery,
): void {
    const expectedMetadata = cloneConversationMetadata(chat)
    const metadata = { ...expectedMetadata }
    if (recovery) metadata.rerollRecovery = recovery
    else delete metadata.rerollRecovery
    if (session?.isActive) {
        session.applyOperation({
            expectedVersion: session.version,
            expectedMetadata,
            metadata,
            range: {
                position: session.positionAt(start),
                deleteCount: chat.message.length - start,
                messages,
            },
        })
    } else {
        chat.message.splice(start, chat.message.length - start, ...safeStructuredClone(messages))
        if (recovery) chat.rerollRecovery = recovery
        else delete chat.rerollRecovery
    }
}

export function recoverInterruptedReroll(chat: Chat, session: ActiveConversationSession | null): boolean {
    if (!chat.rerollRecovery) return false
    const messages = recoverRerollMessages(chat)
    // Keep the unchanged prefix outside the mutation and copy budget.
    let start = 0
    while (start < chat.message.length && chat.message[start] === messages[start]) start++
    replaceResponseTail(chat, session, start, messages.slice(start), null)
    return true
}

export function moveResponseCandidate(
    chat: Chat,
    session: ActiveConversationSession | null,
    direction: -1 | 1,
    createId: () => string,
): boolean {
    if (chat.rerollRecovery) return false
    const range = responseRange(chat.message)
    if (!range) return false
    const variants = captureResponseVariants(chat.message.slice(range.start, range.end), createId)
    const index =
        variants.candidates.findIndex((candidate) => candidate.id === variants.selectedId) + direction
    if (index < 0 || index >= variants.candidates.length) return false
    replaceResponseTail(chat, session, range.start, [
        ...projectResponseVariant(variants, variants.candidates[index].id),
        ...chat.message.slice(range.end),
    ])
    return true
}

interface GenerateResponseCandidateOptions {
    chat: Chat
    currentChat?(): Chat | undefined
    session(): ActiveConversationSession | null
    isCurrent(): boolean
    createId(): string
    flush(): Promise<void>
    generate(): Promise<boolean>
    aborted(): boolean
}

export async function generateResponseCandidate(options: GenerateResponseCandidateOptions): Promise<boolean> {
    const id = options.chat.id!
    if (get(activeRerollConversations).includes(id)) return false
    activeRerollConversations.update((values) => [...values, id])
    try {
        return await runResponseCandidate(options)
    } finally {
        activeRerollConversations.update((values) => values.filter((value) => value !== id))
    }
}

async function runResponseCandidate(options: GenerateResponseCandidateOptions): Promise<boolean> {
    let chat = options.chat
    if (chat.rerollRecovery || !options.isCurrent()) return false
    const range = responseRange(chat.message)
    if (!range) return false
    const variants = captureResponseVariants(chat.message.slice(range.start, range.end), options.createId)
    const original = [
        ...projectResponseVariant(variants, variants.selectedId),
        ...safeStructuredClone(chat.message.slice(range.end)),
    ]
    const recovery: RerollRecovery = {
        attemptId: options.createId(),
        phase: 'prepared',
        startIndex: range.start,
        anchorId: chat.message[range.start - 1]?.chatId,
        original,
        responseCount: range.end - range.start,
        outputs: {},
    }
    replaceResponseTail(chat, options.session(), range.start, original, recovery)
    // A failed checkpoint leaves the original projection and candidates intact.
    await options.flush()
    if (!options.isCurrent()) return false
    chat = options.currentChat?.() ?? chat
    const prepared = chat.rerollRecovery!
    if (JSON.stringify(chat.message.slice(range.start)) !== JSON.stringify(prepared.original)) return false
    replaceResponseTail(chat, options.session(), range.start, [], { ...prepared, phase: 'generating' })
    let success = false
    try {
        success = await options.generate()
        chat = options.currentChat?.() ?? chat
        if (!success || options.aborted() || !options.isCurrent()) return false
        const generated = chat.message.slice(range.start)
        if (
            !generated.some(
                (message) =>
                    message.role === 'char' &&
                    !message.isComment &&
                    !message.disabled &&
                    message.data.length > 0,
            )
        )
            return false
        const generatedRange = responseRange(generated)
        if (!generatedRange) return false
        const generatedVariants = captureResponseVariants(
            generated.slice(generatedRange.start, generatedRange.end),
            options.createId,
        )
        for (const candidate of generatedVariants.candidates) {
            variants.candidates.push({
                id: options.createId(),
                messages: snapshotResponse([
                    ...generated.slice(0, generatedRange.start),
                    ...candidate.messages,
                    ...generated.slice(generatedRange.end),
                ]),
            })
        }
        variants.selectedId =
            variants.candidates[variants.candidates.length - generatedVariants.candidates.length].id
        const trailing = original.slice(recovery.responseCount)
        replaceResponseTail(
            chat,
            options.session(),
            range.start,
            [...projectResponseVariant(variants, variants.selectedId), ...trailing],
            null,
        )
        try {
            await options.flush()
        } catch (error) {
            replaceResponseTail(chat, options.session(), range.start, original, null)
            throw error
        }
        return true
    } finally {
        chat = options.currentChat?.() ?? chat
        if (chat.rerollRecovery?.attemptId === recovery.attemptId) {
            recoverInterruptedReroll(chat, options.session())
            await options.flush()
        }
    }
}
