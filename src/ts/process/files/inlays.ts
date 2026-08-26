import localforage from "localforage";
import { v4 } from "uuid";
import { getImageType } from "src/ts/media";
import { getDatabase } from "../../storage/database.svelte";
import { getModelInfo, LLMFlags, LLMFormat } from "src/ts/model/modellist";
import { asBuffer } from "../../util";
import { type BlobMetadata, type BlobStore, type BlobWriteMetadata, type InlayBlobMetadata } from "../../storage/blobStore";
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

const inlayImageExts = [
    'jpg', 'jpeg', 'png', 'gif', 'webp', 'avif'
]

const inlayAudioExts = [
    'wav', 'mp3', 'ogg', 'flac'
]

const inlayVideoExts = [
    'webm', 'mp4', 'mkv'
]

const inlayStorage = localforage.createInstance({
    name: 'inlay',
    storeName: 'inlay'
})

export async function postInlayAsset(img:{
    name:string,
    data:Uint8Array
}){

    const extention = img.name.split('.').at(-1)
    const imgObj = new Image()

    if(inlayImageExts.includes(extention)){
        const url = URL.createObjectURL(new Blob([asBuffer(img.data)], {type: `image/${extention}`}))
        try {
            return await writeInlayImage(imgObj, {
                name: img.name,
                ext: extention
            }, url)
        }
        finally {
            URL.revokeObjectURL(url)
        }
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

export async function writeInlayImage(imgObj:HTMLImageElement, arg:{name?:string, ext?:string, id?:string} = {}, sourceUrl?: string) {

    let drawHeight = 0
    let drawWidth = 0
    const canvas = document.createElement('canvas')
    const ctx = canvas.getContext('2d')
    await new Promise<void>((resolve, reject) => {
        imgObj.onload = () => {
            drawHeight = imgObj.height
            drawWidth = imgObj.width

            //resize image to fit inlay, if total pixels exceed 1024*1024
            const maxPixels = 1024 * 1024
            const currentPixels = drawHeight * drawWidth
            
            if(currentPixels > maxPixels){
                const scaleFactor = Math.sqrt(maxPixels / currentPixels)
                drawWidth = Math.floor(drawWidth * scaleFactor)
                drawHeight = Math.floor(drawHeight * scaleFactor)
            }

            canvas.width = drawWidth
            canvas.height = drawHeight
            ctx.drawImage(imgObj, 0, 0, drawWidth, drawHeight)
            resolve(null)
        }
        imgObj.onerror = () => reject(new Error('Failed to load image'))
        if (sourceUrl) imgObj.src = sourceUrl
    })
    const imageBlob = await new Promise<Blob>((resolve) => canvas.toBlob((blob) => resolve(blob), 'image/png'));


    const imgid = arg.id ?? v4()

    await setInlayAsset(imgid, {
        name: arg.name ?? imgid,
        data: imageBlob,
        ext: 'png',
        height: drawHeight,
        width: drawWidth,
        type: 'image'
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

export async function setInlayAsset(id: string, img: InlayAsset){
    const { bytes, mime } = await inlayBytes(img)
    await (await resolveBlobStore()).put(id, bytes, {
        kind: 'inlay',
        inlayType: img.type,
        mime,
        name: img.name,
        ext: img.ext,
        width: img.width,
        height: img.height,
    })
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
