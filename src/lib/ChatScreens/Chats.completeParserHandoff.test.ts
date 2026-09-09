// @vitest-environment happy-dom

import { afterEach, describe, expect, test, vi } from 'vitest'
import { mount, unmount } from 'svelte'
import type { character, Message } from 'src/ts/storage/database.svelte'
import { ActiveConversationSession } from 'src/ts/storage/activeConversationSession'
import {
    PersistentConversationViewportSource,
    SynchronousSessionConversationViewportSource,
    type ConversationViewportSource,
} from 'src/ts/conversationViewportSource'
import { createSelectedConversationLiveParserProjectionResolver } from 'src/ts/selectedConversationLiveParserProjection'
import type { ProcessScriptCaptureContext } from 'src/ts/process/scripts'

vi.mock('src/ts/characters', () => ({
    getCharImage: async (source: string) => source,
}))
vi.mock('src/ts/globalApi.svelte', () => ({
    chatFoldedStateMessageIndex: { index: -1 },
}))
vi.mock('src/ts/ui/yieldToUi', () => ({
    yieldToMainThread: () => Promise.resolve(),
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { streamingDisplayOptimizationMode: 'balanced' } },
        selectedCharID: writable(0),
        ReloadChatPointer: writable({}),
        ReloadGUIPointer: writable(0),
        createSimpleCharacter: (char: character) => ({
            type: 'simple',
            chaId: char.chaId,
            virtualscript: char.virtualscript,
            customscript: char.customscript,
            additionalAssets: char.additionalAssets,
            emotionImages: char.emotionImages,
            triggerscript: char.triggerscript,
        }),
    }
})
vi.mock('./Chat.svelte', async () => ({
    default: (await import('./ChatMountProbe.test.svelte')).default,
}))
vi.mock('./CreatorQuote.svelte', async () => ({
    default: (await import('./ChatMountProbe.test.svelte')).default,
}))

import { ReloadGUIPointer } from 'src/ts/stores.svelte'
import ChatsHarness from './ChatsHarness.test.svelte'
import { resetChatMountProbe } from './chatMountProbe'

interface HarnessInstance {
    getCurrentCharacter(): character
    jumpTo(index: number, options?: { align?: 'start' }): Promise<boolean>
    setViewportNavigationGeneration(generation: number): void
    switchCharacter(character: character, messages: Message[]): void
    switchCharacterAndSource(
        character: character,
        source: ConversationViewportSource,
    ): void
}

function contextFor(character: character): ProcessScriptCaptureContext {
    return {
        presetRegex: [],
        moduleRegexScripts: [],
        moduleAssets: [],
        dynamicAssets: false,
        dynamicAssetsEditDisplay: false,
        parserContext: {
            database: { characters: [character] } as any,
            character,
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

function probeElements(target: HTMLElement): HTMLElement[] {
    return [
        ...target.querySelectorAll<HTMLElement>('[data-chat-probe]'),
    ].filter((element) => element.dataset.index !== '-1')
}
function makeMessage(index: number): Message {
    return {
        role: index % 2 === 0 ? 'char' : 'user',
        data: `message-${index}`,
        chatId: `message-id-${index}`,
    }
}

function makeCharacter(messages: Message[]): character {
    return {
        type: 'character',
        name: 'Character',
        image: 'character.png',
        chaId: 'character-id',
        chatPage: 0,
        chats: [
            {
                id: 'chat-room-id',
                message: messages,
                isStreaming: false,
                activeStreamingDisplayOptimizationMode: 'balanced',
            },
        ],
        firstMessage: 'first greeting',
        alternateGreetings: [],
        creatorNotes: '',
        removedQuotes: false,
        customscript: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: [],
    } as unknown as character
}

function makeMetadataOnlyCharacter(): character {
    const conversation = {
        id: 'chat-room-id',
        isStreaming: false,
        activeStreamingDisplayOptimizationMode: 'balanced',
    } as character['chats'][number]
    Object.defineProperty(conversation, 'message', {
        get() {
            throw new Error('metadata-only conversation body was accessed')
        },
    })
    return { ...makeCharacter([]), chats: [conversation] } as character
}

function makePersistentViewportSource(messages: readonly Message[]) {
    return new PersistentConversationViewportSource({
        reader: {
            async readConversationWindow({
                characterId,
                conversationId,
                startIndex,
                limit,
            }) {
                const endIndex = Math.min(messages.length, startIndex + limit)
                return {
                    revision: 1,
                    value: {
                        characterId,
                        conversationId,
                        startIndex: Math.min(startIndex, messages.length),
                        endIndex,
                        totalMessages: messages.length,
                        messages: messages.slice(startIndex, endIndex),
                        hasMoreBefore: startIndex > 0,
                        hasMoreAfter: endIndex < messages.length,
                    },
                }
            },
        },
        characterId: 'character-id',
        conversationId: 'chat-room-id',
        revision: 1,
        totalMessages: messages.length,
        rowBudget: 64,
    })
}

describe('Chats complete parser source handoff', () => {
    let mounted: ReturnType<typeof mount> | undefined
    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
    })

    test.each([
        {
            reload: false,
            totalMessages: 0,
            advanceRevisionDuringAcquire: false,
        },
        { reload: true, totalMessages: 0, advanceRevisionDuringAcquire: false },
        {
            reload: false,
            totalMessages: 2,
            advanceRevisionDuringAcquire: false,
        },
        { reload: true, totalMessages: 2, advanceRevisionDuringAcquire: false },
        { reload: false, totalMessages: 0, advanceRevisionDuringAcquire: true },
    ])(
        'settles a complete parser promotion with publication reload=$reload, $totalMessages messages, and revision advance=$advanceRevisionDuringAcquire',
        async ({ reload, totalMessages, advanceRevisionDuringAcquire }) => {
            resetChatMountProbe()
            ReloadGUIPointer.set(0)
            const target = document.createElement('div')
            document.body.appendChild(target)
            const messages = Array.from({ length: totalMessages }, (_, index) =>
                makeMessage(index),
            )
            let windowed = true
            let promotions = 0
            let demotions = 0
            let stopped = false
            let currentCharacter = makeMetadataOnlyCharacter()
            let currentSource: ConversationViewportSource =
                makePersistentViewportSource(messages)
            let currentSession: ActiveConversationSession | null = null
            const initialSource = currentSource
            const initialCharacter = currentCharacter
            const selection = {
                characterId: 'character-id',
                conversationId: 'chat-room-id',
                navigationGeneration: 1,
                storeRevision: 1,
            }
            const current = () => {
                const character =
                    (mounted as HarnessInstance)?.getCurrentCharacter() ??
                    currentCharacter
                return { character, conversation: character.chats[0] } as any
            }
            const publish = (
                character: character,
                source: ConversationViewportSource,
            ) => {
                currentCharacter = character
                currentSource.dispose()
                currentSource = source
                ;(mounted as HarnessInstance).switchCharacterAndSource(
                    character,
                    source,
                )
                if (reload) ReloadGUIPointer.set(promotions + demotions)
            }
            const resolver =
                createSelectedConversationLiveParserProjectionResolver({
                    runtime: {
                        store: {
                            readConversationWindow: async ({
                                startIndex,
                                limit,
                            }: {
                                startIndex: number
                                limit: number
                            }) => ({
                                revision: 1,
                                value: {
                                    characterId: selection.characterId,
                                    conversationId: selection.conversationId,
                                    startIndex,
                                    endIndex: Math.min(
                                        messages.length,
                                        startIndex + limit,
                                    ),
                                    totalMessages: messages.length,
                                    messages: messages.slice(
                                        startIndex,
                                        startIndex + limit,
                                    ),
                                    hasMoreBefore: startIndex > 0,
                                    hasMoreAfter:
                                        startIndex + limit < messages.length,
                                },
                            }),
                        } as any,
                        captureSelectedConversationTarget: () =>
                            ({ ...selection }) as any,
                        captureSelectedConversationAuthority: () =>
                            windowed
                                ? ({ ...selection, totalMessages } as any)
                                : null,
                        acquireCompleteConversation: async () => {
                            if (stopped)
                                throw new Error('bounded reproduction stopped')
                            if (windowed) {
                                // Production promotion flushes pending settings/root saves before reading history.
                                if (advanceRevisionDuringAcquire)
                                    selection.storeRevision += 1
                                promotions++
                                if (promotions > 8) {
                                    stopped = true
                                    throw new Error('promotion cycle')
                                }
                                windowed = false
                                const completeCharacter = makeCharacter([
                                    ...messages,
                                ])
                                ;(mounted as HarnessInstance).switchCharacter(
                                    completeCharacter,
                                    messages,
                                )
                                // The production working set creates its session from the published Svelte proxy.
                                const publishedCharacter = (
                                    mounted as HarnessInstance
                                ).getCurrentCharacter()
                                const session = new ActiveConversationSession({
                                    characterId: selection.characterId,
                                    conversationId: selection.conversationId,
                                    conversation: publishedCharacter.chats[0],
                                    storeRevision: selection.storeRevision,
                                    onPinReleased: () =>
                                        queueMicrotask(() => {
                                            if (
                                                stopped ||
                                                windowed ||
                                                currentSession !== session ||
                                                session.activePinReasons.some(
                                                    (reason) =>
                                                        reason !== 'viewport',
                                                )
                                            )
                                                return
                                            demotions++
                                            windowed = true
                                            publish(
                                                makeMetadataOnlyCharacter(),
                                                makePersistentViewportSource(
                                                    messages,
                                                ),
                                            )
                                        }),
                                })
                                expect(
                                    session.matchesConversation(
                                        selection.characterId,
                                        completeCharacter.chats[0],
                                    ),
                                ).toBe(false)
                                expect(
                                    session.matchesConversation(
                                        selection.characterId,
                                        publishedCharacter.chats[0],
                                    ),
                                ).toBe(true)
                                currentSession = session
                                const source =
                                    new SynchronousSessionConversationViewportSource(
                                        { session, captureCurrent: current },
                                    )
                                publish(publishedCharacter, source)
                                await Promise.resolve()
                            }
                            const session = currentSession!
                            const pin = session.acquirePin('compatibility')
                            return {
                                reason: 'live-chat-parser',
                                session,
                                target: { ...selection } as any,
                                release: () => pin.release(),
                            }
                        },
                    },
                    maxProjectionMessages: 2,
                    captureCurrent: current,
                    createBoundedContextSeed: () =>
                        contextFor(makeCharacter([])),
                    createCompleteContext: ({ character }) =>
                        contextFor(character as character),
                    parserSource: () => '',
                    unsafeDependencies: () => ['lua'],
                })
            const acquireGreeting = vi.fn(resolver.acquireConversationStart!)
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialCharacter,
                    initialViewportSource: initialSource,
                    parserProjectionResolver: resolver,
                    acquireConversationStartParserLease: acquireGreeting,
                },
            })
            try {
                await vi.waitFor(() =>
                    expect({
                        promotions,
                        demotions,
                        windowed,
                        rows: probeElements(target).length,
                    }).toEqual({
                        promotions: 1,
                        demotions: 0,
                        windowed: false,
                        rows: totalMessages,
                    }),
                )
                await vi.waitFor(() =>
                    expect(
                        target.querySelector(
                            '[data-chat-probe][data-index="-1"]',
                        ),
                    ).not.toBeNull(),
                )
                // Flush queued release/demotion work, including when only the greeting owns a lease.
                await new Promise((resolve) => setTimeout(resolve, 0))
                expect(promotions).toBe(1)
                expect(demotions).toBe(0)
                // An explicit global reload reclassifies module/parser dependencies once.
                expect(acquireGreeting).toHaveBeenCalledTimes(reload ? 2 : 1)
            } finally {
                stopped = true
            }
        },
    )

    test.each([
        'unchanged',
        'target-before-proof',
        'target-after-proof',
        'count-after-proof',
        'navigation-before-proof',
    ] as const)(
        'resumes a greeting-triggered first-message jump only for an unchanged handoff: %s',
        async (handoffChange) => {
            resetChatMountProbe()
            ReloadGUIPointer.set(0)
            const target = document.createElement('div')
            document.body.appendChild(target)
            const messages = Array.from({ length: 128 }, (_, index) =>
                makeMessage(index),
            )
            const selection = {
                characterId: 'character-id',
                conversationId: 'chat-room-id',
                navigationGeneration: 1,
                storeRevision: 1,
            }
            const withGreeting = (character: character) => ({
                ...character,
                firstMessage: '{{history}}',
            })
            const initialCharacter = withGreeting(makeMetadataOnlyCharacter())
            let currentSource: ConversationViewportSource =
                makePersistentViewportSource(messages)
            const initialSource = currentSource
            let currentSession: ActiveConversationSession | null = null
            let windowed = true
            let promotions = 0
            let demotions = 0
            let stopped = false
            let appliedChanges = 0
            const current = () => {
                const character =
                    (mounted as HarnessInstance)?.getCurrentCharacter() ??
                    initialCharacter
                return { character, conversation: character.chats[0] } as any
            }
            const publish = (
                character: character,
                source: ConversationViewportSource,
            ) => {
                currentSource.dispose()
                currentSource = source
                ;(mounted as HarnessInstance).switchCharacterAndSource(
                    character,
                    source,
                )
            }
            const resolver = createSelectedConversationLiveParserProjectionResolver(
                {
                    runtime: {
                        store: {
                            readConversationWindow: async ({
                                startIndex,
                                limit,
                            }: {
                                startIndex: number
                                limit: number
                            }) => ({
                                revision: 1,
                                value: {
                                    characterId: selection.characterId,
                                    conversationId: selection.conversationId,
                                    startIndex,
                                    endIndex: Math.min(
                                        messages.length,
                                        startIndex + limit,
                                    ),
                                    totalMessages: messages.length,
                                    messages: messages.slice(
                                        startIndex,
                                        startIndex + limit,
                                    ),
                                    hasMoreBefore: startIndex > 0,
                                    hasMoreAfter:
                                        startIndex + limit < messages.length,
                                },
                            }),
                        } as any,
                        captureSelectedConversationTarget: () =>
                            ({ ...selection }) as any,
                        captureSelectedConversationAuthority: () =>
                            windowed
                                ? ({
                                      ...selection,
                                      totalMessages: messages.length,
                                  } as any)
                                : null,
                        acquireCompleteConversation: async () => {
                            if (stopped)
                                throw new Error('bounded reproduction stopped')
                            if (windowed) {
                                promotions++
                                if (promotions > 8) {
                                    stopped = true
                                    throw new Error('promotion cycle')
                                }
                                windowed = false
                                const completeCharacter = withGreeting(
                                    makeCharacter([...messages]),
                                )
                                if (handoffChange === 'target-before-proof') {
                                    completeCharacter.chats[0].message[0] = {
                                        ...messages[0],
                                        data: 'changed before source handoff proof',
                                    }
                                    appliedChanges++
                                }
                                ;(mounted as HarnessInstance).switchCharacter(
                                    completeCharacter,
                                    messages,
                                )
                                const publishedCharacter = (
                                    mounted as HarnessInstance
                                ).getCurrentCharacter()
                                const session = new ActiveConversationSession({
                                    characterId: selection.characterId,
                                    conversationId: selection.conversationId,
                                    conversation: publishedCharacter.chats[0],
                                    storeRevision: selection.storeRevision,
                                    onPinReleased: () =>
                                        queueMicrotask(() => {
                                            if (
                                                stopped ||
                                                windowed ||
                                                currentSession !== session ||
                                                session.activePinReasons.some(
                                                    (reason) =>
                                                        reason !== 'viewport',
                                                )
                                            )
                                                return
                                            demotions++
                                            windowed = true
                                            publish(
                                                withGreeting(
                                                    makeMetadataOnlyCharacter(),
                                                ),
                                                makePersistentViewportSource(
                                                    messages,
                                                ),
                                            )
                                        }),
                                })
                                expect(
                                    session.matchesConversation(
                                        selection.characterId,
                                        completeCharacter.chats[0],
                                    ),
                                ).toBe(false)
                                expect(
                                    session.matchesConversation(
                                        selection.characterId,
                                        publishedCharacter.chats[0],
                                    ),
                                ).toBe(true)
                                currentSession = session
                                const nextSource =
                                    new SynchronousSessionConversationViewportSource(
                                        {
                                            session,
                                            captureCurrent: current,
                                        },
                                    )
                                if (
                                    handoffChange === 'target-after-proof' ||
                                    handoffChange === 'count-after-proof'
                                ) {
                                    const captureTarget =
                                        nextSource.captureMessageTarget.bind(
                                            nextSource,
                                        )
                                    nextSource.captureMessageTarget = (key) => {
                                        const captured = captureTarget(key)
                                        if (
                                            captured?.absoluteIndex === 0 &&
                                            appliedChanges === 0
                                        ) {
                                            expect(captured.message).toEqual(
                                                messages[0],
                                            )
                                            expect(
                                                nextSource.snapshot().version,
                                            ).toBe(0)
                                            appliedChanges++
                                            // The returned target proves an unchanged representation, but the
                                            // session changes before the suspended jump resumes on that source.
                                            queueMicrotask(() => {
                                                if (
                                                    handoffChange ===
                                                    'count-after-proof'
                                                ) {
                                                    session.append(
                                                        makeMessage(
                                                            messages.length,
                                                        ),
                                                    )
                                                } else {
                                                    session.edit(
                                                        session.locate(0),
                                                        {
                                                            ...messages[0],
                                                            data: 'changed after source handoff proof',
                                                        },
                                                    )
                                                }
                                            })
                                        }
                                        return captured
                                    }
                                }
                                if (handoffChange === 'navigation-before-proof') {
                                    selection.navigationGeneration++
                                    ;(
                                        mounted as HarnessInstance
                                    ).setViewportNavigationGeneration(
                                        selection.navigationGeneration,
                                    )
                                    appliedChanges++
                                }
                                publish(publishedCharacter, nextSource)
                                await Promise.resolve()
                            }
                            const session = currentSession!
                            const pin = session.acquirePin('compatibility')
                            return {
                                reason: 'live-chat-parser',
                                session,
                                target: { ...selection } as any,
                                release: () => pin.release(),
                            }
                        },
                    },
                    maxProjectionMessages: 8,
                    captureCurrent: current,
                    createBoundedContextSeed: () =>
                        contextFor(withGreeting(makeCharacter([]))),
                    createCompleteContext: ({ character }) =>
                        contextFor(character as character),
                    parserSource: () => '',
                    unsafeDependencies: () => [],
                },
            )
            const acquireGreeting = vi.fn(resolver.acquireConversationStart!)
            const alignedRows: Array<{
                index: string | undefined
                block: string | undefined
            }> = []
            const scrollIntoView = vi
                .spyOn(HTMLElement.prototype, 'scrollIntoView')
                .mockImplementation(function (this: HTMLElement, options) {
                    alignedRows.push({
                        index: this.querySelector<HTMLElement>('[data-chat-probe]')
                            ?.dataset.index,
                        block:
                            typeof options === 'object' ? options.block : undefined,
                    })
                })
            mounted = mount(ChatsHarness, {
                target,
                props: {
                    initialCharacter,
                    initialViewportSource: initialSource,
                    initialViewportNavigationGeneration:
                        selection.navigationGeneration,
                    parserProjectionResolver: resolver,
                    acquireConversationStartParserLease: acquireGreeting,
                },
            })
            try {
                await vi.waitFor(() =>
                    expect(probeElements(target)).toHaveLength(64),
                )
                expect({ promotions, demotions, windowed }).toEqual({
                    promotions: 0,
                    demotions: 0,
                    windowed: true,
                })
                expect(
                    target.querySelector('[data-chat-probe][data-index="-1"]'),
                ).toBeNull()
                const completed = await (mounted as HarnessInstance).jumpTo(0, {
                    align: 'start',
                })
                await vi.waitFor(() =>
                    expect({ promotions, demotions, windowed }).toEqual({
                        promotions: 1,
                        demotions: 0,
                        windowed: false,
                    }),
                )
                if (handoffChange === 'unchanged') {
                    expect(completed).toBe(true)
                    expect(
                        target.querySelector('[data-chat-probe][data-index="0"]'),
                    ).not.toBeNull()
                    expect(alignedRows).toContainEqual({
                        index: '0',
                        block: 'start',
                    })
                } else {
                    expect(appliedChanges).toBe(1)
                    expect(completed).toBe(false)
                    expect(alignedRows).toEqual([])
                    if (
                        handoffChange === 'target-after-proof' ||
                        handoffChange === 'count-after-proof'
                    ) {
                        expect(currentSession?.version).toBe(1)
                    }
                    if (handoffChange === 'count-after-proof') {
                        expect(currentSession?.totalMessages).toBe(129)
                    }
                }
                await new Promise((resolve) => setTimeout(resolve, 0))
                expect({ promotions, demotions }).toEqual({
                    promotions: 1,
                    // A changed count invalidates the greeting's original history request.
                    // Releasing that aborted lease permits the fixture to demote once.
                    demotions: handoffChange === 'count-after-proof' ? 1 : 0,
                })
                expect(acquireGreeting).toHaveBeenCalledTimes(1)
            } finally {
                stopped = true
                scrollIntoView.mockRestore()
            }
        },
    )
})
