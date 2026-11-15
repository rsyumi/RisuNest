// @vitest-environment happy-dom

import { writable } from 'svelte/store'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import type { Message } from 'src/ts/storage/database.svelte'
import type { ConversationViewportKey, ConversationViewportRow } from 'src/ts/conversationViewportSource'

const live = vi.hoisted(() => ({
    db: {} as Record<string, any>,
    popup: { openId: 0, children: null as unknown, mouseX: 0, mouseY: 0 },
    selection: { selId: 0 },
    resolvePosition: vi.fn(async (_characterId: string, _conversationId: string | null) => ({ characterIndex: 0, chatIndex: 0 })),
}))

vi.mock('./ChatBody.svelte', async () => ({
    default: (await import('./ChatBodyCaptureProbe.test.svelte')).default,
}))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: { get db() { return live.db } },
    ReloadChatPointer: writable([]),
    CurrentTriggerIdStore: writable(null),
    popupStore: live.popup,
    alertStore: writable({ type: 'none', msg: '' }),
    selectedCharID: writable(0),
    HideIconStore: writable(false),
    ReloadGUIPointer: writable(0),
    selIdState: live.selection,
    createSimpleCharacter: (char: unknown) => char,
}))
vi.mock('src/ts/characters', () => ({ getCharImage: async () => '' }))
vi.mock('src/ts/gui/colorscheme', () => ({ ColorSchemeTypeStore: writable('light') }))
vi.mock('src/ts/globalApi.svelte', () => ({
    aiLawApplies: () => false,
    changeChatTo: vi.fn(),
    foldChatToMessage: vi.fn(),
    getFileSrc: vi.fn(async (source: string) => source),
    createChatCopyName: vi.fn(),
}))
vi.mock('src/ts/process/scripts', () => ({ risuChatParser: (value: string) => value }))
vi.mock('src/ts/model/modellist', () => ({ getModelInfo: (model: string) => ({ shortName: model || 'model' }) }))
vi.mock('src/ts/process/scriptings', () => ({ runLuaInteractionTrigger: vi.fn() }) satisfies Partial<typeof import('src/ts/process/scriptings')>)
vi.mock('src/ts/process/triggers', () => ({ runTrigger: vi.fn() }))
vi.mock('src/ts/process/tts', () => ({ sayTTS: vi.fn() }))
vi.mock('src/ts/util', () => ({
    capitalize: (value: string) => value,
    getUserIcon: () => '',
    getUserName: () => 'Live User',
    sleep: () => Promise.resolve(),
}))
vi.mock('../../ts/alert', () => ({
    alertClear: vi.fn(), alertConfirm: vi.fn(), alertError: vi.fn(), alertInput: vi.fn(), alertNormal: vi.fn(),
    alertCheckboxConfirm: vi.fn(async () => ({ confirmed: false, checked: false })),
    alertRequestData: vi.fn(), alertWait: vi.fn(), alertToast: vi.fn(),
}))
vi.mock('../../ts/translator/translator', () => ({ getLLMCache: vi.fn(), setLLMCache: vi.fn() }))
vi.mock('src/ts/process/files/inlayRenderSource', () => ({
    DeferredInlayMarkerRegistry: class {}, withResolvedDeferredInlaySources: vi.fn(),
}))
vi.mock('src/ts/process/files/chatCopyInlays', () => ({ copyImageSourceToDataUrl: vi.fn() }))
vi.mock('src/ts/plugins/pinnedConversationPosition', () => ({ resolvePinnedConversationPosition: live.resolvePosition }))
vi.mock('../../ts/storage/persistentDataRuntime.svelte', () => ({
    acquireDestructiveReplacementFence: vi.fn(),
    capturePersistentMutationToken: vi.fn(),
    getActiveConversationSession: () => null,
    getPersistentDataRuntime: () => ({}),
}))

import Chat from './Chat.svelte'
import PopupList from '../UI/PopupList.svelte'
import { additionalMessageButtons, type MessageButtonDef } from 'src/ts/plugins/messageButtons.svelte'
import { createMetadataOnlySelectedConversation } from 'src/ts/storage/selectedConversationLifecycle'
import { language } from 'src/lang'
import { alertCheckboxConfirm } from '../../ts/alert'
import { getLLMCache } from '../../ts/translator/translator'

const messages: Message[] = [
    { role: 'user', data: 'question', chatId: 'm-0' },
    { role: 'char', data: 'answer', chatId: 'm-1' },
    { role: 'char', data: 'unnamed' },
]

function button(id: string, roles?: MessageButtonDef['roles']): MessageButtonDef {
    return { id, name: `Button ${id}`, icon: '', iconType: 'none', callback: vi.fn(), roles }
}

function installCharacter(conversation: object) {
    live.db = {
        theme: '',
        clickToEdit: false,
        characters: [{
            type: 'character',
            name: 'Live Character',
            chaId: 'character-a',
            chatPage: 0,
            chats: [conversation],
            ttsMode: 'none',
        }],
    }
}

describe('plugin message buttons in the message action row', () => {
    let target: HTMLDivElement
    const mounted: ReturnType<typeof mount>[] = []
    const innerWidth = window.innerWidth

    const setWidth = (width: number) => Object.defineProperty(window, 'innerWidth', { configurable: true, value: width })
    const mountChat = async (props: Record<string, unknown>, into = target) => {
        mounted.push(mount(Chat, { target: into, props: { name: 'Live Character', isLastMemory: false, ...props } }))
        await tick()
    }
    const pluginButtons = (root: ParentNode = target) =>
        [...root.querySelectorAll<HTMLButtonElement>('.button-icon-plugin')].map((element) => element.title)
    const menuButton = () => target.querySelector<HTMLButtonElement>('.button-icon-menu')!
    const openPopup = async () => {
        menuButton().click()
        await vi.waitFor(() => expect(live.popup.children).not.toBeNull())
        const popup = document.createElement('div')
        document.body.append(popup)
        mounted.push(mount(PopupList, { target: popup }))
        await tick()
        return popup
    }

    beforeEach(() => {
        additionalMessageButtons.push(button('char-only', ['char']), button('user-only', ['user']), button('both'))
        installCharacter({ id: 'conversation-a', message: messages, isStreaming: false })
        live.popup.children = null
        live.popup.openId = 0
        live.selection.selId = 0
        live.resolvePosition.mockClear()
        setWidth(1024)
        target = document.createElement('div')
        document.body.append(target)
    })

    afterEach(async () => {
        for (const instance of mounted.splice(0)) await unmount(instance)
        additionalMessageButtons.splice(0)
        setWidth(innerWidth)
        document.body.replaceChildren()
    })

    test('shows matching buttons after the major buttons on desktop and hands over the message', async () => {
        await mountChat({ message: 'answer', role: 'char', idx: 1, totalLength: 3 })

        expect(pluginButtons()).toEqual(['Button char-only', 'Button both'])
        const remove = target.querySelector('.button-icon-remove')!
        const first = target.querySelector<HTMLButtonElement>('.button-icon-plugin')!
        expect(remove.compareDocumentPosition(first) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
        expect(first.closest('.button-icon-menu')).toBeNull()

        first.click()
        await vi.waitFor(() => expect(additionalMessageButtons[0].callback).toHaveBeenCalledWith({
            characterIndex: 0, chatIndex: 0, messageIndex: 1, messageId: 'm-1', role: 'char',
            characterId: 'character-a', conversationId: 'conversation-a',
        }))

        const userRow = document.createElement('div')
        document.body.append(userRow)
        await mountChat({ message: 'question', role: 'user', idx: 0, totalLength: 3 }, userRow)
        expect(pluginButtons(userRow)).toEqual(['Button user-only', 'Button both'])
    })

    test('reports a missing message ID as null and follows registration changes', async () => {
        await mountChat({ message: 'unnamed', role: 'char', idx: 2, totalLength: 4 })
        target.querySelectorAll<HTMLButtonElement>('.button-icon-plugin')[1].click()
        await vi.waitFor(() => expect(additionalMessageButtons[2].callback).toHaveBeenCalledWith(expect.objectContaining({ messageIndex: 2, messageId: null })))

        additionalMessageButtons.push(button('late', ['char']))
        await tick()
        expect(pluginButtons()).toEqual(['Button char-only', 'Button both', 'Button late'])
        additionalMessageButtons.splice(0, 1)
        await tick()
        expect(pluginButtons()).toEqual(['Button both', 'Button late'])
    })

    test('lists the buttons by name inside the action popup on narrow screens', async () => {
        setWidth(400)
        await mountChat({ message: 'answer', role: 'char', idx: 1, totalLength: 3 })
        expect(pluginButtons()).toEqual([])

        const popup = await openPopup()

        expect(pluginButtons(popup)).toEqual(['Button char-only', 'Button both'])
        expect([...popup.querySelectorAll('.button-icon-plugin span')].map((element) => element.textContent)).toEqual(['Button char-only', 'Button both'])
        popup.querySelector<HTMLButtonElement>('.button-icon-plugin')!.click()
        await vi.waitFor(() => expect(additionalMessageButtons[0].callback).toHaveBeenCalledWith(expect.objectContaining({ messageIndex: 1, messageId: 'm-1' })))
    })

    test('opens the action popup again with one tap after a tap closed it, and closes it with a tap on its button', async () => {
        setWidth(400)
        await mountChat({ message: 'answer', role: 'char', idx: 1, totalLength: 3 })
        const popup = await openPopup()

        popup.querySelector<HTMLButtonElement>('.button-icon-plugin')!.click()
        expect(live.popup.children).toBeNull()
        menuButton().click()
        await vi.waitFor(() => expect(live.popup.children).not.toBeNull())

        menuButton().click()
        await tick()
        expect(live.popup.children).toBeNull()
        menuButton().click()
        await vi.waitFor(() => expect(live.popup.children).not.toBeNull())
    })

    test('closes the action popup when its remove button is held', async () => {
        setWidth(400)
        await mountChat({ message: 'answer', role: 'char', idx: 1, totalLength: 3 })
        const popup = await openPopup()

        // No click follows a held press, so the popup's own click handler never runs.
        popup.querySelector('.button-icon-remove')!.dispatchEvent(new PointerEvent('pointerdown', {
            bubbles: true, pointerId: 2, pointerType: 'touch', isPrimary: true, button: 0,
        }))
        await vi.waitFor(() => expect(alertCheckboxConfirm).toHaveBeenCalledOnce())
        expect(live.popup).toMatchObject({ children: null, openId: 0 })
    })

    test('offers a row without a stored message only the actions that work on it', async () => {
        // A chat nothing was sent in keeps no message list to read.
        const install = (options: Record<string, unknown>) => {
            installCharacter(createMetadataOnlySelectedConversation({ id: 'conversation-a', name: 'Chat', note: '', localLore: [], bookmarks: [] }))
            Object.assign(live.db, { enableBookmark: true, translator: 'llm', translatorType: 'llm', translateBeforeHTMLFormatting: true }, options)
        }
        const greeting = { message: 'greeting', role: 'char', idx: -1, firstMessage: true, totalLength: 0 }
        const actions = (root: ParentNode) => [...root.querySelectorAll('button')].flatMap((element) => element.className.match(/button-icon-[\w-]+/) ?? [])

        install({ useChatCopy: true })
        await mountChat(greeting)
        expect(actions(target)).toEqual(['button-icon-translate', 'button-icon-copy'])

        vi.mocked(getLLMCache).mockResolvedValue('Cached translation')
        target.querySelector<HTMLButtonElement>('.button-icon-translate')!.click()
        ;(await vi.waitFor(() => {
            const edit = [...target.querySelectorAll('button')].find((element) => element.textContent?.includes(language.editTranslation))
            expect(edit).toBeDefined()
            return edit!
        })).click()
        await vi.waitFor(() => expect(target.querySelector<HTMLTextAreaElement>('textarea.message-edit-area')?.value).toBe('Cached translation'))

        setWidth(400)
        target.replaceChildren()
        await mountChat(greeting)
        expect(actions(target)).toEqual(['button-icon-translate', 'button-icon-menu'])
        expect(actions(await openPopup())).toEqual(['button-icon-copy'])

        install({ useChatCopy: false })
        const bare = document.createElement('div')
        document.body.append(bare)
        await mountChat(greeting, bare)
        expect(actions(bare)).toEqual(['button-icon-translate'])
    })

    test('moves the buttons between the row and the popup when the window is resized', async () => {
        await mountChat({ message: 'answer', role: 'char', idx: 1, totalLength: 3 })
        expect(pluginButtons()).toEqual(['Button char-only', 'Button both'])

        setWidth(400)
        window.dispatchEvent(new Event('resize'))
        await tick()
        expect(pluginButtons()).toEqual([])
        expect(target.querySelector('.button-icon-menu')).not.toBeNull()

        setWidth(1000)
        window.dispatchEvent(new Event('resize'))
        await tick()
        expect(pluginButtons()).toEqual(['Button char-only', 'Button both'])
    })

    test('addresses a windowed row by its absolute index without reading the conversation body', async () => {
        const conversation = { id: 'conversation-a', isStreaming: false }
        Object.defineProperty(conversation, 'message', { get() { throw new Error('windowed conversation body was accessed') } })
        installCharacter(conversation)
        const row: ConversationViewportRow = {
            key: 'row-4123' as ConversationViewportKey,
            absoluteIndex: 4123,
            message: { role: 'char', data: 'far answer', chatId: 'm-4123' },
            sourceVersion: 1,
        }
        await mountChat({ message: 'far answer', role: 'char', idx: 4123, totalLength: 5000, viewportRow: row, viewportSourceToken: 'source' })

        target.querySelector<HTMLButtonElement>('.button-icon-plugin')!.click()
        await vi.waitFor(() => expect(additionalMessageButtons[0].callback).toHaveBeenCalledWith({
            characterIndex: 0, chatIndex: 0, messageIndex: 4123, messageId: 'm-4123', role: 'char',
            characterId: 'character-a', conversationId: 'conversation-a',
        }))
    })

    test('reports the position the index APIs resolve, not the working-set index', async () => {
        const selected = { type: 'character', name: 'Selected', chaId: 'character-b', chatPage: 1, ttsMode: 'none', chats: [
            { id: 'conversation-b0', message: [] },
            { id: 'conversation-b1', message: messages, isStreaming: false },
        ] }
        live.db = { theme: '', clickToEdit: false, characters: [{ type: 'character', name: 'Archived', chaId: 'archived-a', chatPage: 0, chats: [] }, selected] }
        live.selection.selId = 1
        live.resolvePosition.mockResolvedValueOnce({ characterIndex: 0, chatIndex: 1 })
        await mountChat({ message: 'answer', role: 'char', idx: 1, totalLength: 3 })

        target.querySelector<HTMLButtonElement>('.button-icon-plugin')!.click()
        await vi.waitFor(() => expect(additionalMessageButtons[0].callback).toHaveBeenCalledWith({
            characterIndex: 0, chatIndex: 1, messageIndex: 1, messageId: 'm-1', role: 'char',
            characterId: 'character-b', conversationId: 'conversation-b1',
        }))
        expect(live.resolvePosition).toHaveBeenCalledExactlyOnceWith('character-b', 'conversation-b1')
    })

    test('leaves out comments, rows without a stored index and the streamed message', async () => {
        await mountChat({ message: 'note', role: 'char', idx: 1, totalLength: 3, isComment: true })
        expect(pluginButtons()).toEqual([])
        expect(target.querySelector('.button-icon-remove')).not.toBeNull()

        const unsaved = document.createElement('div')
        document.body.append(unsaved)
        await mountChat({ message: 'greeting', role: 'char', idx: -1, totalLength: 3 }, unsaved)
        expect(pluginButtons(unsaved)).toEqual([])

        installCharacter({ id: 'conversation-a', message: messages, isStreaming: true })
        const streamed = document.createElement('div')
        const earlier = document.createElement('div')
        document.body.append(streamed, earlier)
        await mountChat({ message: 'unnamed', role: 'char', idx: 2, totalLength: 3 }, streamed)
        await mountChat({ message: 'answer', role: 'char', idx: 1, totalLength: 3 }, earlier)
        expect(pluginButtons(streamed)).toEqual([])
        expect(pluginButtons(earlier)).toEqual(['Button char-only', 'Button both'])
    })

    test('leaves out screenshot capture rows', async () => {
        const character = live.db.characters[0]
        await mountChat({
            message: 'answer', role: 'char', idx: 1, totalLength: 3,
            captureMessage: messages[1],
            captureContext: {
                character: null, characterName: 'Live Character', characterImageSource: '', characterLargePortrait: false,
                userName: 'Live User', userImageSource: '', userLargePortrait: false,
                moduleAssets: [], presetRegex: [], moduleRegexScripts: [], assetStyle: '',
                parserContext: {
                    database: { characters: [character] }, character, userName: 'Live User', personaPrompt: '',
                    modules: [], moduleLorebooks: [], selectedCharID: 0, chatVariables: {}, globalChatVariables: {}, currentTime: 1,
                },
                totalTurns: 3, selectionStart: 0, firstParserMessageIndex: 0,
                settings: { theme: '', iconSize: 100, zoomSize: 100, lineHeight: 1.25, hideIcons: false, translator: '' },
            },
        })
        expect(target.querySelector('[data-chat-body-probe]')).not.toBeNull()
        expect(pluginButtons()).toEqual([])
    })
})

describe('response candidate controls in the message row', () => {
    test('follow a candidate position update on the mounted row', async () => {
        installCharacter({ id: 'conversation-a', message: messages, isStreaming: false })
        const target = document.createElement('div')
        document.body.append(target)
        const instance = mount(Chat, {
            target,
            props: { name: 'Live Character', isLastMemory: false, message: 'answer', role: 'char', idx: 1, totalLength: 2, rerollIcon: 'dynamic' },
        }) as unknown as { updateCandidatePosition(page: number, total: number): void }
        try {
            await tick()
            const previous = target.querySelector<HTMLButtonElement>('.button-icon-unreroll')!
            const counter = () => target.querySelector('.tabular-nums')?.textContent ?? null
            expect(previous.disabled).toBe(true)
            expect(counter()).toBeNull()

            instance.updateCandidatePosition(2, 2)
            await tick()
            expect(previous.disabled).toBe(false)
            expect(counter()).toBe('2/2')
        } finally {
            await unmount(instance)
            document.body.replaceChildren()
        }
    })
})
