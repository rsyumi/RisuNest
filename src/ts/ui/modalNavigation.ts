import { isCompositionKey } from '../hotkeyModifier'

/** Let browser/WebView Back dismiss a local dialog and restore its opener. */
export function modalNavigation(node: HTMLElement, options: { close(): void }) {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const token = crypto.randomUUID()
    const key = 'risunestModal'
    const historyTokens = (): string[] => Array.isArray(history.state?.[key]) ? history.state[key] : []
    history.pushState({ ...history.state, [key]: [...historyTokens(), token] }, '')
    const isTopmost = () => historyTokens().at(-1) === token
    const closeOnBack = () => {
        if (!historyTokens().includes(token)) options.close()
    }
    const keydown = (event: KeyboardEvent) => {
        if (!isTopmost() || isCompositionKey(event)) return
        if (event.key === 'Escape') {
            event.preventDefault()
            event.stopImmediatePropagation()
            options.close()
        }
        if (event.key === 'Tab') {
            const items = [
                ...node.querySelectorAll<HTMLElement>(
                    'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), select:not(:disabled), a[href], [contenteditable]:not([contenteditable="false"]), [tabindex="0"]',
                ),
            ].filter((item) => item.getClientRects().length > 0)
            const first = items[0],
                last = items.at(-1)
            if (
                first &&
                last &&
                ((event.shiftKey && document.activeElement === first) ||
                    (!event.shiftKey && document.activeElement === last))
            ) {
                event.preventDefault()
                ;(event.shiftKey ? last : first).focus()
            }
        }
    }
    window.addEventListener('popstate', closeOnBack)
    window.addEventListener('keydown', keydown, true)
    queueMicrotask(() => {
        if (node.isConnected && isTopmost()) node.querySelector<HTMLElement>('button')?.focus()
    })
    return {
        update(value: { close(): void }) {
            options = value
        },
        destroy() {
            window.removeEventListener('popstate', closeOnBack)
            window.removeEventListener('keydown', keydown, true)
            if (isTopmost()) history.back()
            if (opener?.isConnected) opener.focus()
        },
    }
}
