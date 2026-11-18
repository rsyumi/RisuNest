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
const domListeners: Array<[string, EventListenerOrEventListenerObject]> = []

function deliverySpy() {
    const delivered: string[][] = []
    const deliver = vi.fn(async (paths: string[]) => {
        delivered.push(paths)
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

    it('delivers each iOS cold and warm open together and discards its staged files afterwards', async () => {
        mocks.ios = true
        let take = 0
        mocks.invoke.mockImplementation(async command => {
            if (command.endsWith('take_opened_files')) return { files: ++take === 1
                ? [{ path: '/staged/first.charx' }, { path: '/staged/broken.risum' }]
                : [{ path: '/staged/last.charx' }] }
        })
        const delivered: string[][] = []
        const discardsBeforeDelivery: number[] = []
        registerOpenedFileListeners(async (paths) => {
            delivered.push(paths)
            discardsBeforeDelivery.push(mocks.invoke.mock.calls.filter(([command]) => command.endsWith('discard_file')).length)
            if (paths.some(path => path.includes('broken'))) throw new Error('synthetic failure')
        })
        window.dispatchEvent(new Event('risunest-ios-opened-files'))
        await vi.waitFor(() => expect(delivered).toHaveLength(2))
        await flush()
        expect(delivered).toEqual([['/staged/first.charx', '/staged/broken.risum'], ['/staged/last.charx']])
        expect(discardsBeforeDelivery).toEqual([0, 2])
        expect(mocks.listen).not.toHaveBeenCalled()
        expect(mocks.invoke.mock.calls.filter(([command]) => command.endsWith('discard_file'))
            .map(([, args]) => args)).toEqual(delivered.flat().map(path => ({ path })))
        expect(mocks.alertError).toHaveBeenCalledOnce()
        expect(String(mocks.alertError.mock.calls[0][0])).toContain('/staged/broken.risum')
    })

    it('acknowledges Android opened files only after their delivery ended, and runs deliveries in order', async () => {
        mocks.android = true
        const acknowledgeOpenedFile = vi.fn((_path: string) => {})
        window.RisuLifecycleBridge = { acknowledgeOpenedFile }
        const started: string[][] = []
        let complete!: () => void
        registerOpenedFileListeners((paths) => {
            started.push(paths)
            return new Promise<void>(resolve => { complete = resolve })
        })
        const first = consumeOpenedFiles(['/cache/opened_files/card.charx', '/cache/opened_files/preset.risup'])
        const second = consumeOpenedFiles(['/cache/opened_files/later.charx'])
        await flush()
        expect(started).toEqual([['/cache/opened_files/card.charx', '/cache/opened_files/preset.risup']])
        expect(acknowledgeOpenedFile).not.toHaveBeenCalled()
        complete()
        await first
        await flush()
        expect(acknowledgeOpenedFile.mock.calls.map(([path]) => path))
            .toEqual(['/cache/opened_files/card.charx', '/cache/opened_files/preset.risup'])
        expect(started).toHaveLength(2)
        complete()
        await second
        expect(acknowledgeOpenedFile).toHaveBeenLastCalledWith('/cache/opened_files/later.charx')
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
