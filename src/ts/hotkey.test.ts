import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

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
        MobileGUIStack: store(0),
        MobileSideBar: store(0),
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
    MobileGUIStack: mocks.MobileGUIStack,
    MobileSideBar: mocks.MobileSideBar,
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

import { initHotkey, initMobileGesture } from './hotkey'

afterEach(() => vi.restoreAllMocks())

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

    it.each([
        { isComposing: true },
        { isComposing: false, keyCode: 229 },
    ])('does not run global actions for IME-owned keys %j', async (composition) => {
        let keydown!: (event: KeyboardEvent) => Promise<void>
        vi.spyOn(document, 'addEventListener').mockImplementation((type, handler) => {
            if (type === 'keydown') keydown = handler as unknown as typeof keydown
        })
        initHotkey()
        const event = new KeyboardEvent('keydown', { key: 'h', cancelable: true, ...composition })
        await keydown(event)
        expect(mocks.deactivateActiveWorkingSet).not.toHaveBeenCalled()
        expect(event.defaultPrevented).toBe(false)
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

describe('mobile gesture ownership', () => {
    function gestures() {
        const handlers = new Map<string, (event: TouchEvent) => void>()
        vi.spyOn(document, 'addEventListener').mockImplementation((type, handler) => {
            handlers.set(type, handler as (event: TouchEvent) => void)
        })
        mocks.selectedCharID.set(-1)
        mocks.MobileGUIStack.set(0)
        mocks.MobileSideBar.set(0)
        initMobileGesture()
        return (type: string, touches: Array<{ identifier: number; clientX: number; target: Element; clientY?: number }>) => {
            const event = new Event(type)
            Object.defineProperty(event, 'changedTouches', { value: touches.map((touch) => ({ clientY: 0, ...touch })) })
            handlers.get(type)!(event as TouchEvent)
        }
    }

    it('ignores buttons, their SVG children, and editable ancestors without losing another finger', () => {
        const dispatch = gestures()
        const parent = document.createElement('div')
        parent.innerHTML = '<button><svg><path/></svg></button><div contenteditable="true"><span>text</span></div>'
        const targets = [parent.querySelector('button')!, parent.querySelector('path')!, parent.querySelector('span')!]
        for (const [identifier, target] of targets.entries()) {
            dispatch('touchstart', [{ identifier, clientX: 100, target }])
            expect(() => dispatch('touchend', [{ identifier, clientX: 0, target }])).not.toThrow()
        }
        expect(get(mocks.MobileGUIStack)).toBe(0)
        dispatch('touchstart', [
            { identifier: 10, clientX: 100, target: targets[0] },
            { identifier: 11, clientX: 100, target: parent },
        ])
        dispatch('touchend', [{ identifier: 11, clientX: 0, target: parent }])
        expect(get(mocks.MobileGUIStack)).toBe(1)
    })

    it('forgets cancelled and completed touches before processing any late end event', () => {
        const dispatch = gestures()
        const target = document.createElement('div')
        dispatch('touchstart', [{ identifier: 1, clientX: 100, target }])
        dispatch('touchcancel', [{ identifier: 1, clientX: 100, target }])
        dispatch('touchend', [{ identifier: 1, clientX: 0, target }])
        expect(get(mocks.MobileGUIStack)).toBe(0)
        dispatch('touchstart', [{ identifier: 1, clientX: 100, target }])
        dispatch('touchend', [{ identifier: 1, clientX: 0, target }])
        dispatch('touchend', [{ identifier: 1, clientX: 0, target }])
        expect(get(mocks.MobileGUIStack)).toBe(1)
    })

    it('preserves horizontal navigation and ignores mostly vertical movement', () => {
        const dispatch = gestures()
        const target = document.createElement('div')
        mocks.selectedCharID.set(0)
        dispatch('touchstart', [{ identifier: 1, clientX: 100, target }])
        dispatch('touchend', [{ identifier: 1, clientX: 0, clientY: 150, target }])
        expect(get(mocks.MobileSideBar)).toBe(0)
        dispatch('touchstart', [{ identifier: 2, clientX: 100, target }])
        dispatch('touchend', [{ identifier: 2, clientX: 0, target }])
        expect(get(mocks.MobileSideBar)).toBe(1)
        dispatch('touchstart', [{ identifier: 3, clientX: 0, target }])
        dispatch('touchend', [{ identifier: 3, clientX: 100, target }])
        expect(get(mocks.MobileSideBar)).toBe(0)
    })
})
