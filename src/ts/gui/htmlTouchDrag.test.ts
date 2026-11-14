import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { htmlTouchDrag } from './htmlTouchDrag'
import { stubDragEvent, touch } from './htmlTouchDrag.testSupport'

const TYPE = 'application/x-risu-test-row'
let node: HTMLElement
let source: HTMLElement
let label: HTMLElement
let zone: HTMLElement
let zoneChild: HTMLElement
let other: HTMLElement
let action: ReturnType<typeof htmlTouchDrag>
let started: ReturnType<typeof vi.fn<(event: DragEvent) => void>>
let dropped: ReturnType<typeof vi.fn<(event: DragEvent) => void>>
let ended: ReturnType<typeof vi.fn<(event: DragEvent) => void>>
let hit: Element | null

const finger = (type: string, x = 30, y = 60, count = 1) => touch(label, type, x, y, count)
const held = () => source.classList.contains('risu-sort-held')
const lifted = () => source.classList.contains('risu-drag-source')
function click() {
    const event = new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 })
    label.dispatchEvent(event)
    return event
}
function dragTo(x: number, y: number) {
    finger('touchstart')
    vi.advanceTimersByTime(450)
    finger('touchmove', x, y)
}

beforeEach(() => {
    vi.useFakeTimers()
    stubDragEvent()
    node = document.createElement('div')
    node.innerHTML = '<div draggable="true"><span>One</span></div><div><span>Zone</span></div><p>Other</p>'
    document.body.append(node)
    source = node.children[0] as HTMLElement
    label = source.children[0] as HTMLElement
    zone = node.children[1] as HTMLElement
    zoneChild = zone.children[0] as HTMLElement
    other = node.children[2] as HTMLElement
    hit = zoneChild
    vi.spyOn(node, 'getClientRects').mockReturnValue([{}] as unknown as DOMRectList)
    vi.spyOn(source, 'getBoundingClientRect').mockReturnValue({ top: 50, left: 10, width: 60, height: 20 } as DOMRect)
    vi.spyOn(document, 'elementFromPoint').mockImplementation(() => hit)
    started = vi.fn((event: DragEvent) => event.dataTransfer!.setData(TYPE, 'one'))
    dropped = vi.fn()
    ended = vi.fn()
    source.addEventListener('dragstart', started)
    source.addEventListener('dragend', ended)
    zone.addEventListener('dragover', event => {
        if (!event.dataTransfer!.types.includes(TYPE)) return
        event.preventDefault()
        event.dataTransfer!.dropEffect = 'move'
    })
    zone.addEventListener('drop', dropped)
    action = htmlTouchDrag(node)
})

afterEach(() => {
    action.destroy()
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
    vi.useRealTimers()
})

it('leaves a short tap clickable and excludes the legacy touch polyfill', () => {
    const legacy = vi.fn()
    document.addEventListener('touchstart', legacy)
    try {
        expect(finger('touchstart').defaultPrevented).toBe(false)
        vi.advanceTimersByTime(100)
        expect(finger('touchend').defaultPrevented).toBe(false)
        expect(click().defaultPrevented).toBe(false)
        expect(legacy).not.toHaveBeenCalled()
        expect(started).not.toHaveBeenCalled()
    } finally { document.removeEventListener('touchstart', legacy) }
})

it('preserves scrolling started before the hold delay, including a later pause', () => {
    finger('touchstart')
    vi.advanceTimersByTime(80)
    expect(finger('touchmove', 30, 30).defaultPrevented).toBe(false)
    vi.advanceTimersByTime(700)
    expect(held()).toBe(false)
    expect(finger('touchmove', 30, 160).defaultPrevented).toBe(false)
    finger('touchend', 30, 160)
    expect(started).not.toHaveBeenCalled()
    expect(dropped).not.toHaveBeenCalled()
})

it('marks the held row, then drags a card from where it was grabbed onto the element under the finger', () => {
    finger('touchstart')
    finger('touchmove', 32, 62)
    expect(held()).toBe(false)
    vi.advanceTimersByTime(450)
    expect(held()).toBe(true)
    expect(finger('touchmove', 33, 63).defaultPrevented).toBe(true)
    expect(document.querySelector('.risu-drag-card')).toBeNull()
    expect(lifted()).toBe(false)
    expect(finger('touchmove', 30, 160).defaultPrevented).toBe(true)
    expect(started).toHaveBeenCalledOnce()
    const card = document.querySelector<HTMLElement>('.risu-drag-card')!
    // The row shows where its card came from, and only the card looks lifted.
    expect([held(), lifted()]).toEqual([false, true])
    expect(card.classList.contains('risu-sort-held') || card.classList.contains('risu-drag-source')).toBe(false)
    expect(card.textContent).toBe('One')
    expect(card.style.pointerEvents).toBe('none')
    // Grabbed 20 px right of and 10 px below the row's corner.
    expect(card.style.transform).toBe('translate(10px, 150px)')
    expect(finger('touchend', 30, 170).defaultPrevented).toBe(true)
    expect(dropped).toHaveBeenCalledOnce()
    expect(dropped.mock.calls[0][0].target).toBe(zoneChild)
    expect(dropped.mock.calls[0][0].clientY).toBe(170)
    expect(ended).toHaveBeenCalledOnce()
    expect(document.querySelector('.risu-drag-card')).toBeNull()
    expect([held(), lifted()]).toEqual([false, false])
    expect(click().defaultPrevented).toBe(true)
})

it('enters and leaves elements under the finger in native order and cancels a drop nothing accepts', () => {
    const seen: [string, EventTarget | null, EventTarget | null][] = []
    const record = (event: Event) => seen.push([event.type, event.target, (event as MouseEvent).relatedTarget])
    const types = ['dragenter', 'dragleave', 'drop']
    for (const type of types) document.addEventListener(type, record)
    let effect = ''
    source.addEventListener('dragend', event => { effect = event.dataTransfer!.dropEffect })
    try {
        dragTo(30, 160)
        hit = other
        finger('touchmove', 30, 200)
        finger('touchend', 30, 200)
    } finally { for (const type of types) document.removeEventListener(type, record) }
    expect(seen).toEqual([
        ['dragenter', zoneChild, null],
        ['dragenter', other, zoneChild],
        ['dragleave', zoneChild, other],
        ['dragleave', other, null],
    ])
    expect(dropped).not.toHaveBeenCalled()
    expect(ended).toHaveBeenCalledOnce()
    expect(effect).toBe('none')
})

it('opens the existing context menu only on release of a stationary hold', () => {
    const menu = vi.fn()
    label.addEventListener('contextmenu', menu)
    finger('touchstart')
    vi.advanceTimersByTime(500)
    const nativeMenu = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
    label.dispatchEvent(nativeMenu)
    expect(nativeMenu.defaultPrevented).toBe(true)
    expect(menu).not.toHaveBeenCalled()
    finger('touchend')
    expect(menu).toHaveBeenCalledOnce()
    expect(started).not.toHaveBeenCalled()
    expect(click().defaultPrevented).toBe(true)
})

it.each(['touchcancel', 'multitouch', 'blur', 'destroy', 'undraggable'])('cancels an active drag on %s without dropping', reason => {
    dragTo(30, 160)
    if (reason === 'touchcancel') finger('touchcancel')
    if (reason === 'multitouch') touch(document.body, 'touchstart', 200, 160, 2)
    if (reason === 'blur') window.dispatchEvent(new Event('blur'))
    if (reason === 'destroy') action.destroy()
    if (reason === 'undraggable') {
        source.setAttribute('draggable', 'false')
        vi.advanceTimersByTime(20)
    }
    finger('touchend', 30, 170)
    expect(dropped).not.toHaveBeenCalled()
    expect(ended).toHaveBeenCalledOnce()
    expect(document.querySelector('.risu-drag-card')).toBeNull()
    expect([held(), lifted()]).toEqual([false, false])
})

it('ends the drag and leaves no card when dragstart takes the row out of the list', () => {
    source.addEventListener('dragstart', () => source.setAttribute('draggable', 'false'))
    dragTo(30, 160)
    expect(document.querySelector('.risu-drag-card')).toBeNull()
    expect(ended).toHaveBeenCalledOnce()
    expect([held(), lifted()]).toEqual([false, false])
    finger('touchend', 30, 170)
    expect(dropped).not.toHaveBeenCalled()
})

it('shows the item around the touched source as dragged when one matches inside the list, and the source otherwise', () => {
    action.destroy()
    // The outer section is outside the list, so it never counts as the item.
    const outer = document.createElement('section')
    document.body.append(outer)
    outer.append(node)
    const row = document.createElement('section')
    row.innerHTML = '<span>Row</span>'
    source.before(row)
    row.append(source)
    const loose = document.createElement('div')
    loose.setAttribute('draggable', 'true')
    loose.innerHTML = '<span>Loose</span>'
    node.append(loose)
    action = htmlTouchDrag(node, { item: 'section' })
    const classes = (element: Element) => ['risu-sort-held', 'risu-drag-source'].filter(name => element.classList.contains(name))

    finger('touchstart')
    vi.advanceTimersByTime(450)
    expect([classes(row), classes(source)]).toEqual([['risu-sort-held'], []])
    finger('touchmove', 30, 160)
    expect(started).toHaveBeenCalledOnce()
    const card = document.querySelector('.risu-drag-card')!
    expect(card.textContent).toBe('RowOne')
    expect(classes(card)).toEqual([])
    expect([classes(row), classes(source)]).toEqual([['risu-drag-source'], []])
    finger('touchend', 30, 170)
    expect(dropped).toHaveBeenCalledOnce()
    expect(ended).toHaveBeenCalledOnce()
    expect(classes(row)).toEqual([])

    dragTo(30, 160)
    expect(classes(row)).toEqual(['risu-drag-source'])
    finger('touchcancel')
    expect(classes(row)).toEqual([])
    expect(dropped).toHaveBeenCalledOnce()

    finger('touchstart')
    vi.advanceTimersByTime(450)
    expect(classes(row)).toEqual(['risu-sort-held'])
    finger('touchend')
    expect(classes(row)).toEqual([])

    const looseLabel = loose.firstElementChild!
    touch(looseLabel, 'touchstart', 30, 60)
    vi.advanceTimersByTime(450)
    touch(looseLabel, 'touchmove', 30, 160)
    expect(document.querySelector('.risu-drag-card')!.textContent).toBe('Loose')
    expect([classes(loose), classes(outer)]).toEqual([['risu-drag-source'], []])
    touch(looseLabel, 'touchend', 30, 170)
    expect(classes(loose)).toEqual([])
})

it('narrows the card to the ghost inside the item', () => {
    action.destroy()
    const row = document.createElement('section')
    row.innerHTML = '<b>Title</b>'
    source.before(row)
    row.append(source)
    action = htmlTouchDrag(node, { item: 'section', ghost: 'b' })
    dragTo(30, 160)
    expect(document.querySelector('.risu-drag-card')!.textContent).toBe('Title')
    finger('touchend', 30, 170)
})

it('keeps held and source classes the list set itself, and keeps them off the card', () => {
    action.destroy()
    const row = document.createElement('section')
    source.before(row)
    row.append(source)
    row.classList.add('risu-sort-held', 'risu-drag-source')
    action = htmlTouchDrag(node, { item: 'section' })
    dragTo(30, 160)
    const card = document.querySelector('.risu-drag-card')!
    expect(card.classList.contains('risu-sort-held') || card.classList.contains('risu-drag-source')).toBe(false)
    finger('touchend', 30, 170)
    expect(['risu-sort-held', 'risu-drag-source'].filter(name => row.classList.contains(name))).toEqual(['risu-sort-held', 'risu-drag-source'])
})

it('scrolls the scroll container under the finger near its edge, not one covered by another layer', () => {
    const scroller = document.createElement('div')
    scroller.style.overflowY = 'auto'
    Object.defineProperties(scroller, { scrollHeight: { value: 900 }, clientHeight: { value: 300 } })
    vi.spyOn(scroller, 'getBoundingClientRect').mockReturnValue({ top: 0, bottom: 300, left: 0, right: 80, height: 300 } as DOMRect)
    document.body.append(scroller)
    scroller.append(node)
    dragTo(30, 290)
    vi.advanceTimersByTime(100)
    const scrolled = scroller.scrollTop
    expect(scrolled).toBeGreaterThan(0)
    expect(node.scrollTop).toBe(0)
    hit = document.body
    vi.advanceTimersByTime(100)
    expect(scroller.scrollTop).toBe(scrolled)
    finger('touchend', 30, 290)
})

it('does not intercept mouse dragging, right click or keyboard activation', () => {
    const drag = new DragEvent('dragstart', { bubbles: true, cancelable: true, dataTransfer: new DataTransfer() })
    source.dispatchEvent(drag)
    expect(drag.defaultPrevented).toBe(false)
    const menu = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
    label.dispatchEvent(menu)
    expect(menu.defaultPrevented).toBe(false)
    const keyboardClick = new MouseEvent('click', { bubbles: true, cancelable: true, detail: 0 })
    label.dispatchEvent(keyboardClick)
    expect(keyboardClick.defaultPrevented).toBe(false)
})

it('turns off text selection on the touched source only and restores its previous style', () => {
    source.style.setProperty('user-select', 'text', 'important')
    finger('touchstart')
    expect(source.style.getPropertyValue('user-select')).toBe('none')
    expect(node.style.getPropertyValue('user-select')).toBe('')
    finger('touchend')
    expect(source.style.getPropertyValue('user-select')).toBe('text')
    expect(source.style.getPropertyPriority('user-select')).toBe('important')
    finger('touchstart')
    action.destroy()
    expect(source.style.getPropertyValue('user-select')).toBe('text')
    expect(source.style.getPropertyPriority('user-select')).toBe('important')
})
