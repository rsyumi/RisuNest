type ChosenEvent = Event & { originalEvent?: Event }
type DraggingSortable = { nativeDraggable?: boolean }

/** Whether the row was picked up by a finger, which Sortable always drags without the browser's native dragging. */
export function chosenByTouch(event: ChosenEvent): boolean {
    const original = event.originalEvent as PointerEvent | undefined
    return !!original && (original.type.startsWith('touch') || original.pointerType === 'touch')
}

const nativeDragging = new WeakSet<object>()

export const sortableOptions = {
    delay: 300, // time in milliseconds to define when the sorting should start
    delayOnTouchOnly: true,
    filter: '.no-sort',
    onMove: (event) => {
        return event.related.className.indexOf('no-sort') === -1
    },
    // Sortable scrolls a list under a resting touch drag only where it finds no native dragging.
    // A touch drag never uses native dragging, so the list reports none while its row is held.
    onChoose(this: object, event: ChosenEvent) {
        const sortable = this as DraggingSortable
        if (!chosenByTouch(event) || !sortable.nativeDraggable) return
        nativeDragging.add(sortable)
        sortable.nativeDraggable = false
    },
    onUnchoose(this: object) {
        if (nativeDragging.delete(this)) (this as DraggingSortable).nativeDraggable = true
    },
} as const
