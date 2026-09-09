import { responseEditReplacement } from './responseVariants'
import {
    requireCurrentConversationSession,
    type ActiveConversationSession,
    type MessageLocator,
} from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'
import type { ChatViewportJumpOptions } from './chatViewport'
import type {
    CompleteConversationLease,
    SelectedConversationTarget,
} from './storage/activeWorkingSet.svelte'
import { isSameSelectedConversationTarget } from './storage/activeWorkingSet.svelte'
import type {
    ConversationWindow,
} from './storage/persistentDataStore'
import { isMetadataOnlySelectedConversation } from './storage/selectedConversationLifecycle'

export interface CurrentChatMessageTarget {
    character: Database['characters'][number]
    conversation: Chat
}

export interface ChatMessageUiContext {
    captureCurrent(): CurrentChatMessageTarget | null
    getCurrentSession(): ActiveConversationSession | null
}

export interface AnchoredChatMessageUiContext extends ChatMessageUiContext {
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    acquirePersistentRevision(revision: number): Promise<AnchoredPersistentRevisionLease>
    acquireCompleteConversation(
        reason: string,
        target?: SelectedConversationTarget | null,
    ): Promise<CompleteConversationLease>
}

interface AnchoredPersistentRevisionLease {
    readonly revision: number
    readConversationWindow(
        query: Parameters<import('./storage/persistentDataStore').PersistentRevisionLease['readConversationWindow']>[0],
    ): ReturnType<import('./storage/persistentDataStore').PersistentRevisionLease['readConversationWindow']>
    release(): void | Promise<void>
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

interface CapturedPersistentChatMessageTarget extends CapturedChatMessageTargetBase {
    kind: 'persistent'
    session: null
    locator: null
    selection: SelectedConversationTarget
}

export type CapturedChatMessageTarget =
    | CapturedSessionChatMessageTarget
    | CapturedLegacyChatMessageTarget
    | CapturedPersistentChatMessageTarget

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

export async function queryChatMessageTargetAt(
    context: AnchoredChatMessageUiContext,
    absoluteIndex: number,
): Promise<CapturedChatMessageTarget | null> {
    const current = context.captureCurrent()
    if (!current || !Number.isSafeInteger(absoluteIndex) || absoluteIndex < 0) return null
    if (!isMetadataOnlySelectedConversation(current.conversation)) {
        return captureChatMessageTarget({ ...context, absoluteIndex })
    }
    const selection = captureMatchingPersistentSelection(context, current)
    if (!selection) return null
    return readPersistentTarget(context, current, selection, {
        startIndex: absoluteIndex,
        limit: 1,
    })
}

export async function queryChatMessageTargetById(
    context: AnchoredChatMessageUiContext,
    messageId: string,
    occurrence: 'first' | 'last' = 'first',
): Promise<CapturedChatMessageTarget | null> {
    return (await queryChatMessageTargetsByIds(context, [messageId], occurrence))[0] ?? null
}

export async function queryChatMessageTargetsByIds(
    context: AnchoredChatMessageUiContext,
    messageIds: readonly string[],
    occurrence: 'first' | 'last' = 'first',
): Promise<CapturedChatMessageTarget[]> {
    const current = context.captureCurrent()
    if (!current || messageIds.length === 0) return []
    if (!isMetadataOnlySelectedConversation(current.conversation)) {
        return captureChatMessageTargetsByIds(context, messageIds, occurrence)
    }
    const selection = captureMatchingPersistentSelection(context, current)
    if (!selection) return []
    const lease = await context.acquirePersistentRevision(selection.storeRevision)
    let results: CapturedChatMessageTarget[] = []
    let operationFailed = false
    try {
        if (lease.revision !== selection.storeRevision) {
            throw new Error('Persistent message query acquired a mismatched revision')
        }
        const found = new Map<string, CapturedChatMessageTarget>()
        for (const messageId of new Set(messageIds)) {
            const window = await lease.readConversationWindow({
                characterId: selection.characterId,
                conversationId: selection.conversationId,
                anchorMessageId: messageId,
                ...(occurrence === 'last' ? { anchorOccurrence: 'last' as const } : {}),
                before: 0,
                after: 0,
            })
            if (!isPersistentQueryCurrent(context, current, selection)) {
                found.clear()
                break
            }
            const target = capturePersistentWindowTarget(
                context,
                current,
                selection,
                window,
                messageId,
            )
            if (target) found.set(messageId, target)
        }
        results = messageIds.flatMap((messageId) => {
            const target = found.get(messageId)
            return target ? [target] : []
        })
    } catch (error) {
        operationFailed = true
        throw error
    } finally {
        try {
            await releaseAnchoredPersistentRevisionLease(lease)
        } catch (error) {
            if (!operationFailed) throw error
        }
    }
    return isPersistentQueryCurrent(context, current, selection) ? results : []
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

    if (target.kind === 'persistent') {
        if (
            current?.character !== target.character ||
            current.conversation !== target.conversation ||
            !isMetadataOnlySelectedConversation(current.conversation)
        ) return null
        const selection = contextHasAnchoredReads(context)
            ? context.captureSelectedConversationTarget()
            : null
        return selection && isSameSelectedConversationTarget(selection, target.selection)
            ? target
            : null
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
        if (initial.kind === 'persistent') {
            return mutatePersistentBookmark(initial, context, 'toggle-bookmark', (current) =>
                setCapturedBookmark(current, context, false),
            )
        }
        return setCapturedBookmark(initial, context, false)
    }

    const messageId = existingMessageId ?? options.createMessageId()
    const requestedName = await options.requestName(
        initial.conversation.bookmarkNames?.[messageId] ?? '',
    )
    const name = requestedName?.trim() ? requestedName : options.defaultName(initial.message)
    if (initial.kind === 'persistent') {
        return mutatePersistentBookmark(initial, context, 'toggle-bookmark', (current) =>
            setCapturedBookmark(current, context, true, messageId, name),
        )
    }
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
    if (initial.kind === 'persistent') {
        return mutatePersistentBookmark(initial, context, 'rename-bookmark', (current) => {
            if (!current.conversation.bookmarks?.includes(messageId)) return false
            current.session.renameBookmark(current.locator, newName)
            return true
        })
    }
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

export async function removeCapturedBookmark(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
): Promise<boolean> {
    if (target.kind === 'persistent') {
        return mutatePersistentBookmark(target, context, 'remove-bookmark', (current) =>
            setCapturedBookmark(current, context, false),
        )
    }
    return setCapturedBookmark(target, context, false)
}

async function mutatePersistentBookmark(
    target: CapturedPersistentChatMessageTarget,
    context: ChatMessageUiContext,
    reason: string,
    mutate: (target: CapturedSessionChatMessageTarget) => boolean,
): Promise<boolean> {
    if (!contextHasAnchoredReads(context) || !resolveChatMessageTarget(target, context)) return false
    let lease: CompleteConversationLease
    try {
        lease = await context.acquireCompleteConversation(reason, target.selection)
    } catch {
        return false
    }
    try {
        const current = context.captureCurrent()
        if (
            !current ||
            !lease.session.matchesConversation(current.character.chaId, current.conversation) ||
            target.message.chatId === undefined
        ) return false
        const locator = lease.session.locate(target.absoluteIndex)
        const message = lease.session.readMessage(locator)
        if (message.chatId !== target.message.chatId) return false
        return mutate({
            kind: 'session',
            absoluteIndex: target.absoluteIndex,
            ...current,
            message,
            session: lease.session,
            locator,
        })
    } catch {
        return false
    } finally {
        lease.release()
    }
}

function editCapturedMessage(
    target: CapturedChatMessageTarget,
    context: ChatMessageUiContext,
    update: (message: Message) => Message,
): boolean {
    const current = resolveChatMessageTarget(target, context)
    if (!current) return false
    const updated = update(current.message)
    const replacement = responseEditReplacement(current.conversation.message, current.absoluteIndex, updated)
    if (current.session) {
        if (replacement.length === 1) current.session.edit(current.locator, replacement[0])
        else
            current.session.replaceRange(
                current.session.positionAt(current.absoluteIndex),
                replacement.length,
                replacement,
            )
    } else {
        current.conversation.message.splice(current.absoluteIndex, replacement.length, ...replacement)
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
    // Persistent targets must be promoted to a session through
    // mutatePersistentBookmark before any bookmark metadata mutation.
    if (target.kind === 'persistent') return false
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

function contextHasAnchoredReads(
    context: ChatMessageUiContext,
): context is AnchoredChatMessageUiContext {
    const candidate = context as Partial<AnchoredChatMessageUiContext>
    return typeof candidate.captureSelectedConversationTarget === 'function' &&
        typeof candidate.acquirePersistentRevision === 'function' &&
        typeof candidate.acquireCompleteConversation === 'function'
}

function captureMatchingPersistentSelection(
    context: AnchoredChatMessageUiContext,
    current: CurrentChatMessageTarget,
): SelectedConversationTarget | null {
    const selection = context.captureSelectedConversationTarget()
    return selection &&
        selection.characterId === current.character.chaId &&
        selection.conversationId === current.conversation.id
        ? selection
        : null
}

async function readPersistentTarget(
    context: AnchoredChatMessageUiContext,
    current: CurrentChatMessageTarget,
    selection: SelectedConversationTarget,
    query: { startIndex: number; limit: number },
): Promise<CapturedChatMessageTarget | null> {
    const lease = await context.acquirePersistentRevision(selection.storeRevision)
    let target: CapturedChatMessageTarget | null = null
    let operationFailed = false
    try {
        if (lease.revision !== selection.storeRevision) {
            throw new Error('Persistent message query acquired a mismatched revision')
        }
        const window = await lease.readConversationWindow({
            characterId: selection.characterId,
            conversationId: selection.conversationId,
            ...query,
        })
        target = capturePersistentWindowTarget(
            context,
            current,
            selection,
            window,
            undefined,
            query.startIndex,
        )
    } catch (error) {
        operationFailed = true
        throw error
    } finally {
        try {
            await releaseAnchoredPersistentRevisionLease(lease)
        } catch (error) {
            if (!operationFailed) throw error
        }
    }
    return isPersistentQueryCurrent(context, current, selection) ? target : null
}

function isPersistentQueryCurrent(
    context: AnchoredChatMessageUiContext,
    current: CurrentChatMessageTarget,
    selection: SelectedConversationTarget,
): boolean {
    const recaptured = context.captureSelectedConversationTarget()
    const latest = context.captureCurrent()
    return recaptured !== null &&
        isSameSelectedConversationTarget(recaptured, selection) &&
        latest?.character === current.character &&
        latest.conversation === current.conversation &&
        isMetadataOnlySelectedConversation(latest.conversation)
}

function capturePersistentWindowTarget(
    context: AnchoredChatMessageUiContext,
    current: CurrentChatMessageTarget,
    selection: SelectedConversationTarget,
    result: { revision: number; value: ConversationWindow } | null,
    expectedMessageId?: string,
    expectedAbsoluteIndex?: number,
): CapturedPersistentChatMessageTarget | null {
    const recaptured = context.captureSelectedConversationTarget()
    if (!recaptured || !isSameSelectedConversationTarget(recaptured, selection)) return null
    const latest = context.captureCurrent()
    if (
        latest?.character !== current.character ||
        latest.conversation !== current.conversation ||
        !result
    ) return null
    if (result.revision !== selection.storeRevision) {
        throw new Error('Persistent message query returned mismatched revision evidence')
    }
    const window = result.value
    if (
        window.characterId !== selection.characterId ||
        window.conversationId !== selection.conversationId ||
        window.messages.length !== 1 ||
        window.endIndex !== window.startIndex + 1 ||
        (expectedAbsoluteIndex !== undefined && window.startIndex !== expectedAbsoluteIndex) ||
        (expectedMessageId !== undefined && window.messages[0].chatId !== expectedMessageId)
    ) return null
    return {
        kind: 'persistent',
        absoluteIndex: window.startIndex,
        ...current,
        message: window.messages[0],
        session: null,
        locator: null,
        selection,
    }
}

async function releaseAnchoredPersistentRevisionLease(
    lease: AnchoredPersistentRevisionLease,
): Promise<void> {
    try {
        await lease.release()
    } catch (firstError) {
        try {
            await lease.release()
        } catch {
            throw firstError
        }
    }
}
