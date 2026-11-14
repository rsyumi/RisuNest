import { touchHoldMs } from './gui/touchHold'

type ChosenEvent = Event & { originalEvent?: Event, item?: HTMLElement }
type DraggingSortable = { nativeDraggable?: boolean, _onDrop?: (event: Event) => void }

/** Whether the row was picked up by a finger, which Sortable always drags without the browser's native dragging. */
export function chosenByTouch(event: ChosenEvent): boolean {
    const original = event.originalEvent as PointerEvent | undefined
    return !!original && (original.type.startsWith('touch') || original.pointerType === 'touch')
}

/**
 * Watches a drag of one of the rows of `list` until `stop` is called, to tell a drag that was abandoned from a drop.
 * The browser ends its own dragging the same way for Escape and for a release that nothing takes, and the app
 * refuses its own drags where no list takes them. So a release over the list is taken here, and only the end of
 * a drag that was last over the list counts as Escape.
 */
export function watchSortDrag(list: Element) {
    let overList = false
    const track = (event: DragEvent) => {
        overList = event.target instanceof Node && list.contains(event.target)
    }
    const takeRelease = (event: DragEvent) => {
        if (!overList) return
        event.preventDefault()
        if (event.dataTransfer) event.dataTransfer.dropEffect = 'move'
    }
    document.addEventListener('dragover', track, true)
    window.addEventListener('dragover', takeRelease)
    return {
        /** Whether the drag that ended was abandoned: a cancelled touch, or Escape over the list. */
        cancelled(event: Event & { originalEvent?: Event }): boolean {
            const type = event.originalEvent?.type
            return type === 'pointercancel' || type === 'touchcancel' || (type === 'dragend' && overList)
        },
        stop() {
            document.removeEventListener('dragover', track, true)
            window.removeEventListener('dragover', takeRelease)
        },
    }
}

const heldClass = 'risu-sort-held'
/** Lists whose native dragging is switched off while a finger holds a row, with what ends that drag on a cancelled touch. */
const nativeDragging = new WeakMap<object, (cancelled: Event) => void>()

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
        sortable.nativeDraggable = false
        // Sortable ends a drag on a cancelled touch only where it never drags natively;
        // elsewhere the row would stay held until a later touch dropped it.
        const endDrag = (cancelled: Event) => sortable._onDrop?.(cancelled)
        nativeDragging.set(sortable, endDrag)
        document.addEventListener('pointercancel', endDrag)
    },
    onUnchoose(this: object, event: ChosenEvent) {
        event.item?.classList.remove(heldClass)
        const sortable = this as DraggingSortable
        const endDrag = nativeDragging.get(sortable)
        if (!endDrag) return
        nativeDragging.delete(sortable)
        document.removeEventListener('pointercancel', endDrag)
        sortable.nativeDraggable = true
    },
} as const
