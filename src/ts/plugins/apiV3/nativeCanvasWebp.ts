import { invoke } from '@tauri-apps/api/core'
import { consumeBoundedNativeMediaOutput, invokeWithBoundedNativeMediaInput } from '../../storage/nativeMediaIpc'

// Native media transfers are shared with Inlay encoding and refuse requests beyond a few at once.
let queue: Promise<unknown> = Promise.resolve()

export async function encodeNativeCanvasWebp(
    data: unknown, width: unknown, height: unknown, quality: unknown, cancelled: () => boolean = () => false,
): Promise<Uint8Array> {
    if (!(data instanceof Uint8Array)
        || !Number.isInteger(width) || !Number.isInteger(height)
        || (width as number) <= 0 || (height as number) <= 0
        || (width as number) > 16383 || (height as number) > 16383
        || (width as number) * (height as number) > 16 * 1024 * 1024
        || data.byteLength !== (width as number) * (height as number) * 4
        || typeof quality !== 'number' || !Number.isFinite(quality) || quality < 0 || quality > 1) {
        throw new TypeError('Invalid plugin WebP canvas input')
    }
    const encoded = queue.then(() => {
        if (cancelled()) throw new Error('Plugin WebP canvas request was cancelled')
        return encode(data, width as number, height as number, quality)
    })
    queue = encoded.catch(() => undefined)
    return await encoded
}

async function encode(data: Uint8Array, width: number, height: number, quality: number): Promise<Uint8Array> {
    const result = await invokeWithBoundedNativeMediaInput(invoke, {
        data,
        directCommand: 'native_media_encode_canvas_webp',
        streamedFinishCommand: 'native_media_encode_canvas_webp_finish',
        args: { width, height, quality },
    })
    const output = await consumeBoundedNativeMediaOutput(invoke, result, value => {
        const metadata = value as { mime?: unknown, size?: unknown, width?: unknown, height?: unknown }
        if (!metadata || metadata.mime !== 'image/webp' || metadata.width !== width || metadata.height !== height) {
            throw new TypeError('Invalid plugin WebP canvas output')
        }
        return metadata
    })
    if (output.metadata.size !== output.data.byteLength
        || output.data.byteLength < 20
        || new TextDecoder().decode(output.data.subarray(0, 4)) !== 'RIFF'
        || new TextDecoder().decode(output.data.subarray(8, 12)) !== 'WEBP'
        || new DataView(output.data.buffer).getUint32(4, true) + 8 !== output.data.byteLength) {
        throw new TypeError('Invalid plugin WebP canvas bytes')
    }
    return output.data
}
