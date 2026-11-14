import { isTextField, suppressTouchCallout } from './longtouch'
import { touchHoldMs } from './touchHold'

const MOVE_PX = 8
// The click produced by a held touch arrives right after its release.
const CLICK_GUARD_MS = 700

/**
 * A stationary touch hold reaches the node's `contextmenu` handlers on release.
 * iOS raises no `contextmenu` for a long press; Android raises its own during the hold, which is left alone.
 */
export function contextHold(node: HTMLElement, enabled = true) {
    type Gesture = { identifier: number; origin: Element; started: number; x: number; y: number; native: MouseEvent | null }
    let gesture: Gesture | null = null
    let guardUntil = 0
    let restoreStyles: (() => void) | null = null

    function setEnabled(next: boolean) {
        enabled = next
        gesture = null
        if (enabled && !restoreStyles && !isTextField(node)) restoreStyles = suppressTouchCallout(node)
        if (!enabled && restoreStyles) {
            restoreStyles()
            restoreStyles = null
        }
    }

    function tracked(event: TouchEvent) {
        const g = gesture
        return g ? Array.from(event.changedTouches).find(t => t.identifier === g.identifier) : undefined
    }

    function start(event: TouchEvent) {
        gesture = null
        if (!enabled || event.touches.length !== 1) return
        const touch = event.changedTouches[0]
        guardUntil = 0
        gesture = {
            identifier: touch.identifier, origin: event.target instanceof Element ? event.target : node,
            started: performance.now(), x: touch.clientX, y: touch.clientY, native: null,
        }
    }

    function move(event: TouchEvent) {
        const g = gesture
        const touch = tracked(event)
        if (!g) return
        if (event.touches.length !== 1 || (touch && Math.hypot(touch.clientX - g.x, touch.clientY - g.y) > MOVE_PX)) gesture = null
    }

    function end(event: TouchEvent) {
        const g = gesture
        const touch = tracked(event)
        if (!g || !touch) return
        gesture = null
        if (performance.now() - g.started < touchHoldMs || Math.hypot(touch.clientX - g.x, touch.clientY - g.y) > MOVE_PX) return
        let handled = g.native?.defaultPrevented ?? false
        if (!g.native && g.origin.isConnected && node.contains(g.origin)) {
            const menu = new MouseEvent('contextmenu', {
                bubbles: true, cancelable: true, clientX: touch.clientX, clientY: touch.clientY,
            })
            g.origin.dispatchEvent(menu)
            handled = menu.defaultPrevented
        }
        // A menu nobody handled leaves the touch to the browser.
        if (!handled) return
        if (event.cancelable) event.preventDefault()
        guardUntil = performance.now() + CLICK_GUARD_MS
    }

    function cancel() { gesture = null }

    // The gesture is over before its own menu is dispatched, so only a platform menu lands here.
    function contextmenu(event: MouseEvent) {
        if (gesture) gesture.native = event
    }

    function click(event: MouseEvent) {
        if (event.detail === 0 || performance.now() >= guardUntil) return
        guardUntil = 0
        event.preventDefault()
        event.stopPropagation()
    }

    setEnabled(enabled)
    node.addEventListener('touchstart', start, { passive: true })
    node.addEventListener('touchmove', move, { passive: true })
    node.addEventListener('touchend', end)
    node.addEventListener('touchcancel', cancel)
    node.addEventListener('contextmenu', contextmenu, true)
    node.addEventListener('click', click, true)
    return {
        update(next = true) {
            if (next !== enabled) setEnabled(next)
        },
        destroy() {
            setEnabled(false)
            node.removeEventListener('touchstart', start)
            node.removeEventListener('touchmove', move)
            node.removeEventListener('touchend', end)
            node.removeEventListener('touchcancel', cancel)
            node.removeEventListener('contextmenu', contextmenu, true)
            node.removeEventListener('click', click, true)
        },
    }
}
