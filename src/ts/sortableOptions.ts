import { touchHoldMs } from './gui/touchHold'

type ChosenEvent = Event & { originalEvent?: Event, item?: HTMLElement }
type DraggingSortable = { nativeDraggable?: boolean }

/** Whether the row was picked up by a finger, which Sortable always drags without the browser's native dragging. */
export function chosenByTouch(event: ChosenEvent): boolean {
    const original = event.originalEvent as PointerEvent | undefined
    return !!original && (original.type.startsWith('touch') || original.pointerType === 'touch')
}

const heldClass = 'risu-sort-held'
const nativeDragging = new WeakSet<object>()

export const sortableOptions = {
    delay: touchHoldMs, // time in milliseconds to define when the sorting should start
    delayOnTouchOnly: true,
    filter: '.no-sort',
    onMove: (event) => {
        return event.related.className.indexOf('no-sort') === -1
    },
    onChoose(this: object, event: ChosenEvent) {
        if (!chosenByTouch(event)) return
        event.item?.classList.add(heldClass)
        // Sortable scrolls a list under a resting touch drag only where it finds no native dragging.
        // A touch drag never uses native dragging, so the list reports none while its row is held.
        const sortable = this as DraggingSortable
        if (!sortable.nativeDraggable) return
        nativeDragging.add(sortable)
        sortable.nativeDraggable = false
    },
    onUnchoose(this: object, event: ChosenEvent) {
        event.item?.classList.remove(heldClass)
        if (nativeDragging.delete(this)) (this as DraggingSortable).nativeDraggable = true
    },
} as const
