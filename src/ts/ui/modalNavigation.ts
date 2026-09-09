/** Let browser/WebView Back dismiss a local dialog and restore its opener. */
export function modalNavigation(node: HTMLElement, options: { close(): void }) {
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const token = crypto.randomUUID()
    const key = 'risunestModal'
    history.pushState({ ...history.state, [key]: token }, '')
    const closeOnBack = () => {
        if (history.state?.[key] !== token) options.close()
    }
    const keydown = (event: KeyboardEvent) => {
        if (event.key === 'Escape') {
            event.preventDefault()
            event.stopImmediatePropagation()
            options.close()
        }
        if (event.key === 'Tab') {
            const items = [
                ...node.querySelectorAll<HTMLElement>(
                    'button:not(:disabled), input:not(:disabled), select:not(:disabled), [tabindex="0"]',
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
        if (node.isConnected) node.querySelector<HTMLElement>('button')?.focus()
    })
    return {
        update(value: { close(): void }) {
            options = value
        },
        destroy() {
            window.removeEventListener('popstate', closeOnBack)
            window.removeEventListener('keydown', keydown, true)
            if (history.state?.[key] === token) history.back()
            if (opener?.isConnected) opener.focus()
        },
    }
}
