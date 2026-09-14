import { asBuffer } from "../../util";
import type { InlayEncodeOptions } from "../../storage/blobStore";

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

/**
 * Images the browser stores exactly as they arrived. Animations would lose every
 * frame but one, and AVIF is left to the decoder that can already display it.
 */
export function keepsBrowserOriginal(data: Uint8Array): boolean {
    return isAnimatedInlayImage(data) || hasAvifBrand(data)
}

export function hasAvifBrand(data: Uint8Array): boolean {
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength)
    const text = new TextDecoder()
    for (let offset = 0; offset + 8 <= data.byteLength;) {
        const size32 = view.getUint32(offset)
        const type = text.decode(data.subarray(offset + 4, offset + 8))
        let headerSize = 8
        let boxSize = size32
        if (size32 === 1) {
            if (offset + 16 > data.byteLength) return false
            const high = view.getUint32(offset + 8)
            const low = view.getUint32(offset + 12)
            boxSize = high * 0x1_0000_0000 + low
            headerSize = 16
        } else if (size32 === 0) {
            boxSize = data.byteLength - offset
        }
        if (!Number.isSafeInteger(boxSize) || boxSize < headerSize || offset + boxSize > data.byteLength) return false
        if (type === 'ftyp') {
            const brandsStart = offset + headerSize
            if (brandsStart + 8 > offset + boxSize) return false
            for (let brandOffset = brandsStart; brandOffset + 4 <= offset + boxSize; brandOffset += brandOffset === brandsStart ? 8 : 4) {
                const brand = text.decode(data.subarray(brandOffset, brandOffset + 4)).toLowerCase()
                if (brand === 'avif' || brand === 'avis') return true
            }
            return false
        }
        offset += boxSize
    }
    return false
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
    if (hasAvifBrand(data)) return { mime: 'image/avif', ext: 'avif' }
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
 * Re-encodes stored inlay bytes without writing them. Animations and AVIF keep
 * their original bytes: the canvas encoder would drop every frame but the first.
 */
export async function encodeInlayImageBytes(
    data: Uint8Array,
    options: InlayEncodeOptions,
    mime?: string,
): Promise<CanvasEncodedInlayImage> {
    if (keepsBrowserOriginal(data)) throw new Error('This device cannot re-encode this Inlay image')
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

/**
 * The type stored bytes keep when they are saved untouched. The signature decides,
 * and a file name extension fills in for bytes nothing recognizes.
 */
export function preservedInlayOutput(data: Uint8Array, nameOrExt: string): { mime: string, ext: string } {
    const signature = inlayImageSignature(data)
    if (signature) return signature
    const ext = (nameOrExt.includes('.') ? nameOrExt.split('.').at(-1)! : nameOrExt)
        .replace(/^\.+/, '')
        .toLowerCase()
    const mime = ext === 'png' ? 'image/png'
        : ext === 'jpg' || ext === 'jpeg' ? 'image/jpeg'
            : ext === 'webp' ? 'image/webp'
                : ext === 'gif' ? 'image/gif'
                    : ext === 'avif' ? 'image/avif'
                        : ext === 'bmp' ? 'image/bmp'
                            : 'application/octet-stream'
    return { mime, ext }
}
