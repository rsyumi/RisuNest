import { getDatabase, type Database } from "../../storage/database.svelte";
import { normalizeInlayEncodeOptions } from "../../storage/blobStore";
import {
    decodeInlayImageBitmap,
    encodeInlayImageWithCanvas,
    isAnimatedInlayImage,
} from "./inlayImageEncoding";

export interface InlayProviderImage {
    data: string
    width?: number
    height?: number
}

/** What every image provider reads. Anything else travels as a still WebP. */
const providerReadableMimes = new Set(['image/png', 'image/jpeg', 'image/webp'])
const maxRememberedStillFrames = 16
const stillFrames = new Map<string, InlayProviderImage>()

function dataUriParts(dataUri: string): { mime: string, base64: string } | null {
    const match = /^data:([^;,]*)(;base64)?,(.*)$/s.exec(dataUri)
    if (!match || !match[2]) return null
    return { mime: match[1].split(';', 1)[0].trim().toLowerCase(), base64: match[3] }
}

function decodeBase64(value: string): Uint8Array {
    const binary = atob(value)
    const bytes = new Uint8Array(binary.length)
    for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index)
    return bytes
}

function encodeBase64(bytes: Uint8Array): string {
    let binary = ''
    for (const byte of bytes) binary += String.fromCharCode(byte)
    return btoa(binary)
}

type ProviderImageSettings = Partial<Pick<Database,
    'risunestInlayAnimationStillFrame' | 'risunestInlayWebpQuality'>>

function providerImageSettings(): ProviderImageSettings {
    return (getDatabase() ?? {}) as ProviderImageSettings
}

function remember(id: string, image: InlayProviderImage): InlayProviderImage {
    stillFrames.set(id, image)
    while (stillFrames.size > maxRememberedStillFrames) {
        const oldest = stillFrames.keys().next()
        if (oldest.done) break
        stillFrames.delete(oldest.value)
    }
    return image
}

export function forgetInlayProviderImages(): void {
    stillFrames.clear()
}

/**
 * Turns a stored attachment into something the model can actually read: an
 * animation becomes its first scene, and a format providers do not list becomes
 * a still WebP. Whatever cannot be converted travels exactly as it is stored.
 */
export async function inlayImageForProvider(
    id: string,
    asset: InlayProviderImage,
): Promise<InlayProviderImage> {
    const settings = providerImageSettings()
    if (settings.risunestInlayAnimationStillFrame === false) return asset
    const remembered = stillFrames.get(id)
    if (remembered) return remembered
    const parts = dataUriParts(asset.data)
    if (!parts) return asset
    try {
        const bytes = decodeBase64(parts.base64)
        if (providerReadableMimes.has(parts.mime) && !isAnimatedInlayImage(bytes)) return asset
        const bitmap = await decodeInlayImageBitmap(bytes, parts.mime)
        try {
            const encoded = await encodeInlayImageWithCanvas(
                bitmap,
                bitmap.width,
                bitmap.height,
                {
                    ...normalizeInlayEncodeOptions({ quality: settings.risunestInlayWebpQuality }),
                    format: 'webp',
                    maxDimension: 0,
                },
            )
            return remember(id, {
                data: `data:${encoded.mime};base64,${encodeBase64(encoded.data)}`,
                width: encoded.width,
                height: encoded.height,
            })
        } finally {
            bitmap.close?.()
        }
    } catch (error) {
        void error
        // Sending the stored bytes is better than dropping the attachment.
        return asset
    }
}
