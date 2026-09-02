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
                viewLog: 'View error log',
                copyLog: 'Copy error log',
                fileLog: 'Save error log to a file',
                fileLogHelp: 'File logging help',
                logEmpty: 'No errors recorded.',
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

    it('shows native entries newest first with their time and level', async () => {
        nativeLog.getNativeLogTail.mockResolvedValue([
            { tsMs: 0, level: 'warn', target: 'native', message: 'older' },
            { tsMs: 1_000, level: 'error', target: 'native', message: 'newer' },
        ])

        await render()

        const text = target.textContent ?? ''
        expect(text).toContain('[1970-01-01T00:00:01.000Z] [error] newer')
        expect(text.indexOf('newer')).toBeLessThan(text.indexOf('older'))
    })

    it('shows the localized empty state', async () => {
        await render()

        expect(target.textContent).toContain('No errors recorded.')
    })

    it('views logs with alertMd and copies with the project clipboard fallback', async () => {
        nativeLog.getNativeLogTail.mockResolvedValue([
            { tsMs: 0, level: 'error', target: 'native', message: 'failure' },
        ])
        Object.defineProperty(document, 'execCommand', {
            configurable: true,
            value: vi.fn(() => true),
        })
        const execCommand = vi.spyOn(document, 'execCommand')
        Object.defineProperty(navigator, 'clipboard', {
            configurable: true,
            value: { writeText: vi.fn(async () => { throw new Error('denied') }) },
        })
        await render()

        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await Promise.resolve()
        await Promise.resolve()

        expect(alerts.alertMd).toHaveBeenCalledWith('[1970-01-01T00:00:00.000Z] [error] failure')
        expect(execCommand).toHaveBeenCalledWith('copy')
    })

    it('synchronizes file logging with native state and device settings', async () => {
        await render()

        expect(target.textContent).toContain('/data/logs/risunest.log')
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!
        expect(checkbox.classList.contains('sr-only')).toBe(true)
        expect(checkbox.classList.contains('hidden')).toBe(false)
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
        nativeLog.getNativeLogTail.mockRejectedValue(new Error('native command detail'))
        const error = vi.spyOn(console, 'error').mockImplementation(() => undefined)

        await render()

        expect(target.textContent).toContain('Localized error')
        expect(target.textContent).not.toContain('native command detail')
        expect(error).toHaveBeenCalledWith('Native log viewer command failed', expect.any(Error))
        expect(target.querySelector('[role="alert"][aria-live="assertive"]')).not.toBeNull()
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
        expect(error).toHaveBeenCalledWith('Native log viewer command failed', expect.any(Error))
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
})
