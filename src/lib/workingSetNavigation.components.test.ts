import { afterAll, afterEach, beforeEach, describe, expect, it, onTestFinished, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'
import NavigationPanel from './test-fixtures/NavigationPanel.test.svelte'

const persistence = vi.hoisted(() => ({
    deactivate: vi.fn<() => Promise<boolean>>(),
    changeChar: vi.fn<(index: number) => Promise<boolean>>(),
    markDirty: vi.fn(),
    alertError: vi.fn(),
}))

vi.mock('@lucide/svelte', () => Object.fromEntries([
    'ArrowLeft', 'MenuIcon', 'ShellIcon', 'Settings', 'ListIcon', 'LayoutGridIcon',
    'FolderIcon', 'FolderOpenIcon', 'HomeIcon', 'WrenchIcon', 'User2Icon',
].map((name) => [name, () => {}])))

vi.mock('src/ts/stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    const { navigationDBState } = await import('./test-fixtures/navigationState.test.svelte')
    return {
        DBState: navigationDBState,
        selectedCharID: writable(0), PlaygroundStore: writable(0), settingsOpen: writable(false),
        MobileGUIStack: writable(0), MobileSearch: writable(''), SettingsMenuIndex: writable(-1),
        MobileSideBar: writable(0), CharEmotion: writable({}), DynamicGUI: writable(false),
        botMakerMode: writable(false), sideBarClosing: writable(false), sideBarStore: writable(false),
        OpenRealmStore: writable(false), SizeStore: writable({ w: 1200, h: 800 }),
        QuickSettings: { open: false }, additionalHamburgerMenu: [], alertStore: writable(null),
    }
})
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    deactivateActiveWorkingSet: persistence.deactivate,
    markPersistentDataDirty: persistence.markDirty,
}))
vi.mock('src/ts/characters', () => ({
    changeChar: persistence.changeChar,
    characterFormatUpdate: (character: unknown) => character,
    commitDetachedCharacter: vi.fn(), createBlankChar: vi.fn(), addCharacter: vi.fn(), getCharImage: vi.fn(),
}))
vi.mock('src/ts/storage/database.svelte', () => ({ setDatabase: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertError: persistence.alertError, alertInput: vi.fn(), alertSelect: vi.fn() }))
vi.mock('src/ts/globalApi.svelte', () => ({ checkCharOrder: vi.fn(), getFileSrc: vi.fn(), saveAsset: vi.fn() }))
vi.mock('src/ts/util', async () => {
    const { DBState } = await import('src/ts/stores.svelte')
    return {
        getCharacterIndexObject: () => ({}), selectSingleFile: vi.fn(),
        findCharacterIndexbyId: (id: string) => DBState.db.characters.findIndex((character) => character.chaId === id),
    }
})
vi.mock('src/ts/sync/multiuser', async () => {
    const { writable } = await import('svelte/store')
    return {
        joinMultiuserRoom: vi.fn(), ConnectionIsHost: writable(false),
        ConnectionOpenStore: writable(false), RoomIdStore: writable('synthetic-room'),
    }
})
vi.mock('src/ts/gui/guisize', async () => ({ sideBarSize: (await import('svelte/store')).writable(300) }))
vi.mock('src/lang', () => ({
    language: {
        goback: 'Go Back', menu: 'Menu', search: 'Search', home: 'Home', settings: 'Settings',
        character: 'Character', Chat: 'Chat', playground: { playground: 'Playground', inlayExplorer: 'Inlays' },
        embedding: 'Embedding', tokenizer: 'Tokenizer', syntax: 'Syntax', imageGeneration: 'Images',
        subtitles: 'Subtitles', imageTranslation: 'Image translation', translator: 'Translation',
        promptConvertion: 'Prompts', joinMultiUserRoom: 'Join room',
        navigationBlockedWhileGenerating: 'Generation is active',
    },
}))

import { language } from 'src/lang'
import { DBState, MobileSideBar, PlaygroundStore, selectedCharID, settingsOpen } from 'src/ts/stores.svelte'

const visualPanels = [
    './SideBars/SidebarIndicator.svelte', './SideBars/CharConfig.svelte',
    './SideBars/SelectedConversationEditor.svelte', './SideBars/SidebarAvatar.svelte',
    './SideBars/SideChatList.svelte', './SideBars/DevTool.svelte',
    './Others/QuickSettingsGUI.svelte', './Others/PluginDefinedIcon.svelte',
    './Playground/PlaygroundEmbedding.svelte', './Playground/PlaygroundTokenizer.svelte',
    './Playground/PlaygroundJinja.svelte', './Playground/PlaygroundSyntax.svelte',
    './Playground/PlaygroundImageGen.svelte', './Playground/PlaygroundParser.svelte',
    './Playground/ToolConversion.svelte', './Playground/PlaygroundSubtitle.svelte',
    './Playground/PlaygroundImageTrans.svelte', './Playground/PlaygroundTranslation.svelte',
    './Playground/PlaygroundMCP.svelte', './Playground/PlaygroundDocs.svelte',
    './Playground/PlaygroundInlayExplorer.svelte',
]
for (const path of visualPanels) vi.doMock(path, () => ({ default: NavigationPanel }))
const [{ default: MobileHeader }, { default: Sidebar }, { default: PlaygroundMenu }] = await Promise.all([
    import('./Mobile/MobileHeader.svelte'),
    import('./SideBars/Sidebar.svelte'),
    import('./Playground/PlaygroundMenu.svelte'),
])
let target: HTMLDivElement
let mounted: ReturnType<typeof mount> | undefined

beforeEach(() => {
    vi.resetAllMocks()
    DBState.db.characters = [
        { type: 'character', chaId: 'synthetic-owner', name: 'Synthetic owner', chatPage: 0, chats: [] },
        { type: 'character', chaId: '§playground', name: 'Playground owner', chatPage: 0, chats: [] },
    ] as typeof DBState.db.characters
    DBState.db.characterOrder = []
    DBState.db.menuSideBar = true
    selectedCharID.set(0)
    PlaygroundStore.set(0)
    settingsOpen.set(false)
    MobileSideBar.set(0)
    persistence.deactivate.mockResolvedValue(true)
    persistence.changeChar.mockImplementation(async (index) => {
        selectedCharID.set(index)
        return true
    })
    target = document.createElement('div')
    document.body.append(target)
})

afterEach(async () => {
    try {
        if (mounted) await unmount(mounted)
    } finally {
        mounted = undefined
        target.remove()
    }
})
afterAll(() => {
    for (const path of visualPanels) vi.doUnmock(path)
})

function deferredPersistence() {
    let resolve!: (value: boolean) => void
    let reject!: (reason: Error) => void
    const promise = new Promise<boolean>((yes, no) => { resolve = yes; reject = no })
    onTestFinished(async () => {
        resolve(false)
        await promise.catch(() => undefined)
        await tick()
    })
    return { promise, resolve, reject }
}

function buttonNamed(name: string): HTMLButtonElement {
    const button = [...target.querySelectorAll('button')].find((candidate) =>
        (candidate.getAttribute('aria-label') ?? candidate.textContent?.trim()) === name)
    expect(button, `Navigation button named ${name}`).toBeDefined()
    return button!
}

describe('mounted navigation waits for persistence', () => {
    it.each(['resolve', 'reject', 'refuse'] as const)('mobile back handles %s without clearing selection early', async (outcome) => {
        const pending = deferredPersistence()
        persistence.deactivate.mockReturnValueOnce(pending.promise)
        mounted = mount(MobileHeader, { target })
        await tick()
        buttonNamed(language.goback).click()
        await tick()
        expect(persistence.deactivate).toHaveBeenCalledOnce()
        expect(get(selectedCharID)).toBe(0)
        expect(target.textContent).toContain('Synthetic owner')
        const error = new Error('Synthetic persistence failure')
        if (outcome === 'reject') pending.reject(error)
        else pending.resolve(outcome === 'resolve')
        if (outcome === 'resolve') {
            await vi.waitFor(() => expect(get(selectedCharID)).toBe(-1))
            expect(target.textContent).not.toContain('Synthetic owner')
        } else {
            if (outcome === 'reject') await vi.waitFor(() => expect(persistence.alertError).toHaveBeenCalledExactlyOnceWith(error))
            else { await pending.promise; await tick() }
            expect(get(selectedCharID)).toBe(0)
            buttonNamed(language.goback).click()
            await vi.waitFor(() => expect(get(selectedCharID)).toBe(-1))
            expect(persistence.deactivate).toHaveBeenCalledTimes(2)
        }
    })

    it.each(['home', 'playground'] as const)('sidebar %s keeps the current view on refusal and changes after retry', async (destination) => {
        PlaygroundStore.set(2)
        const first = deferredPersistence()
        persistence.deactivate.mockReturnValueOnce(first.promise)
        mounted = mount(Sidebar, { target })
        await tick()
        const name = destination === 'home' ? language.home : language.playground.playground
        buttonNamed(name).click()
        await tick()
        expect(persistence.deactivate).toHaveBeenCalledOnce()
        expect(get(selectedCharID)).toBe(0)
        expect(get(PlaygroundStore)).toBe(2)
        first.resolve(false)
        await first.promise
        await tick()
        expect(get(selectedCharID)).toBe(0)
        expect(get(PlaygroundStore)).toBe(2)
        const retry = deferredPersistence()
        persistence.deactivate.mockReturnValueOnce(retry.promise)
        buttonNamed(name).click()
        await tick()
        expect(get(selectedCharID)).toBe(0)
        expect(get(PlaygroundStore)).toBe(2)
        retry.resolve(true)
        await vi.waitFor(() => {
            expect(get(selectedCharID)).toBe(-1)
            expect(get(PlaygroundStore)).toBe(destination === 'home' ? 0 : 1)
        })
        expect(persistence.deactivate).toHaveBeenCalledTimes(2)
    })

    it('opens playground chat only after the real activation helper succeeds', async () => {
        PlaygroundStore.set(1)
        const pending = deferredPersistence()
        persistence.changeChar.mockReturnValueOnce(pending.promise)
        mounted = mount(PlaygroundMenu, { target })
        await tick()
        buttonNamed(language.Chat).click()
        await tick()
        expect(persistence.changeChar).toHaveBeenCalledExactlyOnceWith(1)
        expect(get(PlaygroundStore)).toBe(1)
        expect(get(selectedCharID)).toBe(0)
        pending.resolve(false)
        await pending.promise
        await tick()
        expect(get(PlaygroundStore)).toBe(1)
        expect(get(selectedCharID)).toBe(0)
        expect(persistence.markDirty).not.toHaveBeenCalled()
        const retry = deferredPersistence()
        persistence.changeChar.mockImplementationOnce(async (index) => {
            if (!await retry.promise) return false
            selectedCharID.set(index)
            return true
        })
        buttonNamed(language.Chat).click()
        await tick()
        expect(get(PlaygroundStore)).toBe(1)
        expect(get(selectedCharID)).toBe(0)
        retry.resolve(true)
        await vi.waitFor(() => expect(get(PlaygroundStore)).toBe(2))
        expect(get(selectedCharID)).toBe(1)
        expect(DBState.db.characters[1]).toMatchObject({ name: 'assistant', utilityBot: true, firstMessage: '{{none}}' })
        expect(persistence.changeChar).toHaveBeenCalledTimes(2)
        expect(persistence.markDirty).toHaveBeenCalledOnce()
    })
})
