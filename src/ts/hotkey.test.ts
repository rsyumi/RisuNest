import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => {
    function store<T>(initial: T, onSet?: (value: T) => void) {
        let value = initial
        return {
            subscribe(run: (value: T) => void) {
                run(value)
                return () => undefined
            },
            set(next: T) {
                value = next
                onSet?.(next)
            },
            update(updater: (value: T) => T) {
                value = updater(value)
                onSet?.(value)
            },
        }
    }

    const events: string[] = []
    return {
        store,
        events,
        alertToast: vi.fn((message: string) => events.push(`toast:${message}`)),
        changeToPreset: vi.fn(),
        selectedCharID: store(1, (value) => events.push(`select:${value}`)),
        changeChar: vi.fn(async () => true),
        deactivateActiveWorkingSet: vi.fn(async () => {
            events.push('deactivate')
            return true
        }),
        database: {
            botPresets: [{ name: 'Target' }],
            characters: [
                { chaId: 'char-a', name: 'Alpha' },
                { chaId: 'char-b', name: 'Bravo' },
                { chaId: 'char-c', name: 'Charlie' },
            ],
            hotkeys: [
                { key: 'p', action: 'prevChar' },
                { key: 'n', action: 'nextChar' },
                { key: 'h', action: 'home' },
            ],
        },
    }
})

vi.mock('./alert', () => ({
    alertMd: vi.fn(),
    alertSelect: vi.fn(),
    alertToast: mocks.alertToast,
    alertWait: vi.fn(),
    doingAlert: vi.fn(() => false),
    alertRequestLogs: vi.fn(),
}))
vi.mock('./characters', () => ({ changeChar: mocks.changeChar }))
vi.mock('./storage/database.svelte', () => ({
    changeToPreset: mocks.changeToPreset,
    getDatabase: () => mocks.database,
}))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    deactivateActiveWorkingSet: mocks.deactivateActiveWorkingSet,
}))
vi.mock('./stores.svelte', () => ({
    alertStore: mocks.store({ type: 'none' }),
    DBState: { db: mocks.database },
    loadoutModalStore: { open: false },
    MobileGUIStack: mocks.store(0),
    MobileSideBar: mocks.store(0),
    openPersonaList: mocks.store(false),
    openPresetList: mocks.store(false),
    OpenRealmStore: mocks.store(false),
    PlaygroundStore: mocks.store(0),
    QuickSettings: { open: false, index: 0 },
    SafeModeStore: mocks.store(false),
    selectedCharID: mocks.selectedCharID,
    settingsOpen: mocks.store(false),
}))
vi.mock('src/lang', () => ({ language: { presets: '', persona: '', cancel: '', hotkeyDesc: {} } }))
vi.mock('./gui/colorscheme', () => ({ updateTextThemeAndCSS: vi.fn() }))
vi.mock('./defaulthotkeys', () => ({ defaultHotkeys: [] }))
vi.mock('./process/index.svelte', () => ({
    doingChat: mocks.store(false),
    previewBody: '{}',
    sendChat: vi.fn(),
}))
vi.mock('./dragTypes', () => ({ RISU_SIDEBAR_DRAG_TYPE: 'character' }))

import { initHotkey } from './hotkey'

describe('character hotkeys', () => {
    beforeEach(() => {
        mocks.events.length = 0
        mocks.changeChar.mockClear()
        mocks.changeToPreset.mockReset()
        mocks.alertToast.mockClear()
        mocks.deactivateActiveWorkingSet.mockClear()
        mocks.selectedCharID.set(1)
        mocks.events.length = 0
    })

    it('hydrates previous and next characters and invalidates pending navigation before Home', async () => {
        let keydown!: (event: KeyboardEvent) => Promise<void>
        const listener = vi.spyOn(document, 'addEventListener').mockImplementation((type, handler) => {
            if (type === 'keydown') keydown = handler as unknown as typeof keydown
        })
        initHotkey()

        await keydown(new KeyboardEvent('keydown', { key: 'p' }))
        await keydown(new KeyboardEvent('keydown', { key: 'n' }))

        expect(mocks.changeChar).toHaveBeenNthCalledWith(1, 0)
        expect(mocks.changeChar).toHaveBeenNthCalledWith(2, 2)
        expect(mocks.events).toEqual([])

        await keydown(new KeyboardEvent('keydown', { key: 'h' }))

        expect(mocks.events).toEqual(['deactivate', 'select:-1'])
        listener.mockRestore()
    })

    it('waits for preset activation before reporting the hotkey switch', async () => {
        let keydown!: (event: KeyboardEvent) => Promise<void>
        vi.spyOn(document, 'addEventListener').mockImplementation((type, handler) => {
            if (type === 'keydown') keydown = handler as unknown as typeof keydown
        })
        let finish!: () => void
        mocks.changeToPreset.mockReturnValueOnce(new Promise<void>((resolve) => {
            finish = resolve
        }))
        mocks.database.botPresets = [{ name: 'Target' }]
        initHotkey()

        const event = new KeyboardEvent('keydown', {
            key: '1',
            ctrlKey: true,
            cancelable: true,
        })
        const switching = keydown(event)

        expect(event.defaultPrevented).toBe(true)
        expect(mocks.changeToPreset).toHaveBeenCalledWith(0)
        expect(mocks.alertToast).not.toHaveBeenCalled()

        finish()
        await switching

        expect(mocks.alertToast).toHaveBeenCalledWith('Changed to Preset: Target')
        vi.restoreAllMocks()
    })
})
