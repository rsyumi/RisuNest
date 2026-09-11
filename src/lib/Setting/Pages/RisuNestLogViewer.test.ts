// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const nativeLog = vi.hoisted(() => ({
    getNativeLogTail: vi.fn(),
    getNativeLogFilePath: vi.fn(),
    setNativeLogFileEnabled: vi.fn(),
}))
const alerts = vi.hoisted(() => ({ alertMd: vi.fn() }))
const deviceSettings = vi.hoisted(() => {
    let listener: ((settings: { nativeFileLogEnabled: boolean }) => void) | undefined
    let settings = { nativeFileLogEnabled: true }
    return {
        getDeviceSettings: vi.fn(() => ({ ...settings })),
        updateDeviceSettings: vi.fn((partial: { nativeFileLogEnabled: boolean }) => {
            settings = { ...settings, ...partial }
            listener?.({ ...settings })
        }),
        subscribeDeviceSettings: vi.fn((nextListener) => {
            listener = nextListener
            return () => { listener = undefined }
        }),
        emit(nextSettings: { nativeFileLogEnabled: boolean }) {
            settings = { ...nextSettings }
            listener?.({ ...settings })
        },
        reset(nativeFileLogEnabled = true) {
            settings = { nativeFileLogEnabled }
            listener = undefined
        },
    }
})

vi.mock('src/ts/nativeLog', () => nativeLog)
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/storage/deviceSettings', () => deviceSettings)
vi.mock('src/lang', () => ({
    language: {
        error: 'Localized error',
        risuNest: {
            diag: {
                title: 'Diagnostics',
                logTitle: 'Error log',
                logHelp: 'Recent errors.',
                viewLog: 'View error log',
                copyLog: 'Copy error log',
                fileLog: 'Save error log to a file',
                fileLogHelp: 'File logging help',
                logEmpty: 'No errors recorded.',
                actionFailed: 'Localized error',
            },
        },
    },
}))

import RisuNestLogViewer from './RisuNestLogViewer.svelte'

describe('RisuNestLogViewer', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        deviceSettings.reset()
        nativeLog.getNativeLogTail.mockReset()
        nativeLog.getNativeLogFilePath.mockReset()
        nativeLog.setNativeLogFileEnabled.mockReset()
        alerts.alertMd.mockReset()
        target = document.createElement('div')
        document.body.append(target)
        nativeLog.getNativeLogTail.mockResolvedValue([])
        nativeLog.getNativeLogFilePath.mockResolvedValue('/data/logs/risunest.log')
        nativeLog.setNativeLogFileEnabled.mockResolvedValue(undefined)
        Object.defineProperty(navigator, 'clipboard', {
            configurable: true,
            value: { writeText: vi.fn(async () => undefined) },
        })
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        vi.clearAllMocks()
        document.body.replaceChildren()
    })

    async function render() {
        mounted = mount(RisuNestLogViewer, { target })
        await tick()
        await Promise.resolve()
        await tick()
    }

    async function settleAction() {
        await Promise.resolve()
        await Promise.resolve()
        await tick()
    }

    it('fetches a fresh tail for view while keeping native entries off the page', async () => {
        nativeLog.getNativeLogTail.mockResolvedValueOnce([
            { tsMs: 0, level: 'warn', target: 'native', message: 'older' },
            { tsMs: 1_000, level: 'error', target: 'native', message: 'newer' },
        ])

        await render()

        expect(nativeLog.getNativeLogTail).not.toHaveBeenCalled()
        expect(target.textContent).not.toContain('newer')
        expect(target.textContent).not.toContain('older')

        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        await settleAction()

        expect(nativeLog.getNativeLogTail).toHaveBeenCalledOnce()
        expect(alerts.alertMd).toHaveBeenCalledWith(
            '~~~~\n[1970-01-01T00:00:01.000Z] [error] newer\n[1970-01-01T00:00:00.000Z] [warn] older\n~~~~',
        )
    })

    it('lengthens the fence past any tilde run inside the log', async () => {
        nativeLog.getNativeLogTail.mockResolvedValueOnce([
            { tsMs: 0, level: 'error', target: 'native', message: '<b>raw</b> ~~~~~ "quoted"' },
        ])

        await render()
        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        await settleAction()

        expect(alerts.alertMd).toHaveBeenCalledWith(
            '~~~~~~\n[1970-01-01T00:00:00.000Z] [error] <b>raw</b> ~~~~~ "quoted"\n~~~~~~',
        )
    })

    it('shows the localized empty state', async () => {
        await render()

        expect(target.textContent).not.toContain('No errors recorded.')
        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        await settleAction()
        expect(alerts.alertMd).toHaveBeenCalledWith('No errors recorded.')
    })

    it('fetches a fresh tail for copy and uses the project clipboard fallback', async () => {
        nativeLog.getNativeLogTail.mockResolvedValueOnce([
            { tsMs: 1_000, level: 'error', target: 'native', message: 'post-mount failure' },
        ])
        Object.defineProperty(document, 'execCommand', {
            configurable: true,
            value: vi.fn(() => true),
        })
        const execCommand = vi.spyOn(document, 'execCommand')
        const writeText = vi.fn(async () => { throw new Error('denied') })
        Object.defineProperty(navigator, 'clipboard', {
            configurable: true,
            value: { writeText },
        })
        await render()

        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await settleAction()

        expect(nativeLog.getNativeLogTail).toHaveBeenCalledOnce()
        expect(writeText).toHaveBeenCalledWith(
            '[1970-01-01T00:00:01.000Z] [error] post-mount failure',
        )
        expect(document.querySelector('textarea')).toBeNull()
        expect(execCommand).toHaveBeenCalledWith('copy')
    })

    it('ignores a stale view result when a newer view finishes first', async () => {
        await render()
        const resolvers: Array<(entries: Array<{ tsMs: number, level: string, target: string, message: string }>) => void> = []
        nativeLog.getNativeLogTail.mockImplementation(() => new Promise((resolve) => {
            resolvers.push(resolve)
        }))

        const viewButton = target.querySelector<HTMLButtonElement>('[data-view-log]')!
        viewButton.click()
        viewButton.click()
        expect(resolvers).toHaveLength(2)
        resolvers[1]([{ tsMs: 2_000, level: 'error', target: 'native', message: 'newest' }])
        await settleAction()
        resolvers[0]([{ tsMs: 1_000, level: 'error', target: 'native', message: 'stale' }])
        await settleAction()

        expect(alerts.alertMd).toHaveBeenCalledOnce()
        expect(alerts.alertMd).toHaveBeenCalledWith('~~~~\n[1970-01-01T00:00:02.000Z] [error] newest\n~~~~')
    })

    it('ignores a stale copy result when a newer copy finishes first', async () => {
        await render()
        const resolvers: Array<(entries: Array<{ tsMs: number, level: string, target: string, message: string }>) => void> = []
        nativeLog.getNativeLogTail.mockImplementation(() => new Promise((resolve) => {
            resolvers.push(resolve)
        }))
        const writeText = vi.spyOn(navigator.clipboard, 'writeText')

        const copyButton = target.querySelector<HTMLButtonElement>('[data-copy-log]')!
        copyButton.click()
        copyButton.click()
        expect(resolvers).toHaveLength(2)
        resolvers[1]([{ tsMs: 2_000, level: 'error', target: 'native', message: 'newest' }])
        await settleAction()
        resolvers[0]([{ tsMs: 1_000, level: 'error', target: 'native', message: 'stale' }])
        await settleAction()

        expect(writeText).toHaveBeenCalledOnce()
        expect(writeText).toHaveBeenCalledWith('[1970-01-01T00:00:02.000Z] [error] newest')
    })

    it('serializes clipboard writes so a late fallback cannot overwrite the newest copy', async () => {
        nativeLog.getNativeLogTail
            .mockResolvedValueOnce([
                { tsMs: 1_000, level: 'error', target: 'native', message: 'older' },
            ])
            .mockResolvedValueOnce([
                { tsMs: 2_000, level: 'error', target: 'native', message: 'newest' },
            ])
        let rejectFirstWrite: ((error: Error) => void) | undefined
        let clipboardText = ''
        const writeText = vi.fn((text: string) => {
            if (writeText.mock.calls.length === 1) {
                return new Promise<void>((_resolve, reject) => {
                    rejectFirstWrite = reject
                })
            }
            clipboardText = text
            return Promise.resolve()
        })
        Object.defineProperty(navigator, 'clipboard', {
            configurable: true,
            value: { writeText },
        })
        Object.defineProperty(document, 'execCommand', {
            configurable: true,
            value: vi.fn(() => {
                clipboardText = document.querySelector<HTMLTextAreaElement>('textarea')!.value
                return true
            }),
        })
        await render()
        const copyButton = target.querySelector<HTMLButtonElement>('[data-copy-log]')!

        copyButton.click()
        await settleAction()
        copyButton.click()
        await settleAction()
        rejectFirstWrite!(new Error('first clipboard write denied'))
        await vi.waitFor(() => expect(writeText).toHaveBeenCalledTimes(2))
        await settleAction()

        expect(clipboardText).toBe('[1970-01-01T00:00:02.000Z] [error] newest')
    })

    it('synchronizes file logging with native state and device settings', async () => {
        await render()

        expect(target.textContent).toContain('/data/logs/risunest.log')
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!
        expect(checkbox.classList.contains('sr-only')).toBe(true)
        expect(checkbox.classList.contains('hidden')).toBe(false)
        expect(checkbox.closest('label')?.className).toContain('focus-within:outline-darkborderc')
        checkbox.checked = false
        checkbox.dispatchEvent(new Event('change', { bubbles: true }))
        await Promise.resolve()
        await tick()

        expect(nativeLog.setNativeLogFileEnabled).toHaveBeenCalledWith(false)
        expect(deviceSettings.updateDeviceSettings).toHaveBeenCalledWith({ nativeFileLogEnabled: false })

        deviceSettings.emit({ nativeFileLogEnabled: true })
        await tick()
        expect(checkbox.checked).toBe(true)
    })

    it('shows localized failure copy without rendering raw command details', async () => {
        nativeLog.getNativeLogTail.mockRejectedValueOnce(new Error('native command detail'))
        const error = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        await render()

        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        await settleAction()

        expect(target.textContent).toContain('Localized error')
        expect(target.textContent).not.toContain('native command detail')
        expect(error).not.toHaveBeenCalled()
        expect(alerts.alertMd).not.toHaveBeenCalled()
        expect(target.querySelector('[role="alert"][aria-live="assertive"]')).not.toBeNull()
    })

    it('maps a copy tail failure to localized safe copy', async () => {
        nativeLog.getNativeLogTail.mockRejectedValueOnce(new Error('copy command detail'))
        const writeText = vi.spyOn(navigator.clipboard, 'writeText')
        await render()

        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await settleAction()

        expect(target.textContent).toContain('Localized error')
        expect(target.textContent).not.toContain('copy command detail')
        expect(writeText).not.toHaveBeenCalled()
    })

    it('disables the file logging checkbox while its native update is pending', async () => {
        let resolveNativeUpdate: (() => void) | undefined
        nativeLog.setNativeLogFileEnabled.mockImplementation(() => new Promise<void>((resolve) => {
            resolveNativeUpdate = resolve
        }))
        await render()
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!

        checkbox.checked = false
        checkbox.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()

        expect(checkbox.disabled).toBe(true)
        expect(nativeLog.setNativeLogFileEnabled).toHaveBeenCalledOnce()

        resolveNativeUpdate!()
        await Promise.resolve()
        await tick()
        expect(checkbox.disabled).toBe(false)
        expect(checkbox.checked).toBe(false)
    })

    it('rolls back a failed native update and re-enables the checkbox', async () => {
        let rejectNativeUpdate: ((error: Error) => void) | undefined
        nativeLog.setNativeLogFileEnabled.mockImplementation(() => new Promise<void>((_resolve, reject) => {
            rejectNativeUpdate = reject
        }))
        const error = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        await render()
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!

        checkbox.checked = false
        checkbox.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()
        expect(checkbox.disabled).toBe(true)

        rejectNativeUpdate!(new Error('native toggle failed'))
        await Promise.resolve()
        await tick()
        expect(checkbox.disabled).toBe(false)
        expect(checkbox.checked).toBe(true)
        expect(deviceSettings.updateDeviceSettings).not.toHaveBeenCalled()
        expect(error).not.toHaveBeenCalled()
    })

    it('loads the file path once when enabling file logging', async () => {
        deviceSettings.reset(false)
        await render()
        nativeLog.getNativeLogFilePath.mockClear()
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!

        checkbox.checked = true
        checkbox.dispatchEvent(new Event('change', { bubbles: true }))
        await Promise.resolve()
        await tick()

        expect(nativeLog.getNativeLogFilePath).toHaveBeenCalledOnce()
    })

    it('provides a visible theme-token focus outline for the custom toggle', async () => {
        await render()

        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!
        const label = checkbox.closest('label')!
        expect(label.className).toContain('focus-within:outline')
        expect(label.className).toContain('focus-within:outline-darkborderc')
    })
})
