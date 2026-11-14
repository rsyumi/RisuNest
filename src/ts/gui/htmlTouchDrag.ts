import { touchHoldMs } from './touchHold'

const MOVE_PX = 8
const HELD = 'risu-sort-held'
const SOURCE = 'risu-drag-source'
const CARD = 'risu-drag-card'
const TOUCH_STYLES = ['-webkit-touch-callout', '-webkit-user-select', 'user-select']
const CARD_TEXT_STYLES = ['color', 'font-family', 'font-size', 'font-weight', 'line-height']

export interface HtmlTouchDragOptions {
    /** Elements a finger can pick up, matched from the touched element upward inside the node. Default '[draggable="true"]'. */
    source?: string
    /** Ancestor of the source, matched upward inside the node, that is shown as dragged: it carries the held and source classes and the card is cloned from it. Default: the source itself. */
    item?: string
    /** Element inside the dragged item that the following card is cloned from. Default: the item itself. */
    ghost?: string
    /**
     * Elements the drag events go to, matched from the element under the finger upward inside the node.
     * Default: the element under the finger wherever it is, as with native drag events.
     */
    target?: string
    /** Attribute that names the item a source holds. A source without it is not picked up, and a drag stops when it changes. */
    identity?: string
}

/**
 * Touch input for lists that already handle HTML drag and drop: a held finger raises the same
 * drag events a mouse drag does, while a finger that moves before the hold keeps scrolling.
 */
export function htmlTouchDrag(node: HTMLElement, options: HtmlTouchDragOptions = {}): { destroy(): void } {
    const sourceSelector = options.source ?? '[draggable="true"]'
    type Gesture = {
        source: HTMLElement
        item: HTMLElement
        origin: Element
        identity: string | null
        identifier: number
        started: number
        startX: number
        startY: number
        x: number
        y: number
        scrolling: boolean
        // Whether the adapter added the held or the source class, so that it removes only its own.
        held: boolean
        lifted: boolean
        styles: { name: string, value: string, priority: string }[]
        data: DataTransfer | null
        card: HTMLElement | null
        offsetX: number
        offsetY: number
        scroller: HTMLElement
        target: Element | null
        accepted: boolean
    }
    let gesture: Gesture | null = null
    let frame = 0
    let holdTimer: ReturnType<typeof setTimeout> | undefined
    let suppressClickUntil = 0
    let previousFrame = 0

    function valid(g: Gesture) {
        return node.isConnected && node.getClientRects().length > 0 && node.contains(g.source)
            && g.source.matches(sourceSelector)
            && (!options.identity || g.source.getAttribute(options.identity) === g.identity)
    }

    function dispatch(g: Gesture, type: string, target: Element, relatedTarget: Element | null = null) {
        return !target.dispatchEvent(new DragEvent(type, {
            bubbles: true, cancelable: true, dataTransfer: g.data,
            clientX: g.x, clientY: g.y, relatedTarget,
        }))
    }

    function hover(g: Gesture) {
        const hit = document.elementFromPoint(g.x, g.y)
        let next = options.target ? hit?.closest(options.target) ?? null : hit
        if (options.target && next && !node.contains(next)) next = null
        if (g.target !== next) {
            const previous = g.target
            g.target = next
            // Native order: the new element hears dragenter before the previous one hears dragleave.
            if (next) dispatch(g, 'dragenter', next, previous)
            if (previous) dispatch(g, 'dragleave', previous, next)
        }
        g.accepted = next !== null && dispatch(g, 'dragover', next)
    }

    function finish(drop = false) {
        const g = gesture
        if (!g) return
        gesture = null
        clearTimeout(holdTimer)
        cancelAnimationFrame(frame)
        frame = 0
        try {
            if (g.data) {
                let dropped = false
                if (drop && valid(g)) {
                    hover(g)
                    if (g.target && g.accepted) {
                        dispatch(g, 'drop', g.target)
                        dropped = true
                    }
                }
                if (!dropped) {
                    if (g.target) dispatch(g, 'dragleave', g.target)
                    g.data.dropEffect = 'none'
                }
                dispatch(g, 'dragend', g.source)
            }
        } finally {
            g.card?.remove()
            if (g.held) g.item.classList.remove(HELD)
            if (g.lifted) g.item.classList.remove(SOURCE)
            for (const { name, value, priority } of g.styles) g.source.style.setProperty(name, value, priority)
            if (g.data || g.scrolling || performance.now() - g.started >= touchHoldMs) {
                suppressClickUntil = performance.now() + 700
            }
        }
    }

    function scrollerOf(source: HTMLElement) {
        for (let element = source.parentElement; element; element = element.parentElement) {
            const { overflowY } = getComputedStyle(element)
            if (/^(auto|scroll|overlay)$/.test(overflowY) && element.scrollHeight > element.clientHeight) return element
        }
        return node
    }

    function scrollEdge(g: Gesture, elapsed: number) {
        // Only the container under the finger scrolls, not one covered by a dialog.
        const hit = document.elementFromPoint(g.x, g.y)
        if (!hit || !g.scroller.contains(hit)) return
        const bounds = g.scroller.getBoundingClientRect()
        if (g.x < bounds.left || g.x > bounds.right || g.y < bounds.top || g.y > bounds.bottom) return
        const edge = Math.min(36, bounds.height / 4)
        const direction = g.y < bounds.top + edge ? -1 : g.y > bounds.bottom - edge ? 1 : 0
        g.scroller.scrollTop += direction * elapsed * 0.45
    }

    function animate(now: number) {
        const g = gesture
        if (!g?.data) return
        if (!valid(g)) { finish(); return }
        const elapsed = Math.min(32, now - previousFrame)
        previousFrame = now
        scrollEdge(g, elapsed)
        hover(g)
        frame = requestAnimationFrame(animate)
    }

    function hold() {
        const g = gesture
        if (!g || g.scrolling || g.data) return
        if (!valid(g)) { finish(); return }
        g.held = !g.item.classList.contains(HELD)
        g.item.classList.add(HELD)
    }

    function placeCard(g: Gesture) {
        g.card!.style.transform = `translate(${g.x - g.offsetX}px, ${g.y - g.offsetY}px)`
    }

    function startDrag(g: Gesture) {
        if (!valid(g)) { finish(); return false }
        g.data = new DataTransfer()
        g.data.effectAllowed = 'move'
        if (dispatch(g, 'dragstart', g.source)) {
            // A cancelled dragstart is not a drag, so nothing hears dragend.
            g.data = null
            finish()
            return false
        }
        if (!valid(g)) { finish(); return false }
        const from = (options.ghost ? g.item.querySelector<HTMLElement>(options.ghost) : null) ?? g.item
        const bounds = from.getBoundingClientRect()
        const text = getComputedStyle(from)
        g.offsetX = g.startX - bounds.left
        g.offsetY = g.startY - bounds.top
        g.card = from.cloneNode(true) as HTMLElement
        // From here the row marks the place its card was lifted from.
        if (g.held) g.item.classList.remove(HELD)
        g.held = false
        g.lifted = !g.item.classList.contains(SOURCE)
        g.item.classList.add(SOURCE)
        for (const element of [g.card, ...g.card.querySelectorAll('[id]')]) element.removeAttribute('id')
        g.card.classList.remove(HELD, SOURCE)
        g.card.classList.add(CARD)
        g.card.setAttribute('aria-hidden', 'true')
        // The card leaves the list's subtree, so it keeps the text style it had there.
        for (const name of CARD_TEXT_STYLES) g.card.style.setProperty(name, text.getPropertyValue(name))
        Object.assign(g.card.style, {
            position: 'fixed', top: '0', left: '0', width: `${bounds.width}px`, height: `${bounds.height}px`,
            margin: '0', boxSizing: 'border-box', pointerEvents: 'none', zIndex: '2147483647',
        })
        for (const child of g.card.querySelectorAll<HTMLElement>('*')) child.style.pointerEvents = 'none'
        placeCard(g)
        document.body.append(g.card)
        g.scroller = scrollerOf(g.source)
        previousFrame = performance.now()
        frame = requestAnimationFrame(animate)
        return true
    }

    function start(event: TouchEvent) {
        const origin = event.target instanceof Element ? event.target : null
        const source = origin?.closest<HTMLElement>(sourceSelector)
        if (!source || !node.contains(source)) return
        // Keep the legacy document-level mobile polyfill out of this gesture.
        event.stopPropagation()
        finish()
        const identity = options.identity ? source.getAttribute(options.identity) : null
        if (event.touches.length !== 1 || (options.identity && !identity)) return
        const touch = event.touches[0]
        suppressClickUntil = 0
        // Native image callouts and text selection can take over a stationary hold on iOS.
        const styles = TOUCH_STYLES.map(name => ({
            name, value: source.style.getPropertyValue(name), priority: source.style.getPropertyPriority(name),
        }))
        for (const name of TOUCH_STYLES) source.style.setProperty(name, 'none')
        const item = options.item ? source.closest<HTMLElement>(options.item) : null
        gesture = {
            source, item: item && node.contains(item) ? item : source, origin, identity, identifier: touch.identifier,
            started: performance.now(), startX: touch.clientX, startY: touch.clientY,
            x: touch.clientX, y: touch.clientY, scrolling: false, held: false, lifted: false, styles,
            data: null, card: null, offsetX: 0, offsetY: 0, scroller: node, target: null, accepted: false,
        }
        holdTimer = setTimeout(hold, touchHoldMs)
    }

    function multitouch(event: TouchEvent) {
        if (gesture && event.touches.length !== 1) finish()
    }

    function move(event: TouchEvent) {
        const g = gesture
        if (!g) return
        const touch = Array.from(event.touches).find(t => t.identifier === g.identifier)
        if (event.touches.length !== 1 || !touch || !valid(g)) { finish(); return }
        g.x = touch.clientX
        g.y = touch.clientY
        if (g.scrolling) return
        const moved = Math.hypot(g.x - g.startX, g.y - g.startY) > MOVE_PX
        if (!g.data) {
            if (performance.now() - g.started < touchHoldMs) {
                if (moved) g.scrolling = true
                return
            }
            if (!event.cancelable) { finish(); return }
            // WebKit can commit scrolling before the movement reaches the drag threshold.
            event.preventDefault()
            if (!moved) return
            if (!startDrag(g)) return
        }
        event.preventDefault()
        placeCard(g)
        hover(g)
    }

    function end(event: TouchEvent) {
        const g = gesture
        const touch = g && Array.from(event.changedTouches).find(t => t.identifier === g.identifier)
        if (!g || !touch) return
        g.x = touch.clientX
        g.y = touch.clientY
        const held = !g.scrolling && performance.now() - g.started >= touchHoldMs
        const contextTarget = held && !g.data && valid(g) ? g.origin : null
        if (g.data || held) event.preventDefault()
        finish(true)
        // A stationary hold opens the menu a right click or a native long press would.
        contextTarget?.dispatchEvent(new MouseEvent('contextmenu', {
            bubbles: true, cancelable: true, clientX: g.x, clientY: g.y,
        }))
    }

    function cancel() { finish() }
    function visibility() { if (document.hidden) finish() }
    function nativeDrag(event: DragEvent) {
        if (gesture && event.isTrusted) { event.preventDefault(); event.stopPropagation() }
    }
    function contextmenu(event: MouseEvent) {
        if (gesture) { event.preventDefault(); event.stopPropagation() }
    }
    function click(event: MouseEvent) {
        if (event.detail !== 0 && performance.now() < suppressClickUntil) {
            event.preventDefault()
            event.stopPropagation()
        }
    }

    node.addEventListener('touchstart', start, { passive: true })
    node.addEventListener('dragstart', nativeDrag, true)
    node.addEventListener('contextmenu', contextmenu, true)
    node.addEventListener('click', click, true)
    document.addEventListener('touchstart', multitouch, { passive: true, capture: true })
    document.addEventListener('touchmove', move, { passive: false })
    document.addEventListener('touchend', end, { passive: false })
    document.addEventListener('touchcancel', cancel)
    document.addEventListener('visibilitychange', visibility)
    window.addEventListener('blur', cancel)
    return {
        destroy() {
            finish()
            node.removeEventListener('touchstart', start)
            node.removeEventListener('dragstart', nativeDrag, true)
            node.removeEventListener('contextmenu', contextmenu, true)
            node.removeEventListener('click', click, true)
            document.removeEventListener('touchstart', multitouch, true)
            document.removeEventListener('touchmove', move)
            document.removeEventListener('touchend', end)
            document.removeEventListener('touchcancel', cancel)
            document.removeEventListener('visibilitychange', visibility)
            window.removeEventListener('blur', cancel)
        },
    }
}
