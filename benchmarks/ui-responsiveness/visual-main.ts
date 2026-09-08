import { mount, tick } from 'svelte'
import '../../src/styles.css'
import { DBState, selectedCharID } from '../../src/ts/stores.svelte'
import { navigationActivity } from '../../src/ts/ui/navigationActivity'

declare global {
    interface Window {
        __risuUiVisualReady?: boolean
        __risuUiVisualUnderlyingActivations: number
        __risuUiVisualVerify?: () => Record<string, unknown>
    }
}

Object.assign(DBState.db, {
    theme: 'classic',
    customBackground: '',
    textScreenColor: '',
    textBorder: false,
    textScreenRounded: false,
    textScreenBorder: '',
    classicMaxWidth: false,
    characters: [],
})
selectedCharID.set(-1)
navigationActivity.set({ token: 1, kind: 'conversation' })
window.__risuUiVisualUnderlyingActivations = 0

const { default: VisualHarness } = await import('./VisualHarness.svelte')
mount(VisualHarness, { target: document.getElementById('app')! })
await tick()
await new Promise<void>((resolve) =>
    requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
)

function roundedRect(element: Element) {
    const rect = element.getBoundingClientRect()
    return {
        x: Math.round(rect.x * 10) / 10,
        y: Math.round(rect.y * 10) / 10,
        width: Math.round(rect.width * 10) / 10,
        height: Math.round(rect.height * 10) / 10,
    }
}

window.__risuUiVisualVerify = () => {
    const pane = document.querySelector<HTMLElement>('[data-synthetic-pane]')!
    const chatScreen = pane.firstElementChild as HTMLElement
    const inertContent = chatScreen.firstElementChild as HTMLElement
    const status = chatScreen.querySelector<HTMLElement>('[role="status"]')!
    const overlay = status.closest<HTMLElement>('.absolute.inset-0')!
    const ring = status.querySelector<HTMLElement>('.loading-ring')!
    const paneRect = pane.getBoundingClientRect()
    const overlayRect = overlay.getBoundingClientRect()
    const statusRect = status.getBoundingClientRect()
    const centerError = Math.hypot(
        statusRect.x + statusRect.width / 2 - (paneRect.x + paneRect.width / 2),
        statusRect.y +
            statusRect.height / 2 -
            (paneRect.y + paneRect.height / 2),
    )
    const overlayEdgeError = Math.max(
        Math.abs(overlayRect.left - paneRect.left - 2),
        Math.abs(overlayRect.top - paneRect.top - 2),
        Math.abs(overlayRect.right - paneRect.right + 2),
        Math.abs(overlayRect.bottom - paneRect.bottom + 2),
    )
    return {
        validOutput:
            chatScreen.getAttribute('aria-busy') === 'true' &&
            inertContent.hasAttribute('inert') &&
            overlay.contains(status) &&
            centerError <= 1 &&
            overlayEdgeError <= 1 &&
            ring.getBoundingClientRect().width >= 20 &&
            window.__risuUiVisualUnderlyingActivations === 0,
        pane: roundedRect(pane),
        overlay: roundedRect(overlay),
        status: roundedRect(status),
        centerErrorPx: Math.round(centerError * 10) / 10,
        overlayEdgeErrorPx: Math.round(overlayEdgeError * 10) / 10,
        contentInert: inertContent.hasAttribute('inert'),
        ariaBusy: chatScreen.getAttribute('aria-busy'),
        underlyingActivations: window.__risuUiVisualUnderlyingActivations,
    }
}

window.__risuUiVisualReady = true
