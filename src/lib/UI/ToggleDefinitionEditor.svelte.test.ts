import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { writable } from 'svelte/store'
import { language } from 'src/lang'
import { RISU_TOGGLE_DRAG_TYPE } from 'src/ts/dragTypes'
import { stubDragEvent, touch } from 'src/ts/gui/htmlTouchDrag.testSupport'
import ToggleDefinitionEditor from './ToggleDefinitionEditor.svelte'

const textAreas = vi.hoisted(() => [] as { value: string; optimaizedInput?: boolean; onInput?: () => void }[])

vi.mock('src/ts/storage/database.svelte', () => ({}))
vi.mock('src/ts/characters', () => ({}))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: { db: {} },
    selectedCharID: writable(0),
    alertStore: writable({ type: 'none', msg: '' }),
}))
vi.mock('src/ts/process/modules', () => ({ getModuleToggles: () => '' }))
vi.mock('src/lib/UI/PopupList.svelte', () => ({ default: {} }))
vi.mock('src/lib/Others/Help.svelte', () => ({ default: () => {} }))
// The text area exposes the props it receives, so a raw edit can go through its value binding.
vi.mock('src/lib/UI/GUI/TextAreaInput.svelte', () => ({
    default: (_anchor: unknown, props: (typeof textAreas)[number]) => { textAreas.push(props) },
}))

let instance: ReturnType<typeof mount> | undefined
let props: { value: unknown; onChange: ReturnType<typeof vi.fn<(value: string) => void>> }

async function open(value: unknown) {
    const next = $state({ value, onChange: vi.fn<(value: string) => void>() })
    props = next
    instance = mount(ToggleDefinitionEditor, { target: document.body, props })
    await tick()
}
const all = <T extends Element>(selector: string, root: ParentNode = document) => [...root.querySelectorAll<T>(selector)]
const names = (root?: ParentNode) => all<HTMLInputElement>(`input[aria-label="${language.name}"]`, root)
const keys = () => all<HTMLInputElement>(`input[aria-label="${language.key}"]`)
const types = () => all<HTMLSelectElement>(`select[aria-label="${language.type}"]`)
const buttons = (label: string, root?: ParentNode) => all<HTMLButtonElement>(`button[aria-label="${label}"]`, root)
const button = (text: string, root?: ParentNode) => all<HTMLButtonElement>('button', root).find((element) => element.textContent?.trim() === text)!
const views = (root?: ParentNode) => all<HTMLButtonElement>('button[aria-pressed]', root).map((element) => element.textContent?.trim())
async function type(input: HTMLInputElement, value: string) {
    input.value = value
    input.dispatchEvent(new Event('input', { bubbles: true }))
    await tick()
}
async function show(view: string, root?: ParentNode) {
    button(view, root).click()
    await tick()
}
/** The preview switch a toggle name labels. */
function previewSwitch(name: string) {
    const label = all<HTMLLabelElement>('label').find((element) => element.textContent === name)
    return label ? document.getElementById(label.htmlFor) as HTMLInputElement : null
}
/** The row grip next to the name input that holds this name. */
function gripOf(name: string) {
    let element = names().find((input) => input.value === name)!.parentElement!
    while (![...element.children].some((child) => child.getAttribute('draggable') === 'true')) element = element.parentElement!
    return [...element.children].find((child) => child.getAttribute('draggable') === 'true')!
}
/** Lays elements out top to bottom in document order, 10 px each, every element as tall as its subtree. */
function layOut() {
    vi.spyOn(Element.prototype, 'getBoundingClientRect').mockImplementation(function (this: Element) {
        const order = [...document.body.querySelectorAll('*')]
        const inner = this.querySelectorAll('*')
        const top = order.indexOf(this) * 10
        const bottom = order.indexOf(inner.length ? inner[inner.length - 1] : this) * 10 + 10
        return { x: 0, y: top, top, bottom, left: 0, right: 100, width: 100, height: bottom - top, toJSON: () => ({}) } as DOMRect
    })
}
/** A height just inside the top edge of an element, which the drag reads as the gap before it. */
const topOf = (element: Element) => element.getBoundingClientRect().top + 1
function transfer() {
    const data = new Map<string, string>()
    return { get types() { return [...data.keys()] }, setData: (key: string, value: string) => data.set(key, value),
        getData: (key: string) => data.get(key) ?? '', setDragImage: vi.fn(), effectAllowed: 'all', dropEffect: 'none' }
}
async function dragEvent(element: Element, type: string, dataTransfer: ReturnType<typeof transfer>, clientY = 0) {
    const event = new Event(type, { bubbles: true, cancelable: true })
    Object.assign(event, { dataTransfer, clientX: 0, clientY })
    element.dispatchEvent(event)
    await tick()
    return event
}
async function drag(grip: Element, target: Element, clientY: number) {
    const data = transfer()
    await dragEvent(grip, 'dragstart', data)
    await dragEvent(target, 'dragenter', data, clientY)
    await dragEvent(target, 'dragover', data, clientY)
    await dragEvent(target, 'drop', data, clientY)
    await dragEvent(grip, 'dragend', data)
}

afterEach(async () => {
    if (instance) await unmount(instance)
    instance = undefined
    textAreas.length = 0
    vi.restoreAllMocks()
    document.body.replaceChildren()
})

describe('toggle definition editor', () => {
    it('writes nothing when it opens, switches views or opens its window', async () => {
        const texts = [undefined, 7, '', 'a=A\r\n\n=G=group\r\nb=B\n  junk\n', '=Open=group\nx=X=select=1,2', '\n\n']
        for (const value of texts) {
            await open(value)
            for (const view of [language.risuNest.toggleEditor.raw, language.preview, language.hotkeyDesc.edit]) await show(view)
            buttons(language.risuNest.textEditor.open)[0].click()
            await tick()
            const dialog = document.querySelector('[role="dialog"]')!
            for (const view of [language.risuNest.toggleEditor.raw, language.preview, language.hotkeyDesc.edit]) await show(view, dialog)
            await show(language.close, dialog)
            expect(props.onChange, JSON.stringify(value)).not.toHaveBeenCalled()
            await unmount(instance!)
            instance = undefined
        }
    })

    it('changes only the edited line and keeps its inputs when the saved text comes back', async () => {
        const text = 'a=A\r\n\n=G=group\nb=B\n=G=groupEnd\n  junk line\r\nc=C=select=x,y\r\n'
        await open(text)
        expect(names().map((input) => input.value)).toEqual(['A', 'G', 'B', '', 'C'])
        const name = names()[2]
        await type(name, 'Bee')
        const saved = 'a=A\r\n\n=G=group\nb=Bee\n=G=groupEnd\n  junk line\r\nc=C=select=x,y\r\n'
        expect(props.onChange).toHaveBeenLastCalledWith(saved)
        props.value = saved
        await tick()
        expect(names()[2]).toBe(name)

        const key = keys()[0]
        await type(key, 'a=')
        expect(key.value).toBe('a')
        expect(props.onChange).toHaveBeenCalledTimes(1)
        await type(key, 'k=ey')
        expect(key.value).toBe('key')
        expect(props.onChange).toHaveBeenLastCalledWith(saved.replace('a=A', 'key=A'))
    })

    it('reads a text that changes elsewhere again', async () => {
        await open('a=A')
        props.value = 'b=B\nc=C'
        await tick()
        expect(names().map((input) => input.value)).toEqual(['B', 'C'])
        expect(props.onChange).not.toHaveBeenCalled()
    })

    it('shows each row type, changes it, and offers groups only outside a group', async () => {
        const text = '=G=group\nc=C=select=x\n=G=groupEnd\nd=D=textarea\n==divider\n=Cap=caption\ne=E=text\r\nf=F'
        await open(text)
        expect(types().map((select) => select.value)).toEqual(['group', 'select', 'textarea', 'divider', 'caption', 'text', 'switch'])
        const offered = (select: HTMLSelectElement) => [...select.options].map((option) => option.value)
        expect(offered(types()[0])).toContain('group')
        expect(offered(types()[1])).not.toContain('group')
        const select = types()[6]
        select.value = 'group'
        select.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith(text.replace('f=F', '=F=group\n=F=groupEnd'))
    })

    it('moves and removes rows', async () => {
        await open('a=A\nb=B\nc=C\n')
        expect(buttons(language.movePresetUp).map((element) => element.disabled)).toEqual([true, false, false])
        expect(buttons(language.movePresetDown).map((element) => element.disabled)).toEqual([false, false, true])
        buttons(language.movePresetDown)[0].click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('b=B\na=A\nc=C\n')
        buttons(language.remove)[2].click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('b=B\na=A\n')
    })

    it('adds a row of the type chosen from a menu, at the end or at the end of a group', async () => {
        const copy = language.risuNest.toggleEditor
        await open('a=A\n=G=group\nb=B\n=G=groupEnd\n')
        const menuItems = () => all('[role="menuitem"]').map((item) => item.textContent)
        // The group's own button comes before the one for the whole list.
        const [groupAdd, listAdd] = all<HTMLButtonElement>('button[aria-haspopup="menu"]')
        listAdd.click()
        await tick()
        expect(menuItems()).toEqual([copy.switch, copy.select, copy.text, copy.textarea, copy.group, copy.divider, copy.caption])
        button(copy.select).click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('a=A\n=G=group\nb=B\n=G=groupEnd\n==select=\n')
        expect(menuItems()).toEqual([])
        expect(names().map((input) => input.value)).toEqual(['A', 'G', 'B', ''])
        groupAdd.click()
        await tick()
        expect(menuItems()).toEqual([copy.switch, copy.select, copy.text, copy.textarea, copy.divider, copy.caption])
        button(copy.text).click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('a=A\n=G=group\nb=B\n==text\n=G=groupEnd\n==select=\n')
        expect(props.onChange).toHaveBeenCalledTimes(2)
    })

    it('shows a line the sidebar skips as a row with a warning on its empty field, and leaves blank lines out', async () => {
        const copy = language.risuNest.toggleEditor
        await open('a=A\n\n=Orphan\nb=\n=G=group\n==caption\n=G=groupEnd\n  \nc=C')
        expect(names().map((input) => input.value)).toEqual(['A', 'Orphan', '', 'G', '', 'C'])
        const warning = (input: HTMLInputElement) => input.getAttribute('aria-invalid') === 'true'
            ? document.getElementById(input.getAttribute('aria-describedby')!)?.textContent
            : undefined
        expect(keys().map(warning)).toEqual([undefined, copy.missingKey, undefined, undefined])
        expect(names().map(warning)).toEqual([undefined, undefined, copy.missingName, undefined, copy.missingName, undefined])
        await show(language.preview)
        expect(document.body.textContent).toContain('A')
        expect(document.body.textContent).not.toContain('Orphan')

        // Filling in the empty field makes it a row the sidebar shows.
        await show(language.hotkeyDesc.edit)
        await type(keys()[1], 'orphan')
        expect(props.onChange).toHaveBeenLastCalledWith('a=A\n\norphan=Orphan\nb=\n=G=group\n==caption\n=G=groupEnd\n  \nc=C')
        expect(warning(keys()[1])).toBeUndefined()
        await show(language.preview)
        expect(document.body.textContent).toContain('Orphan')
        await show(language.hotkeyDesc.edit)
        buttons(language.remove)[2].click()
        await tick()
        expect(props.onChange).toHaveBeenLastCalledWith('a=A\n\norphan=Orphan\n=G=group\n==caption\n=G=groupEnd\n  \nc=C')
    })

    it('shows the same text in the raw view and saves raw edits', async () => {
        const text = 'a=A\r\n=G=group\nb=B'
        await open(text)
        await show(language.risuNest.toggleEditor.raw)
        const raw = textAreas.find((area) => area.optimaizedInput === false)!
        expect(raw.value).toBe(text)
        raw.onInput!()
        expect(props.onChange).not.toHaveBeenCalled()
        raw.value = 'x=X'
        raw.onInput!()
        expect(props.onChange).toHaveBeenLastCalledWith('x=X')
        await show(language.hotkeyDesc.edit)
        expect(names().map((input) => input.value)).toEqual(['X'])
    })

    it('previews the edited rows with groups open, without saving preview values', async () => {
        await open('=Empty group=group\n=Empty group=groupEnd')
        await show(language.preview)
        // An empty group shows nothing.
        expect(document.body.textContent).not.toContain('Empty group')
        await unmount(instance!)
        await open('=Mode=group\np=Power\n=Mode=groupEnd\nq=Quiet')
        await type(names()[1], 'Boost')
        await show(language.preview)
        expect(names()).toEqual([])
        const boost = previewSwitch('Boost')!
        expect(previewSwitch('Quiet')).not.toBeNull()
        props.onChange.mockClear()
        boost.click()
        await tick()
        expect(boost.checked).toBe(true)
        expect(props.onChange).not.toHaveBeenCalled()
    })

    it('moves a row dragged by its grip to the gap it is dropped on, writing once', async () => {
        await open('a=A\n\n=G=group\nb=B\n=G=groupEnd\nc=C\n')
        layOut()
        const rowOf = (name: string) => gripOf(name).parentElement!
        // The gaps before and after a row are its own.
        await drag(gripOf('C'), rowOf('C'), topOf(rowOf('C')))
        await drag(gripOf('A'), rowOf('G'), topOf(rowOf('G')))
        expect(props.onChange).not.toHaveBeenCalled()
        await drag(gripOf('C'), rowOf('B'), topOf(rowOf('B')))
        expect(props.onChange).toHaveBeenCalledTimes(1)
        expect(props.onChange).toHaveBeenLastCalledWith('a=A\n\n=G=group\nc=C\nb=B\n=G=groupEnd\n')
        expect(names().map((input) => input.value)).toEqual(['A', 'G', 'C', 'B'])
        // A row dropped below every other row leaves the group.
        await drag(gripOf('B'), rowOf('A'), 100000)
        expect(props.onChange).toHaveBeenLastCalledWith('a=A\n\n=G=group\nc=C\n=G=groupEnd\nb=B\n')
    })

    it('drags a group with its rows between groups, never into one', async () => {
        await open('=One=group\nx=X\n=One=groupEnd\n=Two=group\ny=Y\n=Two=groupEnd\n=Three=group\nz=Z\n=Three=groupEnd')
        layOut()
        // Over the end of the first group's rows, a dragged group is placed after that group.
        const firstGroupAdd = all<HTMLButtonElement>('button[aria-haspopup="menu"]')[0]
        await drag(gripOf('Three'), firstGroupAdd, topOf(firstGroupAdd))
        expect(props.onChange).toHaveBeenCalledTimes(1)
        expect(props.onChange).toHaveBeenLastCalledWith('=One=group\nx=X\n=One=groupEnd\n=Three=group\nz=Z\n=Three=groupEnd\n=Two=group\ny=Y\n=Two=groupEnd')
    })

    it('leaves drags of other types to the page and keeps its own from it', async () => {
        await open('a=A\nb=B')
        layOut()
        const outside = vi.fn()
        document.addEventListener('dragover', outside)
        document.addEventListener('drop', outside)
        try {
            const target = gripOf('A').parentElement!
            const other = transfer()
            other.setData('text/plain', 'b=B')
            expect((await dragEvent(target, 'dragenter', other, 0)).defaultPrevented).toBe(false)
            expect((await dragEvent(target, 'dragover', other, 0)).defaultPrevented).toBe(false)
            await dragEvent(target, 'drop', other, 0)
            expect(outside).toHaveBeenCalledTimes(2)
            expect(props.onChange).not.toHaveBeenCalled()

            const own = transfer()
            await dragEvent(gripOf('B'), 'dragstart', own)
            expect(own.types).toEqual([RISU_TOGGLE_DRAG_TYPE])
            expect((await dragEvent(target, 'dragenter', own, topOf(target))).defaultPrevented).toBe(true)
            await dragEvent(target, 'dragover', own, topOf(target))
            await dragEvent(target, 'drop', own, topOf(target))
            expect(outside).toHaveBeenCalledTimes(2)
            expect(props.onChange).toHaveBeenLastCalledWith('b=B\na=A')
        } finally {
            document.removeEventListener('dragover', outside)
            document.removeEventListener('drop', outside)
        }
    })

    it('moves a row held by its grip by touch', async () => {
        await open('a=A\nb=B\nc=C')
        layOut()
        const target = gripOf('A').parentElement!
        const grip = gripOf('C')
        const y = topOf(target)
        vi.useFakeTimers()
        stubDragEvent()
        vi.spyOn(HTMLElement.prototype, 'getClientRects').mockReturnValue([{}] as unknown as DOMRectList)
        vi.spyOn(document, 'elementFromPoint').mockReturnValue(target)
        try {
            touch(grip, 'touchstart', 10, 5000)
            vi.advanceTimersByTime(450)
            // The whole row is shown as held, not only its grip.
            expect(grip.parentElement!.classList).toContain('risu-sort-held')
            touch(grip, 'touchmove', 10, y)
            touch(grip, 'touchend', 10, y)
            await tick()
            expect(props.onChange).toHaveBeenCalledTimes(1)
            expect(props.onChange).toHaveBeenLastCalledWith('c=C\na=A\nb=B')
        } finally {
            vi.runOnlyPendingTimers()
            vi.useRealTimers()
            vi.unstubAllGlobals()
        }
    })

    it('edits in its pop-out window through the same change callback and closes back to the editor', async () => {
        await open('a=A\nb=B')
        expect(document.querySelector('[role="dialog"]')).toBeNull()
        buttons(language.risuNest.textEditor.open)[0].click()
        await tick()
        const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!
        expect(buttons(language.risuNest.textEditor.open, dialog)).toEqual([])
        expect(views(dialog)).toEqual([language.hotkeyDesc.edit, language.preview, language.risuNest.toggleEditor.raw])
        await type(names(dialog)[1], 'Bee')
        expect(props.onChange).toHaveBeenLastCalledWith('a=A\nb=Bee')
        props.value = 'a=A\nb=Bee'
        await tick()
        await show(language.close, dialog)
        expect(document.querySelector('[role="dialog"]')).toBeNull()
        expect(names().map((input) => input.value)).toEqual(['A', 'Bee'])
    })

    it('keeps the preview beside the rows in a wide pop-out window', async () => {
        vi.spyOn(HTMLElement.prototype, 'clientWidth', 'get').mockReturnValue(1000)
        await open('p=Power')
        buttons(language.risuNest.textEditor.open)[0].click()
        await tick()
        const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!
        expect(views(dialog)).toEqual([language.hotkeyDesc.edit, language.risuNest.toggleEditor.raw])
        await type(names(dialog)[0], 'Boost')
        expect(previewSwitch('Boost')).not.toBeNull()
    })
})
