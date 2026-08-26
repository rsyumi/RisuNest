import { describe, expect, it, vi } from 'vitest'
import type { Chat, Database, Message, character } from './storage/database.svelte'
import { ActiveConversationSession } from './storage/activeConversationSession'
import type {
    PersistentDataStore,
    PersistentRevisionLease,
} from './storage/persistentDataStore'
import { openChatScreenshotSourceLease } from './chatScreenshotSourceLease'
import type { ChatScreenshotRenderContext } from './chatScreenshotRange'

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

describe('chat screenshot source lease', () => {
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
