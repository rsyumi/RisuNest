import { asBuffer } from "../../util";
import { isTauri } from "../../platform";
import type { InlayBlobMetadata, InlayEncodeOptions } from "../../storage/blobStore";
import type { NewInlayImageEncoder } from "../../storage/assetRepository";
import { createNativeNewInlayImageEncoder } from "../../storage/nativeAssetRepository";

export function isGifImage(data: Uint8Array): boolean {
    return data.byteLength >= 6 && new TextDecoder().decode(data.subarray(0, 6)).startsWith('GIF8')
}

export function isAnimatedWebP(data: Uint8Array): boolean {
    if (data.byteLength < 12
        || new TextDecoder().decode(data.subarray(0, 4)) !== 'RIFF'
        || new TextDecoder().decode(data.subarray(8, 12)) !== 'WEBP') return false
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength)
    for (let offset = 12; offset + 8 <= data.byteLength;) {
        const type = new TextDecoder().decode(data.subarray(offset, offset + 4))
        if (type === 'ANIM' || type === 'ANMF') return true
        const length = view.getUint32(offset + 4, true)
        offset += 8 + length + (length % 2)
    }
    return false
}

export function isAnimatedPng(data: Uint8Array): boolean {
    const pngSignature = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
    if (data.byteLength < pngSignature.length
        || !pngSignature.every((value, index) => data[index] === value)) return false
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength)
    for (let offset = 8; offset + 12 <= data.byteLength;) {
        const length = view.getUint32(offset)
        const chunkEnd = offset + 12 + length
        if (chunkEnd > data.byteLength) return false
        const type = new TextDecoder().decode(data.subarray(offset + 4, offset + 8))
        if (type === 'acTL') return true
        offset = chunkEnd
    }
    return false
}

/**
 * A GIF counts as animated without scanning its frames. Drawing one on a canvas
 * keeps the first frame only, so the browser encoder must never touch it.
 */
export function isAnimatedInlayImage(data: Uint8Array): boolean {
    return isGifImage(data) || isAnimatedWebP(data) || isAnimatedPng(data)
}

export function inlayImageSignature(data: Uint8Array): { mime: string, ext: string } | null {
    const isPng = data.byteLength >= 8
        && data.slice(0, 8).every((value, index) => value === [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a][index])
    if (isPng) return { mime: 'image/png', ext: 'png' }
    if (data.byteLength >= 3 && data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff) return { mime: 'image/jpeg', ext: 'jpg' }
    if (data.byteLength >= 12
        && new TextDecoder().decode(data.subarray(0, 4)) === 'RIFF'
        && new TextDecoder().decode(data.subarray(8, 12)) === 'WEBP') return { mime: 'image/webp', ext: 'webp' }
    if (isGifImage(data)) return { mime: 'image/gif', ext: 'gif' }
    return null
}

export interface CanvasEncodedInlayImage {
    data: Uint8Array
    mime: string
    ext: string
    width: number
    height: number
}

function encoderOutput(mime: string): { mime: string, ext: string } | null {
    switch (mime.toLowerCase()) {
        case 'image/webp': return { mime: 'image/webp', ext: 'webp' }
        case 'image/png': return { mime: 'image/png', ext: 'png' }
        case 'image/jpeg': return { mime: 'image/jpeg', ext: 'jpg' }
        default: return null
    }
}

/** Scales to `maxDimension` and re-encodes with the browser canvas encoder. */
export async function encodeInlayImageWithCanvas(
    source: CanvasImageSource,
    sourceWidth: number,
    sourceHeight: number,
    options: InlayEncodeOptions,
): Promise<CanvasEncodedInlayImage> {
    let drawWidth = sourceWidth
    let drawHeight = sourceHeight
    if (options.maxDimension > 0 && Math.max(drawWidth, drawHeight) > options.maxDimension) {
        const ratio = options.maxDimension / Math.max(drawWidth, drawHeight)
        drawWidth = Math.max(1, Math.round(drawWidth * ratio))
        drawHeight = Math.max(1, Math.round(drawHeight * ratio))
    }
    const canvas = document.createElement('canvas')
    const ctx = canvas.getContext('2d')
    canvas.width = drawWidth
    canvas.height = drawHeight
    if (!ctx) throw new Error('Image canvas is unavailable')
    ctx.drawImage(source, 0, 0, drawWidth, drawHeight)
    const imageBlob = await new Promise<Blob>((resolve, reject) => {
        canvas.toBlob(
            (blob) => blob ? resolve(blob) : reject(new Error('Failed to encode Inlay image')),
            `image/${options.format}`,
            options.quality / 100,
        )
    })
    const output = encoderOutput(imageBlob.type)
    if (!output) throw new Error(`Unsupported browser Inlay encoder MIME: ${imageBlob.type || '(empty)'}`)
    return {
        data: new Uint8Array(await imageBlob.arrayBuffer()),
        mime: output.mime,
        ext: output.ext,
        width: drawWidth,
        height: drawHeight,
    }
}

/** Decodes stored bytes with the WebView decoder, which covers every format it can display. */
export async function decodeInlayImageBitmap(data: Uint8Array, mime?: string): Promise<ImageBitmap> {
    if (typeof createImageBitmap !== 'function') throw new Error('Image decoding is unavailable on this device')
    const type = mime || inlayImageSignature(data)?.mime || 'application/octet-stream'
    return createImageBitmap(new Blob([asBuffer(data)], { type }))
}

/**
 * Re-encodes stored inlay bytes without writing them. Animated images keep their
 * original bytes instead: the canvas encoder would drop every frame but the first.
 */
export async function encodeInlayImageBytes(
    data: Uint8Array,
    options: InlayEncodeOptions,
    mime?: string,
): Promise<CanvasEncodedInlayImage> {
    if (isAnimatedInlayImage(data)) throw new Error('This device cannot re-encode animated Inlay images')
    const bitmap = await decodeInlayImageBitmap(data, mime)
    try {
        if (options.format === 'original') {
            const signature = inlayImageSignature(data)
            if (!signature) throw new Error('Original Inlay image format is unrecognized')
            return { data, mime: signature.mime, ext: signature.ext, width: bitmap.width, height: bitmap.height }
        }
        return await encodeInlayImageWithCanvas(bitmap, bitmap.width, bitmap.height, options)
    } finally {
        bitmap.close?.()
    }
}

function webInlayImageEncoder(): NewInlayImageEncoder {
    return {
        async encodeNewInlayImage(key, data, input) {
            const options = input.options ?? { format: 'webp', quality: 85, maxDimension: 0, skipReencode: true }
            const encoded = await encodeInlayImageBytes(data, options)
            const metadata: Omit<InlayBlobMetadata, 'key' | 'size'> = {
                kind: 'inlay',
                inlayType: 'image',
                mime: encoded.mime,
                name: input.name,
                ext: encoded.ext,
                width: encoded.width,
                height: encoded.height,
            }
            return { data: encoded.data, metadata }
        },
    }
}

/** The encoder that re-encodes without writing, native where one exists. */
export function resolveInlayImageEncoder(): NewInlayImageEncoder {
    return isTauri ? createNativeNewInlayImageEncoder() : webInlayImageEncoder()
}
