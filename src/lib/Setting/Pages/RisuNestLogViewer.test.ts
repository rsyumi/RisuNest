// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { consoleLog } from 'src/ts/consoleLog'
import { createLocalDeviceMarkers } from 'src/ts/storage/deviceMarkers'
import { getDeviceSettings, loadDeviceSettings, updateDeviceSettings } from 'src/ts/storage/deviceSettings'
import versionData from '../../../../version.json'
import { appVer } from 'src/ts/appVersion'

const testUA = vi.hoisted(() => {
    const value = 'Mozilla/5.0 (Windows NT 10.0; Win64; x64)'
    Object.defineProperty(window, '__TAURI_INTERNALS__', { configurable: true, value: {} })
    Object.defineProperty(navigator, 'userAgent', { configurable: true, value })
    return value
})
const header = `RisuNest ${versionData.version} (RisuAI ${appVer}) | OS: Windows (NT 10.0.26100) | Device: Synthetic PC (x86_64) | UA: ${testUA}\n\n`

// One handler per native command; each receives the arguments the product sent with that command.
const commands = vi.hoisted(() => ({
    diagnostic_device_info: vi.fn(),
    native_log_tail: vi.fn(),
    native_log_file_path: vi.fn(),
    native_log_set_file_enabled: vi.fn(),
    native_log_file_read: vi.fn(),
    native_log_file_export: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', async (importOriginal) => ({
    ...await importOriginal<typeof import('@tauri-apps/api/core')>(),
    invoke: vi.fn((command: string, args?: Record<string, unknown>) => Object.hasOwn(commands, command)
        ? commands[command as keyof typeof commands](args)
        : Promise.reject(new Error(`unexpected command ${command}`))),
}))

vi.mock('@tauri-apps/plugin-os', () => ({ arch: () => 'x86_64', type: () => 'windows', version: () => '10.0.26100' }))
vi.mock('@tauri-apps/plugin-dialog', async () => (await import('src/ts/globalApi.testSupport')).tauriDialogModule())

import { language } from 'src/lang'
import RisuNestLogViewer from './RisuNestLogViewer.svelte'
import { alertStore } from 'src/ts/alertQueue'
import { save } from '@tauri-apps/plugin-dialog'

const strings = language.risuNest.diag

describe('RisuNestLogViewer', () => {
    let target: HTMLDivElement
    let mounted: ReturnType<typeof mount> | undefined
    let dialogs: string[]
    let wrappedDialogs: boolean[]
    let toasts: string[]
    let unsubscribeAlerts: () => void

    beforeEach(() => {
        consoleLog.clear()
        localStorage.clear()
        loadDeviceSettings(createLocalDeviceMarkers(localStorage))
        commands.diagnostic_device_info.mockResolvedValue({ model: 'Synthetic PC', arch: 'x86_64' })
        commands.native_log_tail.mockReset()
        commands.native_log_file_path.mockReset()
        commands.native_log_set_file_enabled.mockReset()
        commands.native_log_file_read.mockReset()
        commands.native_log_file_read.mockResolvedValue({ entries: [], older: null })
        commands.native_log_file_export.mockReset()
        vi.mocked(save).mockReset()
        dialogs = []
        wrappedDialogs = []
        toasts = []
        unsubscribeAlerts = alertStore.subscribe(dialog => {
            if (dialog.type === 'toast') toasts.push(dialog.msg)
            if (dialog.type !== 'markdown') return
            dialogs.push(dialog.msg)
            wrappedDialogs.push(!!dialog.wrapCode)
            queueMicrotask(() => alertStore.set({ type: 'none', msg: '' }))
        })
        target = document.createElement('div')
        document.body.append(target)
        commands.native_log_tail.mockResolvedValue([])
        commands.native_log_file_path.mockResolvedValue('/data/logs/risunest.log')
        commands.native_log_set_file_enabled.mockResolvedValue(undefined)
        Object.defineProperty(navigator, 'clipboard', {
            configurable: true,
            value: { writeText: vi.fn(async () => undefined) },
        })
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        await alertStore.idle()
        alertStore.clearStatus()
        unsubscribeAlerts()
        vi.clearAllMocks()
        document.body.replaceChildren()
    })

    async function render() {
        mounted = mount(RisuNestLogViewer, { target })
        await settleAction()
    }

    // Runs every queued promise continuation, through the nativeLog wrappers around invoke, then flushes the DOM.
    async function settleAction() {
        await new Promise((resolve) => setTimeout(resolve, 0))
        await tick()
    }

    it('waits for loaded device details on the first view and copy while keeping entries off the page', async () => {
        let resolveDevice!: (device: { model: string; arch: string }) => void
        commands.diagnostic_device_info.mockImplementation(() => new Promise(resolve => { resolveDevice = resolve }))
        commands.native_log_tail.mockResolvedValue([
            { tsMs: 0, level: 'warn', target: 'native', message: 'older' },
            { tsMs: 1_000, level: 'error', target: 'native', message: 'newer' },
        ])

        await render()

        expect(commands.native_log_tail).not.toHaveBeenCalled()
        expect(target.textContent).not.toContain('newer')
        expect(target.textContent).not.toContain('older')

        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await settleAction()
        expect(dialogs).toEqual([])
        expect(navigator.clipboard.writeText).not.toHaveBeenCalled()
        resolveDevice({ model: 'Synthetic PC', arch: 'x86_64' })
        await settleAction()
        expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
            header + '[1970-01-01T00:00:01.000Z] [error] [native] newer\n[1970-01-01T00:00:00.000Z] [warn] [native] older',
        )

        expect(commands.native_log_tail).toHaveBeenCalledWith({ limit: 500 })
        expect(wrappedDialogs).toEqual([true])
        expect(dialogs).toContain(
            '~~~~\n' + header + '[1970-01-01T00:00:01.000Z] [error] [native] newer\n[1970-01-01T00:00:00.000Z] [warn] [native] older\n~~~~',
        )
    })

    it('lengthens the fence past any tilde run inside the log', async () => {
        commands.native_log_tail.mockResolvedValueOnce([
            { tsMs: 0, level: 'error', target: 'native', message: '<b>raw</b> ~~~~~ "quoted"' },
        ])

        await render()
        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        await settleAction()

        expect(dialogs).toContain(
            '~~~~~~\n' + header + '[1970-01-01T00:00:00.000Z] [error] [native] <b>raw</b> ~~~~~ "quoted"\n~~~~~~',
        )
    })

    it('shows the localized empty state', async () => {
        await render()

        expect(target.textContent).not.toContain(strings.logEmpty)
        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        await settleAction()
        expect(dialogs).toContain(strings.logEmpty)
        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await settleAction()
        expect(navigator.clipboard.writeText).toHaveBeenLastCalledWith(strings.logEmpty)
    })

    it('views, copies and clears console logs with the existing dialog and clipboard behavior', async () => {
        consoleLog.append('info', 'App', 'startup', 2000)
        consoleLog.append('warn', 'synthetic-plugin', 'plugin output ~~~~~', 1000)
        await render()
        expect(target.textContent).toContain(strings.consoleTitle)
        expect(target.textContent).not.toContain('plugin output')
        target.querySelector<HTMLButtonElement>('[data-view-console-log]')!.click()
        await settleAction()
        const text = header + '[1970-01-01T00:00:02.000Z] [info] [App] startup\n[1970-01-01T00:00:01.000Z] [warn] [synthetic-plugin] plugin output ~~~~~'
        expect(dialogs.at(-1)).toBe('~~~~~~\n' + text + '\n~~~~~~')
        target.querySelector<HTMLButtonElement>('[data-copy-console-log]')!.click()
        await settleAction()
        expect(navigator.clipboard.writeText).toHaveBeenLastCalledWith(text)
        target.querySelector<HTMLButtonElement>('[data-clear-console-log]')!.click()
        expect(consoleLog.snapshot()).toEqual([])
        target.querySelector<HTMLButtonElement>('[data-view-console-log]')!.click()
        await settleAction()
        expect(dialogs.at(-1)).toBe(strings.consoleEmpty)
        expect(commands.native_log_tail).not.toHaveBeenCalled()
    })

    it('cancels a console snapshot when cleared before its copy starts', async () => {
        consoleLog.append('log', 'App', 'stale')
        await render()
        target.querySelector<HTMLButtonElement>('[data-copy-console-log]')!.click()
        target.querySelector<HTMLButtonElement>('[data-clear-console-log]')!.click()
        await settleAction()
        expect(navigator.clipboard.writeText).not.toHaveBeenCalled()
        consoleLog.append('log', 'App', 'fresh', 0)
        target.querySelector<HTMLButtonElement>('[data-copy-console-log]')!.click()
        await settleAction()
        expect(navigator.clipboard.writeText).toHaveBeenLastCalledWith(header + '[1970-01-01T00:00:00.000Z] [log] [App] fresh')
        let rejectCopy!: (error: Error) => void
        vi.mocked(navigator.clipboard.writeText).mockImplementationOnce(() => new Promise<void>((_, reject) => { rejectCopy = reject }))
        Object.defineProperty(document, 'execCommand', { configurable: true, value: vi.fn(() => true) })
        target.querySelector<HTMLButtonElement>('[data-copy-console-log]')!.click()
        await vi.waitFor(() => expect(rejectCopy).toBeTypeOf('function'))
        target.querySelector<HTMLButtonElement>('[data-clear-console-log]')!.click()
        rejectCopy(new Error('permission denied'))
        await settleAction()
        expect(document.execCommand).not.toHaveBeenCalled()
    })

    it('fetches a fresh tail for copy and uses the project clipboard fallback', async () => {
        commands.native_log_tail.mockResolvedValueOnce([
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

        expect(writeText).toHaveBeenCalledWith(
            header + '[1970-01-01T00:00:01.000Z] [error] [native] post-mount failure',
        )
        expect(document.querySelector('textarea')).toBeNull()
        expect(execCommand).toHaveBeenCalledWith('copy')
        expect(toasts).toEqual([language.copied])
        expect(document.querySelector('[role="status"].sr-only')!.textContent).toBe(language.copied)
    })

    it('says the clipboard refused a copy instead of confirming it', async () => {
        commands.native_log_tail.mockResolvedValue([{ tsMs: 1_000, level: 'error', target: 'native', message: 'refused' }])
        Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: vi.fn(async () => { throw new Error('denied') }) } })
        Object.defineProperty(document, 'execCommand', { configurable: true, value: vi.fn().mockReturnValueOnce(true).mockReturnValue(false) })
        await render()

        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await settleAction()
        expect(toasts).toEqual([language.copied])

        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await settleAction()

        expect(target.querySelector('[role="alert"]')!.textContent).toBe(language.risuNest.networkLog.clipboardDenied)
        expect(toasts).toEqual([language.copied])
        expect(document.querySelector('[role="status"].sr-only')!.textContent).toBe('')
    })

    it('ignores a stale view result when a newer view finishes first', async () => {
        await render()
        const resolvers: Array<(entries: Array<{ tsMs: number, level: string, target: string, message: string }>) => void> = []
        commands.native_log_tail.mockImplementation(() => new Promise((resolve) => {
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

        expect(dialogs).toHaveLength(1)
        expect(dialogs).toContain('~~~~\n' + header + '[1970-01-01T00:00:02.000Z] [error] [native] newest\n~~~~')
    })

    it('ignores a stale copy result when a newer copy finishes first', async () => {
        await render()
        const resolvers: Array<(entries: Array<{ tsMs: number, level: string, target: string, message: string }>) => void> = []
        commands.native_log_tail.mockImplementation(() => new Promise((resolve) => {
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
        expect(writeText).toHaveBeenCalledWith(header + '[1970-01-01T00:00:02.000Z] [error] [native] newest')
    })

    it('serializes clipboard writes so a late fallback cannot overwrite the newest copy', async () => {
        commands.native_log_tail
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

        expect(clipboardText).toBe(header + '[1970-01-01T00:00:02.000Z] [error] [native] newest')
    })

    it('synchronizes file logging with native state and device settings', async () => {
        await render()

        expect(target.textContent).toContain('/data/logs/risunest.log')
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!
        checkbox.checked = false
        checkbox.dispatchEvent(new Event('change', { bubbles: true }))
        await settleAction()

        expect(commands.native_log_set_file_enabled).toHaveBeenCalledWith({ enabled: false })
        expect(getDeviceSettings().nativeFileLogEnabled).toBe(false)

        updateDeviceSettings({ nativeFileLogEnabled: true })
        await tick()
        expect(checkbox.checked).toBe(true)
    })

    it('shows localized failure copy without rendering raw command details', async () => {
        commands.native_log_tail.mockRejectedValueOnce(new Error('native command detail'))
        const error = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        await render()

        target.querySelector<HTMLButtonElement>('[data-view-log]')!.click()
        await settleAction()

        expect(target.textContent).toContain(strings.actionFailed)
        expect(target.textContent).not.toContain('native command detail')
        expect(error).not.toHaveBeenCalled()
        expect(dialogs).toEqual([])
        expect(target.querySelector('[role="alert"][aria-live="assertive"]')).not.toBeNull()
    })

    it('maps a copy tail failure to localized safe copy', async () => {
        commands.native_log_tail.mockRejectedValueOnce(new Error('copy command detail'))
        const writeText = vi.spyOn(navigator.clipboard, 'writeText')
        await render()

        target.querySelector<HTMLButtonElement>('[data-copy-log]')!.click()
        await settleAction()

        expect(target.textContent).toContain(strings.actionFailed)
        expect(target.textContent).not.toContain('copy command detail')
        expect(writeText).not.toHaveBeenCalled()
    })

    it('disables the file logging checkbox while its native update is pending', async () => {
        let resolveNativeUpdate: (() => void) | undefined
        commands.native_log_set_file_enabled.mockImplementation(() => new Promise<void>((resolve) => {
            resolveNativeUpdate = resolve
        }))
        await render()
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!

        checkbox.checked = false
        checkbox.dispatchEvent(new Event('change', { bubbles: true }))
        await tick()

        expect(checkbox.disabled).toBe(true)

        resolveNativeUpdate!()
        await settleAction()
        expect(checkbox.disabled).toBe(false)
        expect(checkbox.checked).toBe(false)
    })

    it('rolls back a failed native update and re-enables the checkbox', async () => {
        let rejectNativeUpdate: ((error: Error) => void) | undefined
        commands.native_log_set_file_enabled.mockImplementation(() => new Promise<void>((_resolve, reject) => {
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
        await settleAction()
        expect(checkbox.disabled).toBe(false)
        expect(checkbox.checked).toBe(true)
        expect(getDeviceSettings().nativeFileLogEnabled).toBe(true)
        expect(error).not.toHaveBeenCalled()
    })

    it('shows the file path when enabling file logging', async () => {
        updateDeviceSettings({ nativeFileLogEnabled: false })
        commands.native_log_file_path.mockResolvedValue('/data/logs/enabled.log')
        await render()
        const checkbox = target.querySelector<HTMLInputElement>('input[type="checkbox"]')!
        expect(target.textContent).not.toContain('/data/logs/enabled.log')

        checkbox.checked = true
        checkbox.dispatchEvent(new Event('change', { bubbles: true }))
        await settleAction()

        expect(target.textContent).toContain('/data/logs/enabled.log')
        expect(getDeviceSettings().nativeFileLogEnabled).toBe(true)
    })

    describe('saved log', () => {
        const newestPage = {
            entries: [
                { tsMs: 10_000, level: 'info', target: 'app-start', message: 'RisuNest 9.9.9' },
                { tsMs: 11_000, level: 'warn', target: 'slow-operation', message: 'current run warning' },
            ],
            older: { file: 'current', before: 120 },
        }
        const olderPage = {
            entries: [
                { tsMs: 0, level: '', target: '', message: 'unsplit line' },
                { tsMs: 5_000, level: 'error', target: 'native-command', message: 'previous run failure\nsecond line' },
            ],
            older: null,
        }

        function shownRows() {
            return [...target.querySelectorAll('[data-saved-log-entry], [data-saved-log-run]')].map((row) => ({
                run: row.hasAttribute('data-saved-log-run'),
                text: row.textContent!.replace(/\s+/g, ' ').trim(),
            }))
        }

        function button(selector: string) {
            return target.querySelector<HTMLButtonElement>(selector)!
        }

        it('shows the newest page first and pages back into the previous run', async () => {
            commands.native_log_file_read.mockImplementation(async (args?: Record<string, unknown>) => args?.before === 120 ? olderPage : newestPage)
            await render()
            expect(target.querySelector('[data-saved-log]')).toBeNull()

            button('[data-view-saved-log]').click()
            await settleAction()

            expect(commands.native_log_file_read).toHaveBeenLastCalledWith({ file: 'current', before: null })
            expect(button('[data-view-saved-log]').textContent).toContain(language.close)
            expect(button('[data-view-saved-log]').getAttribute('aria-expanded')).toBe('true')
            const rows = shownRows()
            expect(rows.map((row) => row.run)).toEqual([false, true])
            expect(rows[0].text).toContain('warn slow-operation current run warning')
            expect(rows[1].text).toContain(`${strings.appStarted} RisuNest 9.9.9`)
            expect(button('[data-saved-log-newer]').disabled).toBe(true)

            button('[data-saved-log-older]').click()
            await settleAction()

            expect(commands.native_log_file_read).toHaveBeenLastCalledWith({ file: 'current', before: 120 })
            expect(shownRows().map((row) => row.text)).toEqual([
                expect.stringContaining('error native-command previous run failure second line'),
                'unsplit line',
            ])
            expect(button('[data-saved-log-older]').disabled).toBe(true)

            button('[data-saved-log-newer]').click()
            await settleAction()
            expect(commands.native_log_file_read).toHaveBeenLastCalledWith({ file: 'current', before: null })
            expect(shownRows()[0].text).toContain('current run warning')

            button('[data-view-saved-log]').click()
            await tick()
            expect(target.querySelector('[data-saved-log]')).toBeNull()
            expect(button('[data-view-saved-log]').textContent).toContain(strings.viewLog)
        })

        it('copies the page on screen, or the newest page while the viewer is closed', async () => {
            commands.native_log_file_read.mockImplementation(async (args?: Record<string, unknown>) => args?.before === 120 ? olderPage : newestPage)
            await render()

            button('[data-copy-saved-log]').click()
            await settleAction()
            expect(navigator.clipboard.writeText).toHaveBeenLastCalledWith(
                header + '[1970-01-01T00:00:11.000Z] [warn] [slow-operation] current run warning\n[1970-01-01T00:00:10.000Z] [info] [app-start] RisuNest 9.9.9',
            )

            button('[data-view-saved-log]').click()
            await settleAction()
            button('[data-saved-log-older]').click()
            await settleAction()
            const reads = commands.native_log_file_read.mock.calls.length
            button('[data-copy-saved-log]').click()
            await settleAction()

            expect(commands.native_log_file_read).toHaveBeenCalledTimes(reads)
            expect(navigator.clipboard.writeText).toHaveBeenLastCalledWith(
                header + '[1970-01-01T00:00:05.000Z] [error] [native-command] previous run failure\nsecond line\nunsplit line',
            )
        })

        it('keeps the focus on the paging buttons and moves it to the other one at the last page', async () => {
            const middlePage = { entries: [{ tsMs: 8_000, level: 'info', target: 'middle', message: 'middle page' }], older: { file: 'previous', before: null } }
            commands.native_log_file_read.mockImplementation(async (args?: Record<string, unknown>) =>
                args?.file === 'previous' ? olderPage : args?.before === 120 ? middlePage : newestPage)
            await render()
            button('[data-view-saved-log]').focus()
            button('[data-view-saved-log]').click()
            await settleAction()
            await settleAction()
            expect(document.activeElement).toBe(button('[data-view-saved-log]'))

            button('[data-saved-log-older]').focus()
            button('[data-saved-log-older]').click()
            await settleAction()
            await settleAction()
            expect(shownRows()[0].text).toContain('middle page')
            expect(document.activeElement).toBe(button('[data-saved-log-older]'))

            button('[data-saved-log-older]').click()
            await settleAction()
            await settleAction()
            expect(shownRows()[0].text).toContain('previous run failure')
            expect(button('[data-saved-log-older]').disabled).toBe(true)
            expect(document.activeElement).toBe(button('[data-saved-log-newer]'))
        })

        it('saves the whole log to the picked file and confirms it', async () => {
            commands.native_log_file_read.mockResolvedValue(newestPage)
            commands.native_log_file_export.mockResolvedValue({ path: null, bytes: 2048 })
            vi.mocked(save).mockResolvedValueOnce(null).mockResolvedValueOnce('/synthetic/saved.txt')
            await render()

            button('[data-save-saved-log]').click()
            await vi.waitFor(() => expect(save).toHaveBeenCalled())
            await settleAction()
            expect(save).toHaveBeenCalledWith(expect.objectContaining({ defaultPath: expect.stringMatching(/^risunest-error-log-\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}Z\.txt$/) }))
            expect(commands.native_log_file_export).not.toHaveBeenCalled()
            expect(toasts).toEqual([])

            button('[data-save-saved-log]').click()
            await vi.waitFor(() => expect(toasts).toEqual([strings.savedLogSaved]))
            expect(commands.native_log_file_export).toHaveBeenCalledWith({ destination: '/synthetic/saved.txt' })
            expect(toasts).toEqual([strings.savedLogSaved])
            expect(target.querySelector('[role="alert"]')).toBeNull()
            await vi.waitFor(() => expect(document.querySelector('[role="status"].sr-only')!.textContent).toBe(strings.savedLogSaved))

            vi.mocked(save).mockResolvedValueOnce('/synthetic/saved.txt')
            commands.native_log_file_export.mockRejectedValueOnce('native-log-file-export-failed')
            button('[data-save-saved-log]').click()
            await vi.waitFor(() => expect(target.querySelector('[role="alert"]')?.textContent).toBe(strings.saveFailed))
            expect(document.querySelector('[role="status"].sr-only')!.textContent).toBe('')
        })

        it('reports nothing to save without asking for a file, and safe copy for a failed save', async () => {
            await render()
            button('[data-save-saved-log]').click()
            await settleAction()
            expect(save).not.toHaveBeenCalled()
            expect(target.querySelector('[data-saved-log-notice]')!.textContent).toBe(strings.savedLogEmpty)

            commands.native_log_file_read.mockResolvedValue(newestPage)
            vi.mocked(save).mockResolvedValue('/synthetic/saved.txt')
            for (const [failure, shown] of [
                ['native-log-file-empty', strings.savedLogEmpty],
                ['native-log-file-invalid-destination', language.risuNest.networkLog.destinationInvalid],
                ['native-log-file-export-failed', strings.saveFailed],
            ]) {
                const saves = commands.native_log_file_export.mock.calls.length
                commands.native_log_file_export.mockRejectedValueOnce(failure)
                button('[data-save-saved-log]').click()
                await vi.waitFor(() => expect(commands.native_log_file_export).toHaveBeenCalledTimes(saves + 1))
                await settleAction()
                expect(target.textContent).toContain(shown)
                expect(target.textContent).not.toContain(failure)
            }
            expect(toasts).toEqual([])
        })

        it('shows the empty state and localized failure copy without raw details', async () => {
            await render()
            button('[data-view-saved-log]').click()
            await settleAction()
            expect(target.querySelector('[data-saved-log]')!.textContent).toContain(strings.savedLogEmpty)
            button('[data-copy-saved-log]').click()
            await settleAction()
            expect(navigator.clipboard.writeText).toHaveBeenLastCalledWith(strings.savedLogEmpty)

            button('[data-view-saved-log]').click()
            await tick()
            commands.native_log_file_read.mockRejectedValueOnce(new Error('saved log command detail'))
            button('[data-view-saved-log]').click()
            await settleAction()

            expect(target.querySelector('[data-saved-log]')).toBeNull()
            expect(target.textContent).toContain(strings.actionFailed)
            expect(target.textContent).not.toContain('saved log command detail')
        })
    })
})
