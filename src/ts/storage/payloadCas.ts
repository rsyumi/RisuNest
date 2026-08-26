import type { BlobKeyValueBackend, BlobStore } from './blobStore'

export interface ImmutablePayloadBackend {
    putIfAbsent(key: string, data: Uint8Array): Promise<boolean>
    read(key: string): Promise<Uint8Array | null>
    stat(key: string): Promise<number | null>
}

export interface PreparedImmutablePayload {
    contentHash: string
    byteSize: number
    physicalKey: string
    deduplicated: boolean
}

export interface ImmutablePayloadCas {
    prepare(data: Uint8Array): Promise<PreparedImmutablePayload>
    readObject(contentHash: string): Promise<Uint8Array | null>
    statObject(contentHash: string): Promise<number | null>
}

async function sha256(bytes: Uint8Array): Promise<string> {
    const digest = await globalThis.crypto.subtle.digest(
        'SHA-256',
        bytes.slice().buffer as ArrayBuffer,
    )
    return Buffer.from(digest).toString('hex')
}

function objectPhysicalKey(contentHash: string): string {
    if (!/^[0-9a-f]{64}$/.test(contentHash)) {
        throw new TypeError('Content hash must be 64 lowercase hexadecimal characters')
    }
    return `assets-v2/objects/${contentHash.slice(0, 2)}/${contentHash.slice(2)}`
}

async function verifyObject(
    backend: ImmutablePayloadBackend,
    physicalKey: string,
    contentHash: string,
    byteSize: number,
): Promise<void> {
    const storedSize = await backend.stat(physicalKey)
    const stored = storedSize === byteSize ? await backend.read(physicalKey) : null
    if (
        stored === null
        || stored.byteLength !== byteSize
        || await sha256(stored) !== contentHash
    ) {
        throw new Error(`Payload collision or corruption at ${physicalKey}`)
    }
}

export function createImmutablePayloadCas(backend: ImmutablePayloadBackend): ImmutablePayloadCas {
    return {
        async prepare(data) {
            const ownedData = data.slice()
            const contentHash = await sha256(ownedData)
            const physicalKey = objectPhysicalKey(contentHash)
            const created = await backend.putIfAbsent(physicalKey, ownedData)
            await verifyObject(backend, physicalKey, contentHash, ownedData.byteLength)
            return {
                contentHash,
                byteSize: ownedData.byteLength,
                physicalKey,
                deduplicated: !created,
            }
        },
        async readObject(contentHash) {
            return backend.read(objectPhysicalKey(contentHash))
        },
        async statObject(contentHash) {
            return backend.stat(objectPhysicalKey(contentHash))
        },
    }
}

export function createBlobKeyValuePayloadBackend(
    backend: BlobKeyValueBackend,
): ImmutablePayloadBackend {
    const exists = async (key: string): Promise<boolean> => {
        if (backend.size) return await backend.size(key) !== null
        return await backend.read(key) !== null
    }
    return {
        async putIfAbsent(key, data) {
            if (await exists(key)) return false
            await backend.write(key, data.slice())
            return true
        },
        read: (key) => backend.read(key),
        async stat(key) {
            if (backend.size) return backend.size(key)
            return (await backend.read(key))?.byteLength ?? null
        },
    }
}

export function createShadowCopyBlobStore(
    authoritative: BlobStore,
    cas: ImmutablePayloadCas,
    observe?: (prepared: PreparedImmutablePayload) => void | Promise<void>,
): BlobStore {
    return {
        async put(key, data, metadata) {
            const ownedData = data.slice()
            const prepared = await cas.prepare(ownedData)
            await observe?.(prepared)
            return authoritative.put(key, ownedData, metadata)
        },
        read: (key, range) => authoritative.read(key, range),
        stat: (key) => authoritative.stat(key),
        list: (query) => authoritative.list(query),
        remove: (key) => authoritative.remove(key),
        resolveUrl: (key) => authoritative.resolveUrl(key),
    }
}
