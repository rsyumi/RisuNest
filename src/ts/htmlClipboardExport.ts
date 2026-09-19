import type { alertData } from './alert'

export function offerHtmlClipboardExport(html: string, filename: string, dependencies: {
    present(value: alertData): void
    download(filename: string, html: string): Promise<boolean>
    success(copied: boolean): void
    error(error: unknown): void
    labels: { copy: string; download: string; cancel: string }
}): void {
    const canCopy = typeof ClipboardItem !== 'undefined' && typeof navigator.clipboard?.write === 'function'
    const choices = [...(canCopy ? [dependencies.labels.copy] : []), `${dependencies.labels.download} (.html)`, dependencies.labels.cancel]
    dependencies.present({ type: 'select', msg: choices.join('||'), onSelect: (index) => {
        if (canCopy && index === 0) {
            try {
                const item = new ClipboardItem({
                    'text/html': new Blob([html], { type: 'text/html' }),
                    'text/plain': new Blob([html], { type: 'text/plain' }),
                })
                // Invoke directly in the selection click, before any asynchronous preparation.
                void navigator.clipboard.write([item]).then(() => dependencies.success(true), () => {
                    dependencies.present({ type: 'select', msg: [`${dependencies.labels.download} (.html)`, dependencies.labels.cancel].join('||'),
                        onSelect: index => { if (index === 0) download() } })
                })
            } catch {
                dependencies.present({ type: 'select', msg: [`${dependencies.labels.download} (.html)`, dependencies.labels.cancel].join('||'),
                    onSelect: index => { if (index === 0) download() } })
            }
        } else if (index === (canCopy ? 1 : 0)) download()
    } })
    function download(): void {
        void dependencies.download(filename, html).then(saved => {
            if (saved) dependencies.success(false)
        }, dependencies.error)
    }
}
