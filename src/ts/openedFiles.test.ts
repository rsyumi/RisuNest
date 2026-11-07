import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    ios: false,
    android: false,
    readFile: vi.fn(async (_path: string) => new Uint8Array([1])),
    invoke: vi.fn<(command: string, args?: unknown) => Promise<unknown>>(async () => []),
    listen: vi.fn(async (_event: string, _handler: (payload: unknown) => void) => () => {}),
    alertError: vi.fn(),
    listeners: [] as Array<(payload: unknown) => void>,
}))

vi.mock('@tauri-apps/plugin-fs', () => ({ readFile: mocks.readFile }))
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

function importerSpy() {
    const imported: string[] = []
    const importFile = vi.fn(async (name: string, _data: Uint8Array) => {
        imported.push(name)
    })
    return { imported, importFile }
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
        mocks.readFile.mockClear()
        mocks.readFile.mockImplementation(async (_path: string) => new Uint8Array([1]))
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

    it('drains iOS cold and warm opens in order and discards staged sources after import', async () => {
        mocks.ios = true
        let take = 0
        mocks.invoke.mockImplementation(async command => {
            if (command.endsWith('take_opened_files')) return { files: ++take === 1
                ? [{ path: '/staged/first.charx' }, { path: '/staged/broken.risum' }]
                : [{ path: '/staged/last.charx' }] }
        })
        const order: string[] = []
        const importPath = vi.fn(async (path: string) => {
            order.push(path)
            if (path.includes('broken')) throw new Error('synthetic failure')
            return true
        })
        registerOpenedFileListeners(vi.fn(), importPath)
        window.dispatchEvent(new Event('risunest-ios-opened-files'))
        await vi.waitFor(() => expect(order).toHaveLength(3))
        await flush()
        expect(order).toEqual(['/staged/first.charx', '/staged/broken.risum', '/staged/last.charx'])
        expect(mocks.readFile).not.toHaveBeenCalled()
        expect(mocks.listen).not.toHaveBeenCalled()
        expect(mocks.invoke.mock.calls.filter(([command]) => command.endsWith('discard_file'))
            .map(([, args]) => args)).toEqual(order.map(path => ({ path })))
        expect(mocks.alertError).toHaveBeenCalledOnce()
    })

    it('acknowledges an Android queued file only after its importer completes', async () => {
        mocks.android = true
        const acknowledgeOpenedFile = vi.fn(async () => {})
        window.RisuLifecycleBridge = { acknowledgeOpenedFile }
        let complete!: () => void
        const importFile = vi.fn(() => new Promise<void>(resolve => { complete = resolve }))
        ;(window as Window & { tauriOpenedFiles?: string[] }).tauriOpenedFiles = ['/cache/opened_files/card.charx']
        registerOpenedFileListeners(importFile)
        await flush()
        expect(importFile).toHaveBeenCalledOnce()
        expect(acknowledgeOpenedFile).not.toHaveBeenCalled()
        complete()
        await flush()
        expect(acknowledgeOpenedFile).toHaveBeenCalledWith('/cache/opened_files/card.charx')
    })

    it('acknowledges every Android delivery once its outcome has been reported', async () => {
        mocks.android = true
        const acknowledgeOpenedFile = vi.fn((_path: string) => {})
        window.RisuLifecycleBridge = { acknowledgeOpenedFile }
        const importFile = vi.fn(async (path: string): Promise<void | 'failed'> => {
            if (path === 'reported.charx') return 'failed'
            throw new Error('synthetic import failure')
        })
        const importPath = vi.fn(async (path: string): Promise<boolean | 'failed'> =>
            path === 'failed.charx' ? 'failed' : path === 'complete.charx')
        registerOpenedFileListeners(importFile, importPath)
        await consumeOpenedFiles(['failed.charx', 'legacy.charx', 'reported.charx', 'complete.charx'])
        expect(mocks.readFile.mock.calls.map(([path]) => path)).toEqual(['legacy.charx', 'reported.charx'])
        expect(mocks.alertError).toHaveBeenCalledOnce()
        expect(acknowledgeOpenedFile.mock.calls.map(([path]) => path))
            .toEqual(['failed.charx', 'legacy.charx', 'reported.charx', 'complete.charx'])
    })

    it('reads and imports every file in order', async () => {
        const { imported, importFile } = importerSpy()
        registerOpenedFileListeners(importFile)

        await consumeOpenedFiles(['a.charx', 'b.risup'])

        expect(mocks.readFile.mock.calls.map((call) => call[0])).toEqual(['a.charx', 'b.risup'])
        expect(imported).toEqual(['a.charx', 'b.risup'])
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('reports a failing file and keeps importing the rest', async () => {
        const { imported, importFile } = importerSpy()
        mocks.readFile.mockImplementation(async (path: string) => {
            if (path === 'broken.risup') {
                throw new Error('unreadable')
            }
            return new Uint8Array([1])
        })
        registerOpenedFileListeners(importFile)

        await consumeOpenedFiles(['broken.risup', 'good.risum'])

        expect(imported).toEqual(['good.risum'])
        expect(mocks.alertError).toHaveBeenCalledTimes(1)
        expect(String(mocks.alertError.mock.calls[0][0])).toContain('broken.risup')
    })

    it('drains the Android cold start injection once', async () => {
        const holder = window as Window & { tauriOpenedFiles?: unknown }
        holder.tauriOpenedFiles = ['/cache/opened_files/1-0-card.charx']
        const { imported, importFile } = importerSpy()

        registerOpenedFileListeners(importFile)
        await flush()

        expect(imported).toEqual(['/cache/opened_files/1-0-card.charx'])
        expect(holder.tauriOpenedFiles).toBeUndefined()
    })

    it('imports the files an Android warm start dispatches', async () => {
        const { imported, importFile } = importerSpy()
        registerOpenedFileListeners(importFile)
        await flush()

        window.dispatchEvent(
            new CustomEvent(OPENED_FILES_EVENT, {
                detail: { files: ['/cache/opened_files/2-0-preset.risup'] },
            }),
        )
        await flush()

        expect(imported).toEqual(['/cache/opened_files/2-0-preset.risup'])
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
            const { imported, importFile } = importerSpy()
            const importBatch = vi.fn(async (_paths: string[]) => {})

            registerOpenedFileListeners(importFile, undefined, { accepts: path => path.endsWith('.charx'), importBatch })
            await flush()

            expect(mocks.invoke).toHaveBeenCalledWith(OPENED_FILES_TAKE_COMMAND)
            expect(mocks.listen).toHaveBeenCalledWith(OPENED_FILES_EVENT, expect.any(Function))
            expect(imported).toEqual(['C:\\cards\\first.charx'])

            // Explorer starts one process per selected file, so one selection arrives as several notifications.
            for (const file of ['C:\\cards\\second.charx', 'C:\\cards\\third.charx']) {
                pending.push(file)
                mocks.listeners[0]?.({ payload: { files: [file] } })
                await vi.advanceTimersByTimeAsync(api.DESKTOP_OPENED_FILES_SETTLE_MS - 1)
            }
            expect(mocks.invoke).toHaveBeenCalledTimes(1)
            await vi.advanceTimersByTimeAsync(1)
            await flush()

            expect(mocks.invoke).toHaveBeenCalledTimes(2)
            expect(importBatch).toHaveBeenCalledExactlyOnceWith(['C:\\cards\\second.charx', 'C:\\cards\\third.charx'])
            expect(imported).toEqual(['C:\\cards\\first.charx'])
        } finally {
            vi.useRealTimers()
        }
    })

    it('imports two or more opened characters or modules in one batch after the other files', async () => {
        const order: string[] = []
        const importFile = vi.fn(async (name: string) => { order.push(`file:${name}`) })
        const importPath = vi.fn(async (path: string) => {
            order.push(`path:${path}`)
            return path.endsWith('.risunest')
        })
        const importBatch = vi.fn(async (paths: string[]) => { order.push(`batch:${paths.join(',')}`) })
        registerOpenedFileListeners(importFile, importPath, {
            accepts: path => /\.(charx|risum)$/.test(path),
            importBatch,
        })

        await consumeOpenedFiles(['a.charx', 'backup.risunest', 'preset.risup', 'b.risum'])
        // A single character keeps its own import.
        await consumeOpenedFiles(['c.charx', 'other.risup'])

        expect(order).toEqual([
            'path:backup.risunest',
            'path:preset.risup', 'file:preset.risup',
            'batch:a.charx,b.risum',
            'path:c.charx', 'file:c.charx',
            'path:other.risup', 'file:other.risup',
        ])
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('discards each staged iOS file of a batch once, also when the batch throws', async () => {
        mocks.ios = true
        mocks.invoke.mockImplementation(async command =>
            command.endsWith('take_opened_files') ? { files: [] } : undefined)
        const importBatch = vi.fn(async (_paths: string[]) => { throw new Error('synthetic batch failure') })
        registerOpenedFileListeners(vi.fn(), vi.fn(async () => true), { accepts: path => path.endsWith('.charx'), importBatch })
        await flush()

        await consumeOpenedFiles(['/staged/first.charx', '/staged/second.charx'])

        expect(importBatch).toHaveBeenCalledExactlyOnceWith(['/staged/first.charx', '/staged/second.charx'])
        expect(mocks.invoke.mock.calls.filter(([command]) => command.endsWith('discard_file'))
            .map(([, args]) => args)).toEqual([{ path: '/staged/first.charx' }, { path: '/staged/second.charx' }])
        expect(mocks.alertError).toHaveBeenCalledOnce()
    })

    it('imports files queued while the desktop subscription is still being established', async () => {
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
        const { imported, importFile } = importerSpy()

        registerOpenedFileListeners(importFile)
        await flush()
        // A second process queues a file before native event delivery is subscribed.
        pending.push('C:\\cards\\during-startup.risup')
        subscriptionReady(() => {})
        await flush()

        expect(imported).toEqual(['C:\\cards\\during-startup.risup'])
        expect(pending).toEqual([])
    })
})
