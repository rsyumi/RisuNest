import { afterEach, beforeEach, expect, it, vi } from 'vitest'
import { sidebarTouchDrag } from './sidebarTouchDrag'
import { stubDragEvent, touch } from './htmlTouchDrag.testSupport'
import { RISU_SIDEBAR_DRAG_TYPE } from '../dragTypes'

let node: HTMLElement
let source: HTMLElement
let avatar: HTMLElement
let target: HTMLElement
let action: ReturnType<typeof sidebarTouchDrag>
let dropped: ReturnType<typeof vi.fn<(event: DragEvent) => void>>
let ended: ReturnType<typeof vi.fn<(event: DragEvent) => void>>
let hit: Element | null

const finger = (type: string, x = 30, y = 60) => touch(avatar, type, x, y)
function accept(element: HTMLElement) {
    element.addEventListener('dragover', event => {
        if (event.dataTransfer!.types.includes(RISU_SIDEBAR_DRAG_TYPE)) event.preventDefault()
    })
}

beforeEach(() => {
    vi.useFakeTimers()
    stubDragEvent()
    node = document.createElement('div')
    node.innerHTML = '<div draggable="true" data-sidebar-drag-id="one" data-sidebar-drop><span class="avatar">One</span><span>Name</span></div><div data-sidebar-drop><span>Gap</span></div>'
    document.body.append(node)
    source = node.children[0] as HTMLElement
    avatar = source.children[0] as HTMLElement
    target = node.children[1] as HTMLElement
    hit = target.children[0]
    vi.spyOn(node, 'getClientRects').mockReturnValue([{}] as unknown as DOMRectList)
    vi.spyOn(node, 'getBoundingClientRect').mockReturnValue({ top: 0, bottom: 300, left: 0, right: 80, height: 300 } as DOMRect)
    vi.spyOn(document, 'elementFromPoint').mockImplementation(() => hit)
    dropped = vi.fn()
    ended = vi.fn()
    source.addEventListener('dragstart', event => event.dataTransfer!.setData(RISU_SIDEBAR_DRAG_TYPE, 'true'))
    source.addEventListener('dragend', ended)
    accept(target)
    target.addEventListener('drop', dropped)
    action = sidebarTouchDrag(node)
})

afterEach(() => {
    action.destroy()
    document.body.replaceChildren()
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
    vi.useRealTimers()
})

it('lifts the avatar and drops on the rail target around the finger', () => {
    finger('touchstart')
    vi.advanceTimersByTime(450)
    finger('touchmove', 30, 160)
    expect(document.querySelector('.risu-drag-card')?.textContent).toBe('One')
    finger('touchend', 30, 170)
    expect(dropped).toHaveBeenCalledOnce()
    expect(dropped.mock.calls[0][0].target).toBe(target)
    expect(ended).toHaveBeenCalledOnce()
})

it('does not drop outside the rail and scrolls the rail near its edge during a drag', () => {
    const outside = document.createElement('div')
    outside.dataset.sidebarDrop = ''
    accept(outside)
    outside.addEventListener('drop', dropped)
    document.body.append(outside)
    finger('touchstart')
    vi.advanceTimersByTime(450)
    finger('touchmove', 30, 290)
    vi.advanceTimersByTime(100)
    expect(node.scrollTop).toBeGreaterThan(0)
    hit = outside
    finger('touchend', 150, 290)
    expect(dropped).not.toHaveBeenCalled()
    expect(ended).toHaveBeenCalledOnce()
})

it('cancels a drag without dropping when its row is replaced', () => {
    finger('touchstart')
    vi.advanceTimersByTime(450)
    finger('touchmove', 30, 160)
    source.dataset.sidebarDragId = 'different'
    vi.advanceTimersByTime(20)
    finger('touchend', 30, 170)
    expect(dropped).not.toHaveBeenCalled()
    expect(ended).toHaveBeenCalledOnce()
    expect(document.querySelector('.risu-drag-card')).toBeNull()
})
