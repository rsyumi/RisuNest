/** Installed only in native plugin realms; keep this function self-contained for srcdoc. */
export function installCanvasWebpCompatibility(
    encode: (data: Uint8Array, width: number, height: number, quality: number) => Promise<Uint8Array>,
): void {
    async function webp(blob: Blob, quality: unknown): Promise<Blob> {
        if (!blob || blob.type !== 'image/png') return blob
        // Decode the browser's snapshot, rather than reading a canvas the plugin may have changed.
        const bitmap = await createImageBitmap(blob)
        const canvas = document.createElement('canvas')
        const { width, height } = bitmap
        let pixels: ImageData
        try {
            if (width > 16383 || height > 16383 || width * height > 16 * 1024 * 1024) {
                throw new RangeError('Plugin WebP canvas exceeds the encoding limit')
            }
            canvas.width = width
            canvas.height = height
            const context = canvas.getContext('2d', { colorSpace: 'srgb' })
            if (!context) throw new Error('Plugin WebP canvas is unavailable')
            context.drawImage(bitmap, 0, 0)
            pixels = context.getImageData(0, 0, width, height)
        } finally {
            bitmap.close()
            canvas.width = canvas.height = 0
        }
        const normalizedQuality = typeof quality === 'number' && quality >= 0 && quality <= 1 ? quality : 0.92
        const data = await encode(new Uint8Array(pixels.data.buffer), width, height, normalizedQuality)
        return new Blob([data as Uint8Array<ArrayBuffer>], { type: 'image/webp' })
    }

    const toBlob = HTMLCanvasElement.prototype.toBlob
    HTMLCanvasElement.prototype.toBlob = function(callback, type, quality) {
        if (typeof callback !== 'function' || String(type).toLowerCase() !== 'image/webp') {
            return toBlob.apply(this, arguments)
        }
        return toBlob.call(this, blob => {
            if (!blob) { callback(null); return }
            void webp(blob, quality).then(callback, () => callback(null))
        }, type, quality)
    }

    if (typeof OffscreenCanvas !== 'undefined' && OffscreenCanvas.prototype.convertToBlob) {
        const convertToBlob = OffscreenCanvas.prototype.convertToBlob
        OffscreenCanvas.prototype.convertToBlob = function(options) {
            const result = convertToBlob.apply(this, arguments)
            if (String(options?.type).toLowerCase() !== 'image/webp') return result
            const quality = options?.quality
            return result.then(blob => webp(blob, quality))
        }
    }
}
