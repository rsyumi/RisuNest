import { afterEach, describe, expect, it, vi } from 'vitest'
import { offerHtmlClipboardExport } from './htmlClipboardExport'
import type { alertData } from './alert'

afterEach(() => vi.unstubAllGlobals())
function fixture() {
    let selection: alertData
    const dependencies = { present: vi.fn((value: alertData) => { selection = value }),
        download: vi.fn(async () => true), success: vi.fn(), error: vi.fn(),
        labels: { copy: 'Copy', download: 'Download', cancel: 'Cancel' } }
    return { dependencies, select: (index: number) => selection.onSelect!(index) }
}
describe('prepared HTML clipboard export', () => {
    it('starts the clipboard write in the selection callback and reports success only after completion', async () => {
        let finish!: () => void
        const write = vi.fn(() => new Promise<void>(resolve => { finish = resolve }))
        vi.stubGlobal('navigator', { clipboard: { write } })
        vi.stubGlobal('ClipboardItem', class { constructor(readonly data: unknown) {} })
        const test = fixture()
        offerHtmlClipboardExport('<table>synthetic</table>', 'chat.html', test.dependencies)
        expect(write).not.toHaveBeenCalled()
        test.select(0)
        expect(write).toHaveBeenCalledOnce()
        expect(test.dependencies.success).not.toHaveBeenCalled()
        finish()
        await Promise.resolve()
        expect(test.dependencies.success).toHaveBeenCalledWith(true)
    })
    it('offers the same HTML as a file after a rejected write', async () => {
        vi.stubGlobal('navigator', { clipboard: { write: vi.fn().mockRejectedValue(new Error('denied')) } })
        vi.stubGlobal('ClipboardItem', class {})
        const test = fixture()
        offerHtmlClipboardExport('<table>synthetic</table>', 'chat.html', test.dependencies)
        test.select(0)
        await Promise.resolve()
        expect(test.dependencies.success).not.toHaveBeenCalled()
        test.select(0)
        await Promise.resolve()
        expect(test.dependencies.download).toHaveBeenCalledExactlyOnceWith('chat.html', '<table>synthetic</table>')
        expect(test.dependencies.success).toHaveBeenCalledWith(false)
    })
    it('offers only file download and cancellation when clipboard HTML is unavailable', async () => {
        vi.stubGlobal('ClipboardItem', undefined)
        const test = fixture()
        offerHtmlClipboardExport('<table/>', 'chat.html', test.dependencies)
        expect(test.dependencies.present.mock.calls[0][0].msg).toBe('Download (.html)||Cancel')
        test.select(1)
        expect(test.dependencies.download).not.toHaveBeenCalled()
        test.dependencies.download.mockResolvedValue(false)
        test.select(0)
        await Promise.resolve()
        expect(test.dependencies.success).not.toHaveBeenCalled()
    })
})
