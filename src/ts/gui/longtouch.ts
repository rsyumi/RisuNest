import { touchHoldMs } from './touchHold'

const MOVE_PX = 8
// The click produced by a held press arrives right after its release.
const CLICK_GUARD_MS = 700

export type LongPressOptions = {
	onLongPress: (e: MouseEvent) => void
	/** A touch hold on a text field selects text, so it runs the action only when this is set. */
	touch?: boolean
}

export function isTextField(node: HTMLElement) {
	return node instanceof HTMLTextAreaElement || node instanceof HTMLInputElement || node.isContentEditable
}

/** Keeps the native callout and text selection from taking over a held element; returns the restore. */
export function suppressTouchCallout(node: HTMLElement) {
	const styles = ['-webkit-touch-callout', '-webkit-user-select', 'user-select'].map(name => ({
		name, value: node.style.getPropertyValue(name), priority: node.style.getPropertyPriority(name),
	}))
	for (const { name } of styles) node.style.setProperty(name, 'none')
	return () => {
		for (const { name, value, priority } of styles) node.style.setProperty(name, value, priority)
	}
}

function options(param: ((e: MouseEvent) => void) | LongPressOptions): LongPressOptions {
	return typeof param === 'function' ? { onLongPress: param } : param
}

export function longpress(node:HTMLElement, param:((e:MouseEvent)=>void) | LongPressOptions) {
	let current = options(param)
	const textField = isTextField(node)
	const restoreStyles = textField ? null : suppressTouchCallout(node)
	let press: { event: PointerEvent, touch: boolean, timer: number, fired: boolean } | null = null
	let guardUntil = 0
	let releaseTouchGuard: (() => void) | null = null

	// A held touch must not end as a tap on whatever the action put under the finger, and while the finger stays down
	// the platform's own long press must not select text or open a menu there.
	// Touch events stay on the pressed element, so the guard also works once the action has removed it.
	function guardHeldTouch(held: PointerEvent) {
		releaseTouchGuard?.()
		const target = held.target
		if (!target) return
		const stop = (e: Event) => {
			if (e.cancelable) e.preventDefault()
			release()
		}
		const block = (e: Event) => {
			e.preventDefault()
			if (e.type === 'contextmenu') e.stopPropagation()
		}
		const lifted = (e: PointerEvent) => {
			// A new press means the release never reached the page.
			if (e.type === 'pointerdown' ? e.isPrimary : e.pointerId === held.pointerId) unblock()
		}
		const unblock = () => {
			document.removeEventListener('selectstart', block, true)
			document.removeEventListener('contextmenu', block, true)
			for (const type of ['pointerup', 'pointercancel', 'pointerdown']) window.removeEventListener(type, lifted as EventListener, true)
		}
		const release = () => {
			unblock()
			target.removeEventListener('touchend', stop)
			if (releaseTouchGuard === release) releaseTouchGuard = null
		}
		target.addEventListener('touchend', stop, { passive: false })
		document.addEventListener('selectstart', block, true)
		document.addEventListener('contextmenu', block, true)
		for (const type of ['pointerup', 'pointercancel', 'pointerdown']) window.addEventListener(type, lifted as EventListener, true)
		releaseTouchGuard = release
	}

	function end() {
		if (!press) return
		window.clearTimeout(press.timer)
		press = null
		window.removeEventListener('pointermove', handleMove)
		window.removeEventListener('pointerup', handleRelease)
		window.removeEventListener('pointercancel', handleRelease)
		window.removeEventListener('blur', end)
	}
	function fire() {
		if (!press || press.fired) return
		press.fired = true
		window.clearTimeout(press.timer)
		if (press.touch) guardHeldTouch(press.event)
		current.onLongPress(press.event)
	}
	function handlePointerDown(e:PointerEvent) {
		end()
		releaseTouchGuard?.()
		guardUntil = 0
		if (!e.isPrimary || e.button !== 0) return
		const touch = e.pointerType !== 'mouse'
		if (textField && touch && !current.touch) return
		press = { event: e, touch, timer: window.setTimeout(fire, touchHoldMs), fired: false }
		window.addEventListener('pointermove', handleMove)
		window.addEventListener('pointerup', handleRelease)
		window.addEventListener('pointercancel', handleRelease)
		window.addEventListener('blur', end)
	}
	function handleMove(e:PointerEvent) {
		if (!press || press.fired || e.pointerId !== press.event.pointerId) return
		// A mouse that moves in a text field is selecting text.
		const slop = textField && !press.touch ? 0 : MOVE_PX
		if (Math.hypot(e.clientX - press.event.clientX, e.clientY - press.event.clientY) > slop) end()
	}
	function handleRelease(e:PointerEvent) {
		if (!press || e.pointerId !== press.event.pointerId) return
		if (press.fired) guardUntil = performance.now() + CLICK_GUARD_MS
		end()
	}
	function handleContextMenu(e:MouseEvent) {
		// Android raises its own long press, which can come before the timer.
		if (press?.touch) fire()
		if (press?.fired || performance.now() < guardUntil) {
			e.preventDefault()
			e.stopPropagation()
		}
	}
	function handleClick(e:MouseEvent) {
		if (e.detail === 0 || performance.now() >= guardUntil) return
		guardUntil = 0
		e.preventDefault()
		e.stopPropagation()
	}
	node.addEventListener('pointerdown', handlePointerDown);
	node.addEventListener('contextmenu', handleContextMenu, true);
	node.addEventListener('click', handleClick, true);
	return {
		update: (next:((e:MouseEvent)=>void) | LongPressOptions) => {
			current = options(next)
		},
		destroy: () => {
			// A touch guard already armed outlives the element until its touch ends.
			end()
			restoreStyles?.()
			node.removeEventListener('pointerdown', handlePointerDown);
			node.removeEventListener('contextmenu', handleContextMenu, true);
			node.removeEventListener('click', handleClick, true);
		}
	};
}
