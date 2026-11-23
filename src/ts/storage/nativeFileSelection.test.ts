import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { Buffer } from 'node:buffer'
import { imageFixture } from '../../../tests/support/imageFixtures'

const mocks = vi.hoisted(() => ({
    os: 'android' as 'android' | 'ios', invoke: vi.fn(), open: vi.fn(), pick: vi.fn(), discard: vi.fn(),
}))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({ runtime: 'tauri', os: mocks.os })))
vi.mock('@tauri-apps/api/core', async original => ({ ...(await original<typeof import('@tauri-apps/api/core')>()), invoke: mocks.invoke }))
vi.mock('@tauri-apps/plugin-fs', async () => (await import('./tauriFs.testSupport')).pluginFsModule({ open: mocks.open }))
import { language } from 'src/lang'
import { selectNativeFileData, selectNativeFiles, withNativeSelectedFile, withNativeSelectedPath, type NativeSelectedFile } from './nativeFileSelection'
import { selectDomFiles } from './fileSelection'
import { isLibraryFileOperationReserved } from './libraryFileOperation'
import { readableFile } from './tauriFs.testSupport'

const metadata = (name = 'synthetic.risup', bytes = 4): NativeSelectedFile => ({
    path: '/owned/selection/source', name, bytes, mimeType: 'application/octet-stream',
})
const handles: ReturnType<typeof readableFile>[] = []

function readable(bytes = new Uint8Array([0, 128, 255, 10]), maxRead = Infinity) {
    const handle = readableFile(bytes, maxRead)
    handles.push(handle)
    return handle
}

beforeEach(() => {
    vi.clearAllMocks()
    mocks.invoke.mockReset()
    mocks.os = 'android'
    handles.length = 0
    window.RisuNativeFileSelection = { pick: mocks.pick, discard: mocks.discard }
    mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata()] })
    mocks.discard.mockResolvedValue(undefined)
    mocks.open.mockImplementation(async () => readable())
})
afterEach(() => { delete window.RisuNativeFileSelection })

describe('native mobile file selection', () => {
    it('reads exact binary bytes across chunks with short native reads and releases the handle and source', async () => {
        const bytes = Uint8Array.from({ length: 600_000 }, (_, index) => index % 256)
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('synthetic.risup', bytes.length)] })
        mocks.open.mockImplementation(async () => readable(bytes, 65_537))
        const [file] = await selectNativeFileData({ accept: '.json,.risup' })
        expect(file.name).toBe('synthetic.risup')
        expect(Buffer.compare(file.data, bytes)).toBe(0)
        expect(mocks.open).toHaveBeenCalledWith('/owned/selection/source', { read: true })
        expect(handles[0].close).toHaveBeenCalledOnce()
        expect(mocks.discard).toHaveBeenCalledWith('/owned/selection/source')
        expect(isLibraryFileOperationReserved()).toBe(false)
    })

    // A fresh module graph has no cached root, as on a fresh launch, and the native side rejects staged work until it arrives.
    async function iOSSelectionBeforeDataRoot() {
        vi.resetModules()
        mocks.os = 'ios'
        let delivered = false
        let deliver!: () => void
        const root = new Promise(resolve => { deliver = () => { delivered = true; resolve({ data: '/owned' }) } })
        mocks.invoke.mockImplementation(async (command, args) => {
            if (command === 'app_paths_roots') return root
            if (command === 'plugin:ios-native|pick_files') {
                if (!delivered) throw new Error('The application data root has not been received yet')
                return { cancelled: false, files: args.multiple ? [metadata('첫째.RISUP'), metadata('second.json')] : [metadata('첫째.RISUP')] }
            }
        })
        return { ...(await import('./nativeFileSelection')), deliver: () => deliver() }
    }
    const iOSPickerCalls = () => mocks.invoke.mock.calls.filter(([command]) => command === 'plugin:ios-native|pick_files')

    it.each([false, true])('opens the iOS picker only after the data root arrives, preserving order and names (multiple %s)', async multiple => {
        const { selectNativeFileData, deliver } = await iOSSelectionBeforeDataRoot()
        const selection = selectNativeFileData({ multiple, accept: '.risup,.json' })
        await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledWith('app_paths_roots'))
        expect(iOSPickerCalls()).toEqual([])
        deliver()
        expect((await selection).map(file => file.name)).toEqual(multiple ? ['첫째.RISUP', 'second.json'] : ['첫째.RISUP'])
        expect(iOSPickerCalls()).toEqual([['plugin:ios-native|pick_files', { multiple }]])
        expect(mocks.invoke.mock.calls.filter(([command]) => command === 'plugin:ios-native|discard_file')).toHaveLength(multiple ? 2 : 1)
    })

    it('opens no iOS picker when cancelled while the data root is pending', async () => {
        const { selectNativeFileData, deliver } = await iOSSelectionBeforeDataRoot()
        const controller = new AbortController()
        const selection = selectNativeFileData({ signal: controller.signal })
        await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledWith('app_paths_roots'))
        controller.abort()
        deliver()
        expect(await selection).toEqual([])
        expect(iOSPickerCalls()).toEqual([])
    })

    it('routes the DOM-compatible selector through native reads on mobile without mounting an input', async () => {
        const files = await selectDomFiles({ accept: '.risup' })
        expect(document.querySelector('input[type=file]')).toBeNull()
        expect(files[0].name).toBe('synthetic.risup')
        expect(new Uint8Array(await files[0].arrayBuffer())).toEqual(new Uint8Array([0, 128, 255, 10]))
    })

    it('filters metadata before reading and still releases rejected selections', async () => {
        await expect(selectNativeFiles({ accept: '.png' })).rejects.toThrow(language.risuNest.importDialog.reasonUnsupportedFormat)
        expect(mocks.open).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('rejects a non-image after a bounded header read and releases the selection', async () => {
        const bytes = new Uint8Array(1024 * 1024)
        bytes.set(new TextEncoder().encode('%PDF-1.7'))
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('renamed.png', bytes.length)] })
        mocks.open.mockImplementation(async () => readable(bytes))
        await expect(selectNativeFileData({ image: true, accept: '.png' })).rejects.toThrow(language.risuNest.imageFormats.unsupported)
        const requested = handles.flatMap(handle => handle.read.mock.calls.map(([buffer]) => buffer.length))
        expect(requested.reduce((total, bytes) => total + bytes, 0)).toBeLessThanOrEqual(64 * 1024)
        expect(handles.every(handle => handle.close.mock.calls.length === 1)).toBe(true)
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it.each(['png', 'avif'] as const)('keeps exact %s bytes despite an unrecognized filename and a later format box', async format => {
        let bytes = imageFixture(format)
        if (format === 'avif') {
            const container = new Uint8Array(70_000 + bytes.length)
            new DataView(container.buffer).setUint32(0, 70_000)
            container.set(new TextEncoder().encode('free'), 4)
            container.set(bytes, 70_000)
            bytes = container
        }
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('portrait.bin', bytes.length)] })
        mocks.open.mockImplementation(async () => readable(bytes))
        const [selected] = await selectNativeFileData({ image: true, accept: '.png' })
        expect(selected.data).toEqual(bytes)
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('rejects a mixed-format batch instead of silently importing only part of it', async () => {
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('first.risup'), metadata('second.png')] })
        await expect(selectNativeFileData({ multiple: true, accept: '.risup' })).rejects.toThrow(language.risuNest.importDialog.reasonUnsupportedFormat)
        expect(mocks.open).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledTimes(2)
    })

    it('reads small slices above 2 GiB without materializing a large backup, then releases the source', async () => {
        const size = 3 * 1024 ** 3
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('large.bin', size)] })
        const readSizes: number[] = []
        mocks.open.mockImplementation(async () => {
            let position = 0
            return {
                seek: vi.fn(async (offset: number) => { position = offset }),
                close: vi.fn(async () => {}),
                read: vi.fn(async (target: Uint8Array) => {
                    readSizes.push(target.length)
                    const count = Math.min(target.length, size - position)
                    for (let i = 0; i < count; i++) target[i] = (position + i) % 251
                    position += count
                    return count || null
                }),
            }
        })
        let retained!: ReturnType<import('./nativeFileSelection').SelectedFileSlices['slice']>
        const result = await withNativeSelectedFile({ accept: '.bin' }, async file => {
            expect(file.size).toBe(size)
            expect(mocks.open).not.toHaveBeenCalled()
            expect(mocks.discard).not.toHaveBeenCalled()
            expect(isLibraryFileOperationReserved()).toBe(true)
            const start = 2 ** 31 + 13
            expect([...new Uint8Array(await file.slice(start, start + 4).arrayBuffer())])
                .toEqual([start % 251, (start + 1) % 251, (start + 2) % 251, (start + 3) % 251])
            expect(new Uint8Array(await file.slice(-1).arrayBuffer())[0]).toBe((size - 1) % 251)
            retained = file.slice(0, 4)
            return 'imported'
        })
        expect(result).toBe('imported')
        expect(readSizes.every(length => length <= 256 * 1024)).toBe(true)
        expect(readSizes.reduce((sum, length) => sum + length, 0)).toBeLessThanOrEqual(2 * 1024 * 1024)
        expect(mocks.discard).toHaveBeenCalledOnce()
        expect(isLibraryFileOperationReserved()).toBe(false)
        await expect(retained.arrayBuffer()).rejects.toThrow('released')
    })

    it('coalesces many entry headers and deferred payloads into a few bounded native reads', async () => {
        const entries = 4_000
        const stride = 283
        const bytes = Uint8Array.from({ length: entries * stride }, (_, index) => index % 251)
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('many.bin', bytes.length)] })
        mocks.open.mockImplementation(async () => readable(bytes))
        await withNativeSelectedFile({}, async file => {
            for (let entry = 0; entry < entries; entry++) {
                const start = entry * stride
                for (const [from, to] of [[start, start + 4], [start + 4, start + 23], [start + 23, start + 27]]) {
                    expect(Buffer.compare(new Uint8Array(await file.slice(from, to).arrayBuffer()), bytes.slice(from, to))).toBe(0)
                }
            }
            for (let entry = 0; entry < entries; entry++) {
                const start = entry * stride + 27
                expect(Buffer.compare(new Uint8Array(await file.slice(start, start + 256).arrayBuffer()), bytes.slice(start, start + 256))).toBe(0)
            }
        })
        expect(mocks.open.mock.calls.length).toBeLessThan(10)
        expect(handles.every(handle => handle.read.mock.calls.every(([target]) => target.length <= 256 * 1024))).toBe(true)
        expect(handles.every(handle => handle.close.mock.calls.length === 1)).toBe(true)
    })

    it('keeps cached slices independent, handles boundary and large reads, and honors cancellation on a cache hit', async () => {
        const bytes = Uint8Array.from({ length: 3 * 1024 * 1024 + 7 }, (_, index) => index % 251)
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('boundary.bin', bytes.length)] })
        mocks.open.mockImplementation(async () => readable(bytes, 65_537))
        const controller = new AbortController()
        await withNativeSelectedFile({ signal: controller.signal }, async file => {
            const first = new Uint8Array(await file.slice(0, 4).arrayBuffer())
            first.fill(255)
            expect(new Uint8Array(await file.slice(0, 4).arrayBuffer())).toEqual(bytes.slice(0, 4))
            expect(mocks.open).toHaveBeenCalledOnce()
            for (const [from, to] of [[1024 * 1024 - 2, 1024 * 1024 + 2], [2, 2 * 1024 * 1024], [12, 8], [-4, bytes.length]]) {
                expect(Buffer.compare(new Uint8Array(await file.slice(from, to).arrayBuffer()), bytes.slice(from, to))).toBe(0)
            }
            controller.abort()
            await expect(file.slice(-2).arrayBuffer()).rejects.toThrow()
        })
        expect(handles.every(handle => handle.close.mock.calls.length === 1)).toBe(true)
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('allows release before a consumer reloads the app and discards only once', async () => {
        await withNativeSelectedFile({}, async (file, release) => {
            await file.slice(0, 4).arrayBuffer()
            await release()
            expect(mocks.discard).toHaveBeenCalledOnce()
            await expect(file.slice(0, 1).arrayBuffer()).rejects.toThrow('released')
        })
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('releases a native sliced source after consumer failure, keeping the consumer error and logging cleanup failure', async () => {
        mocks.discard.mockRejectedValueOnce(new Error('cleanup denied'))
        await expect(withNativeSelectedFile({}, async file => {
            expect(mocks.discard).not.toHaveBeenCalled()
            await file.slice(0, 4).arrayBuffer()
            throw new Error('invalid backup')
        })).rejects.toThrow(/^invalid backup$/)
        expect(handles[0].close).toHaveBeenCalledOnce()
        expect(mocks.discard).toHaveBeenCalledOnce()
        expect(mocks.invoke).toHaveBeenCalledWith('native_log_error', { message: expect.stringContaining('cleanup denied') })
        expect(isLibraryFileOperationReserved()).toBe(false)
    })

    it('keeps the picked copy while a native reader given its path runs, then removes it', async () => {
        expect(await withNativeSelectedPath({}, async file => {
            expect(mocks.discard).not.toHaveBeenCalled()
            return file.path
        })).toBe('/owned/selection/source')
        expect(mocks.discard).toHaveBeenCalledWith('/owned/selection/source')
        await expect(withNativeSelectedPath({}, async () => { throw new Error('unreadable') })).rejects.toThrow('unreadable')
        expect(mocks.discard).toHaveBeenCalledTimes(2)
        // The page reads none of the file itself.
        expect(mocks.open).not.toHaveBeenCalled()
    })

    it('returns cancellation without attempting a read', async () => {
        mocks.pick.mockResolvedValue({ cancelled: true })
        expect(await selectNativeFileData()).toEqual([])
        expect(mocks.open).not.toHaveBeenCalled()
    })

    it('does not turn a picker failure into cancellation', async () => {
        mocks.pick.mockRejectedValue(new Error('SecurityException: document access denied'))
        await expect(selectNativeFileData()).rejects.toThrow('SecurityException: document access denied')
        expect(isLibraryFileOperationReserved()).toBe(false)
    })

    it('rejects concurrent pickers and retains admission until the native picker returns', async () => {
        let finish!: (value: { cancelled: boolean }) => void
        mocks.pick.mockImplementation(() => new Promise(resolve => { finish = resolve }))
        const first = selectNativeFileData()
        await expect(selectNativeFileData()).rejects.toThrow('already open')
        expect(isLibraryFileOperationReserved()).toBe(true)
        finish({ cancelled: true })
        await first
        expect(isLibraryFileOperationReserved()).toBe(false)
    })

    it('discards a result cancelled while the native picker was open', async () => {
        const controller = new AbortController()
        mocks.pick.mockImplementation(async () => {
            controller.abort()
            return { cancelled: false, files: [metadata()] }
        })
        expect(await selectNativeFileData({ signal: controller.signal })).toEqual([])
        expect(mocks.open).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('reports truncated data and closes the handle', async () => {
        mocks.open.mockImplementation(async () => readable(new Uint8Array([0])))
        await expect(selectNativeFileData()).rejects.toThrow('ended before all bytes')
        expect(handles[0].close).toHaveBeenCalledOnce()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })

    it('releases every source after partial multi-file failure, preserving the read error', async () => {
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata('first'), { ...metadata('second'), path: '/owned/second/source' }] })
        mocks.open.mockResolvedValueOnce(readable()).mockRejectedValueOnce(new Error('native read denied'))
        await expect(selectNativeFileData({ multiple: true })).rejects.toThrow('native read denied')
        expect(mocks.discard).toHaveBeenCalledTimes(2)
        expect(handles[0].close).toHaveBeenCalledOnce()
    })

    it('keeps a completed selection when handle close and source cleanup fail, logging both', async () => {
        mocks.pick.mockResolvedValue({ cancelled: false, files: [metadata(), { ...metadata(), path: '/owned/second/source' }] })
        mocks.open.mockImplementation(async () => {
            const handle = readable()
            handle.close.mockRejectedValue(new Error('close denied'))
            return handle
        })
        mocks.discard.mockRejectedValueOnce(new Error('cleanup denied')).mockResolvedValueOnce(undefined)
        const [file] = await selectNativeFileData()
        expect([...file.data]).toEqual([0, 128, 255, 10])
        expect(mocks.discard).toHaveBeenCalledTimes(2)
        expect(mocks.invoke).toHaveBeenCalledWith('native_log_error', { message: expect.stringContaining('close denied') })
        expect(mocks.invoke).toHaveBeenCalledWith('native_log_error', { message: expect.stringContaining('cleanup denied') })
        expect(isLibraryFileOperationReserved()).toBe(false)
    })

    it('releases more files than the Android control transport permits in flight', async () => {
        const { createAndroidControlClient } = await import('../androidNativeControl')
        const port: import('../androidNativeControl').AndroidControlMessagePort = {
            postMessage: message => {
                const { id } = JSON.parse(message)
                queueMicrotask(() => port.onmessage?.({ data: JSON.stringify({ id, result: null }) }))
            },
        }
        const client = createAndroidControlClient(port)
        mocks.pick.mockResolvedValue({ cancelled: false, files: Array.from({ length: 40 }, (_, index) => ({
            ...metadata(), path: `/owned/${index}/source`,
        })) })
        mocks.discard.mockImplementation(path => client.request('files.discard', path))
        await expect(selectNativeFileData({ multiple: true, accept: '.txt' })).rejects.toThrow(language.risuNest.importDialog.reasonUnsupportedFormat)
        expect(mocks.discard).toHaveBeenCalledTimes(40)
        expect(isLibraryFileOperationReserved()).toBe(false)
    })
})
