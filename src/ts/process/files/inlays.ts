import { alertToast } from "../../alert";
import { language } from "src/lang";
import { getRuntimePerformanceBudgets } from "../../runtimePerformanceProfile";
import { v4 } from "uuid";
import { getImageType } from "src/ts/media";
import { getDatabase, type Database } from "../../storage/database.svelte";
import { getModelInfo, LLMFlags, LLMFormat } from "src/ts/model/modellist";
import { asBuffer } from "../../util";
import {
    normalizeInlayEncodeOptions,
    type BlobMetadata,
    type BlobStore,
    type BlobWriteMetadata,
    type InlayBlobMetadata,
    type InlayEncodeOptions,
} from "../../storage/blobStore";
import { resolveBlobStore } from "../../storage/platformBlobStore";
import {
    encodeInlayImageWithCanvas,
    inlayImageSignature,
    keepsBrowserOriginal,
    preservedInlayOutput,
} from "./inlayImageEncoding";
import { isTauri } from "../../platform";

export type InlayAsset = {
    data: string | Blob
    /** File extension */
    ext: string
    height?: number
    name: string
    type: 'image' | 'video' | 'audio' | 'signature'
    width?: number
}

/** Well below the native transfer ceiling, and the only reason an attachment is refused. */
export const maxNewInlayInputBytes = 64 * 1024 * 1024

export class InlayInputTooLargeError extends Error {
    constructor(message: string) {
        super(message)
        this.name = 'InlayInputTooLargeError'
    }
}

const inlayImageExts = [
    'jpg', 'jpeg', 'png', 'gif', 'webp', 'avif'
]

const inlayAudioExts = [
    'wav', 'mp3', 'ogg', 'flac'
]

const inlayVideoExts = [
    'webm', 'mp4', 'mkv'
]

function inlayWriteId(id?: string): string {
    if (id === undefined) return v4()
    let inlayId = id
    while (inlayId.startsWith('assets/')) inlayId = inlayId.slice('assets/'.length)
    if (inlayId.length === 0) throw new TypeError('Inlay image id is empty after removing the asset namespace')
    return inlayId
}

export function getInlayEncodeOptions(): InlayEncodeOptions {
    const db = (getDatabase() ?? {}) as Partial<Pick<Database,
        'risunestInlayFormat' | 'risunestInlayWebpQuality' | 'risunestInlayMaxDimension'
        | 'risunestInlaySkipReencode' | 'risunestInlayAnimationMaxFps'>>
    return normalizeInlayEncodeOptions({
        format: db.risunestInlayFormat,
        quality: db.risunestInlayWebpQuality,
        maxDimension: db.risunestInlayMaxDimension,
        skipReencode: db.risunestInlaySkipReencode,
        animationMaxFps: db.risunestInlayAnimationMaxFps,
        animationDecodeBytes: getRuntimePerformanceBudgets().inlayAnimationDecodeBytes,
    })
}

export async function postInlayAsset(img:{
    name:string,
    data:Uint8Array
}){

    const extention = img.name.split('.').at(-1)

    if(inlayImageExts.includes(extention)){
        const imgid = v4()
        await setInlayAsset(imgid, {
            name: img.name,
            data: new Blob([asBuffer(img.data)], {type: `image/${extention}`}),
            ext: extention,
            type: 'image',
        })
        return imgid
    }

    if(inlayAudioExts.includes(extention)){
        const audioBlob = new Blob([asBuffer(img.data)], {type: `audio/${extention}`})
        const imgid = v4()

        await setInlayAsset(imgid, {
            name: img.name,
            data: audioBlob,
            ext: extention,
            type: 'audio'
        })

        return `${imgid}`
    }

    if(inlayVideoExts.includes(extention)){
        const videoBlob = new Blob([asBuffer(img.data)], {type: `video/${extention}`})
        const imgid = v4()

        await setInlayAsset(imgid, {
            name: img.name,
            data: videoBlob,
            ext: extention,
            type: 'video'
        })

        return `${imgid}`
    }

    return null
}

function imageReadiness(imgObj: HTMLImageElement, sourceUrl?: string): Promise<void> {
    let settled = false
    let resolveReady!: () => void
    let rejectReady!: (error: Error) => void
    const ready = new Promise<void>((resolve, reject) => {
        resolveReady = resolve
        rejectReady = reject
    })
    const resolve = () => {
        if (settled) return
        settled = true
        resolveReady()
    }
    const reject = () => {
        if (settled) return
        settled = true
        rejectReady(new Error('Failed to load image'))
    }
    imgObj.onload = resolve
    imgObj.onerror = reject
    if (sourceUrl) imgObj.src = sourceUrl
    if (imgObj.complete) {
        if (imgObj.naturalWidth > 0 && imgObj.naturalHeight > 0) resolve()
        else reject()
    }
    void ready.catch(() => undefined)
    return ready
}

export async function writeInlayImage(
    imgObj: HTMLImageElement,
    arg: { name?: string, ext?: string, id?: string, data?: Uint8Array } = {},
    sourceUrl?: string,
) {
    const imgid = inlayWriteId(arg.id)
    const source = sourceUrl || imgObj.currentSrc || imgObj.src
    if (!source) throw new Error('Inlay image source is unavailable')
    const data = arg.data ?? await (async () => {
        const response = await fetch(source)
        if (!response.ok) throw new Error(`Failed to read Inlay image source: ${response.status}`)
        return new Uint8Array(await response.arrayBuffer())
    })()
    validateNewInlayInput(data)
    const options = getInlayEncodeOptions()
    if (isTauri) {
        // The native encoder decides what it can improve and keeps the rest as it is.
        const blobStore = await resolveBlobStore()
        if (!blobStore.putNewInlayImage) throw new Error('Native Inlay image writer is unavailable')
        const metadata = await blobStore.putNewInlayImage(imgid, data, { name: arg.name ?? imgid, options })
        if (metadata.preservationReason) alertToast(language.risuNest.inlay.animationPreserved)
        return imgid
    }
    let drawHeight = 0
    let drawWidth = 0
    let decoded = true
    try {
        await imageReadiness(imgObj, sourceUrl)
        drawHeight = imgObj.naturalHeight || imgObj.height
        drawWidth = imgObj.naturalWidth || imgObj.width
    } catch (error) {
        void error
        decoded = false
    }
    // A canvas keeps the first frame only, so the browser stores animations untouched.
    if (!decoded || options.format === 'original' || keepsBrowserOriginal(data)) {
        const output = preservedInlayOutput(data, arg.ext ?? arg.name ?? '')
        await (await resolveBlobStore()).put(imgid, data, {
            kind: 'inlay', inlayType: 'image', mime: output.mime,
            name: arg.name ?? imgid, ext: output.ext,
            ...(decoded ? { height: drawHeight, width: drawWidth } : {}),
        })
        return `${imgid}`
    }
    if (options.format === 'webp' && options.skipReencode && inlayImageSignature(data)?.mime === 'image/webp'
        && (options.maxDimension === 0 || Math.max(drawWidth, drawHeight) <= options.maxDimension)) {
        await (await resolveBlobStore()).put(imgid, data, {
            kind: 'inlay', inlayType: 'image', mime: 'image/webp',
            name: arg.name ?? imgid, ext: 'webp', height: drawHeight, width: drawWidth,
        })
        return `${imgid}`
    }
    const encoded = await encodeInlayImageWithCanvas(imgObj, drawWidth, drawHeight, options)

    await (await resolveBlobStore()).put(imgid, encoded.data, {
        kind: 'inlay', inlayType: 'image', mime: encoded.mime,
        name: arg.name ?? imgid, ext: encoded.ext, height: encoded.height, width: encoded.width,
    })

    return `${imgid}`
}

export type InlaySignature = {
    signatures: {
        type: 'function'|'text'
        content: string
    }[],
    sourceFormat: LLMFormat,
    source: string
}

export async function saveInlayedSignature(sigid:string,signature:InlaySignature){
    await setInlayAsset(sigid, {
        name: sigid,
        data: JSON.stringify(signature),
        ext: 'json',
        type: 'signature'
    } satisfies InlayAsset)
    return sigid
}

function base64ToBlob(b64: string): Blob {
    const splitDataURI = b64.split(',');
    const byteString = atob(splitDataURI[1]);
    const mimeString = splitDataURI[0].split(':')[1].split(';')[0];

    const ab = new ArrayBuffer(byteString.length);
    const ia = new Uint8Array(ab);
    for (let i = 0; i < byteString.length; i++) {
        ia[i] = byteString.charCodeAt(i);
    }

    return new Blob([ab], { type: mimeString });
}

function blobToBase64(blob: Blob): Promise<string> {
    const reader = new FileReader();
    reader.readAsDataURL(blob);
    return new Promise<string>((resolve, reject) => {
        reader.onloadend = () => {
            resolve(reader.result as string);
        };
        reader.onerror = reject;
    });
}

function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
    return left.byteLength === right.byteLength && left.every((value, index) => value === right[index])
}

async function inlayBytes(asset: InlayAsset): Promise<{ bytes: Uint8Array; mime: string }> {
    if (asset.data instanceof Blob) {
        return { bytes: new Uint8Array(await asset.data.arrayBuffer()), mime: asset.data.type }
    }
    if (asset.type === 'signature') {
        return { bytes: new TextEncoder().encode(asset.data), mime: 'application/json' }
    }
    const blob = base64ToBlob(asset.data)
    return { bytes: new Uint8Array(await blob.arrayBuffer()), mime: blob.type }
}

function metadataToAsset<T extends string | Blob>(metadata: InlayBlobMetadata, data: T): Omit<InlayAsset, 'data'> & { data: T } {
    return {
        data,
        ext: metadata.ext,
        height: metadata.height,
        name: metadata.name,
        type: metadata.inlayType,
        width: metadata.width,
    }
}

async function getInlayAssetMetadataInStore(
    id: string,
    blobStore: BlobStore,
): Promise<InlayBlobMetadata | null> {
    const metadata = await blobStore.stat(id)
    return metadata?.kind === 'inlay' ? metadata : null
}

export async function getInlayAssetMetadata(id: string): Promise<InlayBlobMetadata | null> {
    return getInlayAssetMetadataInStore(id, await resolveBlobStore())
}

// Returns with base64 data URI
export async function getInlayAsset(id: string){
    const blobStore = await resolveBlobStore()
    const metadata = await getInlayAssetMetadataInStore(id, blobStore)
    if (!metadata) return null
    const bytes = await blobStore.read(id)
    if (!bytes) return null
    const data = metadata.inlayType === 'signature'
        ? new TextDecoder().decode(bytes)
        : await blobToBase64(new Blob([asBuffer(bytes)], { type: metadata.mime }))
    return metadataToAsset(metadata, data)
}

// Returns with Blob
export async function getInlayAssetBlob(id: string){
    const blobStore = await resolveBlobStore()
    const metadata = await getInlayAssetMetadataInStore(id, blobStore)
    if (!metadata) return null
    const bytes = await blobStore.read(id)
    if (!bytes) return null
    return metadataToAsset(metadata, new Blob([asBuffer(bytes)], { type: metadata.mime }))
}

export async function listInlayAssets(): Promise<[id: string, InlayAsset][]> {
    const blobStore = await resolveBlobStore()
    const assets: [id: string, InlayAsset][] = []
    for (const metadata of await blobStore.list({ kind: 'inlay' })) {
        if (metadata.kind !== 'inlay') continue
        const bytes = await blobStore.read(metadata.key)
        if (!bytes) continue
        const data = metadata.inlayType === 'signature'
            ? new TextDecoder().decode(bytes)
            : await blobToBase64(new Blob([asBuffer(bytes)], { type: metadata.mime }))
        assets.push([metadata.key, metadataToAsset(metadata, data)])
    }
    return assets
}

export async function listInlayAssetMetadata(): Promise<InlayBlobMetadata[]> {
    const blobStore = await resolveBlobStore()
    const metadata = await blobStore.list({ kind: 'inlay' })
    return metadata.filter((item): item is InlayBlobMetadata => item.kind === 'inlay')
}

export async function getInlayAssetRenderUrl(
    id: string,
    store?: BlobStore,
): Promise<string | null> {
    const blobStore = store ?? await resolveBlobStore()
    const metadata = await blobStore.stat(id)
    if (metadata?.kind !== 'inlay') return null
    const url = await blobStore.resolveUrl(id)
    if (!url) return null
    return url
}

/**
 * Nothing about the format refuses an attachment any more: whatever the encoder
 * cannot improve is stored as it arrived. Only a file too large to move through
 * the native transfer is turned away.
 */
function validateNewInlayInput(data: Uint8Array): void {
    if (data.byteLength > maxNewInlayInputBytes) {
        throw new InlayInputTooLargeError('The Inlay attachment is larger than the input limit')
    }
}

export async function setInlayAsset(id: string, img: InlayAsset): Promise<string> {
    const inlayId = inlayWriteId(id)
    const { bytes, mime } = await inlayBytes(img)
    const blobStore = await resolveBlobStore()
    validateNewInlayInput(bytes)
    if (isTauri && img.type === 'image') {
        if (!blobStore.putNewInlayImage) throw new Error('Native Inlay image writer is unavailable')
        const metadata = await blobStore.putNewInlayImage(inlayId, bytes, { name: img.name, options: getInlayEncodeOptions() })
        if (metadata.preservationReason) alertToast(language.risuNest.inlay.animationPreserved)
        return inlayId
    }
    if (img.type === 'image') {
        const sourceUrl = URL.createObjectURL(new Blob([asBuffer(bytes)], { type: mime }))
        try {
            await writeInlayImage(new Image(), { id: inlayId, name: img.name, ext: img.ext, data: bytes }, sourceUrl)
        } finally {
            URL.revokeObjectURL(sourceUrl)
        }
        return inlayId
    }
    await blobStore.put(inlayId, bytes, {
        kind: 'inlay',
        inlayType: img.type,
        mime,
        name: img.name,
        ext: img.ext,
        width: img.width,
        height: img.height,
    })
    return inlayId
}

export async function removeInlayAsset(id: string){
    await (await resolveBlobStore()).remove(id)
}

export function supportsInlayImage(){
    const db = getDatabase()
    return getModelInfo(db.aiModel).flags.includes(LLMFlags.hasImageInput)
}

export async function reencodeImage(img:Uint8Array){
    if(getImageType(img) === 'PNG'){
        return img
    }
    const canvas = document.createElement('canvas')
    const imgObj = new Image()
    const url = URL.createObjectURL(new Blob([asBuffer(img)], {type: `image/png`}))
    try {
        imgObj.src = url
        await imgObj.decode()
        let drawHeight = imgObj.height
        let drawWidth = imgObj.width
        canvas.width = drawWidth
        canvas.height = drawHeight
        const ctx = canvas.getContext('2d')
        ctx.drawImage(imgObj, 0, 0, drawWidth, drawHeight)
        const b64 = canvas.toDataURL('image/png').split(',')[1]
        const b = Buffer.from(b64, 'base64')
        return b
    }
    finally {
        URL.revokeObjectURL(url)
    }
}
