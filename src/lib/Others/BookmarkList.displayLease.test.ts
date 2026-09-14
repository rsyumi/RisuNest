import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { createMetadataOnlySelectedConversation } from 'src/ts/storage/selectedConversationLifecycle'
import { createBookmarkDisplayDatabase } from './BookmarkDisplayState.test.svelte'

const state = vi.hoisted(() => ({
    database: { characters: [] as any[], presetRegex: [] },
    reactiveDatabase: null as null | {
        current: { characters: any[]; presetRegex: any[] }
    },
    selectedIndex: 0,
    selection: null as any,
    acquireCompleteConversation: vi.fn(),
    acquireRevision: vi.fn(),
    selectionListeners: new Set<() => void>(),
}))
vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => state.reactiveDatabase?.current ?? state.database,
    getCurrentCharacter: () =>
        (state.reactiveDatabase?.current ?? state.database).characters[
            state.selectedIndex
        ],
    getCurrentChat: () =>
        (state.reactiveDatabase?.current ?? state.database).characters[
            state.selectedIndex
        ]?.chats[0],
}))
vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: {
            get db() {
                return state.reactiveDatabase?.current ?? state.database
            },
        },
        selectedCharID: writable(0),
        ReloadGUIPointer: writable(0),
        bookmarkListOpen: writable(true),
        ScrollToMessageStore: writable(null),
        createSimpleCharacter: (character: any) => ({
            ...character,
            type: 'simple',
        }),
    }
})
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireCompleteConversation: (...args: any[]) =>
        state.acquireCompleteConversation(...args),
    captureSelectedConversationTarget: () => state.selection,
    getActiveConversationSession: () => null,
    getPersistentDataRuntime: () => ({
        subscribeActiveConversationViewportSource: (listener: () => void) => {
            state.selectionListeners.add(listener)
            listener()
            return () => state.selectionListeners.delete(listener)
        },
        captureSelectedConversationTarget: () => state.selection,
        acquireCompleteConversation: (...args: any[]) =>
            state.acquireCompleteConversation(...args),
        store: {
            acquireRevision: (...args: any[]) => state.acquireRevision(...args),
        },
    }),
}))
vi.mock('src/ts/characters', () => ({ getCharImage: async () => '' }))
vi.mock('src/ts/util', () => ({
    findCharacterbyId: (id: string) =>
        state.database.characters.find((character) => character.chaId === id),
    getUserName: () => 'Synthetic user',
    getUserIcon: () => '',
    getPersonaPrompt: () => '',
}))
vi.mock('src/ts/process/modules', () => ({
    getModules: () => [],
    getModuleTriggers: () => [],
    getModuleRegexScripts: () => [],
    getModuleAssets: () => [],
    getModuleLorebooks: () => [],
}))
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    pluginV2: { editdisplay: new Set() },
}))
vi.mock('src/ts/alert', () => ({
    alertError: vi.fn(),
    alertInput: vi.fn(),
}))
vi.mock('src/lang', () => ({
    language: {
        expandAll: 'Expand all',
        collapseAll: 'Collapse all',
        chatDataLoadFailed: 'Synthetic chat load failed',
        loadingChatData: 'Loading synthetic chat',
        hypaV3Modal: { retry: 'Retry' },
    },
}))
vi.mock('../ChatScreens/Chat.svelte', async () => ({
    default: (await import('./BookmarkChatProbe.test.svelte')).default,
}))

import BookmarkList from './BookmarkList.svelte'
import { selectedCharID } from 'src/ts/stores.svelte'

let mounted: ReturnType<typeof mount> | undefined
let target: HTMLDivElement
let mountListener: (event: Event) => void
let mounts: { signal: AbortSignal; character: any; triggerCode?: string }[]

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((done) => {
        resolve = done
    })
    return { promise, resolve }
}

function completeLease(release: () => void) {
    return { release, target: { ...state.selection } }
}

function prepare(lua: boolean, group = false) {
    const message = {
        chatId: 'bookmark-message',
        role: 'char',
        data: 'Synthetic bookmarked message',
        saying: group ? 'speaker-a' : undefined,
    }
    const conversation = createMetadataOnlySelectedConversation({
        id: 'conversation-a',
        bookmarks: [message.chatId],
        bookmarkNames: {},
    } as any)
    const character = {
        type: 'character',
        chaId: 'character-a',
        name: 'Synthetic character',
        image: '',
        chatPage: 0,
        chats: [conversation],
        customscript: [],
        additionalAssets: [],
        emotionImages: [],
        triggerscript: lua
            ? [
                  {
                      type: 'start',
                      effect: [
                          {
                              type: 'triggerlua',
                              code: 'listenEdit("editDisplay", function(id, text) return text end)',
                          },
                      ],
                  },
              ]
            : [],
    }
    state.database.characters = group
        ? [
              { ...character, type: 'group', triggerscript: [] },
              { ...character, chaId: 'speaker-a' },
          ]
        : [character]
    state.selection = {
        characterId: character.chaId,
        conversationId: conversation.id,
        navigationGeneration: 1,
        storeRevision: 1,
    }
    const releaseRevision = vi.fn()
    state.acquireRevision.mockResolvedValue({
        revision: 1,
        readConversationWindow: async () => ({
            revision: 1,
            value: {
                characterId: character.chaId,
                conversationId: conversation.id,
                startIndex: 0,
                endIndex: 1,
                totalMessages: 1,
                messages: [message],
                hasMoreBefore: false,
                hasMoreAfter: false,
            },
        }),
        release: releaseRevision,
    })
    return { character, releaseRevision }
}

async function openExpanded() {
    mounted = mount(BookmarkList, { target })
    await vi.waitFor(() =>
        expect(target.querySelector('[role="button"]')).not.toBeNull(),
    )
    ;(target.querySelector('[role="button"]') as HTMLElement).click()
    await tick()
}

beforeEach(() => {
    vi.clearAllMocks()
    state.reactiveDatabase = null
    state.selectedIndex = 0
    state.selectionListeners.clear()
    selectedCharID.set(0)
    mounts = []
    mountListener = (event) => mounts.push((event as CustomEvent).detail)
    window.addEventListener('bookmark-chat-mounted', mountListener)
    target = document.createElement('div')
    document.body.appendChild(target)
})
afterEach(async () => {
    if (mounted) await unmount(mounted)
    mounted = undefined
    window.removeEventListener('bookmark-chat-mounted', mountListener)
    document.body.replaceChildren()
})

test.each([false, true])(
    'expanded Lua bookmark waits for complete history and holds its lease through display (group=%s)',
    async (group) => {
        const { releaseRevision } = prepare(true, group)
        const pending = deferred<any>()
        const release = vi.fn()
        state.acquireCompleteConversation.mockReturnValue(pending.promise)
        await openExpanded()
        expect(releaseRevision).toHaveBeenCalledOnce()
        expect(mounts).toHaveLength(0)
        expect(state.acquireCompleteConversation).toHaveBeenCalledOnce()
        pending.resolve(completeLease(release))
        await vi.waitFor(() => expect(mounts).toHaveLength(1))
        expect(release).not.toHaveBeenCalled()
        expect(mounts[0].signal.aborted).toBe(false)
        ;(target.querySelector('[role="button"]') as HTMLElement).click()
        await tick()
        expect(mounts[0].signal.aborted).toBe(true)
        expect(release).toHaveBeenCalledOnce()
        await unmount(mounted!)
        mounted = undefined
        expect(release).toHaveBeenCalledOnce()
    },
)

test.each([false, true])(
    'reactive complete promotion preserves a single bookmark display lease (revision advances=%s)',
    async (revisionAdvances) => {
        prepare(true)
        const reactive = createBookmarkDisplayDatabase(state.database)
        state.reactiveDatabase = reactive
        const initialCharacter = reactive.current.characters[0]
        const initialConversation = initialCharacter.chats[0]
        const release = vi.fn()
        let promotions = 0
        state.acquireCompleteConversation.mockImplementation(async () => {
            if (++promotions > 3)
                throw new Error('bounded promotion repetition')
            const currentCharacter = reactive.current.characters[0]
            reactive.replace({
                ...reactive.current,
                characters: [
                    {
                        ...currentCharacter,
                        chats: [
                            {
                                ...currentCharacter.chats[0],
                                message: [
                                    {
                                        chatId: 'bookmark-message',
                                        role: 'char',
                                        data: 'Synthetic bookmarked message',
                                    },
                                ],
                            },
                        ],
                    },
                ],
            })
            if (revisionAdvances) {
                state.selection = { ...state.selection, storeRevision: 2 }
            }
            for (const listener of state.selectionListeners) listener()
            await tick()
            return completeLease(release)
        })
        await openExpanded()
        await vi.waitFor(() => expect(mounts).toHaveLength(1))
        await tick()
        await new Promise((resolve) => setTimeout(resolve, 0))
        expect(reactive.current.characters[0]).not.toBe(initialCharacter)
        expect(reactive.current.characters[0].chats[0]).not.toBe(
            initialConversation,
        )
        expect(reactive.current.characters[0].chaId).toBe(
            initialCharacter.chaId,
        )
        expect(reactive.current.characters[0].chats[0].id).toBe(
            initialConversation.id,
        )
        expect(state.acquireCompleteConversation).toHaveBeenCalledOnce()
        expect(state.selection.storeRevision).toBe(revisionAdvances ? 2 : 1)
        expect(release).not.toHaveBeenCalled()
        expect(mounts[0].signal.aborted).toBe(false)
        expect(target.querySelector('[data-bookmark-chat]')).not.toBeNull()
    },
)

test('same-ID navigation generation aborts synchronously and acquires one new display lease', async () => {
    prepare(true)
    const reactive = createBookmarkDisplayDatabase(state.database)
    state.reactiveDatabase = reactive
    const firstRelease = vi.fn()
    const secondRelease = vi.fn()
    const next = deferred<any>()
    state.acquireCompleteConversation
        .mockResolvedValueOnce(completeLease(firstRelease))
        .mockReturnValueOnce(next.promise)
    await openExpanded()
    await vi.waitFor(() => expect(mounts).toHaveLength(1))
    const firstSignal = mounts[0].signal
    state.selection = {
        ...state.selection,
        navigationGeneration: state.selection.navigationGeneration + 1,
    }
    for (const listener of state.selectionListeners) listener()
    expect(firstSignal.aborted).toBe(true)
    await tick()
    expect(firstRelease).toHaveBeenCalledOnce()
    expect(state.acquireCompleteConversation).toHaveBeenCalledTimes(2)
    expect(target.querySelector('[data-bookmark-chat]')).toBeNull()
    next.resolve(completeLease(secondRelease))
    await vi.waitFor(() => expect(mounts).toHaveLength(2))
    expect(mounts[1].signal).not.toBe(firstSignal)
    expect(mounts[1].signal.aborted).toBe(false)
    expect(secondRelease).not.toHaveBeenCalled()
    expect(state.acquireCompleteConversation).toHaveBeenCalledTimes(2)
})

test('editing existing Lua code replaces the display child and its lease with the updated script', async () => {
    prepare(true)
    const reactive = createBookmarkDisplayDatabase(state.database)
    state.reactiveDatabase = reactive
    const firstRelease = vi.fn()
    const secondRelease = vi.fn()
    const next = deferred<any>()
    state.acquireCompleteConversation
        .mockResolvedValueOnce(completeLease(firstRelease))
        .mockReturnValueOnce(next.promise)
    await openExpanded()
    await vi.waitFor(() => expect(mounts).toHaveLength(1))
    const beforeCode = mounts[0].triggerCode
    const updatedCode =
        'listenEdit("editDisplay", function(id, text) return text .. " synthetic update" end)'
    reactive.current.characters[0].triggerscript[0].effect[0].code = updatedCode
    await tick()
    expect(mounts[0].signal.aborted).toBe(true)
    expect(firstRelease).toHaveBeenCalledOnce()
    expect(state.acquireCompleteConversation).toHaveBeenCalledTimes(2)
    expect(target.querySelector('[data-bookmark-chat]')).toBeNull()
    next.resolve(completeLease(secondRelease))
    await vi.waitFor(() => expect(mounts).toHaveLength(2))
    expect(mounts[0].triggerCode).toBe(beforeCode)
    expect(mounts[1].triggerCode).toBe(updatedCode)
    expect(mounts[1].signal.aborted).toBe(false)
    expect(secondRelease).not.toHaveBeenCalled()
    expect(state.acquireCompleteConversation).toHaveBeenCalledTimes(2)
    await unmount(mounted!)
    mounted = undefined
    expect(firstRelease).toHaveBeenCalledOnce()
    expect(secondRelease).toHaveBeenCalledOnce()
})

test('navigation does not render stale bookmark data while the next query is pending', async () => {
    const { character } = prepare(true)
    const release = vi.fn()
    state.acquireCompleteConversation.mockImplementation(async () =>
        completeLease(release),
    )
    await openExpanded()
    await vi.waitFor(() => expect(mounts).toHaveLength(1))
    state.acquireRevision.mockReturnValue(new Promise(() => {}))
    state.database.characters.push({
        ...character,
        chaId: 'character-b',
        chats: [
            createMetadataOnlySelectedConversation({
                id: 'conversation-b',
                bookmarks: ['bookmark-message'],
            } as any),
        ],
    })
    state.selection = {
        ...state.selection,
        characterId: 'character-b',
        conversationId: 'conversation-b',
        navigationGeneration: 2,
    }
    state.selectedIndex = 1
    selectedCharID.set(1)
    await tick()
    expect(target.querySelector('[data-bookmark-chat]')).toBeNull()
    expect(release).toHaveBeenCalledOnce()
    expect(mounts[0].signal.aborted).toBe(true)
    expect(state.acquireCompleteConversation).toHaveBeenCalledOnce()
})

test('a collapsed pending Lua bookmark releases a late lease without mounting Chat', async () => {
    prepare(true)
    const pending = deferred<any>()
    const release = vi.fn()
    state.acquireCompleteConversation.mockReturnValue(pending.promise)
    await openExpanded()
    expect(mounts).toHaveLength(0)
    ;(target.querySelector('[role="button"]') as HTMLElement).click()
    await tick()
    pending.resolve(completeLease(release))
    await vi.waitFor(() => expect(release).toHaveBeenCalledOnce())
    expect(mounts).toHaveLength(0)
})

test('plain expanded bookmark remains windowed without acquiring complete history', async () => {
    prepare(false)
    await openExpanded()
    await vi.waitFor(() => expect(mounts).toHaveLength(1))
    expect(state.acquireCompleteConversation).not.toHaveBeenCalled()
})

test('failed display lease acquisition renders only an error until retry succeeds', async () => {
    prepare(true)
    const next = deferred<any>()
    const release = vi.fn()
    state.acquireCompleteConversation
        .mockRejectedValueOnce(new Error('Synthetic acquisition failure'))
        .mockReturnValueOnce(next.promise)
    await openExpanded()
    await vi.waitFor(() =>
        expect(
            target.querySelector('[data-live-display-load-error]'),
        ).not.toBeNull(),
    )
    expect(target.querySelector('[role="alert"]')?.textContent).toContain(
        'Synthetic chat load failed',
    )
    expect(mounts).toHaveLength(0)
    expect(target.querySelector('[data-bookmark-chat]')).toBeNull()
    expect(state.acquireCompleteConversation).toHaveBeenCalledOnce()
    const retry = target.querySelector(
        '[data-live-display-load-error] button',
    ) as HTMLButtonElement
    expect(retry.textContent).toBe('Retry')
    retry.click()
    await tick()
    expect(state.acquireCompleteConversation).toHaveBeenCalledTimes(2)
    expect(target.querySelector('[data-live-display-load-error]')).toBeNull()
    expect(mounts).toHaveLength(0)
    next.resolve(completeLease(release))
    await vi.waitFor(() => expect(mounts).toHaveLength(1))
    expect(mounts[0].signal.aborted).toBe(false)
    expect(release).not.toHaveBeenCalled()
    expect(state.acquireCompleteConversation).toHaveBeenCalledTimes(2)
    await unmount(mounted!)
    mounted = undefined
    expect(mounts[0].signal.aborted).toBe(true)
    expect(release).toHaveBeenCalledOnce()
})

test.each([false, true])(
    'navigation cancels bookmark display with lease ready=%s',
    async (ready) => {
        const { character } = prepare(true)
        const pending = deferred<any>()
        const release = vi.fn()
        state.acquireCompleteConversation.mockReturnValue(pending.promise)
        await openExpanded()
        if (ready) {
            pending.resolve(completeLease(release))
            await vi.waitFor(() => expect(mounts).toHaveLength(1))
        }
        state.database.characters.push({
            ...character,
            chaId: 'character-b',
            chats: [
                createMetadataOnlySelectedConversation({
                    id: 'conversation-b',
                    bookmarks: [],
                } as any),
            ],
        })
        state.selection = {
            ...state.selection,
            characterId: 'character-b',
            conversationId: 'conversation-b',
            navigationGeneration: 2,
        }
        state.selectedIndex = 1
        selectedCharID.set(1)
        await tick()
        if (!ready) pending.resolve(completeLease(release))
        await vi.waitFor(() => expect(release).toHaveBeenCalledOnce())
        expect(target.querySelector('[data-bookmark-chat]')).toBeNull()
        if (ready) expect(mounts[0].signal.aborted).toBe(true)
        else expect(mounts).toHaveLength(0)
    },
)
