import { describe, expect, test, vi } from 'vitest'

vi.mock('./globalApi.svelte', () => {
    class AppendableBuffer {
        chunks: Uint8Array[] = []
        append(data: Uint8Array) { this.chunks.push(data) }
    }
    class VirtualWriter {
        buf = new AppendableBuffer()
        write(data: Uint8Array) { this.buf.append(data) }
        close() { /* no-op */ }
    }
    return { AppendableBuffer, VirtualWriter }
})

vi.mock('./util', () => ({
    blobToUint8Array: async (blob: Blob) => new Uint8Array(await blob.arrayBuffer()),
}))

import { PngChunk } from './pngChunk'

describe('PngChunk.streamWriter.end', () => {
    test('resolves only after the writer close finishes flushing', async () => {
        let closed = false
        let releaseClose: () => void = () => undefined
        const writer = {
            write: vi.fn(async () => undefined),
            close: vi.fn(() => new Promise<void>((resolve) => {
                releaseClose = () => {
                    closed = true
                    resolve()
                }
            })),
        }

        const stream = new PngChunk.streamWriter(new Uint8Array(0), writer as any)
        let ended = false
        const pending = stream.end().then(() => {
            ended = true
        })

        await vi.waitFor(() => expect(writer.close).toHaveBeenCalledOnce())
        expect(ended).toBe(false)

        releaseClose()
        await pending
        expect(closed).toBe(true)
        expect(ended).toBe(true)
    })

    test('propagates a failed close to the caller', async () => {
        const sentinel = new Error('tail write failed')
        const writer = {
            write: vi.fn(async () => undefined),
            close: vi.fn(async () => { throw sentinel }),
        }

        const stream = new PngChunk.streamWriter(new Uint8Array(0), writer as any)
        await expect(stream.end()).rejects.toBe(sentinel)
    })
})
