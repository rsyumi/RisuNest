import { touchHoldMs } from './gui/touchHold'

type ChosenEvent = Event & { originalEvent?: Event, item?: HTMLElement }
type DraggingSortable = { nativeDraggable?: boolean, _onDrop?: (event: Event) => void }
type ClosingSortable = DraggingSortable & { destroy(): void }

/** Whether the row was picked up by a finger, which Sortable always drags without the browser's native dragging. */
export function chosenByTouch(event: ChosenEvent): boolean {
    const original = event.originalEvent as PointerEvent | undefined
    return !!original && (original.type.startsWith('touch') || original.pointerType === 'touch')
}

/**
 * Watches a drag of a list's row until `stop` is called, to tell a drag that was abandoned from a drop.
 * The browser ends its own dragging the same way for Escape and for a release that nothing takes, and the app
 * refuses its own drags where no list takes them. So the release is taken here wherever on the page it happens,
 * and a drag that ends without one was abandoned.
 */
export function watchSortDrag() {
    let released = false
    const takeRelease = (event: DragEvent) => {
        event.preventDefault()
        if (event.dataTransfer) event.dataTransfer.dropEffect = 'move'
    }
    const onRelease = (event: DragEvent) => {
        released = true
        // Sortable has the row where it goes and ends the drag when the browser does. The release is taken as
        // Sortable takes one it handles, and nothing on the page is handed the row as a drop.
        event.preventDefault()
        event.stopPropagation()
    }
    window.addEventListener('dragover', takeRelease)
    window.addEventListener('drop', onRelease, true)
    return {
        /** Whether the drag that ended was abandoned: a cancelled touch, or a mouse drag that ended without a release on the page, as Escape ends it. */
        cancelled(event: Event & { originalEvent?: Event }): boolean {
            const type = event.originalEvent?.type
            return type === 'pointercancel' || type === 'touchcancel' || (type === 'dragend' && !released)
        },
        stop() {
            window.removeEventListener('dragover', takeRelease)
            window.removeEventListener('drop', onRelease, true)
        },
    }
}

const heldClass = 'risu-sort-held'
/** Lists whose native dragging is switched off while a finger holds a row. */
const nativeDragging = new WeakSet<object>()
/** The list whose row Sortable has picked up and not yet let go of. */
let dragging: DraggingSortable | null = null
/** Sortables of lists that went away under another list's drag. */
const closedUnderDrag = new Set<ClosingSortable>()

/**
 * Ends the drag in progress as an abandoned one. A list calls this before it destroys a Sortable to build it again:
 * destroying any of them drops Sortable's record of the drag without ending it, which leaves the row held, its copy
 * on the screen and the list waiting for an end that never comes.
 */
export function abandonSortDrag() {
    dragging?._onDrop?.(new Event('pointercancel'))
}

/**
 * Destroys the Sortable of a list that is going away. A drag of its own row is abandoned first. A drag of another
 * list's row carries on, as it does when a paged list turns the page under it, and the Sortable is destroyed once
 * that drag is over.
 */
export function destroySortableOnClose(sortable: ClosingSortable) {
    if (dragging === sortable) abandonSortDrag()
    if (dragging) closedUnderDrag.add(sortable)
    else sortable.destroy()
}

export const sortableOptions = {
    delay: touchHoldMs, // time in milliseconds to define when the sorting should start
    delayOnTouchOnly: true,
    filter: '.no-sort',
    onMove: (event) => {
        return event.related.className.indexOf('no-sort') === -1
    },
    onChoose(this: object, event: ChosenEvent) {
        const sortable = this as DraggingSortable
        dragging = sortable
        if (!chosenByTouch(event)) return
        event.item?.classList.add(heldClass)
        // Sortable scrolls a list under a resting touch drag only where it finds no native dragging.
        // A touch drag never uses native dragging, so the list reports none while its row is held.
        if (!sortable.nativeDraggable) return
        sortable.nativeDraggable = false
        nativeDragging.add(sortable)
        // Sortable ends a drag on a cancelled touch only where it never drags natively; elsewhere the row would
        // stay held until a later touch dropped it. Given its own handler, Sortable removes it however the drag ends.
        if (sortable._onDrop) document.addEventListener('pointercancel', sortable._onDrop)
    },
    onUnchoose(this: object, event: ChosenEvent) {
        const sortable = this as DraggingSortable
        if (dragging === sortable) {
            dragging = null
            // Sortable is still ending the drag here.
            queueMicrotask(() => {
                for (const closed of closedUnderDrag) closed.destroy()
                closedUnderDrag.clear()
            })
        }
        event.item?.classList.remove(heldClass)
        if (nativeDragging.delete(sortable)) sortable.nativeDraggable = true
    },
} as const
