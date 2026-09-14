import { Buffer } from 'buffer'
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

describe('PngChunk stream writer round trip', () => {
    test('preserves card metadata and embedded asset bytes through its reader', async () => {
        const png = Uint8Array.from([
            137, 80, 78, 71, 13, 10, 26, 10,
            0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ])
        const output: Uint8Array[] = []
        const writer = {
            write: vi.fn(async (data: Uint8Array) => output.push(data.slice())),
            close: vi.fn(async () => undefined),
        }
        const metadata = '{"name":"Synthetic card"}'
        const asset = Uint8Array.of(0, 255, 7, 128)
        const stream = new PngChunk.streamWriter(png, writer as any)

        await stream.init()
        await stream.write('ccv3', Buffer.from(metadata).toString('base64'))
        await stream.write('chara-ext-asset_1', Buffer.from(asset).toString('base64'))
        await stream.end()

        const chunks = PngChunk.read(Buffer.concat(output), [
            'ccv3',
            'chara-ext-asset_1',
        ], { checkCrc: true })
        expect(Buffer.from(chunks.ccv3, 'base64').toString()).toBe(metadata)
        expect(Uint8Array.from(Buffer.from(chunks['chara-ext-asset_1'], 'base64')))
            .toEqual(asset)
    })
})
