import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    ios: false,
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
vi.mock('src/ts/platform', () => ({
    isTauri: true,
    isTauriAndroid: false,
    get isTauriDesktop() { return !mocks.ios },
    get isTauriIOS() { return mocks.ios },
}))

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

    it('takes the desktop launch arguments and re-takes them on every notification', async () => {
        mocks.invoke
            .mockImplementationOnce(async (_command: string) => ['C:\\cards\\first.charx'])
            .mockImplementationOnce(async (_command: string) => ['C:\\cards\\second.risup'])
        const { imported, importFile } = importerSpy()

        registerOpenedFileListeners(importFile)
        await flush()

        expect(mocks.invoke).toHaveBeenCalledWith(OPENED_FILES_TAKE_COMMAND)
        expect(mocks.listen).toHaveBeenCalledWith(OPENED_FILES_EVENT, expect.any(Function))

        mocks.listeners[0]?.({ payload: { files: ['C:\\cards\\second.risup'] } })
        await flush()

        expect(imported).toEqual(['C:\\cards\\first.charx', 'C:\\cards\\second.risup'])
        expect(mocks.invoke).toHaveBeenCalledTimes(2)
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
