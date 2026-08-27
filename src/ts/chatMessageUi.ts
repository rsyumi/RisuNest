import {
    requireCurrentConversationSession,
    type ActiveConversationSession,
    type MessageLocator,
} from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import type { ChatViewportJumpOptions } from './chatViewport'

export interface CurrentChatMessageTarget {
    character: Database['characters'][number]
    conversation: Chat
}

export interface ChatMessageUiContext {
    captureCurrent(): CurrentChatMessageTarget | null
    getCurrentSession(): ActiveConversationSession | null
}

export interface CaptureChatMessageTargetOptions extends ChatMessageUiContext {
    absoluteIndex: number
}

interface CapturedChatMessageTargetBase {
    absoluteIndex: number
    character: Database['characters'][number]
    conversation: Chat
    message: Message
}

interface CapturedSessionChatMessageTarget extends CapturedChatMessageTargetBase {
    kind: 'session'
    session: ActiveConversationSession
    locator: MessageLocator
}

interface CapturedLegacyChatMessageTarget extends CapturedChatMessageTargetBase {
    kind: 'legacy'
    session: null
    locator: null
    legacyMessages: Message[]
}

export type CapturedChatMessageTarget =
    | CapturedSessionChatMessageTarget
    | CapturedLegacyChatMessageTarget

export interface ToggleBookmarkOptions {
    requestName(currentName: string): Promise<string>
    createMessageId(): string
    defaultName(message: Message): string
}

export type CapturedChatMessageSaveResult =
    | { saved: true; displayData: string }
    | { saved: false }

export class LatestChatScrollRequestGuard {
    private generation = 0

    begin(): number {
        this.generation += 1
        return this.generation
    }

    isCurrent(generation: number): boolean {
        return generation === this.generation
    }
}

export interface CapturedChatMessageViewport {
    jumpTo(index: number, options?: ChatViewportJumpOptions): Promise<boolean>
}

export async function navigateCapturedChatMessage(options: {
    target: CapturedChatMessageTarget
    context: ChatMessageUiContext
    guard: LatestChatScrollRequestGuard
    requestGeneration: number
    viewport: CapturedChatMessageViewport
}): Promise<boolean> {
    if (!options.guard.isCurrent(options.requestGeneration)) return false
    const resolved = resolveChatMessageTarget(options.target, options.context)
    if (!resolved) return false
    const jumped = await options.viewport.jumpTo(resolved.absoluteIndex, {
        align: 'start',
        highlight: true,
    })
    if (!jumped || !options.guard.isCurrent(options.requestGeneration)) return false
    return resolveChatMessageTarget(options.target, options.context) !== null
}

export function captureChatMessageTarget(
    options: CaptureChatMessageTargetOptions,
): CapturedChatMessageTarget | null {
    const current = options.captureCurrent()
    const message = current?.conversation.message[options.absoluteIndex]
    if (!current || !message) return null
    const session = matchingSession(current, options.getCurrentSession())
    if (session) {
        const locator = session.locate(options.absoluteIndex)
        return {
            kind: 'session',
            absoluteIndex: options.absoluteIndex,
            ...current,
            message: session.readMessage(locator),
            session,
            locator,
        }
    }
    return {
        kind: 'legacy',
        absoluteIndex: options.absoluteIndex,
        ...current,
        legacyMessages: current.conversation.message,
        message,
        session: null,
        locator: null,
    }
}

export function captureChatMessageTargetById(
    context: ChatMessageUiContext,
    messageId: string,
    occurrence: 'first' | 'last' = 'first',
): CapturedChatMessageTarget | null {
    return captureChatMessageTargetsByIds(context, [messageId], occurrence)[0] ?? null
}

export function captureChatMessageTargetsByIds(
    context: ChatMessageUiContext,
    messageIds: readonly string[],
    occurrence: 'first' | 'last' = 'first',
): CapturedChatMessageTarget[] {
    const current = context.captureCurrent()
    if (!current || messageIds.length === 0) return []
    const session = matchingSession(current, context.getCurrentSession())
    if (session) {
        return session.findMessageTargetsByIds(messageIds, occurrence).map((target) => ({
            kind: 'session' as const,
            ...current,
            ...target,
            session,
        }))
    }

    const requested = new Set(messageIds)
    const captured = new Map<string, CapturedLegacyChatMessageTarget>()
    const messages = current.conversation.message
    for (let absoluteIndex = 0; absoluteIndex < messages.length; absoluteIndex++) {
        const messageId = messages[absoluteIndex].chatId
        if (
            messageId === undefined ||
            !requested.has(messageId) ||
            (occurrence === 'first' && captured.has(messageId))
        ) continue
        const target = captureChatMessageTarget({ ...context, absoluteIndex })
        if (target?.kind === 'legacy') captured.set(messageId, target)
    }
    return messageIds.flatMap((messageId) => {
        const target = captured.get(messageId)
        return target ? [target] : []
    })
}

export function resolveChatMessageTarget(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
): CapturedChatMessageTarget | null {
    const current = context.captureCurrent()
    if (target.kind === 'session') {
        if (!current) return null
        const currentSession = matchingSession(current, context.getCurrentSession())
        if (currentSession !== target.session) return null
        try {
            const message = requireCurrentConversationSession(target.session, currentSession)
                .readMessage(target.locator)
            return { ...target, message }
        } catch {
            return null
        }
    }

    if (
        current?.character !== target.character ||
        current.conversation !== target.conversation ||
        current.conversation.message !== target.legacyMessages ||
        current.conversation.message[target.absoluteIndex] !== target.message ||
        matchingSession(current, context.getCurrentSession()) !== null
    ) return null
    return target
}

export function resolveRetainedChatMessageTarget(
    retained: { data: CapturedChatMessageTarget | null },
    context: ChatMessageUiContext,
): CapturedChatMessageTarget | null {
    if (!retained.data) return null
    const resolved = resolveChatMessageTarget(retained.data, context)
    if (!resolved) retained.data = null
    return resolved
}

export function editCapturedChatMessage(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    data: string,
): boolean {
    return editCapturedMessage(target, context, (message) => ({ ...message, data }))
}

export function saveCapturedChatMessage(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    data: string,
): CapturedChatMessageSaveResult {
    const saved = editCapturedChatMessage(target, context, data)
    return saved ? { saved: true, displayData: data } : { saved: false }
}

export function toggleCapturedMessageRole(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
): boolean {
    return editCapturedMessage(target, context, (message) => ({
        ...message,
        role: message.role === 'char' ? 'user' : 'char',
    }))
}

export function toggleCapturedMessageDisabled(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    mode: 'message' | 'allBefore',
): boolean {
    return editCapturedMessage(target, context, (message) => ({
        ...message,
        disabled: mode === 'message'
            ? !message.disabled
            : message.disabled === 'allBefore' ? false : 'allBefore',
    }))
}

export async function toggleCapturedBookmark(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    options: ToggleBookmarkOptions,
): Promise<boolean> {
    const initial = resolveChatMessageTarget(target, context)
    if (!initial) return false
    const existingMessageId = initial.message.chatId
    if (existingMessageId && initial.conversation.bookmarks?.includes(existingMessageId)) {
        return setCapturedBookmark(initial, context, false)
    }

    const messageId = existingMessageId ?? options.createMessageId()
    const requestedName = await options.requestName(
        initial.conversation.bookmarkNames?.[messageId] ?? '',
    )
    const name = requestedName?.trim() ? requestedName : options.defaultName(initial.message)
    return setCapturedBookmark(initial, context, true, messageId, name)
}

export async function renameCapturedBookmark(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    requestName: (currentName: string) => Promise<string>,
): Promise<boolean> {
    const initial = resolveChatMessageTarget(target, context)
    const messageId = initial?.message.chatId
    if (!initial || !messageId || !initial.conversation.bookmarks?.includes(messageId)) {
        return false
    }
    const newName = await requestName(initial.conversation.bookmarkNames?.[messageId] ?? '')
    if (!newName?.trim()) return false
    const current = resolveChatMessageTarget(initial, context)
    if (!current) return false
    if (
        current.message.chatId !== messageId ||
        !current.conversation.bookmarks?.includes(messageId)
    ) return false
    if (current.session) {
        current.session.renameBookmark(current.locator, newName)
    } else {
        current.conversation.bookmarkNames ??= {}
        current.conversation.bookmarkNames[messageId] = newName
    }
    return true
}

export function removeCapturedBookmark(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
): boolean {
    return setCapturedBookmark(target, context, false)
}

function editCapturedMessage(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    update: (message: Message) => Message,
): boolean {
    const current = resolveChatMessageTarget(target, context)
    if (!current) return false
    const updated = update(current.message)
    if (current.session) {
        current.session.edit(current.locator, updated)
    } else {
        Object.assign(current.message, updated)
    }
    return true
}

function setCapturedBookmark(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    bookmarked: boolean,
    messageId?: string,
    name?: string,
): boolean {
    const current = resolveChatMessageTarget(target, context)
    if (!current) return false
    if (current.session) {
        current.session.setBookmark(current.locator, {
            bookmarked,
            messageId,
            name,
        })
        return true
    }

    const resolvedMessageId = current.message.chatId ?? messageId
    if (!resolvedMessageId) return false
    if (bookmarked) {
        current.message.chatId ??= resolvedMessageId
        current.conversation.bookmarks ??= []
        current.conversation.bookmarkNames ??= {}
        if (!current.conversation.bookmarks.includes(resolvedMessageId)) {
            current.conversation.bookmarks.push(resolvedMessageId)
        }
        if (name !== undefined) current.conversation.bookmarkNames[resolvedMessageId] = name
    } else {
        const index = current.conversation.bookmarks?.indexOf(resolvedMessageId) ?? -1
        if (index >= 0) current.conversation.bookmarks!.splice(index, 1)
        if (current.conversation.bookmarkNames) {
            delete current.conversation.bookmarkNames[resolvedMessageId]
        }
    }
    current.conversation.bookmarks = [...(current.conversation.bookmarks ?? [])]
    return true
}

function matchingSession(
    current: CurrentChatMessageTarget,
    session: ActiveConversationSession | null,
): ActiveConversationSession | null {
    return session?.matchesConversation(current.character.chaId, current.conversation) === true
        ? session
        : null
}
