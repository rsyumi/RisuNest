import { describe, expect, it, vi } from 'vitest'

import {
    NATIVE_MEDIA_IPC_CHUNK_BYTES,
    consumeBoundedNativeMediaOutput,
    invokeWithBoundedNativeMediaInput,
} from './nativeMediaIpc'

describe('native media IPC', () => {
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
        })
    })

    it('uploads large input in exact chunks and always cancels the input handle', async () => {
        const source = new Uint8Array(NATIVE_MEDIA_IPC_CHUNK_BYTES + 2)
        source[NATIVE_MEDIA_IPC_CHUNK_BYTES] = 7
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
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
