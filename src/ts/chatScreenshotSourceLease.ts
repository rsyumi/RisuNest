import {
    createChatScreenshotDialogSnapshot,
    createChatScreenshotJobFromDialogSnapshot,
    snapshotChatScreenshotCharacter,
    type ChatScreenshotDialogSnapshot,
    type ChatScreenshotJob,
    type ChatScreenshotRangeReader,
    type ChatScreenshotRenderContext,
} from './chatScreenshotRange'
import type { Chat, character, groupChat } from './storage/database.svelte'
import type { ActiveConversationSession } from './storage/activeConversationSession'
import {
    isSameSelectedConversationTarget,
    type SelectedConversationTarget,
} from './storage/activeWorkingSet.svelte'
import {
    acquireCurrentPersistentRevision,
    PersistentConversationReadStaleError,
    type PersistentConversationReadDependencies,
} from './storage/persistentConversationRead'
import type { PersistentRevisionLease } from './storage/persistentDataStore'
import { releasePersistentRevisionLease } from './storage/persistentRecordIterator'
import type { WindowedConversationPersistenceAuthority } from './storage/saveCoordinator'

export interface ChatScreenshotSourceDependencies extends PersistentConversationReadDependencies {
    getActiveConversationSession(): ActiveConversationSession | null
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    captureSelectedConversationAuthority(): WindowedConversationPersistenceAuthority | null
}

export interface ChatScreenshotSourceLease {
    readonly snapshot: ChatScreenshotDialogSnapshot
    createJob(start: number, end: number, signal?: AbortSignal): Promise<ChatScreenshotJob>
    close(): Promise<void>
}

export interface OpenChatScreenshotSourceInput {
    characterId: string
    chatId: string
    renderContext: ChatScreenshotRenderContext
}

function assertNotAborted(signal?: AbortSignal): void {
    if (signal?.aborted) {
        throw new DOMException('Screenshot capture was cancelled', 'AbortError')
    }
}

function assertOpeningSession(
    input: OpenChatScreenshotSourceInput,
    dependencies: ChatScreenshotSourceDependencies,
    session: ActiveConversationSession,
    sessionVersion: number,
    navigationGeneration: number,
): void {
    if (
        dependencies.getNavigationGeneration() !== navigationGeneration
        || dependencies.getActiveConversationSession() !== session
        || !session.isActive
        || session.characterId !== input.characterId
        || session.conversationId !== input.chatId
        || session.version !== sessionVersion
    ) {
        throw new PersistentConversationReadStaleError()
    }
}

function requireOpeningTarget(
    input: OpenChatScreenshotSourceInput,
    dependencies: ChatScreenshotSourceDependencies,
    captured?: SelectedConversationTarget,
): SelectedConversationTarget {
    const target = dependencies.captureSelectedConversationTarget()
    if (
        !target ||
        target.characterId !== input.characterId ||
        target.conversationId !== input.chatId ||
        target.navigationGeneration !== dependencies.getNavigationGeneration() ||
        (captured && !isSameSelectedConversationTarget(captured, target))
    ) throw new PersistentConversationReadStaleError()
    return target
}

function requireOpeningIdentity(
    input: OpenChatScreenshotSourceInput,
    dependencies: ChatScreenshotSourceDependencies,
    captured: SelectedConversationTarget,
): SelectedConversationTarget {
    const target = requireOpeningTarget(input, dependencies)
    if (
        target.characterId !== captured.characterId
        || target.conversationId !== captured.conversationId
        || target.navigationGeneration !== captured.navigationGeneration
    ) throw new PersistentConversationReadStaleError()
    return target
}

function requireWindowedAuthority(
    dependencies: ChatScreenshotSourceDependencies,
    target: SelectedConversationTarget,
): WindowedConversationPersistenceAuthority {
    const authority = dependencies.captureSelectedConversationAuthority()
    if (
        !authority ||
        authority.characterId !== target.characterId ||
        authority.conversationId !== target.conversationId ||
        authority.storeRevision !== target.storeRevision ||
        authority.sessionVersion !== authority.persistedSessionVersion
    ) throw new PersistentConversationReadStaleError()
    return authority
}

async function readConversationEvidence(
    lease: PersistentRevisionLease,
    characterId: string,
    chatId: string,
) {
    const result = await lease.readConversationWindow({
        characterId,
        conversationId: chatId,
        startIndex: 0,
        limit: 1,
    })
    if (!result) throw new Error(`Screenshot conversation ${chatId} was not found`)
    const window = result.value
    if (
        result.revision !== lease.revision
        || window.characterId !== characterId
        || window.conversationId !== chatId
        || window.startIndex !== 0
        || window.endIndex !== Math.min(1, window.totalMessages)
        || window.messages.length !== Math.min(1, window.totalMessages)
    ) {
        throw new Error(`Screenshot conversation ${chatId} returned mismatched evidence`)
    }
    return window
}

export async function openChatScreenshotSourceLease(
    input: OpenChatScreenshotSourceInput,
    dependencies: ChatScreenshotSourceDependencies,
    signal?: AbortSignal,
): Promise<ChatScreenshotSourceLease> {
    assertNotAborted(signal)
    const openingTarget = requireOpeningTarget(input, dependencies)
    const session = dependencies.getActiveConversationSession()
    const openingAuthority = session ? null : requireWindowedAuthority(
        dependencies,
        openingTarget,
    )
    if (session && (
        !session.isActive
        || session.characterId !== input.characterId
        || session.conversationId !== input.chatId
    )) throw new Error('Screenshot conversation has no matching active session')
    const sessionVersion = session?.version ?? openingAuthority!.sessionVersion
    const navigationGeneration = dependencies.getNavigationGeneration()
    if (session) {
        assertOpeningSession(input, dependencies, session, sessionVersion, navigationGeneration)
    }
    await dependencies.flushPendingData('screenshot-dialog-open')
    assertNotAborted(signal)
    let committedTarget: SelectedConversationTarget
    let committedAuthority: WindowedConversationPersistenceAuthority | null = null
    try {
        committedTarget = requireOpeningIdentity(input, dependencies, openingTarget)
        if (session) {
            assertOpeningSession(input, dependencies, session, sessionVersion, navigationGeneration)
        } else {
            committedAuthority = requireWindowedAuthority(dependencies, committedTarget)
        }
    } catch {
        throw new Error('Screenshot conversation changed while opening')
    }
    if (session && session.persistedVersion !== sessionVersion) {
        throw new Error('Screenshot conversation has unpersisted changes after flush')
    }

    await dependencies.store.open()
    const lease = session
        ? await acquireCurrentPersistentRevision(
            dependencies,
            navigationGeneration,
            signal,
        )
        : await dependencies.store.acquireRevision(committedTarget.storeRevision)
    let keepLease = false
    try {
        assertNotAborted(signal)
        try {
            requireOpeningTarget(input, dependencies, committedTarget)
            if (session) {
                assertOpeningSession(
                    input,
                    dependencies,
                    session,
                    sessionVersion,
                    navigationGeneration,
                )
            } else {
                requireWindowedAuthority(dependencies, committedTarget)
                if (lease.revision !== committedTarget.storeRevision) {
                    throw new PersistentConversationReadStaleError()
                }
            }
        } catch {
            throw new Error('Screenshot conversation changed while opening')
        }
        const evidence = await readConversationEvidence(
            lease,
            input.characterId,
            input.chatId,
        )
        assertNotAborted(signal)
        try {
            requireOpeningTarget(input, dependencies, committedTarget)
            if (session) {
                assertOpeningSession(
                    input,
                    dependencies,
                    session,
                    sessionVersion,
                    navigationGeneration,
                )
            } else {
                requireWindowedAuthority(dependencies, committedTarget)
            }
        } catch {
            throw new Error('Screenshot conversation changed while opening')
        }
        if (
            (session && session.persistedVersion !== sessionVersion)
            || (session ? session.totalMessages : committedAuthority!.totalMessages) !==
                evidence.totalMessages
        ) {
            throw new Error('Screenshot conversation changed while opening')
        }

        const snapshot = createChatScreenshotDialogSnapshot({
            characterId: input.characterId,
            chatId: input.chatId,
            revision: lease.revision,
            sessionVersion,
            totalTurns: evidence.totalMessages,
            renderContext: input.renderContext,
        })
        const reader: ChatScreenshotRangeReader = {
            characterId: snapshot.characterId,
            chatId: snapshot.chatId,
            revision: snapshot.revision,
            totalTurns: snapshot.totalTurns,
            async readRange(startIndex, limit, readSignal) {
                assertNotAborted(readSignal)
                const result = await lease.readConversationWindow({
                    characterId: snapshot.characterId,
                    conversationId: snapshot.chatId,
                    startIndex,
                    limit,
                })
                assertNotAborted(readSignal)
                if (!result) {
                    throw new Error(`Screenshot conversation ${snapshot.chatId} was not found`)
                }
                const window = result.value
                if (
                    result.revision !== snapshot.revision
                    || window.characterId !== snapshot.characterId
                    || window.conversationId !== snapshot.chatId
                    || window.startIndex !== startIndex
                    || window.endIndex !== startIndex + limit
                    || window.totalMessages !== snapshot.totalTurns
                    || window.messages.length !== limit
                ) {
                    throw new Error(
                        `Screenshot conversation ${snapshot.chatId} returned a mismatched range`,
                    )
                }
                return window.messages
            },
            async readCharacter(characterId, readSignal) {
                assertNotAborted(readSignal)
                const result = await lease.readCharacter(characterId)
                assertNotAborted(readSignal)
                if (!result) return null
                if (
                    result.revision !== snapshot.revision
                    || result.value.chaId !== characterId
                ) {
                    throw new Error(
                        `Screenshot character ${characterId} returned mismatched evidence`,
                    )
                }
                const emptyChat: Chat = {
                    id: `screenshot:${characterId}`,
                    name: '',
                    note: '',
                    localLore: [],
                    fmIndex: -1,
                    message: [],
                }
                const hydrated = {
                    ...result.value,
                    chats: [emptyChat],
                    chatPage: 0,
                } as character | groupChat
                return snapshotChatScreenshotCharacter(hydrated, emptyChat)
            },
        }

        let released = false
        let releasePromise: Promise<void> | null = null
        let activeJob: Promise<ChatScreenshotJob> | null = null
        const release = () => {
            if (releasePromise) return releasePromise
            releasePromise = releasePersistentRevisionLease(lease).then(() => {
                released = true
            })
            return releasePromise
        }
        const source: ChatScreenshotSourceLease = {
            snapshot,
            async createJob(start, end, jobSignal) {
                if (released || releasePromise) {
                    throw new Error('Screenshot source lease is closed')
                }
                const job = createChatScreenshotJobFromDialogSnapshot(
                    snapshot,
                    reader,
                    start,
                    end,
                    jobSignal,
                )
                activeJob = job
                try {
                    return await job
                } finally {
                    if (activeJob === job) activeJob = null
                    await release()
                }
            },
            async close() {
                const job = activeJob
                if (job) {
                    try {
                        await job
                    } catch {}
                }
                await release()
            },
        }
        keepLease = true
        return source
    } finally {
        if (!keepLease) await releasePersistentRevisionLease(lease)
    }
}
