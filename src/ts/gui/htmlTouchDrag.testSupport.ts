import { vi } from 'vitest'

/** happy-dom's DragEvent does not implement DragEventInit yet, and its DataTransfer cannot set a drag image. */
export function stubDragEvent() {
    vi.stubGlobal('DragEvent', class extends MouseEvent {
        dataTransfer: DataTransfer | null
        constructor(type: string, init: DragEventInit = {}) {
            super(type, init)
            this.dataTransfer = init.dataTransfer ?? null
        }
    })
    vi.spyOn(DataTransfer.prototype, 'setDragImage').mockImplementation(() => {})
}

/** Dispatches a one-finger touch event, or `count` fingers, on the element the touch started on. */
export function touch(element: Element, type: string, x: number, y: number, count = 1) {
    const point = { identifier: 7, clientX: x, clientY: y, target: element }
    const event = new Event(type, { bubbles: true, cancelable: true })
    Object.defineProperties(event, {
        touches: { value: type === 'touchend' || type === 'touchcancel' ? [] : Array.from({ length: count }, (_, i) => ({ ...point, identifier: 7 + i })) },
        changedTouches: { value: [point] },
    })
    element.dispatchEvent(event)
    return event
}
