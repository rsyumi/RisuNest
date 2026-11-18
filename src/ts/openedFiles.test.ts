import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    ios: false,
    android: false,
    invoke: vi.fn<(command: string, args?: unknown) => Promise<unknown>>(async () => []),
    listen: vi.fn(async (_event: string, _handler: (payload: unknown) => void) => () => {}),
    alertError: vi.fn(),
    listeners: [] as Array<(payload: unknown) => void>,
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('@tauri-apps/api/event', () => ({
    listen: (event: string, handler: (payload: unknown) => void) => {
        mocks.listeners.push(handler)
        return mocks.listen(event, handler)
    },
}))
vi.mock('./alert', () => ({ alertError: mocks.alertError }))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule(() => ({
    runtime: 'tauri', os: mocks.ios ? 'ios' : mocks.android ? 'android' : 'windows',
})))

let api: typeof import('./openedFiles')
let consumeOpenedFiles: typeof api.consumeOpenedFiles
let registerOpenedFileListeners: typeof api.registerOpenedFileListeners
let OPENED_FILES_EVENT: typeof api.OPENED_FILES_EVENT
let OPENED_FILES_TAKE_COMMAND: typeof api.OPENED_FILES_TAKE_COMMAND
type OpenedFile = import('./openedFiles').OpenedFile
const domListeners: Array<[string, EventListenerOrEventListenerObject]> = []

function deliverySpy() {
    const delivered: OpenedFile[][] = []
    const deliver = vi.fn(async (files: OpenedFile[]) => {
        delivered.push(files)
    })
    return { delivered, deliver }
}

async function flush() {
    for (let round = 0; round < 8; round += 1) {
        await Promise.resolve()
    }
}

describe('opened file delivery', () => {
    beforeEach(async () => {
        mocks.ios = false
        mocks.android = false
        delete window.RisuLifecycleBridge
        vi.resetModules()
        api = await import('./openedFiles')
        ;({ consumeOpenedFiles, registerOpenedFileListeners, OPENED_FILES_EVENT, OPENED_FILES_TAKE_COMMAND } = api)
        const addEventListener = window.addEventListener.bind(window)
        vi.spyOn(window, 'addEventListener').mockImplementation((type, listener, options) => {
            if ([OPENED_FILES_EVENT, 'risunest-ios-opened-files'].includes(type)) domListeners.push([type, listener])
            addEventListener(type, listener, options)
        })
        mocks.invoke.mockClear()
        mocks.invoke.mockImplementation(async (_command: string) => [])
        mocks.listen.mockClear()
        mocks.alertError.mockClear()
        mocks.listeners.length = 0
        delete (window as Window & { tauriOpenedFiles?: unknown }).tauriOpenedFiles
    })

    afterEach(async () => {
        await flush()
        for (const [type, listener] of domListeners.splice(0)) window.removeEventListener(type, listener)
        vi.restoreAllMocks()
    })

    it('delivers each iOS cold and warm open with the files it could not prepare and discards its staged files afterwards', async () => {
        mocks.ios = true
        let take = 0
        const unprepared = { name: 'unprepared.risunest', code: 'source-copy-failed' }
        mocks.invoke.mockImplementation(async command => {
            if (command.endsWith('take_opened_files')) return { files: ++take === 1
                ? [{ path: '/staged/first.charx' }, unprepared, { path: '/staged/broken.risum' }]
                : [unprepared] }
        })
        const delivered: OpenedFile[][] = []
        registerOpenedFileListeners(async (files) => {
            delivered.push(files)
            expect(mocks.invoke.mock.calls.filter(([command]) => command.endsWith('discard_file'))).toEqual([])
            if (files.includes('/staged/broken.risum')) throw new Error('synthetic failure')
        })
        window.dispatchEvent(new Event('risunest-ios-opened-files'))
        await vi.waitFor(() => expect(delivered).toHaveLength(2))
        await flush()
        // A file that could not be prepared stays in its opening, also when it is the only one.
        expect(delivered).toEqual([['/staged/first.charx', unprepared, '/staged/broken.risum'], [unprepared]])
        expect(mocks.listen).not.toHaveBeenCalled()
        expect(mocks.invoke.mock.calls.filter(([command]) => command.endsWith('discard_file'))
            .map(([, args]) => args)).toEqual([{ path: '/staged/first.charx' }, { path: '/staged/broken.risum' }])
        expect(mocks.alertError).toHaveBeenCalledOnce()
        expect(String(mocks.alertError.mock.calls[0][0])).toContain('/staged/broken.risum')
    })

    it('hands each Android opening over as it arrives and acknowledges its files only after its own delivery ended', async () => {
        mocks.android = true
        const acknowledgeOpenedFile = vi.fn((_path: string) => {})
        window.RisuLifecycleBridge = { acknowledgeOpenedFile }
        const started: OpenedFile[][] = []
        const completions: Array<() => void> = []
        registerOpenedFileListeners((files) => {
            started.push(files)
            return new Promise<void>(resolve => { completions.push(resolve) })
        })
        const uncopied = { name: 'uncopied.risup', code: 'source-copy-failed' }
        const first = consumeOpenedFiles(['/cache/opened_files/card.charx', uncopied])
        const second = consumeOpenedFiles(['/cache/opened_files/later.charx'])
        await flush()
        // The intake queues openings with dropped files in arrival order, so the second does not wait here.
        expect(started).toEqual([['/cache/opened_files/card.charx', uncopied], ['/cache/opened_files/later.charx']])
        expect(acknowledgeOpenedFile).not.toHaveBeenCalled()
        completions[1]()
        await second
        expect(acknowledgeOpenedFile.mock.calls.map(([path]) => path)).toEqual(['/cache/opened_files/later.charx'])
        completions[0]()
        await first
        expect(acknowledgeOpenedFile.mock.calls.map(([path]) => path))
            .toEqual(['/cache/opened_files/later.charx', '/cache/opened_files/card.charx'])
    })

    it('drains the Android cold start injection once and delivers a warm start with its files', async () => {
        const holder = window as Window & { tauriOpenedFiles?: unknown }
        holder.tauriOpenedFiles = ['/cache/opened_files/1-0-card.charx', '/cache/opened_files/1-1-preset.risup']
        const { delivered, deliver } = deliverySpy()

        registerOpenedFileListeners(deliver)
        await flush()
        expect(holder.tauriOpenedFiles).toBeUndefined()

        window.dispatchEvent(new CustomEvent(OPENED_FILES_EVENT, {
            detail: { files: ['/cache/opened_files/2-0-preset.risup'] },
        }))
        await flush()

        expect(delivered).toEqual([
            ['/cache/opened_files/1-0-card.charx', '/cache/opened_files/1-1-preset.risup'],
            ['/cache/opened_files/2-0-preset.risup'],
        ])
    })

    it('takes the desktop launch arguments and re-takes them once the notifications settle', async () => {
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        try {
            let pending = ['C:\\cards\\first.charx']
            mocks.invoke.mockImplementation(async () => {
                const files = pending
                pending = []
                return files
            })
            const { delivered, deliver } = deliverySpy()

            registerOpenedFileListeners(deliver)
            await flush()

            expect(mocks.invoke).toHaveBeenCalledWith(OPENED_FILES_TAKE_COMMAND)
            expect(mocks.listen).toHaveBeenCalledWith(OPENED_FILES_EVENT, expect.any(Function))
            expect(delivered).toEqual([['C:\\cards\\first.charx']])

            // Explorer starts one process per selected file, so one selection arrives as several notifications.
            for (const file of ['C:\\cards\\second.charx', 'C:\\cards\\backup.risunest']) {
                pending.push(file)
                mocks.listeners[0]?.({ payload: { files: [file] } })
                await vi.advanceTimersByTimeAsync(api.DESKTOP_OPENED_FILES_SETTLE_MS - 1)
            }
            expect(mocks.invoke).toHaveBeenCalledTimes(1)
            await vi.advanceTimersByTimeAsync(1)
            await flush()

            expect(mocks.invoke).toHaveBeenCalledTimes(2)
            expect(delivered).toEqual([
                ['C:\\cards\\first.charx'],
                ['C:\\cards\\second.charx', 'C:\\cards\\backup.risunest'],
            ])
            expect(mocks.alertError).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })

    it('delivers files queued while the desktop subscription is still being established', async () => {
        let subscriptionReady!: (unlisten: () => void) => void
        mocks.listen.mockImplementationOnce(
            () =>
                new Promise((resolve) => {
                    subscriptionReady = resolve
                }),
        )
        let pending: string[] = []
        mocks.invoke.mockImplementation(async () => {
            const files = pending
            pending = []
            return files
        })
        const { delivered, deliver } = deliverySpy()

        registerOpenedFileListeners(deliver)
        await flush()
        // A second process queues a file before native event delivery is subscribed.
        pending.push('C:\\cards\\during-startup.risup')
        subscriptionReady(() => {})
        await flush()

        expect(delivered).toEqual([['C:\\cards\\during-startup.risup']])
        expect(pending).toEqual([])
    })
})
