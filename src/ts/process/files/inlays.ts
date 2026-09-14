import localforage from "localforage";
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

export class UnsupportedAnimatedInlayError extends Error {
    constructor(message: string) {
        super(message)
        this.name = 'UnsupportedAnimatedInlayError'
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

const inlayStorage = localforage.createInstance({
    name: 'inlay',
    storeName: 'inlay'
})

export function getInlayEncodeOptions(): InlayEncodeOptions {
    const db = (getDatabase() ?? {}) as Partial<Pick<Database,
        'risunestInlayFormat' | 'risunestInlayWebpQuality' | 'risunestInlayMaxDimension' | 'risunestInlaySkipReencode'>>
    return normalizeInlayEncodeOptions({
        format: db.risunestInlayFormat,
        quality: db.risunestInlayWebpQuality,
        maxDimension: db.risunestInlayMaxDimension,
        skipReencode: db.risunestInlaySkipReencode,
    })
}

function sourceImageOutput(data: Uint8Array): { mime: string, ext: string } | null {
    if (data.byteLength >= 8 && data.slice(0, 8).every((value, index) => value === [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a][index])) {
        return { mime: 'image/png', ext: 'png' }
    }
    if (data.byteLength >= 3 && data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff) return { mime: 'image/jpeg', ext: 'jpg' }
    if (isNativeInlayFormat(data)) return { mime: 'image/webp', ext: 'webp' }
    return null
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

export async function writeInlayImage(imgObj:HTMLImageElement, arg:{name?:string, ext?:string, id?:string} = {}, sourceUrl?: string) {
    const imgid = inlayWriteId(arg.id)
    const source = sourceUrl || imgObj.currentSrc || imgObj.src
    if (!source) throw new Error('Inlay image source is unavailable')
    const response = await fetch(source)
    if (!response.ok) throw new Error(`Failed to read Inlay image source: ${response.status}`)
    const data = new Uint8Array(await response.arrayBuffer())
    validateNewInlayImage(data, response.headers.get('Content-Type') ?? '', arg.ext ?? '')
    const options = getInlayEncodeOptions()
    const nativeFastPath = isTauri && isNativeInlayFormat(data)
    if (nativeFastPath) {
        const blobStore = await resolveBlobStore()
        if (!blobStore.putNewInlayImage) throw new Error('Native Inlay image writer is unavailable')
        await blobStore.putNewInlayImage(imgid, data, { name: arg.name ?? imgid, options })
        return imgid
    }
    const ready = imageReadiness(imgObj, sourceUrl)

    let drawHeight = 0
    let drawWidth = 0
    await ready
    drawHeight = imgObj.naturalHeight || imgObj.height
    drawWidth = imgObj.naturalWidth || imgObj.width
    if (options.format === 'original') {
        const output = sourceImageOutput(data)
        if (!output) throw new Error('Original Inlay image must be PNG, JPEG, or WebP')
        await (await resolveBlobStore()).put(imgid, data, {
            kind: 'inlay', inlayType: 'image', mime: output.mime,
            name: arg.name ?? imgid, ext: output.ext, height: drawHeight, width: drawWidth,
        })
        return `${imgid}`
    }
    const sourceOutput = sourceImageOutput(data)
    if (options.format === 'webp' && options.skipReencode && sourceOutput?.mime === 'image/webp'
        && (options.maxDimension === 0 || Math.max(drawWidth, drawHeight) <= options.maxDimension)) {
        await (await resolveBlobStore()).put(imgid, data, {
            kind: 'inlay', inlayType: 'image', mime: 'image/webp',
            name: arg.name ?? imgid, ext: 'webp', height: drawHeight, width: drawWidth,
        })
        return `${imgid}`
    }
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
    ctx.drawImage(imgObj, 0, 0, drawWidth, drawHeight)
    const imageBlob = await new Promise<Blob>((resolve, reject) => {
        canvas.toBlob((blob) => blob ? resolve(blob) : reject(new Error('Failed to encode Inlay image')), `image/${options.format}`, options.quality / 100)
    })
    const output = imageBlob.type.toLowerCase() === 'image/webp'
        ? { mime: 'image/webp', ext: 'webp' }
        : imageBlob.type.toLowerCase() === 'image/png'
            ? { mime: 'image/png', ext: 'png' }
            : imageBlob.type.toLowerCase() === 'image/jpeg'
                ? { mime: 'image/jpeg', ext: 'jpg' }
                : null
    if (!output) throw new Error(`Unsupported browser Inlay encoder MIME: ${imageBlob.type || '(empty)'}`)


    await (await resolveBlobStore()).put(imgid, new Uint8Array(await imageBlob.arrayBuffer()), {
        kind: 'inlay', inlayType: 'image', mime: output.mime,
        name: arg.name ?? imgid, ext: output.ext, height: drawHeight, width: drawWidth,
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

export async function listLegacyInlayAssetIds(): Promise<string[]> {
    return await inlayStorage.keys()
}

export async function readLegacyInlayAsset(id: string): Promise<InlayAsset | null> {
    return await inlayStorage.getItem<InlayAsset | null>(id)
}

export async function readLegacyInlayPayload(id: string): Promise<{
    data: Uint8Array
    metadata: BlobWriteMetadata
} | null> {
    const asset = await readLegacyInlayAsset(id)
    if (!asset) return null
    const { bytes, mime } = await inlayBytes(asset)
    return {
        data: bytes,
        metadata: {
            kind: 'inlay',
            inlayType: asset.type,
            mime,
            name: asset.name,
            ext: asset.ext,
            ...(asset.width === undefined ? {} : { width: asset.width }),
            ...(asset.height === undefined ? {} : { height: asset.height }),
        },
    }
}

async function migrateLegacyInlayAssetInStore(id: string, blobStore: BlobStore): Promise<BlobMetadata | null> {
    const existing = await blobStore.stat(id)
    if (existing?.kind === 'inlay') return existing
    const legacy = await readLegacyInlayPayload(id)
    if (!legacy || legacy.metadata.kind !== 'inlay') return null
    const { data: bytes, metadata } = legacy
    let written: BlobMetadata
    try {
        written = await blobStore.put(id, bytes, metadata)
        const verifiedMetadata = await blobStore.stat(id)
        const verifiedBytes = await blobStore.read(id)
        if (!verifiedMetadata || verifiedMetadata.kind !== 'inlay' || !verifiedBytes
            || written.kind !== 'inlay' || verifiedMetadata.inlayType !== metadata.inlayType
            || verifiedMetadata.name !== metadata.name
            || verifiedMetadata.ext !== metadata.ext.replace(/^\.+/, '').toLowerCase()
            || verifiedMetadata.width !== metadata.width || verifiedMetadata.height !== metadata.height
            || verifiedMetadata.mime !== written.mime || verifiedMetadata.size !== bytes.byteLength
            || !bytesEqual(verifiedBytes, bytes)) {
            await blobStore.remove(id)
            return null
        }
        return verifiedMetadata
    } catch (error) {
        await blobStore.remove(id)
        throw error
    }
}

export async function migrateLegacyInlayAsset(id: string): Promise<BlobMetadata | null> {
    const blobStore = await resolveBlobStore()
    if (isTauri) {
        const metadata = await blobStore.stat(id)
        return metadata?.kind === 'inlay' ? metadata : null
    }
    return migrateLegacyInlayAssetInStore(id, blobStore)
}

async function migrateLegacyInlayAssetsInStore(blobStore: BlobStore): Promise<void> {
    for (const id of await listLegacyInlayAssetIds()) {
        if (!id.startsWith('blobstore/')) await migrateLegacyInlayAssetInStore(id, blobStore)
    }
}

async function getInlayAssetMetadataInStore(
    id: string,
    blobStore: BlobStore,
    options: { migrateLegacy?: boolean } = {},
): Promise<InlayBlobMetadata | null> {
    const metadata = isTauri || options.migrateLegacy === false
        ? await blobStore.stat(id)
        : await migrateLegacyInlayAssetInStore(id, blobStore)
    return metadata?.kind === 'inlay' ? metadata : null
}

export async function getInlayAssetMetadata(
    id: string,
    options: { migrateLegacy?: boolean } = {},
): Promise<InlayBlobMetadata | null> {
    return getInlayAssetMetadataInStore(id, await resolveBlobStore(), options)
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
    if (!isTauri) await migrateLegacyInlayAssetsInStore(blobStore)
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

export async function listInlayAssetMetadata(
    options: { migrateLegacy?: boolean } = {},
): Promise<InlayBlobMetadata[]> {
    const blobStore = await resolveBlobStore()
    if (!isTauri && options.migrateLegacy !== false) await migrateLegacyInlayAssetsInStore(blobStore)
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

function isAnimatedWebP(data: Uint8Array): boolean {
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

function isNativeInlayFormat(data: Uint8Array): boolean {
    const isPng = data.byteLength >= 8
        && data[0] === 0x89 && data[1] === 0x50 && data[2] === 0x4e && data[3] === 0x47
        && data[4] === 0x0d && data[5] === 0x0a && data[6] === 0x1a && data[7] === 0x0a
    const isJpeg = data.byteLength >= 3 && data[0] === 0xff && data[1] === 0xd8 && data[2] === 0xff
    const isWebP = data.byteLength >= 12
        && new TextDecoder().decode(data.subarray(0, 4)) === 'RIFF'
        && new TextDecoder().decode(data.subarray(8, 12)) === 'WEBP'
    return isPng || isJpeg || isWebP
}

function isAnimatedPng(data: Uint8Array): boolean {
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

function hasAvifBrand(data: Uint8Array): boolean {
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

function validateNewInlayImage(data: Uint8Array, mime: string, ext: string): void {
    const normalizedExt = ext.replace(/^\.+/, '').toLowerCase()
    const normalizedMime = mime.split(';', 1)[0].trim().toLowerCase()
    const gifSignature = data.byteLength >= 6
        && new TextDecoder().decode(data.subarray(0, 6)).startsWith('GIF8')
    if (normalizedExt === 'gif' || normalizedMime === 'image/gif' || gifSignature) {
        throw new UnsupportedAnimatedInlayError('New GIF Inlay images are unsupported because animation cannot be preserved')
    }
    if (normalizedExt === 'avif' || normalizedMime === 'image/avif' || hasAvifBrand(data)) {
        throw new UnsupportedAnimatedInlayError('New AVIF Inlay images are unsupported')
    }
    if (isAnimatedWebP(data)) throw new UnsupportedAnimatedInlayError('Animated WebP Inlay images are unsupported')
    if (isAnimatedPng(data)) throw new UnsupportedAnimatedInlayError('APNG Inlay images are unsupported')
}

export async function setInlayAsset(id: string, img: InlayAsset): Promise<string> {
    const inlayId = inlayWriteId(id)
    const { bytes, mime } = await inlayBytes(img)
    const blobStore = await resolveBlobStore()
    if (img.type === 'image') validateNewInlayImage(bytes, mime, img.ext)
    if (isTauri && img.type === 'image' && isNativeInlayFormat(bytes)) {
        if (!blobStore.putNewInlayImage) throw new Error('Native Inlay image writer is unavailable')
        await blobStore.putNewInlayImage(inlayId, bytes, { name: img.name, options: getInlayEncodeOptions() })
        return inlayId
    }
    if (img.type === 'image') {
        const sourceUrl = URL.createObjectURL(new Blob([asBuffer(bytes)], { type: mime }))
        try {
            await writeInlayImage(new Image(), { id: inlayId, name: img.name, ext: img.ext }, sourceUrl)
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
    if (!isTauri) await inlayStorage.removeItem(id)
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
