import { tick } from 'svelte'

export interface AppLockSurface {
    element: HTMLElement
    keydown: (event: KeyboardEvent) => void
}

let blocked = false
let framesHidden = false
let gate: AppLockSurface | null = null
let exit: AppLockSurface | null = null
let previousFocus: HTMLElement | null = null
let focusGeneration = 0
let interactionRevision = 0
const blockedFrames = new Map<HTMLIFrameElement, { inert: string | null; covered: string | null }>()
let frameObserver: MutationObserver | null = null

const activeSurface = () => exit?.element.isConnected ? exit : gate?.element.isConnected ? gate : null

function restoreFrameFocus(): void {
    const frame = document.activeElement
    if (!framesHidden || !(frame instanceof HTMLIFrameElement) || !blockedFrames.has(frame)) return
    const surface = activeSurface()
    if (surface) surface.element.focus({ preventScroll: true })
    else frame.blur()
}

function isolateBodyFrames(): void {
    for (const child of document.body.children) {
        if (!(child instanceof HTMLIFrameElement)) continue
        if (!blockedFrames.has(child)) blockedFrames.set(child, {
            inert: child.getAttribute('inert'), covered: child.getAttribute('data-app-lock-covered'),
        })
        child.setAttribute('inert', '')
        // Inert on an iframe does not stop its document from focusing its own controls.
        child.setAttribute('data-app-lock-covered', '')
    }
    restoreFrameFocus()
}

function restoreBodyFrames(): void {
    frameObserver?.disconnect()
    frameObserver = null
    for (const [frame, previous] of blockedFrames) {
        if (previous.inert === null) frame.removeAttribute('inert')
        else frame.setAttribute('inert', previous.inert)
        if (previous.covered === null) frame.removeAttribute('data-app-lock-covered')
        else frame.setAttribute('data-app-lock-covered', previous.covered)
    }
    blockedFrames.clear()
}

export const isAppLockInputBlocked = (): boolean => blocked
const unblockedWaiters: (() => void)[] = []
/** Resolves once the app lock no longer covers the app. */
export function waitForAppLockInputUnblocked(): Promise<void> {
    if (!blocked) return Promise.resolve()
    return new Promise(resolve => unblockedWaiters.push(resolve))
}
export function setAppLockInputBlocked(value: boolean, concealFrames = value): void {
    const changed = blocked !== value
    if (changed && value) previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null
    blocked = value
    if (!value) for (const resolve of unblockedWaiters.splice(0)) resolve()
    if (changed) focusGeneration++
    if (framesHidden !== concealFrames) {
        framesHidden = concealFrames
        if (concealFrames) {
            // Fullscreen plugins live beside the application root and keep running while covered.
            isolateBodyFrames()
            frameObserver = new MutationObserver(isolateBodyFrames)
            frameObserver.observe(document.body, { childList: true })
        } else restoreBodyFrames()
    }
    if (!changed || value) return
    const generation = focusGeneration
    const target = previousFocus
    const oldGate = gate?.element
    const oldExit = exit?.element
    previousFocus = null
    void tick().then(() => {
        if (generation !== focusGeneration || blocked || !target?.isConnected || target.closest('[inert]')) return
        const current = document.activeElement
        if (current === document.body || current === document.documentElement
            || oldGate?.contains(current) || oldExit?.contains(current)) target.focus({ preventScroll: true })
    })
}
export function registerAppLockSurface(surface: AppLockSurface | null): void { gate = surface }
export function registerAppLockExitSurface(surface: AppLockSurface | null): void {
    const previous = exit
    exit = surface
    if (!surface && previous) void tick().then(() => {
        if (!blocked || exit || !gate?.element.isConnected) return
        const current = document.activeElement
        if (current === document.body || current === document.documentElement || previous.element.contains(current)) {
            gate.element.focus({ preventScroll: true })
        }
    })
}

export function captureAppLockPresentationFocus(): () => void {
    const surface = gate
    const generation = focusGeneration
    const interaction = interactionRevision
    return () => {
        if (!blocked || gate !== surface || !surface?.element.isConnected || exit?.element.isConnected
            || generation !== focusGeneration || interaction !== interactionRevision) return
        const current = document.activeElement
        if (current === document.body || current === document.documentElement || surface.element.contains(current)) {
            surface.element.focus({ preventScroll: true })
        }
    }
}

export function handleAppLockSurfaceNavigation(event: KeyboardEvent, root: HTMLElement): void {
    if (event.key === 'Tab') {
        const buttons = [...root.querySelectorAll<HTMLElement>('button:not(:disabled), a[href], summary, [tabindex="0"]')]
            .filter(item => {
                const closedDetails = item.closest('details:not([open])')
                return !item.closest('[inert], [hidden]') && item.getClientRects().length > 0
                    && (!closedDetails || closedDetails.querySelector(':scope > summary')?.contains(item))
            })
        event.preventDefault()
        if (!buttons.length) { root.focus(); return }
        const index = buttons.indexOf(document.activeElement as HTMLElement)
        const next = index < 0 ? (event.shiftKey ? buttons.length - 1 : 0)
            : (index + (event.shiftKey ? -1 : 1) + buttons.length) % buttons.length
        buttons[next].focus()
    } else if ((event.key === 'Enter' || event.key === ' ') && !event.repeat) {
        const focused = document.activeElement
        if (focused instanceof HTMLElement && root.contains(focused)
            && ((focused instanceof HTMLButtonElement && !focused.disabled) || focused.matches('summary'))) {
            event.preventDefault()
            focused.click()
        }
    }
}

export function installAppLockInput(): () => void {
    const keydown = (event: KeyboardEvent) => {
        if (!blocked) return
        interactionRevision++
        const surface = activeSurface()
        if (surface) surface.keydown(event)
        else event.preventDefault()
        if (event.key === 'Escape') event.preventDefault()
        event.stopImmediatePropagation()
    }
    const keyup = (event: KeyboardEvent) => { if (blocked) event.stopImmediatePropagation() }
    const pointerdown = () => { if (blocked) interactionRevision++ }
    const blur = () => { if (framesHidden) queueMicrotask(restoreFrameFocus) }
    const focusin = (event: FocusEvent) => {
        if (!blocked) return
        const surface = activeSurface()
        if (surface && !surface.element.contains(event.target as Node)) {
            event.stopImmediatePropagation()
            surface.element.focus()
        }
    }
    window.addEventListener('keydown', keydown, true)
    window.addEventListener('keyup', keyup, true)
    window.addEventListener('pointerdown', pointerdown, true)
    window.addEventListener('focusin', focusin, true)
    window.addEventListener('blur', blur, true)
    return () => {
        window.removeEventListener('keydown', keydown, true)
        window.removeEventListener('keyup', keyup, true)
        window.removeEventListener('pointerdown', pointerdown, true)
        window.removeEventListener('focusin', focusin, true)
        window.removeEventListener('blur', blur, true)
    }
}
