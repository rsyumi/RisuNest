import { describe, expect, it, vi } from 'vitest'
import type { NativeByteInvoke } from './nativeAssetByteTransport'

import {
    NATIVE_MEDIA_IPC_CHUNK_BYTES,
    consumeBoundedNativeMediaOutput,
    invokeWithBoundedNativeMediaInput,
} from './nativeMediaIpc'
import { rendererDocument } from './rendererDocument'

describe('native media IPC', () => {
    it('does not open an upload after a failed capability probe and can retry the probe', async () => {
        const commands: string[] = []
        const invoke: NativeByteInvoke = async command => {
            commands.push(command)
            throw new Error('probe unavailable')
        }
        const command = { data: new Uint8Array(128 * 1024), directCommand: 'direct', streamedFinishCommand: 'finish', args: {} }
        await expect(invokeWithBoundedNativeMediaInput(invoke, command)).rejects.toThrow('probe unavailable')
        await expect(invokeWithBoundedNativeMediaInput(invoke, command)).rejects.toThrow('probe unavailable')
        expect(commands).toEqual(['native_asset_bytes_probe', 'native_asset_bytes_probe'])
    })

    it.each([false, true])('transfers owned bytes with a cached binary capability (%s)', async raw => {
        const source = Uint8Array.from({ length: 256 * 1024 + 19 }, (_, i) => i % 256)
        const original = source.slice()
        let probes = 0
        let received = new Uint8Array(source.length)
        let offset = 0
        const invoke: NativeByteInvoke = async (command, args, options) => {
            if (command === 'native_asset_bytes_probe') { probes++; return raw }
            const control = args as Record<string, unknown>
            if (command === 'native_media_inlay_input_open') {
                received = new Uint8Array(source.length)
                offset = 0
                return { capacity: raw ? 256 * 1024 : 64 * 1024 }
            }
            if (command.startsWith('native_media_inlay_input_chunk')) {
                const headers = new Headers(options?.headers)
                const start = raw ? Number(headers.get('x-risunest-offset')) : Number(control.offset)
                const part = raw ? args as Uint8Array : Uint8Array.from(control.data as number[])
                expect(start).toBe(offset)
                expect(part.length).toBeLessThanOrEqual(raw ? 256 * 1024 : 64 * 1024)
                if (raw) expect(headers.get('x-risunest-upload-id')).toMatch(/^[0-9a-f-]{36}$/)
                received.set(part, start)
                offset += part.length
                return offset
            }
            if (command === 'finish') return received
        }
        const run = (data: Uint8Array) => invokeWithBoundedNativeMediaInput(invoke, {
            data, directCommand: 'direct', streamedFinishCommand: 'finish', args: {},
        })
        const first = run(source)
        source.fill(0)
        await expect(first).resolves.toEqual(original)
        await expect(run(original)).resolves.toEqual(original)
        expect(probes).toBe(1)
    })

    it.each(['raw-body-unavailable', 'timeout', 'malformed'] as const)('never replays a raw part after %s', async failure => {
        let probes = 0
        let rawParts = 0
        let legacyParts = 0
        let cancelled = 0
        let failed = false
        const invoke: NativeByteInvoke = async (command, args) => {
            if (command === 'native_asset_bytes_probe') { probes++; return true }
            if (command === 'native_media_inlay_input_open') {
                return { capacity: (args as Record<string, unknown>).binary ? 256 * 1024 : 64 * 1024 }
            }
            if (command.endsWith('_chunk_raw')) {
                rawParts++
                // The first attempt may have been accepted before Tauri's resend failed.
                failed = true
                if (failure === 'malformed') return 'unknown acknowledgement'
                throw { code: failure }
            }
            if (command.endsWith('_chunk')) {
                legacyParts++
                const control = args as Record<string, unknown>
                return Number(control.offset) + (control.data as number[]).length
            }
            if (command.endsWith('_cancel')) cancelled++
            if (command === 'finish') return true
        }
        const run = () => invokeWithBoundedNativeMediaInput(invoke, {
            data: new Uint8Array(128 * 1024), directCommand: 'direct', streamedFinishCommand: 'finish', args: {},
        })
        await expect(run()).rejects.toBeDefined()
        expect(failed).toBe(true)
        expect(rawParts).toBe(1)
        expect(legacyParts).toBe(0)
        expect(cancelled).toBe(1)
        if (failure === 'raw-body-unavailable') {
            await expect(run()).resolves.toBe(true)
            expect(legacyParts).toBe(2)
            expect(rawParts).toBe(1)
        } else {
            await expect(run()).rejects.toBeDefined()
            expect(rawParts).toBe(2)
            expect(legacyParts).toBe(0)
        }
        expect(probes).toBe(1)
        expect(cancelled).toBe(2)
    })

    it('keeps small input on the bounded direct command and snapshots its bytes', async () => {
        let release!: () => void
        const pending = new Promise<void>((resolve) => { release = resolve })
        const invoke = vi.fn(async () => {
            await pending
            return { ok: true }
        })
        const source = Uint8Array.of(1, 2, 3)

        const result = invokeWithBoundedNativeMediaInput<{ ok: boolean }>(invoke, {
            data: source,
            directCommand: 'direct',
            streamedFinishCommand: 'finish',
            args: { id: 'image' },
        })
        source[0] = 9
        release()

        await expect(result).resolves.toEqual({ ok: true })
        expect(invoke).toHaveBeenCalledWith('direct', {
            id: 'image',
            data: [1, 2, 3],
            document: rendererDocument(),
        })
    })

    it('uploads large input in exact chunks and always cancels the input handle', async () => {
        const source = new Uint8Array(NATIVE_MEDIA_IPC_CHUNK_BYTES + 2)
        source[NATIVE_MEDIA_IPC_CHUNK_BYTES] = 7
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'native_asset_bytes_probe') return false
            calls.push([command, args])
            if (command === 'native_media_inlay_input_open') {
                return { capacity: NATIVE_MEDIA_IPC_CHUNK_BYTES }
            }
            if (command === 'native_media_inlay_input_chunk') {
                return (args!.offset as number) + (args!.data as number[]).length
            }
            if (command === 'finish') return { ok: true }
            return undefined
        })

        await expect(invokeWithBoundedNativeMediaInput(invoke, {
            data: source,
            directCommand: 'direct',
            streamedFinishCommand: 'finish',
            args: { id: 'image' },
        })).resolves.toEqual({ ok: true })

        expect(calls.map(([command]) => command)).toEqual([
            'native_media_inlay_input_open',
            'native_media_inlay_input_chunk',
            'native_media_inlay_input_chunk',
            'finish',
            'native_media_inlay_input_cancel',
        ])
        expect(calls[0][1]).toMatchObject({ document: rendererDocument() })
        expect((calls[1][1]!.data as number[]).length).toBe(NATIVE_MEDIA_IPC_CHUNK_BYTES)
        expect(calls[2][1]).toMatchObject({
            offset: NATIVE_MEDIA_IPC_CHUNK_BYTES,
            data: [7, 0],
        })
        expect(calls[3][1]).toMatchObject({ id: 'image', uploadId: expect.any(String) })
    })

    it('keeps an input failure primary when cleanup also fails', async () => {
        const primary = new Error('chunk failed')
        const cleanup = new Error('cancel failed')
        const report = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        const invoke = vi.fn(async (command: string) => {
            if (command === 'native_asset_bytes_probe') return false
            if (command === 'native_media_inlay_input_open') {
                return { capacity: NATIVE_MEDIA_IPC_CHUNK_BYTES }
            }
            if (command === 'native_media_inlay_input_chunk') throw primary
            if (command === 'native_media_inlay_input_cancel') throw cleanup
            return undefined
        })

        await expect(invokeWithBoundedNativeMediaInput(invoke, {
            data: new Uint8Array(NATIVE_MEDIA_IPC_CHUNK_BYTES + 1),
            directCommand: 'direct',
            streamedFinishCommand: 'finish',
            args: {},
        })).rejects.toBe(primary)
        expect(report).toHaveBeenCalledWith(
            'Native media input transfer cleanup failed',
            cleanup,
        )
        report.mockRestore()
    })

    it('assembles streamed output in bounded exact ranges and cancels its handle', async () => {
        const outputSize = NATIVE_MEDIA_IPC_CHUNK_BYTES + 2
        const source = Uint8Array.from({ length: outputSize }, (_, index) => index % 251)
        const reads: Array<Record<string, unknown>> = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'native_media_inlay_output_read') {
                reads.push(args!)
                return Array.from(source.subarray(
                    args!.start as number,
                    args!.endExclusive as number,
                ))
            }
            return undefined
        })

        const result = await consumeBoundedNativeMediaOutput(
            invoke,
            {
                data: null,
                outputId: 'output-id',
                outputSize,
                metadata: { size: outputSize },
            },
            (metadata) => metadata as { size: number },
        )

        expect(result.data).toEqual(source)
        expect(reads).toEqual([
            { outputId: 'output-id', start: 0, endExclusive: NATIVE_MEDIA_IPC_CHUNK_BYTES },
            { outputId: 'output-id', start: NATIVE_MEDIA_IPC_CHUNK_BYTES, endExclusive: outputSize },
        ])
        expect(invoke).toHaveBeenLastCalledWith('native_media_inlay_output_cancel', {
            outputId: 'output-id',
        })
    })

    it.each([
        ['bytes that arrive with it', { data: [1, 2, 3] }, Uint8Array.of(1, 2, 3)],
        ['an output transfer', { outputId: 'output-id' }, Uint8Array.of(1, 2, 3)],
        ['an empty output id', { outputId: '' }, Uint8Array.of(1, 2, 3)],
        ['a non-string output id', { outputId: 7 }, Uint8Array.of(1, 2, 3)],
        ['a different size', {}, Uint8Array.of(1, 2)],
        ['no source to keep', {}, undefined],
    ] as const)('rejects a kept source with %s', async (_, fields, source) => {
        const invoke = vi.fn(async () => undefined)

        await expect(consumeBoundedNativeMediaOutput(
            invoke,
            { data: null, outputId: null, outputSize: 3, sourceKept: true, metadata: {}, ...fields },
            (metadata) => metadata,
            source,
        )).rejects.toThrow('invalid kept source')
        expect(invoke).not.toHaveBeenCalledWith('native_media_inlay_output_read', expect.anything())
    })

    it('keeps output validation failure primary and reports cancel failure', async () => {
        const primary = new Error('metadata rejected')
        const cleanup = new Error('output cancel failed')
        const report = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        const invoke = vi.fn(async (command: string) => {
            if (command === 'native_media_inlay_output_cancel') throw cleanup
            return undefined
        })

        await expect(consumeBoundedNativeMediaOutput(
            invoke,
            {
                data: null,
                outputId: 'output-id',
                outputSize: NATIVE_MEDIA_IPC_CHUNK_BYTES + 1,
                metadata: {},
            },
            () => { throw primary },
        )).rejects.toBe(primary)
        expect(report).toHaveBeenCalledWith(
            'Native media output transfer cleanup failed',
            cleanup,
        )
        report.mockRestore()
    })
})
