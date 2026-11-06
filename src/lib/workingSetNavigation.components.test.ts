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
    'DownloadIcon', 'PencilIcon', 'HardDriveUploadIcon', 'TrashIcon', 'SplitIcon', 'FolderPlusIcon', 'BookmarkCheckIcon',
    'PackageIcon', 'ChevronRightIcon',
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
        bookmarkListOpen: writable(false), moduleScopeMenuOpen: writable(false),
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
    addNewChat: vi.fn(), duplicateChat: vi.fn(), editSelectedChatList: vi.fn(), exportChat: vi.fn(),
    importChat: vi.fn(), exportAllChats: vi.fn(), removeChat: vi.fn(),
}))
vi.mock('src/ts/storage/database.svelte', () => ({ setDatabase: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertError: persistence.alertError, alertInput: vi.fn(), alertSelect: vi.fn(),
    alertChatOptions: vi.fn(), alertConfirm: vi.fn(), alertNormal: vi.fn(), alertStore: {subscribe: vi.fn()},
}))
vi.mock('src/ts/chatBindings.svelte', () => ({bindPersona: vi.fn(), chatBindingBlockedByGeneration: () => false, saveChatBinding: vi.fn()}))
vi.mock('src/ts/globalApi.svelte', () => ({ checkCharOrder: vi.fn(), getFileSrc: vi.fn(), saveAsset: vi.fn(), changeChatTo: vi.fn() }))
vi.mock('src/ts/util', async () => {
    const { DBState } = await import('src/ts/stores.svelte')
    return {
        getCharacterIndexObject: () => ({}), selectSingleFile: vi.fn(), sortableOptions: {},
        findCharacterIndexbyId: (id: string) => DBState.db.characters.findIndex((character) => character.chaId === id),
    }
})
vi.mock('src/ts/gui/guisize', async () => ({ sideBarSize: (await import('svelte/store')).writable(300) }))

import { language } from 'src/lang'
import { DBState, MobileSideBar, PlaygroundStore, selectedCharID, settingsOpen, SettingsMenuIndex } from 'src/ts/stores.svelte'
import { createCatalogCharacterStub } from 'src/ts/storage/workingSetCatalog'

const visualPanels = [
    './SideBars/SidebarIndicator.svelte', './SideBars/CharConfig.svelte',
    './SideBars/SelectedConversationEditor.svelte', './SideBars/SidebarAvatar.svelte',
    './SideBars/Toggles.svelte', './SideBars/DevTool.svelte',
    './UI/GUI/Button.svelte', './UI/GUI/CheckInput.svelte', './UI/GUI/TextInput.svelte',
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
        { type: 'character', chaId: 'synthetic-owner', name: 'Synthetic owner', chatPage: 0, chats: [], chatFolders: [] },
        { type: 'character', chaId: '§playground', name: 'Playground owner', chatPage: 0, chats: [], chatFolders: [] },
    ] as typeof DBState.db.characters
    DBState.db.characterOrder = []
    DBState.db.menuSideBar = true
    selectedCharID.set(0)
    PlaygroundStore.set(0)
    settingsOpen.set(false)
    SettingsMenuIndex.set(-1)
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
    it('opens mobile settings from the chat menu and returns without deactivating the conversation', async () => {
        MobileSideBar.set(1)
        persistence.deactivate.mockResolvedValue(false)
        mounted = mount(MobileHeader, { target })
        await tick()
        buttonNamed(language.settings).click()
        await tick()
        expect(get(settingsOpen)).toBe(true)
        expect(get(selectedCharID)).toBe(0)
        expect(get(MobileSideBar)).toBe(1)
        expect(persistence.deactivate).not.toHaveBeenCalled()

        SettingsMenuIndex.set(1)
        await tick()
        buttonNamed(language.goback).click()
        await tick()
        expect(get(SettingsMenuIndex)).toBe(-1)
        expect(get(settingsOpen)).toBe(true)
        buttonNamed(language.goback).click()
        await tick()
        expect(get(settingsOpen)).toBe(false)
        expect(get(selectedCharID)).toBe(0)
        expect(get(MobileSideBar)).toBe(1)
        expect(persistence.deactivate).not.toHaveBeenCalled()
    })

    it('keeps the open chat drawer safe while Home releases the selected character before clearing selection', async () => {
        persistence.deactivate.mockImplementationOnce(async () => {
            DBState.db.characters[0] = createCatalogCharacterStub({id: 'synthetic-owner', name: 'Synthetic owner',
                type: 'character', configuredIndex: 0, recentAt: 0, conversationCount: 0, trashed: false})
            await tick()
            expect(get(selectedCharID)).toBe(0)
            return true
        })
        mounted = mount(Sidebar, {target})
        await tick()
        buttonNamed(language.home).click()
        await vi.waitFor(() => expect(get(selectedCharID)).toBe(-1))
        expect(persistence.alertError).not.toHaveBeenCalled()
        expect(DBState.db.characters[0]).not.toHaveProperty('chatFolders')
    })

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
