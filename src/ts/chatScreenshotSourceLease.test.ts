import { describe, expect, it, vi } from 'vitest'
import type { Chat, Database, Message, character } from './storage/database.svelte'
import { ActiveConversationSession } from './storage/activeConversationSession'
import type {
    PersistentDataStore,
    PersistentRevisionLease,
} from './storage/persistentDataStore'
import { openChatScreenshotSourceLease } from './chatScreenshotSourceLease'
import type { ChatScreenshotRenderContext } from './chatScreenshotRange'
import type { SelectedConversationTarget } from './storage/activeWorkingSet.svelte'
import type { WindowedConversationPersistenceAuthority } from './storage/saveCoordinator'

function chat(messages: Message[]): Chat {
    return {
        id: 'chat-1',
        name: 'Chat',
        note: '',
        localLore: [],
        fmIndex: -1,
        message: messages,
    }
}

function renderContext(owner: character): ChatScreenshotRenderContext {
    const projected = structuredClone(owner)
    projected.chats[0].message = []
    return {
        character: null,
        characterName: owner.name,
        characterImageSource: '',
        characterLargePortrait: false,
        userName: 'User',
        userImageSource: '',
        userLargePortrait: false,
        moduleAssets: [],
        presetRegex: [],
        moduleRegexScripts: [],
        assetStyle: '',
        parserContext: {
            database: { characters: [projected] } as Database,
            character: projected,
            userName: 'User',
            personaPrompt: '',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
        },
        settings: {
            autoTranslate: false,
            autoTranslateCachedOnly: false,
            translatorType: 'google',
            translateBeforeHTMLFormatting: false,
            legacyTranslation: false,
            showTranslationLoading: false,
            newImageHandlingBeta: false,
            assetWidth: -1,
            hideAllImages: false,
            iconSize: 100,
            zoomSize: 100,
            lineHeight: 1.25,
            dynamicAssets: false,
            dynamicAssetsEditDisplay: false,
            legacyMediaFindings: false,
            assetMaxDifference: 0.5,
        },
    }
}

function harness(messages: Message[]) {
    const frozenMessages = structuredClone(messages)
    const conversation = chat(messages)
    const owner = {
        type: 'character',
        chaId: 'character-1',
        name: 'Character',
        chatPage: 0,
        chats: [conversation],
    } as character
    const session = new ActiveConversationSession({
        characterId: owner.chaId,
        conversationId: conversation.id!,
        conversation,
        storeRevision: 7,
    })
    const release = vi.fn(async () => undefined)
    const reads: Array<{ startIndex: number; limit: number }> = []
    const lease = {
        revision: 7,
        readConversationWindow: vi.fn(async ({ startIndex = 0, limit = 1 }) => {
            reads.push({ startIndex, limit })
            const page = frozenMessages.slice(startIndex, startIndex + limit)
            return {
                revision: 7,
                value: {
                    characterId: owner.chaId,
                    conversationId: conversation.id!,
                    messages: structuredClone(page),
                    startIndex,
                    endIndex: startIndex + page.length,
                    totalMessages: frozenMessages.length,
                    hasMoreBefore: startIndex > 0,
                    hasMoreAfter: startIndex + page.length < frozenMessages.length,
                },
            }
        }),
        release,
    } as unknown as PersistentRevisionLease
    const store = {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({ revision: 7, value: {} })),
        acquireRevision: vi.fn(async () => lease),
    } as unknown as PersistentDataStore
    let activeSession: ActiveConversationSession | null = session
    let navigationGeneration = 1
    const dependencies = {
        store,
        flushPendingData: vi.fn(async () => undefined),
        getNavigationGeneration: () => navigationGeneration,
        getActiveConversationSession: () => activeSession,
        captureSelectedConversationTarget: () => activeSession ? {
            characterId: owner.chaId,
            conversationId: conversation.id!,
            navigationGeneration,
            storeRevision: activeSession.storeRevision,
        } as SelectedConversationTarget : null,
        captureSelectedConversationAuthority: () => null,
    }
    return {
        owner,
        conversation,
        session,
        release,
        lease,
        reads,
        dependencies,
        replaceSession(next: ActiveConversationSession | null) {
            activeSession = next
        },
        navigate() {
            navigationGeneration += 1
        },
    }
}

function windowedHarness(messages: Message[]) {
    const source = harness(messages)
    source.replaceSession(null)
    let current = true
    const target = {
        characterId: source.owner.chaId,
        conversationId: source.conversation.id!,
        navigationGeneration: 1,
        storeRevision: 7,
    } as SelectedConversationTarget
    const authority: WindowedConversationPersistenceAuthority = {
        kind: 'windowed',
        characterId: target.characterId,
        conversationId: target.conversationId,
        sessionToken: source.session.sessionToken,
        storeRevision: target.storeRevision,
        persistedSessionVersion: 4,
        sessionVersion: 4,
        totalMessages: messages.length,
    }
    const readConversation = vi.fn()
    Object.assign(source.dependencies.store, { readConversation })
    Object.assign(source.dependencies, {
        captureSelectedConversationTarget: () => current ? target : null,
        captureSelectedConversationAuthority: () => current ? authority : null,
    })
    return {
        ...source,
        target,
        authority,
        readConversation,
        invalidateTarget() {
            current = false
        },
    }
}

describe('chat screenshot source lease', () => {
    it('opens a windowed selected conversation from its exact bounded authority', async () => {
        const source = windowedHarness([
            { role: 'user', data: 'one' },
            { role: 'char', data: 'two' },
            { role: 'user', data: 'three' },
        ])

        const screenshot = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)
        const job = await screenshot.createJob(2, 3)

        expect(job.messages.map((message) => message.data)).toEqual(['two', 'three'])
        expect(screenshot.snapshot).toMatchObject({
            revision: 7,
            sessionVersion: 4,
            totalTurns: 3,
        })
        expect(source.dependencies.store.acquireRevision).toHaveBeenCalledWith(7)
        expect(source.readConversation).not.toHaveBeenCalled()
        expect(source.reads).toContainEqual({ startIndex: 0, limit: 1 })
        expect(source.reads).toContainEqual({ startIndex: 1, limit: 2 })
        expect(source.reads.every(({ limit }) => limit <= 2)).toBe(true)
    })

    it('rejects a windowed target that changes during the flush', async () => {
        const source = windowedHarness([{ role: 'user', data: 'one' }])
        source.dependencies.flushPendingData.mockImplementation(async () => {
            source.invalidateTarget()
        })

        await expect(openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)).rejects.toThrow('Screenshot conversation changed while opening')

        expect(source.dependencies.store.acquireRevision).not.toHaveBeenCalled()
    })

    it('releases a windowed lease when the exact target changes during pinning', async () => {
        const source = windowedHarness([{ role: 'user', data: 'one' }])
        const acquire = source.dependencies.store.acquireRevision as ReturnType<typeof vi.fn>
        acquire.mockImplementation(async () => {
            source.invalidateTarget()
            return source.lease
        })

        await expect(openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)).rejects.toThrow('Screenshot conversation changed while opening')

        expect(source.release).toHaveBeenCalledTimes(1)
    })

    it('reads the exact open-time revision after the live session changes', async () => {
        const source = harness([
            { role: 'user', data: 'open one', chatId: 'one' },
            { role: 'char', data: 'open two', chatId: 'two' },
        ])
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)

        source.session.edit(source.session.locate(0), {
            role: 'user',
            data: 'live changed',
            chatId: 'one',
        })
        source.session.append({ role: 'char', data: 'live appended', chatId: 'three' })

        const job = await lease.createJob(1, 2)

        expect(job.messages.map((message) => message.data)).toEqual(['open one', 'open two'])
        expect(lease.snapshot).toMatchObject({
            characterId: 'character-1',
            chatId: 'chat-1',
            revision: 7,
            sessionVersion: 0,
            totalTurns: 2,
        })
        expect('messages' in lease.snapshot).toBe(false)
        expect(source.release).toHaveBeenCalledTimes(1)
        await lease.close()
        expect(source.release).toHaveBeenCalledTimes(1)
    })

    it('pins the current root when unrelated data advances after the flush', async () => {
        const source = harness([
            { role: 'user', data: 'same conversation', chatId: 'one' },
        ])
        const originalRead = source.lease.readConversationWindow.bind(source.lease)
        const originalReadRoot = source.dependencies.store.readRoot.bind(
            source.dependencies.store,
        )
        const revisionEightLease = {
            ...source.lease,
            revision: 8,
            readConversationWindow: vi.fn(async (input) => {
                const result = await originalRead(input)
                return result ? { ...result, revision: 8 } : null
            }),
        } as unknown as PersistentRevisionLease
        source.dependencies.flushPendingData.mockImplementation(async () => {
            source.dependencies.store.readRoot = vi.fn(async () => ({
                ...await originalReadRoot(),
                revision: 8,
            }))
            source.dependencies.store.acquireRevision = vi.fn(
                async (revision) => {
                    expect(revision).toBe(8)
                    return revisionEightLease
                },
            )
        })

        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)

        expect(source.session.storeRevision).toBe(7)
        expect(lease.snapshot.revision).toBe(8)
        expect((await lease.createJob(1, 1)).messages[0].data).toBe('same conversation')
        expect(source.release).toHaveBeenCalledTimes(1)
    })

    it('recaptures the committed revision after flushing a pending edit', async () => {
        const source = harness([
            { role: 'user', data: 'before flush', chatId: 'one' },
        ])
        source.session.edit(source.session.locate(0), {
            role: 'user',
            data: 'after flush',
            chatId: 'one',
        })
        const revisionEightLease = {
            ...source.lease,
            revision: 8,
            readConversationWindow: vi.fn(async ({ startIndex = 0, limit = 1 }) => {
                const page = source.conversation.message.slice(startIndex, startIndex + limit)
                return {
                    revision: 8,
                    value: {
                        characterId: source.owner.chaId,
                        conversationId: source.conversation.id!,
                        messages: structuredClone(page),
                        startIndex,
                        endIndex: startIndex + page.length,
                        totalMessages: source.conversation.message.length,
                        hasMoreBefore: startIndex > 0,
                        hasMoreAfter: startIndex + page.length < source.conversation.message.length,
                    },
                }
            }),
        } as unknown as PersistentRevisionLease
        const originalReadRoot = source.dependencies.store.readRoot.bind(
            source.dependencies.store,
        )
        source.dependencies.flushPendingData.mockImplementation(async () => {
            source.session.acknowledgePersisted(
                source.session.sessionToken,
                source.session.version,
                8,
            )
            source.dependencies.store.readRoot = vi.fn(async () => ({
                ...await originalReadRoot(),
                revision: 8,
            }))
            source.dependencies.store.acquireRevision = vi.fn(async () => revisionEightLease)
        })

        const screenshot = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)

        expect(screenshot.snapshot).toMatchObject({
            revision: 8,
            sessionVersion: 1,
            totalTurns: 1,
        })
        expect((await screenshot.createJob(1, 1)).messages[0].data).toBe('after flush')
        expect(source.release).toHaveBeenCalledTimes(1)
    })

    it('does not retarget the capture after navigation replaces the active session', async () => {
        const source = harness([
            { role: 'user', data: 'pinned conversation', chatId: 'one' },
        ])
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)
        const replacement = chat([
            { role: 'user', data: 'replacement conversation', chatId: 'replacement' },
        ])
        source.replaceSession(new ActiveConversationSession({
            characterId: source.owner.chaId,
            conversationId: replacement.id!,
            conversation: replacement,
            storeRevision: 8,
        }))
        source.navigate()

        const job = await lease.createJob(1, 1)

        expect(job.messages.map((message) => message.data)).toEqual([
            'pinned conversation',
        ])
        expect(source.release).toHaveBeenCalledTimes(1)
    })

    it('releases an unused pinned revision when the dialog closes', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)

        await lease.close()

        expect(source.release).toHaveBeenCalledTimes(1)
        await expect(lease.createJob(1, 1)).rejects.toThrow(
            'Screenshot source lease is closed',
        )
    })

    it('retries a transient pinned revision release failure', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        source.release
            .mockRejectedValueOnce(new Error('transient release failure'))
            .mockResolvedValueOnce(undefined)
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)

        await lease.close()

        expect(source.release).toHaveBeenCalledTimes(2)
    })

    it('allows close to retry after both immediate release attempts fail', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        const firstError = new Error('first release failure')
        source.release
            .mockRejectedValueOnce(firstError)
            .mockRejectedValueOnce(new Error('second release failure'))
            .mockResolvedValueOnce(undefined)
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)

        await expect(lease.close()).rejects.toBe(firstError)
        await expect(lease.close()).resolves.toBeUndefined()

        expect(source.release).toHaveBeenCalledTimes(3)
    })

    it('preserves cancellation when releasing the pinned revision also fails', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)
        const controller = new AbortController()
        controller.abort()
        source.release
            .mockRejectedValueOnce(new Error('first release failure'))
            .mockRejectedValueOnce(new Error('second release failure'))
            .mockResolvedValueOnce(undefined)

        await expect(lease.createJob(1, 1, controller.signal)).rejects.toMatchObject({
            name: 'AbortError',
        })
        await expect(lease.close()).resolves.toBeUndefined()

        expect(source.release).toHaveBeenCalledTimes(3)
    })

    it('reports release failure after a successful job and permits a close retry', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        const firstError = new Error('first release failure')
        source.release
            .mockRejectedValueOnce(firstError)
            .mockRejectedValueOnce(new Error('second release failure'))
            .mockResolvedValueOnce(undefined)
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)

        await expect(lease.createJob(1, 1)).rejects.toBe(firstError)
        await expect(lease.close()).resolves.toBeUndefined()

        expect(source.release).toHaveBeenCalledTimes(3)
    })

    it('releases the pinned revision when materialization is cancelled', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)
        const controller = new AbortController()
        controller.abort()

        await expect(lease.createJob(1, 1, controller.signal)).rejects.toMatchObject({
            name: 'AbortError',
        })
        expect(source.release).toHaveBeenCalledTimes(1)
    })

    it('releases the pinned revision when owner identity changes while opening', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        source.dependencies.flushPendingData.mockImplementation(async () => {
            source.replaceSession(null)
        })

        await expect(openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)).rejects.toThrow('Screenshot conversation changed while opening')

        expect(source.release).toHaveBeenCalledTimes(0)
    })

    it('releases an acquired revision when final session evidence changes', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        const readEvidence = source.lease.readConversationWindow as ReturnType<typeof vi.fn>
        readEvidence.mockImplementationOnce(async () => {
            source.replaceSession(null)
            return {
                revision: 7,
                value: {
                    characterId: 'character-1',
                    conversationId: 'chat-1',
                    messages: [{ role: 'user', data: 'open' }],
                    startIndex: 0,
                    endIndex: 1,
                    totalMessages: 1,
                    hasMoreBefore: false,
                    hasMoreAfter: false,
                },
            }
        })

        await expect(openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)).rejects.toThrow('Screenshot conversation changed while opening')

        expect(source.release).toHaveBeenCalledTimes(1)
    })

    it('releases the pinned revision after a range read error', async () => {
        const source = harness([{ role: 'user', data: 'open' }])
        const lease = await openChatScreenshotSourceLease({
            characterId: source.owner.chaId,
            chatId: source.conversation.id!,
            renderContext: renderContext(source.owner),
        }, source.dependencies)
        const readRange = source.lease.readConversationWindow as ReturnType<typeof vi.fn>
        readRange.mockRejectedValueOnce(new Error('range failed'))

        await expect(lease.createJob(1, 1)).rejects.toThrow('range failed')
        expect(source.release).toHaveBeenCalledTimes(1)
    })
})
