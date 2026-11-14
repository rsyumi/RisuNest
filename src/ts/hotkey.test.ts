import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'
import { setAppLockInputBlocked } from './appLock/input'

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

vi.mock('./alert', async (importOriginal) => ({
    alertMd: vi.fn(),
    alertSelect: (await importOriginal<typeof import('./alert')>()).alertSelect,
    alertToast: mocks.alertToast,
    alertWait: vi.fn(),
    doingAlert: (await importOriginal<typeof import('./alert')>()).doingAlert,
    alertRequestLogs: vi.fn(),
}))
vi.mock('./storage/deviceMarkers', () => ({ getDeviceMarkers: vi.fn() }))
vi.mock('./characters', () => ({ changeChar: mocks.changeChar }))
vi.mock('./storage/database.svelte', () => ({
    changeToPreset: mocks.changeToPreset,
    getDatabase: () => mocks.database,
}))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    deactivateActiveWorkingSet: mocks.deactivateActiveWorkingSet,
}))
vi.mock('./stores.svelte', async () => ({
    alertStore: (await import('./alertQueue')).createAlertQueue({ type: 'none', msg: '' }, { gapMs: 0 }),
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
vi.mock('./gui/colorscheme', () => ({ updateTextThemeAndCSS: vi.fn() }))
vi.mock('./defaulthotkeys', () => ({ defaultHotkeys: [] }))
vi.mock('./process/index.svelte', () => ({
    doingChat: mocks.store(false),
    previewBody: '{}',
    sendChat: vi.fn(),
}))
vi.mock('./dragTypes', () => ({ RISU_SIDEBAR_DRAG_TYPE: 'character' }))

import { hotkeyMatches, initHotkey, initMobileGesture } from './hotkey'
import { alertStore, settingsOpen } from './stores.svelte'

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

    it.each([
        { name: 'an empty contenteditable', html: '<div contenteditable="" tabindex="0" data-focus></div>' },
        { name: 'an inherited contenteditable', html: '<div contenteditable="true"><span tabindex="0" data-focus>text</span></div>' },
        { name: 'a plaintext-only contenteditable', html: '<div contenteditable="plaintext-only" tabindex="0" data-focus></div>' },
        { name: 'an EditContext element', html: '<div tabindex="0" data-focus data-edit-context></div>' },
    ])('does not run unmodified hotkeys while typing in $name', async ({ html }) => {
        let keydown!: (event: KeyboardEvent) => Promise<void>
        vi.spyOn(document, 'addEventListener').mockImplementation((type, handler) => {
            if (type === 'keydown') keydown = handler as unknown as typeof keydown
        })
        initHotkey()
        const host = document.createElement('div')
        host.innerHTML = html
        document.body.append(host)
        const focused = host.querySelector<HTMLElement>('[data-focus]')!
        if (focused.hasAttribute('data-edit-context')) Object.assign(focused, { editContext: {} })
        try {
            focused.focus()
            expect(document.activeElement).toBe(focused)

            await keydown(new KeyboardEvent('keydown', { key: 'h', cancelable: true }))

            expect(mocks.deactivateActiveWorkingSet).not.toHaveBeenCalled()
            expect(hotkeyMatches({ key: 'h', action: 'home' } as never, new KeyboardEvent('keydown', { key: 'h' }))).toBe(false)

            settingsOpen.set(true)
            const escape = new KeyboardEvent('keydown', { key: 'Escape', cancelable: true })
            await keydown(escape)
            expect(get(settingsOpen)).toBe(true)
            expect(escape.defaultPrevented).toBe(false)
        } finally {
            settingsOpen.set(false)
            host.remove()
        }
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

describe('Escape', () => {
    let keydown!: (event: KeyboardEvent) => Promise<void>
    const escape = () => keydown(new KeyboardEvent('keydown', { key: 'Escape', cancelable: true }))

    beforeEach(() => {
        vi.useFakeTimers()
        mocks.alertToast.mockClear()
        vi.spyOn(document, 'addEventListener').mockImplementation((type, handler) => {
            if (type === 'keydown') keydown = handler as unknown as typeof keydown
        })
        initHotkey()
    })
    afterEach(async () => {
        while (alertStore.hasDialogs()) {
            await vi.advanceTimersByTimeAsync(0)
            alertStore.set({ type: 'none', msg: '' })
        }
        vi.useRealTimers()
    })

    it('closes the visible dialog and toasts only once no dialog is queued', async () => {
        const first = alertStore.open({ type: 'normal', msg: 'First' })
        const second = alertStore.open({ type: 'ask', msg: 'Second' })

        await escape()
        await expect(first).resolves.toBe('')
        expect(mocks.alertToast).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(0)
        expect(get(alertStore)).toMatchObject({ type: 'ask', msg: 'Second' })

        await escape()
        await expect(second).resolves.toBe('')
        expect(mocks.alertToast).toHaveBeenCalledExactlyOnceWith('Alert Closed')
    })

    it('leaves the next dialog and the settings open when Escape arrives before it shows', async () => {
        settingsOpen.set(true)
        const first = alertStore.open({ type: 'normal', msg: 'First' })
        const second = alertStore.open({ type: 'ask', msg: 'Second' })

        await escape()
        await escape()
        await expect(first).resolves.toBe('')
        expect(get(settingsOpen)).toBe(true)
        await vi.advanceTimersByTimeAsync(0)
        expect(get(alertStore)).toMatchObject({ type: 'ask', msg: 'Second' })
        expect(alertStore.hasDialogs()).toBe(true)
        expect(mocks.alertToast).not.toHaveBeenCalled()
        alertStore.set({ type: 'none', msg: 'yes' })
        await expect(second).resolves.toBe('yes')
        settingsOpen.set(false)
    })

    it('keeps a progress status, which is not a dialog', async () => {
        alertStore.set({ type: 'progress', msg: 'Saving', submsg: '50' })

        await escape()
        expect(get(alertStore)).toEqual({ type: 'progress', msg: 'Saving', submsg: '50' })
        expect(mocks.alertToast).not.toHaveBeenCalled()
        alertStore.clearStatus()
    })
})

describe('mobile gesture ownership', () => {
    afterEach(() => setAppLockInputBlocked(false))
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

    it('ignores buttons, their SVG children, sliders, and editable ancestors without losing another finger', () => {
        const dispatch = gestures()
        const parent = document.createElement('div')
        parent.innerHTML = '<button><svg><path/></svg></button><div contenteditable="true"><span>text</span></div>'
            + '<div role="slider" tabindex="0"><b>50</b></div><input type="range">'
        const targets = [parent.querySelector('button')!, parent.querySelector('path')!, parent.querySelector('span')!, parent.querySelector('b')!, parent.querySelector('input')!]
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

    it('does not open the three-finger quick menu while locked', async () => {
        let touchstart!: (event: Event) => void
        vi.spyOn(document, 'addEventListener').mockImplementation((type, handler) => {
            if (type === 'touchstart') touchstart = handler as typeof touchstart
        })
        initHotkey()
        alertStore.set({ type: 'none', msg: '' })
        setAppLockInputBlocked(true)
        for (let index = 0; index < 3; index++) touchstart(new Event('touchstart'))
        expect(get(alertStore).type).toBe('none')
        setAppLockInputBlocked(false)
        for (let index = 0; index < 3; index++) touchstart(new Event('touchstart'))
        expect(get(alertStore).type).toBe('select')
        alertStore.set({ type: 'none', msg: '' })
        await Promise.resolve()
    })

    it('ignores lock-screen swipes and a touch that crosses the lock boundary', () => {
        const dispatch = gestures()
        const target = document.createElement('div')
        dispatch('touchstart', [{ identifier: 1, clientX: 100, target }])
        setAppLockInputBlocked(true)
        dispatch('touchend', [{ identifier: 1, clientX: 0, target }])
        expect(get(mocks.MobileGUIStack)).toBe(0)
        dispatch('touchstart', [{ identifier: 2, clientX: 100, target }])
        dispatch('touchend', [{ identifier: 2, clientX: 0, target }])
        expect(get(mocks.MobileGUIStack)).toBe(0)
        dispatch('touchstart', [{ identifier: 3, clientX: 100, target }])
        setAppLockInputBlocked(false)
        dispatch('touchend', [{ identifier: 3, clientX: 0, target }])
        expect(get(mocks.MobileGUIStack)).toBe(0)
        dispatch('touchstart', [{ identifier: 4, clientX: 100, target }])
        dispatch('touchend', [{ identifier: 4, clientX: 0, target }])
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

    it('leaves a swipe to a horizontally scrollable block while the block could scroll that way when the touch started', () => {
        const dispatch = gestures()
        mocks.MobileGUIStack.set(1)
        const host = document.createElement('div')
        host.innerHTML = '<pre style="overflow-x: auto"><span>wide</span></pre><pre style="overflow-x: auto; direction: rtl"><span>wide</span></pre>'
        document.body.append(host)
        const [ltr, rtl] = Array.from(host.querySelectorAll('pre'))
        for (const block of [ltr, rtl]) {
            Object.defineProperties(block, {
                scrollWidth: { value: 300, configurable: true },
                clientWidth: { value: 100, configurable: true },
                scrollLeft: { value: 0, writable: true, configurable: true },
            })
        }
        const swipe = (block: HTMLElement, from: number, to: number, scrolledTo = block.scrollLeft) => {
            const target = block.querySelector('span')!
            dispatch('touchstart', [{ identifier: 1, clientX: from, target }])
            block.scrollLeft = scrolledTo
            dispatch('touchend', [{ identifier: 1, clientX: to, target }])
            return get(mocks.MobileGUIStack)
        }
        try {
            // The block scrolls to its end during the swipe; it could scroll when the finger landed.
            expect(swipe(ltr, 100, 0, 200)).toBe(1)
            expect(swipe(ltr, 100, 0)).toBe(2)
            expect(swipe(ltr, 0, 100)).toBe(2)
            ltr.scrollLeft = 0
            expect(swipe(ltr, 0, 100)).toBe(1)
            expect(swipe(rtl, 0, 100)).toBe(1)
            expect(swipe(rtl, 100, 0)).toBe(2)
        } finally {
            host.remove()
        }
    })

    it('does not switch screens while text is selected or a list row is dragged', () => {
        const dispatch = gestures()
        const target = document.createElement('div')
        target.textContent = 'Selectable text'
        document.body.append(target)
        const row = document.createElement('div')
        document.body.append(row)
        try {
            document.getSelection()!.selectAllChildren(target)
            dispatch('touchstart', [{ identifier: 1, clientX: 100, target }])
            dispatch('touchend', [{ identifier: 1, clientX: 0, target }])
            expect(get(mocks.MobileGUIStack)).toBe(0)
            document.getSelection()!.removeAllRanges()

            for (const className of ['risu-sort-held', 'sortable-chosen']) {
                dispatch('touchstart', [{ identifier: 2, clientX: 100, target }])
                row.classList.add(className)
                dispatch('touchmove', [{ identifier: 2, clientX: 50, target }])
                // The list drops the row on pointerup, which comes before touchend.
                row.classList.remove(className)
                dispatch('touchend', [{ identifier: 2, clientX: 0, target }])
                expect(get(mocks.MobileGUIStack)).toBe(0)
            }

            dispatch('touchstart', [{ identifier: 3, clientX: 100, target }])
            dispatch('touchmove', [{ identifier: 3, clientX: 50, target }])
            dispatch('touchend', [{ identifier: 3, clientX: 0, target }])
            expect(get(mocks.MobileGUIStack)).toBe(1)
        } finally {
            document.getSelection()!.removeAllRanges()
            target.remove()
            row.remove()
        }
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
