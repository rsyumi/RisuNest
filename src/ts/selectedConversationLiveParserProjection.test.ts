import { describe, expect, test, vi } from 'vitest'
import type { ConversationViewportRow } from './conversationViewportSource'
import type { ProcessScriptCaptureContext } from './process/scripts'
import {
    bindCompleteLiveParserContextAuthority,
    collectLiveChatParserUnsafeDependencies,
    createSelectedConversationLiveParserProjectionResolver,
    type SelectedConversationLiveParserProjectionDependencies,
} from './selectedConversationLiveParserProjection'
import type { Chat, character, Database, Message } from './storage/database.svelte'

const CHARACTER_ID = 'character-id'
const CONVERSATION_ID = 'conversation-id'

function message(index: number, data = `message-${index}`): Message {
    return {
        role: index % 2 === 0 ? 'char' : 'user',
        data,
        chatId: `message-${index}`,
    }
}

function conversation(messages: Message[]): Chat {
    return {
        id: CONVERSATION_ID,
        message: messages,
    } as Chat
}

function owner(chat: Chat): character {
    return {
        type: 'character',
        chaId: CHARACTER_ID,
        name: 'Character',
        chatPage: 0,
        chats: [chat],
        customscript: [],
        triggerscript: [],
        additionalAssets: [],
        emotionImages: [],
    } as unknown as character
}

function context(currentCharacter: character): ProcessScriptCaptureContext {
    return {
        presetRegex: [],
        moduleRegexScripts: [],
        moduleAssets: [],
        dynamicAssets: false,
        dynamicAssetsEditDisplay: false,
        parserContext: {
            database: { characters: [currentCharacter] } as Database,
            character: currentCharacter,
            userName: 'User',
            personaPrompt: '',
            modules: [],
            moduleLorebooks: [],
            selectedCharID: 0,
            chatVariables: {},
            globalChatVariables: {},
            currentTime: 1,
        },
    }
}

function row(index: number, value: Message): ConversationViewportRow {
    return {
        key: `row-${index}` as ConversationViewportRow['key'],
        absoluteIndex: index,
        message: value,
        sourceVersion: 0,
    }
}

function harness(options: {
    messages?: Message[]
    unsafeDependencies?: SelectedConversationLiveParserProjectionDependencies['unsafeDependencies']
    onAcquire?: () => void
    metadataOnly?: boolean
    advanceRevisionDuringAcquire?: boolean
    navigateDuringAcquire?: boolean
} = {}) {
    const messages = options.messages ?? Array.from({ length: 8 }, (_, index) => message(index))
    let currentChat = conversation([])
    let forbiddenHistoryReads = 0
    if (options.metadataOnly) {
        Object.defineProperty(currentChat, 'message', {
            get() {
                forbiddenHistoryReads += 1
                throw new Error('Metadata-only history must not be read')
            },
            enumerable: false,
        })
    }
    let currentCharacter = owner(currentChat)
    let windowed = true
    let releases = 0
    const boundedContextMessageCounts: number[] = []
    const completeContextMessageCounts: number[] = []
    const target = {
        characterId: CHARACTER_ID,
        conversationId: CONVERSATION_ID,
        navigationGeneration: 1,
        storeRevision: 4,
    } as any
    const dependencies: SelectedConversationLiveParserProjectionDependencies = {
        runtime: {
            store: {
                async readConversationWindow({ characterId, conversationId, startIndex, limit }) {
                    const selected = messages.slice(startIndex, startIndex + limit)
                    return {
                        revision: 4,
                        value: {
                            characterId,
                            conversationId,
                            startIndex,
                            endIndex: startIndex + selected.length,
                            totalMessages: messages.length,
                            messages: selected,
                            hasMoreBefore: startIndex > 0,
                            hasMoreAfter: startIndex + selected.length < messages.length,
                        },
                    }
                },
            } as any,
            captureSelectedConversationTarget: () => ({ ...target }),
            captureSelectedConversationAuthority: () => windowed
                ? {
                    kind: 'windowed',
                    characterId: CHARACTER_ID,
                    conversationId: CONVERSATION_ID,
                    storeRevision: 4,
                    totalMessages: messages.length,
                    sessionToken: 'session',
                    persistedSessionVersion: 0,
                    sessionVersion: 0,
                } as any
                : null,
            acquireCompleteConversation: vi.fn(async () => {
                options.onAcquire?.()
                if (options.advanceRevisionDuringAcquire) target.storeRevision += 1
                if (options.navigateDuringAcquire) target.navigationGeneration += 1
                windowed = false
                currentChat = conversation(messages)
                currentCharacter = owner(currentChat)
                return {
                    reason: 'live-render',
                    target: { ...target },
                    session: {
                        totalMessages: messages.length,
                        storeRevision: target.storeRevision,
                        isActive: true,
                        matchesConversation: (characterId: string, chat: Chat) => (
                            characterId === CHARACTER_ID && chat === currentChat
                        ),
                        locate: (absoluteIndex: number) => ({ absoluteIndex }),
                        readMessage: (locator: { absoluteIndex: number }) => messages[locator.absoluteIndex],
                    } as any,
                    release: () => releases += 1,
                }
            }),
        },
        captureCurrent: () => ({ character: currentCharacter, conversation: currentChat }),
        createBoundedContextSeed: ({ character }) => {
            boundedContextMessageCounts.push(character.chats[character.chatPage].message.length)
            return context(character as character)
        },
        createCompleteContext: ({ character }) => {
            completeContextMessageCounts.push(character.chats[character.chatPage].message.length)
            return context(character as character)
        },
        parserSource: () => ({ guiHTML: '' }),
        unsafeDependencies: options.unsafeDependencies ?? (() => []),
        maxProjectionMessages: 6,
    }
    return {
        resolver: createSelectedConversationLiveParserProjectionResolver(dependencies),
        acquire: dependencies.runtime.acquireCompleteConversation,
        releases: () => releases,
        boundedContextMessageCounts,
        completeContextMessageCounts,
        forbiddenHistoryReads: () => forbiddenHistoryReads,
    }
}

describe('selected conversation live parser projection', () => {
    test('keeps a plain greeting windowed without reading metadata-only history', async () => {
        const testHarness = harness({ metadataOnly: true })
        const lease = await testHarness.resolver.acquireConversationStart!({
            greeting: 'Synthetic greeting',
            totalMessages: 8,
        })
        expect(lease).toBeNull()
        expect(testHarness.acquire).not.toHaveBeenCalled()
        expect(testHarness.forbiddenHistoryReads()).toBe(0)
    })

    test('holds complete authority for a Lua greeting even when there are no message rows', async () => {
        const testHarness = harness({
            messages: [],
            metadataOnly: true,
            unsafeDependencies: () => ['lua'],
        })
        const lease = await testHarness.resolver.acquireConversationStart!({
            greeting: 'Synthetic greeting',
            totalMessages: 0,
        })
        expect(testHarness.acquire).toHaveBeenCalledTimes(1)
        expect(testHarness.forbiddenHistoryReads()).toBe(0)
        expect(testHarness.releases()).toBe(0)
        expect(lease).not.toBeNull()
        lease!.release()
        lease!.release()
        expect(testHarness.releases()).toBe(1)
    })

    test('accepts the same empty conversation retargeted after promotion flushes pending data', async () => {
        const testHarness = harness({
            messages: [],
            unsafeDependencies: () => ['lua'],
            advanceRevisionDuringAcquire: true,
        })
        const lease = await testHarness.resolver.acquireConversationStart!({
            greeting: 'Synthetic greeting',
            totalMessages: 0,
        })
        expect(lease).not.toBeNull()
        expect(testHarness.releases()).toBe(0)
        lease!.release()
        expect(testHarness.releases()).toBe(1)
    })

    test('rejects a greeting retargeted by navigation even if character and chat IDs match', async () => {
        const testHarness = harness({
            messages: [],
            unsafeDependencies: () => ['lua'],
            advanceRevisionDuringAcquire: true,
            navigateDuringAcquire: true,
        })
        await expect(
            testHarness.resolver.acquireConversationStart!({
                greeting: 'Synthetic greeting',
                totalMessages: 0,
            }),
        ).rejects.toThrow('Chat parser history projection became stale')
        expect(testHarness.releases()).toBe(1)
    })

    test.each(['{{history}}', '{{previouschatlog::0}}'])(
        'provides complete authority for greeting history expressions: %s',
        async (greeting) => {
            const testHarness = harness({ metadataOnly: true })
            const lease = await testHarness.resolver.acquireConversationStart!({
                greeting,
                totalMessages: 8,
            })
            expect(testHarness.acquire).toHaveBeenCalledTimes(1)
            expect(testHarness.forbiddenHistoryReads()).toBe(0)
            lease!.release()
            expect(testHarness.releases()).toBe(1)
        },
    )

    test('releases a greeting lease when the component is cancelled during promotion', async () => {
        const controller = new AbortController()
        const testHarness = harness({
            unsafeDependencies: () => ['lua'],
            onAcquire: () => controller.abort(),
        })
        await expect(
            testHarness.resolver.acquireConversationStart!({
                greeting: 'Synthetic greeting',
                totalMessages: 8,
                signal: controller.signal,
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(testHarness.releases()).toBe(1)
    })

    test('releases a greeting lease when navigation supersedes its request', async () => {
        let current = true
        const testHarness = harness({
            unsafeDependencies: () => ['lua'],
            onAcquire: () => {
                current = false
            },
        })
        await expect(
            testHarness.resolver.acquireConversationStart!({
                greeting: 'Synthetic greeting',
                totalMessages: 8,
                isCurrent: () => current,
            }),
        ).rejects.toThrow('Chat parser history projection became stale')
        expect(testHarness.releases()).toBe(1)
    })

    test('rejects a greeting whose message count changed while acquiring history', async () => {
        const testHarness = harness({ unsafeDependencies: () => ['lua'] })
        await expect(
            testHarness.resolver.acquireConversationStart!({
                greeting: 'Synthetic greeting',
                totalMessages: 9,
            }),
        ).rejects.toThrow('Chat parser history projection became stale')
        expect(testHarness.releases()).toBe(1)
    })

    test('binds the production complete context to promoted shared authority without mutating the bounded seed', () => {
        const boundedCharacter = owner(conversation([]))
        const boundedSeed = context(boundedCharacter)
        const completeConversation = conversation(Array.from({ length: 4 }, (_, index) => message(index)))
        const completeCharacter = owner(completeConversation)

        const complete = bindCompleteLiveParserContextAuthority(boundedSeed, {
            character: completeCharacter,
            conversation: completeConversation,
        })

        expect(boundedSeed.parserContext.character).toBe(boundedCharacter)
        expect(boundedCharacter.chats[0].message).toEqual([])
        expect(complete.parserContext.character).toBe(completeCharacter)
        expect(complete.parserContext.database.characters[0]).toBe(completeCharacter)
        expect(complete.parserContext.character.chats[0]).toBe(completeConversation)
        expect(complete.parserContext.database.characters[0].chats[0]).toBe(completeConversation)
        expect(completeConversation.message).toHaveLength(4)
        expect(completeConversation.message.every((_, index) => index in completeConversation.message)).toBe(true)
    })

    test('classifies live-only Lua, display trigger, plugin, and inject dependencies', () => {
        expect(collectLiveChatParserUnsafeDependencies({
            triggers: [
                { type: 'manual', effect: [{ type: 'triggerlua' }] },
                { type: 'display', effect: [] },
            ] as any,
            pluginV2EditDisplay: true,
            regexScripts: [{
                comment: '',
                type: 'editdisplay',
                in: 'match',
                out: '@@inject replacement',
            }],
        })).toEqual(['lua', 'display-trigger', 'plugin-v2', 'inject'])
    })

    test('returns a bounded context with absolute and projected parser indices', async () => {
        const testHarness = harness()

        const result = await testHarness.resolver.resolve({
            row: row(7, message(7)),
            totalMessages: 8,
        })

        expect(result.kind).toBe('bounded')
        if (result.kind !== 'bounded') throw new Error('Expected bounded projection')
        expect(result.chatID).toBe(7)
        expect(result.projectedChatID).toBe(4)
        expect(result.context?.parserContext.historyOffset).toBe(3)
        expect(
            result.context?.parserContext.character.chats[0].message.map((entry) => entry.data),
        ).toEqual(['message-3', 'message-4', 'message-5', 'message-6', 'message-7'])
        expect(testHarness.acquire).not.toHaveBeenCalled()
        expect(testHarness.boundedContextMessageCounts).toEqual([0])
    })

    test('promotes unsafe display parsing and exposes an idempotent complete lease', async () => {
        const testHarness = harness({ unsafeDependencies: () => ['lua'] })

        const promoted = await testHarness.resolver.resolve({
            row: row(7, message(7)),
            totalMessages: 8,
        })

        expect(promoted.kind).toBe('complete')
        if (promoted.kind !== 'complete') throw new Error('Expected complete projection')
        expect(testHarness.acquire).toHaveBeenCalledTimes(1)
        expect(testHarness.boundedContextMessageCounts).toEqual([0])
        expect(testHarness.completeContextMessageCounts).toEqual([8])
        promoted.release()
        promoted.release()
        expect(testHarness.releases()).toBe(1)

        const mounted = await testHarness.resolver.resolve({
            row: row(7, message(7)),
            totalMessages: 8,
        })
        expect(mounted.kind).toBe('complete')
        if (mounted.kind !== 'complete') throw new Error('Expected complete projection')
        expect(testHarness.acquire).toHaveBeenCalledTimes(2)
        mounted.release()
        expect(testHarness.releases()).toBe(2)
    })

    test('releases a promoted lease when the request becomes stale', async () => {
        let current = true
        const testHarness = harness({
            unsafeDependencies: () => ['display-trigger'],
            onAcquire: () => {
                current = false
            },
        })

        await expect(testHarness.resolver.resolve({
            row: row(7, message(7)),
            totalMessages: 8,
            isCurrent: () => current,
        })).rejects.toMatchObject({ name: 'ChatParserHistoryProjectionStaleError' })
        expect(testHarness.releases()).toBe(1)
    })
})
