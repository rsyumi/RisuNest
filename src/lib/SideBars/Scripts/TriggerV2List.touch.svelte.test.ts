// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { language } from 'src/lang'
import { stubDragEvent, touch } from 'src/ts/gui/htmlTouchDrag.testSupport'

vi.mock('@lucide/svelte', () => ({ PlusIcon: () => {}, ArrowLeftIcon: () => {}, DownloadIcon: () => {}, UploadIcon: () => {} }))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: { showDeprecatedTriggerV2: false } } }))
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: vi.fn() }) satisfies Partial<typeof import('src/ts/globalApi.svelte')>)
vi.mock('src/ts/storage/fileSelection', () => ({ selectDomFiles: vi.fn() }) satisfies Partial<typeof import('src/ts/storage/fileSelection')>)
vi.mock('src/ts/process/triggers', () => ({ displayAllowList: [], requestAllowList: [] }) satisfies Partial<typeof import('src/ts/process/triggers')>)
vi.mock('src/lib/UI/GUI/TextAreaInput.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))

import TriggerV2List from './TriggerV2List.svelte'

const ROW = 40
const innerWidth = window.innerWidth
let instance: ReturnType<typeof mount> | undefined
let value = $state<any[]>([])
let hits: Element[] = []

const comment = (text: string) => ({ type: 'v2Comment', value: text, indent: 0 })
const buttons = () => [...document.querySelectorAll('button')]
const buttonNamed = (name: string) => buttons().find(button => button.textContent?.trim() === name)!
const offersPaste = () => buttons().some(button => button.textContent?.trim() === language.paste)
const triggerRows = () => [...document.querySelectorAll<HTMLElement>('button.trigger-item')]
// Each effect row holds the effect text and, for effects that can move, the grip.
const effectRows = () => [...document.querySelectorAll<HTMLElement>('[draggable="true"]:not(.trigger-item)')]
    .map(grip => grip.parentElement!)

// Stacks the rows from the top of the screen; the finger hits `hit(row)` inside the row under it.
function layOut(rows: HTMLElement[], hit = (row: HTMLElement): Element => row) {
    hits = rows.map(hit)
    rows.forEach((row, index) => vi.spyOn(row, 'getBoundingClientRect').mockReturnValue({
        top: index * ROW, bottom: (index + 1) * ROW, left: 0, right: 300, width: 300, height: ROW, x: 0, y: index * ROW,
    } as DOMRect))
}

async function setup() {
    instance = mount(TriggerV2List, { target: document.body, props: {
        get value() { return value }, set value(next) { value = next },
    } })
    await tick()
    buttonNamed(language.edit).click()
    await tick()
}

function hold(element: Element, y: number) {
    touch(element, 'touchstart', 30, y)
    vi.advanceTimersByTime(450)
}

function drag(element: Element, fromY: number, toY: number) {
    hold(element, fromY)
    touch(element, 'touchmove', 30, toY)
    touch(element, 'touchend', 30, toY)
    vi.runOnlyPendingTimers()
}

async function openMenu(element: Element, y: number) {
    hold(element, y)
    touch(element, 'touchend', 30, y)
    await tick()
}

beforeEach(() => {
    vi.useFakeTimers()
    stubDragEvent()
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 375 })
    vi.spyOn(Element.prototype, 'getClientRects').mockReturnValue([{}] as unknown as DOMRectList)
    vi.spyOn(document, 'elementFromPoint').mockImplementation((_x, y) => hits[Math.floor(y / ROW)] ?? null)
    value = [
        { comment: '', type: 'start', conditions: [], effect: [] },
        ...['First', 'Second', 'Third'].map(name => ({
            comment: name, type: 'manual', conditions: [], effect: ['one', 'two', 'three'].map(comment),
        })),
    ]
})

afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    hits = []
    document.body.replaceChildren()
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: innerWidth })
    // Globals stay stubbed: unstubbing would also drop the safeStructuredClone the setup file provides.
    vi.restoreAllMocks()
    vi.useRealTimers()
})

describe('TriggerV2List touch on a phone-width screen', () => {
    it('moves a held trigger to where the finger releases it', async () => {
        await setup()
        const rows = triggerRows()
        layOut(rows)
        hold(rows[2], 100)
        touch(rows[2], 'touchmove', 30, 10)
        // The shared look of a dragged row dims its child elements, so the name has to be one.
        expect(rows[2].classList.contains('risu-drag-source')).toBe(true)
        expect([...rows[2].children].map(child => child.textContent)).toEqual(['Third'])
        touch(rows[2], 'touchend', 30, 10)
        vi.runOnlyPendingTimers()
        expect(value.map(trigger => trigger.comment)).toEqual(['', 'Third', 'First', 'Second'])
    })

    it('moves an effect held by its grip to where the finger releases it, showing the whole row as dragged', async () => {
        await setup()
        const rows = effectRows()
        layOut(rows, row => row.firstElementChild!)
        const dots = rows[2].lastElementChild!.firstElementChild!
        hold(dots, 100)
        expect(rows[2].classList.contains('risu-sort-held')).toBe(true)
        touch(dots, 'touchmove', 30, 10)
        expect(document.querySelector('.risu-drag-card')?.textContent).toContain('three')
        expect(rows[2].classList.contains('risu-drag-source')).toBe(true)
        touch(dots, 'touchend', 30, 10)
        vi.runOnlyPendingTimers()
        expect(value[1].effect.map((effect: { value: string }) => effect.value)).toEqual(['three', 'one', 'two'])
        expect(document.querySelector('.risu-sort-held, .risu-drag-source, .risu-drag-card')).toBeNull()
    })

    it('copies and pastes a trigger through the menu a stationary hold opens, and closes it on an outside touch', async () => {
        await setup()
        const rows = triggerRows()
        await openMenu(rows[1], 60)
        buttonNamed(language.copy).click()
        await tick()
        await openMenu(rows[2], 100)
        buttonNamed(language.paste).click()
        await tick()
        expect(value.map(trigger => trigger.comment)).toEqual(['', 'First', 'Second', 'Second', 'Third'])

        await openMenu(rows[0], 20)
        expect(offersPaste()).toBe(true)
        touch(document.body, 'touchstart', 5, 5)
        await tick()
        expect(offersPaste()).toBe(false)
    })

    it('copies an effect and pastes it at the end through the menus a stationary hold opens', async () => {
        await setup()
        await openMenu(effectRows()[0].firstElementChild!.firstElementChild!, 20)
        buttonNamed(language.copy).click()
        await tick()
        await openMenu(buttonNamed('...'), 200)
        buttonNamed(language.paste).click()
        await tick()
        expect(value[1].effect.map((effect: { value: string }) => effect.value)).toEqual(['one', 'two', 'three', 'one'])
    })
})
