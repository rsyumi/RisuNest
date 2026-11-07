import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(), discard: vi.fn(), character: vi.fn(), module: vi.fn(), pick: vi.fn(),
    controller: new AbortController(), events: [] as string[],
    beginBatch: vi.fn(), beginBatchItem: vi.fn(), recordBatchItem: vi.fn(),
}))
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('./androidSafBridge', async original => ({
    ...await original<typeof import('./androidSafBridge')>(), pickAndroidContentSources: mocks.pick,
}))
vi.mock('./nativeFileJobs', async original => ({
    ...await original<typeof import('./nativeFileJobs')>(), syntheticNativeFileJobStatus: vi.fn(),
}))
vi.mock('./nativeFileJobManager', () => {
    // Holds the background task as the manager does: from the start, or once a picker-first operation protects itself.
    const run = async (operation: Function, options?: { protectAfterPicker?: boolean }) => {
        let held = false
        const hold = () => { if (!held) { held = true; mocks.events.push('hold') } }
        if (!options?.protectAfterPicker) hold()
        return operation({
            signal: mocks.controller.signal, setSource: vi.fn(), onStatus: vi.fn(),
            protect: () => { if (options?.protectAfterPicker) hold() },
            beginBatch: mocks.beginBatch, beginBatchItem: mocks.beginBatchItem, recordBatchItem: mocks.recordBatchItem,
        })
    }
    return {
        runSharedNativeFileOperation: (_kind: string, _key: string, operation: Function, options?: { protectAfterPicker?: boolean }) =>
            run(operation, options),
        runExternalAndroidNativeFileOperation: (_kind: string, operation: Function, options?: { protectAfterPicker?: boolean }) =>
            run(operation, options),
    }
})
vi.mock('../characterCards', () => ({ importCharacterProcess: mocks.character }))
vi.mock('../process/modules', () => ({ importModuleData: mocks.module }))
vi.mock('./database.svelte', () => ({ getDatabase: () => ({ characters: [{ chaId: 'imported-character' }] }) }))
import { NativeFileJobError } from './nativeFileJobs'
import {
    importAndroidContentFromPicker,
    importOpenedAndroidContentSpools,
    importReplayedAndroidContentSpool,
} from './androidContentPicker'

const source = { token: 'synthetic-token', displayName: 'synthetic.json', bytes: 100 }
const encoded = (text: string) => new TextEncoder().encode(text).buffer
beforeEach(() => {
    vi.resetAllMocks()
    mocks.controller = new AbortController()
    mocks.events.length = 0
    mocks.character.mockResolvedValue(0)
    mocks.module.mockResolvedValue(true)
    mocks.pick.mockResolvedValue({ sources: [source], failures: [] })
    Object.assign(window, { RisuSafBridge: { discardSource: mocks.discard } })
})

describe('Android bounded JSON compatibility replay', () => {
    it.each([
        ['CCv2', { spec: 'chara_card_v2', data: {} }, 'character'],
        ['CCv3', { spec: 'chara_card_v3', data: {} }, 'character'],
        ['off-spec', { name: 'Synthetic' }, 'character'],
        ['module', { type: 'risuModule', id: 'synthetic', name: 'Synthetic' }, 'module'],
        ['risu lorebook', { type: 'risu', data: [] }, 'module'],
        ['external lorebook', { entries: {} }, 'module'],
        ['regex module', { type: 'regex', data: [] }, 'module'],
        ['ambiguous preset', { type: 'risu', data: {} }, 'character'],
    ] as const)('uses the same converter for picker and external %s', async (_name, json, destination) => {
        mocks.invoke.mockResolvedValue(encoded(JSON.stringify(json)))
        const external = await importReplayedAndroidContentSpool(source, 'auto')
        const picked = await importAndroidContentFromPicker(destination)
        expect(external).toBe(picked)
        const converter = destination === 'module' ? mocks.module : mocks.character
        const other = destination === 'module' ? mocks.character : mocks.module
        expect(converter).toHaveBeenCalledTimes(2)
        expect(converter.mock.calls[0][0]).toEqual({ ...converter.mock.calls[1][0], parsedJson: { value: json } })
        expect(other).not.toHaveBeenCalled()
        expect(mocks.invoke).toHaveBeenCalledTimes(2)
        expect(mocks.invoke).toHaveBeenCalledWith('native_content_source_metadata', { token: source.token })
        expect(mocks.discard).toHaveBeenCalledTimes(2)
    })
    it.each(['oversized', 'unreadable'])('preserves the bounded native reader rejection for %s JSON', async (reason) => {
        const error = new Error(reason)
        mocks.invoke.mockRejectedValue(error)
        await expect(importReplayedAndroidContentSpool(source, 'auto')).rejects.toBe(error)
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.module).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledExactlyOnceWith(source.token)
    })
    it('hands converters the bytes the native reader returned', async () => {
        const text = '﻿{"name":"Synthetic é"}'
        mocks.invoke.mockResolvedValue(encoded(text))
        await importReplayedAndroidContentSpool(source, 'character')
        expect(mocks.character).toHaveBeenCalledWith({
            name: source.displayName,
            data: new TextEncoder().encode(text),
        })
    })
    it('accepts the bytes as a number array', async () => {
        mocks.invoke.mockResolvedValue([...new TextEncoder().encode('{"entries":{}}')])
        await expect(importReplayedAndroidContentSpool(source, 'auto')).resolves.toBe('module')
        expect(mocks.module).toHaveBeenCalledWith({
            name: source.displayName,
            data: new TextEncoder().encode('{"entries":{}}'),
            parsedJson: { value: { entries: {} } },
        })
    })
    it('rejects bytes that are not UTF-8 before any converter executes', async () => {
        mocks.invoke.mockResolvedValue(Uint8Array.of(0x7b, 0xff, 0x7d).buffer)
        await expect(importReplayedAndroidContentSpool(source, 'auto')).rejects.toBeInstanceOf(TypeError)
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.module).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledExactlyOnceWith(source.token)
    })
    it('rejects malformed UTF-8 even inside otherwise valid auto-detected JSON', async () => {
        const prefix = new TextEncoder().encode('{"name":"')
        const bytes = new Uint8Array([...prefix, 0xe2, 0x82, 0x22, 0x7d])
        mocks.invoke.mockResolvedValue(bytes.buffer)
        await expect(importReplayedAndroidContentSpool(source, 'auto')).rejects.toBeInstanceOf(TypeError)
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.module).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledExactlyOnceWith(source.token)
    })
    it('retains a leading BOM so auto-detected JSON rejects it', async () => {
        mocks.invoke.mockResolvedValue(encoded('\uFEFF{}'))
        await expect(importReplayedAndroidContentSpool(source, 'auto')).rejects.toBeInstanceOf(SyntaxError)
        expect(mocks.discard).toHaveBeenCalledExactlyOnceWith(source.token)
    })
    it('discards malformed JSON exactly once', async () => {
        mocks.invoke.mockResolvedValue(encoded('{'))
        await expect(importReplayedAndroidContentSpool(source, 'auto')).rejects.toBeInstanceOf(SyntaxError)
        expect(mocks.discard).toHaveBeenCalledExactlyOnceWith(source.token)
    })
    it('discards cancelled metadata before any converter executes', async () => {
        mocks.invoke.mockImplementation(async () => {
            mocks.controller.abort()
            return encoded('{}')
        })
        await expect(importReplayedAndroidContentSpool(source, 'auto')).rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.module).not.toHaveBeenCalled()
        expect(mocks.discard).toHaveBeenCalledOnce()
    })
})

describe('Android content picker background hold', () => {
    it('holds the background task only once the picker returns, from the first copied bytes', async () => {
        mocks.invoke.mockResolvedValue(encoded(JSON.stringify({ spec: 'chara_card_v3', data: {} })))
        mocks.character.mockImplementation(async () => {
            mocks.events.push('import')
            return 0
        })
        const picked = (copying: boolean) => async (options: {
            onProgress(progress: { copiedBytes: number; totalBytes: number }): void
        }) => {
            mocks.events.push('picker open')
            if (copying) {
                options.onProgress({ copiedBytes: 1, totalBytes: source.bytes })
                mocks.events.push('copying')
            }
            return { sources: [source], failures: [] }
        }

        mocks.pick.mockImplementationOnce(async () => {
            mocks.events.push('picker open')
            return null
        })
        await expect(importAndroidContentFromPicker('character')).resolves.toBeNull()
        expect(mocks.events).toEqual(['picker open'])

        mocks.events.length = 0
        mocks.pick.mockImplementationOnce(picked(true))
        await importAndroidContentFromPicker('character')
        expect(mocks.events).toEqual(['picker open', 'hold', 'copying', 'import'])

        // A small file arrives already copied.
        mocks.events.length = 0
        mocks.pick.mockImplementationOnce(picked(false))
        await importAndroidContentFromPicker('character')
        expect(mocks.events).toEqual(['picker open', 'hold', 'import'])
    })
})

describe('Android multi-file content picker', () => {
    const first = { token: 'first-token', displayName: 'first.json', bytes: 10 }
    const second = { token: 'second-token', displayName: 'second.json', bytes: 20 }

    it('imports every picked file in one operation and lists the refused and failed files', async () => {
        const error = new Error('synthetic conversion failure')
        mocks.invoke.mockResolvedValue(encoded('{}'))
        mocks.character.mockRejectedValueOnce(error).mockResolvedValueOnce(0)
        mocks.pick.mockResolvedValue({ sources: [first, second], failures: [{ displayName: 'program.exe', code: 'unsupported-source' }] })
        await expect(importAndroidContentFromPicker('character')).resolves.toBe('imported-character')
        expect(mocks.beginBatch).toHaveBeenCalledWith(3)
        expect(mocks.recordBatchItem.mock.calls).toEqual([
            ['program.exe', { error: expect.objectContaining({ code: 'unsupported-source' }) }],
            ['first.json', { error }],
            ['second.json', { imported: true }],
        ])
        expect(mocks.discard.mock.calls).toEqual([[first.token], [second.token]])
    })

    it('lists every refused file when none of the picked files can be imported', async () => {
        mocks.pick.mockResolvedValue({ sources: [], failures: [
            { displayName: 'program.exe', code: 'unsupported-source' },
            { displayName: 'notes.txt', code: 'unsupported-source' },
        ] })
        await expect(importAndroidContentFromPicker('character')).resolves.toBeNull()
        expect(mocks.beginBatch).toHaveBeenCalledWith(2)
        expect(mocks.recordBatchItem.mock.calls).toEqual([
            ['program.exe', { error: expect.objectContaining({ code: 'unsupported-source' }) }],
            ['notes.txt', { error: expect.objectContaining({ code: 'unsupported-source' }) }],
        ])
        expect(mocks.character).not.toHaveBeenCalled()
    })

    it('imports shared JSON and native spools in one operation and lists the files that failed', async () => {
        const card = { token: 'card-token', displayName: 'card.json', bytes: 10 }
        const book = { token: 'book-token', displayName: 'book.lorebook', bytes: 10 }
        const charx = { token: 'charx-token', displayName: 'card.charx', bytes: 30 }
        const photo = { token: 'photo-token', displayName: 'photo.jpg', bytes: 40 }
        const plainJpeg = new NativeFileJobError('destination-required', 'synthetic plain JPEG')
        mocks.invoke.mockImplementation(async (_command: string, args: { token: string }) =>
            encoded(JSON.stringify(args.token === book.token ? { entries: {} } : { spec: 'chara_card_v2', data: {} })))
        const importPrepared = vi.fn(async (source: typeof charx) => {
            if (source === photo) throw plainJpeg
            return 'native-character'
        })

        await expect(importOpenedAndroidContentSpools(
            [card, charx, photo, book],
            [{ displayName: 'broken.png', code: 'source-copy-failed' }],
            importPrepared,
        )).resolves.toBe('module')

        expect(mocks.beginBatch).toHaveBeenCalledWith(5)
        expect(mocks.recordBatchItem.mock.calls).toEqual([
            ['broken.png', { error: expect.objectContaining({ code: 'source-copy-failed' }) }],
            ['card.json', { imported: true }],
            ['card.charx', { imported: true }],
            ['photo.jpg', { error: plainJpeg }],
            ['book.lorebook', { imported: true }],
        ])
        expect(mocks.character).toHaveBeenCalledOnce()
        expect(mocks.module).toHaveBeenCalledOnce()
        expect(importPrepared.mock.calls.map(([source]) => source)).toEqual([charx, photo])
        expect(mocks.discard.mock.calls).toEqual([[card.token], [book.token]])
    })

    it('stops a shared batch on cancellation and discards the spools it did not import', async () => {
        const charx = { token: 'charx-token', displayName: 'card.charx', bytes: 30 }
        const importPrepared = vi.fn(async () => {
            mocks.controller.abort()
            throw new DOMException('cancelled', 'AbortError')
        })
        await expect(importOpenedAndroidContentSpools([charx, first, second], [], importPrepared))
            .rejects.toMatchObject({ name: 'AbortError' })
        expect(importPrepared).toHaveBeenCalledOnce()
        expect(mocks.character).not.toHaveBeenCalled()
        expect(mocks.discard.mock.calls).toEqual([[first.token], [second.token]])
    })

    it('stops a batch on cancellation and discards the files it did not import', async () => {
        mocks.invoke.mockResolvedValue(encoded('{}'))
        mocks.character.mockImplementationOnce(async () => {
            mocks.controller.abort()
            throw new DOMException('cancelled', 'AbortError')
        })
        mocks.pick.mockResolvedValue({ sources: [first, second], failures: [] })
        await expect(importAndroidContentFromPicker('character')).rejects.toMatchObject({ name: 'AbortError' })
        expect(mocks.character).toHaveBeenCalledOnce()
        expect(mocks.discard.mock.calls).toEqual([[first.token], [second.token]])
    })
})
