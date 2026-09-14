import {
    BaseDirectory,
    SeekMode,
    exists,
    mkdir,
    open,
    readDir,
    readFile,
    remove,
    stat,
    writeFile,
} from '@tauri-apps/plugin-fs'
import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../platform'
import {
    createKeyValueBlobStore,
    type BlobKeyValueBackend,
    type InlayBlobMetadata,
    type BlobPhysicalKeyMapper,
    type BlobReadRange,
    type BlobStore,
    normalizeInlayEncodeOptions,
} from './blobStore'
import type { StorageMutationGate } from './storageMutationGate'
import { objectPhysicalKey } from './payloadCas'
import { invokeWithBoundedNativeMediaInput } from './nativeMediaIpc'

function logicalKeyHex(key: string): string {
    return Buffer.from(key, 'utf-8').toString('hex')
}

export function physicalBlobKeys(logicalKey: string) {
    return {
        payload: logicalKey.startsWith('assets/')
            ? logicalKey
            : `blobstore/inlays/${logicalKeyHex(logicalKey)}.bin`,
        metadata: `blobstore/metadata/${logicalKeyHex(logicalKey)}.json`,
    }
}

export function createTauriNativeMediaUrl(
    physicalKey: string,
    baseUrl: string,
): string {
    if (!/^http:\/\/127\.0\.0\.1:[1-9][0-9]*\/[a-f0-9]{32}\/$/.test(baseUrl)) {
        throw new TypeError('Invalid native media endpoint')
    }
    return `${baseUrl}${logicalKeyHex(physicalKey)}`
}

export function createNativeMediaEndpointProvider(
    invokeCommand: (command: string) => Promise<unknown> = invoke,
): () => Promise<string> {
    let pending: Promise<string> | undefined
    return () =>
        (pending ??= invokeCommand('native_media_base_url')
            .then((value) => {
                if (typeof value !== 'string')
                    throw new TypeError('Invalid native media endpoint')
                createTauriNativeMediaUrl('assets/validation', value)
                return value
            })
            .catch((error) => {
                pending = undefined
                throw error
            }))
}

export const getNativeMediaEndpoint = createNativeMediaEndpointProvider()

export function createTauriCasObjectUrl(
    input: { contentHash: string; mime: string; size: number },
    baseUrl: string,
): string {
    if (!Number.isSafeInteger(input.size) || input.size < 0) {
        throw new RangeError(
            'CAS object size must be a nonnegative safe integer',
        )
    }
    if (input.mime.length === 0 || !/^[\x20-\x7e]+$/.test(input.mime)) {
        throw new TypeError('CAS object MIME must be a nonempty header value')
    }
    const query = new URLSearchParams({
        mime: input.mime,
        size: input.size.toString(),
    })
    return `${createTauriNativeMediaUrl(objectPhysicalKey(input.contentHash), baseUrl)}?${query}`
}

const blobKeyMapper: BlobPhysicalKeyMapper = {
    payload: (key) => physicalBlobKeys(key).payload,
    metadata: (key) => physicalBlobKeys(key).metadata,
    metadataPrefix: 'blobstore/metadata/',
}

export function createBackedBlobStore(backend: BlobKeyValueBackend): BlobStore {
    return createKeyValueBlobStore(backend, blobKeyMapper)
}

export function createTauriBlobStore(
    backend: BlobKeyValueBackend,
    invokeCommand: (command: string, args?: Record<string, unknown>) => Promise<unknown> = invoke,
): BlobStore {
    return {
        ...createBackedBlobStore(backend),
        async putNewInlayImage(key, data, input) {
            const options = input.options === undefined
                ? undefined
                : normalizeInlayEncodeOptions(input.options)
            return invokeWithBoundedNativeMediaInput<InlayBlobMetadata>(invokeCommand, {
                data,
                directCommand: 'native_media_write_inlay_image',
                streamedFinishCommand: 'native_media_write_inlay_finish',
                args: {
                    id: key,
                    name: input.name,
                    ...(options === undefined ? {} : { options }),
                },
            })
        },
    }
}

export function createGatedBlobStore(store: BlobStore, gate: StorageMutationGate): BlobStore {
    const gated: BlobStore = {
        async put(key, data, metadata) {
            const ownedData = data.slice()
            const ownedMetadata = { ...metadata }
            return gate.runKeyedWrite(key, () => store.put(key, ownedData, ownedMetadata))
        },
        read: (key, range) => store.read(key, range),
        stat: (key) => store.stat(key),
        list: (query) => store.list(query),
        remove: (key) => gate.runKeyedWrite(key, () => store.remove(key)),
        resolveUrl: (key) => store.resolveUrl(key),
    }
    if (store.putNewInlayImage) {
        gated.putNewInlayImage = (key, data, input) => {
            const ownedData = data.slice()
            const ownedInput = { ...input }
            return gate.runKeyedWrite(key, () => store.putNewInlayImage!(key, ownedData, ownedInput))
        }
    }
    return gated
}

export type KeyValueStorage = {
    readonly blobStorageKind?: 'opfs'
    setItem(key: string, value: Uint8Array): Promise<unknown>
    getItem(key: string): Promise<Uint8Array | null>
    keys(): Promise<string[]>
    removeItem(key: string): Promise<unknown>
}

export function createStorageBlobKeyValueBackend(storage: KeyValueStorage): BlobKeyValueBackend {
    return {
        async write(key, value) { await storage.setItem(key, value) },
        async read(key) {
            const value = await storage.getItem(key)
            return value ? new Uint8Array(value) : null
        },
        async keys() { return storage.keys() },
        async remove(key) { await storage.removeItem(key) },
    }
}

const opfsHexNamePattern = /^(?:[0-9a-f]{2})+$/

export function createOpfsBlobBackend(directory: FileSystemDirectoryHandle): BlobKeyValueBackend {
    const fileName = (key: string) => Buffer.from(key, 'utf-8').toString('hex')
    const readFileObject = async (key: string): Promise<File | null> => {
        try {
            return await (await directory.getFileHandle(fileName(key))).getFile()
        } catch (error) {
            if (error instanceof DOMException && error.name === 'NotFoundError') return null
            throw error
        }
    }
    return {
        async write(key, value) {
            const stream = await (await directory.getFileHandle(fileName(key), { create: true })).createWritable()
            try {
                await stream.write(value.slice().buffer as ArrayBuffer)
            } catch (error) {
                try {
                    await stream.abort(error)
                } catch (abortError) {
                    console.error('OPFS blob write abort failed', abortError)
                }
                throw error
            }
            await stream.close()
        },
        async read(key) {
            const file = await readFileObject(key)
            return file ? new Uint8Array(await file.arrayBuffer()) : null
        },
        async readRange(key, range) {
            const file = await readFileObject(key)
            if (!file) return null
            return new Uint8Array(await file.slice(range.start, range.endExclusive).arrayBuffer())
        },
        async size(key) {
            return (await readFileObject(key))?.size ?? null
        },
        async keys() {
            const keys: string[] = []
            for await (const entry of directory.values()) {
                if (entry.kind === 'directory' || !opfsHexNamePattern.test(entry.name)) continue
                keys.push(Buffer.from(entry.name, 'hex').toString('utf-8'))
            }
            return keys
        },
        async remove(key) {
            try {
                await directory.removeEntry(fileName(key))
            } catch (error) {
                if (!(error instanceof DOMException) || error.name !== 'NotFoundError') throw error
            }
        },
    }
}

async function listTauriFiles(path: string): Promise<string[]> {
    if (!await exists(path, { baseDir: BaseDirectory.AppData })) return []
    const output: string[] = []
    for (const entry of await readDir(path, { baseDir: BaseDirectory.AppData })) {
        if (!entry.name) continue
        const child = `${path}/${entry.name}`
        if (entry.isDirectory) output.push(...await listTauriFiles(child))
        else output.push(child)
    }
    return output
}

export interface TauriBlobBackendDependencies {
    exists(key: string): Promise<boolean>
    mkdir(path: string): Promise<void>
    write(key: string, value: Uint8Array): Promise<void>
    read(key: string): Promise<Uint8Array>
    list(path: string): Promise<string[]>
    remove(key: string): Promise<void>
    size(key: string): Promise<number>
    open(key: string): Promise<{
        seek(offset: number, mode: SeekMode): Promise<number>
        read(buffer: Uint8Array): Promise<number | null>
        close(): Promise<void>
    }>
    resolveUrl(key: string): Promise<string>
}

export function createTauriBlobBackend(dependencies?: TauriBlobBackendDependencies): BlobKeyValueBackend {
    const deps = dependencies ?? {
        exists: (key: string) => exists(key, { baseDir: BaseDirectory.AppData }),
        mkdir: (path: string) => mkdir(path, { baseDir: BaseDirectory.AppData, recursive: true }).then(() => undefined),
        write: (key: string, value: Uint8Array) => writeFile(key, value, { baseDir: BaseDirectory.AppData }),
        read: (key: string) => readFile(key, { baseDir: BaseDirectory.AppData }),
        list: async (path: string) => listTauriFiles(path),
        remove: (key: string) => remove(key, { baseDir: BaseDirectory.AppData }),
        size: async (key: string) => (await stat(key, { baseDir: BaseDirectory.AppData })).size,
        open: (key: string) => open(key, { read: true, baseDir: BaseDirectory.AppData }),
        resolveUrl: async (key: string) => createTauriNativeMediaUrl(key, await getNativeMediaEndpoint()),
    }
    return {
        async write(key, value) {
            const parent = key.split('/').slice(0, -1).join('/')
            if (parent) await deps.mkdir(parent)
            await deps.write(key, value)
        },
        async read(key) {
            if (!await deps.exists(key)) return null
            return deps.read(key)
        },
        async readRange(key: string, range: BlobReadRange) {
            if (!await deps.exists(key)) return null
            const file = await deps.open(key)
            try {
                await file.seek(range.start, SeekMode.Start)
                const fileSize = await deps.size(key)
                const requested = Math.max(0, Math.min(range.endExclusive, fileSize) - Math.min(range.start, fileSize))
                const result = new Uint8Array(requested)
                let offset = 0
                while (offset < requested) {
                    const count = await file.read(result.subarray(offset))
                    if (count === null || count === 0) break
                    offset += count
                }
                return result.slice(0, offset)
            } finally {
                await file.close()
            }
        },
        async size(key) { return await deps.exists(key) ? deps.size(key) : null },
        async keys() {
            return [
                ...await deps.list('assets'),
                ...await deps.list('blobstore'),
                ...await deps.list('coldstorage'),
            ]
        },
        async remove(key) {
            if (await deps.exists(key)) await deps.remove(key)
        },
        async resolveUrl(key) { return deps.resolveUrl(key) },
    }
}

let productionStore: Promise<BlobStore> | undefined
let productionBackend: Promise<BlobKeyValueBackend> | undefined
let storageProvider: () => Promise<KeyValueStorage | null> = async () => null

export function configureBlobStoreStorageProvider(provider: () => Promise<KeyValueStorage | null>): void {
    storageProvider = provider
    productionStore = undefined
    productionBackend = undefined
}

export function createStorageBlobStore(
    selected: { storage: KeyValueStorage; isAccount: boolean },
): BlobStore {
    if (selected.isAccount) throw new TypeError('AccountStorage cannot be used as a BlobStore backend')
    return createBackedBlobStore(createStorageBlobKeyValueBackend(selected.storage))
}

export async function createBrowserBlobBackend(
    selected: KeyValueStorage | null,
    getOpfsDirectory: () => Promise<FileSystemDirectoryHandle> = () => navigator.storage.getDirectory(),
): Promise<BlobKeyValueBackend> {
    if (!selected) throw new Error('Blob storage provider is not configured')
    if (selected?.blobStorageKind === 'opfs') return createOpfsBlobBackend(await getOpfsDirectory())
    return createStorageBlobKeyValueBackend(selected)
}

export async function readBlobForFacade(
    store: BlobStore,
    key: string,
    rejectMissing: boolean,
): Promise<Uint8Array | null> {
    const value = await store.read(key)
    if (value === null && rejectMissing) throw new Error(`Missing asset: ${key}`)
    return value
}

async function createProductionBackend(): Promise<BlobKeyValueBackend> {
    if (isTauri) return createTauriBlobBackend()
    return createBrowserBlobBackend(await storageProvider())
}

export function getPlatformBlobKeyValueBackend(): Promise<BlobKeyValueBackend> {
    return productionBackend ??= createProductionBackend()
}

async function createProductionStore(): Promise<BlobStore> {
    const backend = await getPlatformBlobKeyValueBackend()
    return isTauri ? createTauriBlobStore(backend) : createBackedBlobStore(backend)
}

/** Defers backend selection so callers can hold a store before storage is initialized. */
const deferredStore: BlobStore = {
    async put(key, data, metadata) { return (await openProductionStore()).put(key, data, metadata) },
    async putNewInlayImage(key, data, input) {
        const store = await openProductionStore()
        if (!store.putNewInlayImage) throw new Error('Native Inlay image writer is unavailable')
        return store.putNewInlayImage(key, data, input)
    },
    async read(key, range) { return (await openProductionStore()).read(key, range) },
    async stat(key) { return (await openProductionStore()).stat(key) },
    async list(query) { return (await openProductionStore()).list(query) },
    async remove(key) { return (await openProductionStore()).remove(key) },
    async resolveUrl(key) { return (await openProductionStore()).resolveUrl(key) },
}

function openProductionStore(): Promise<BlobStore> {
    return productionStore ??= createProductionStore()
}

let gatedProductionStore: BlobStore | null = null

export function configureActiveBlobStore(
    gate: StorageMutationGate,
    authoritative: BlobStore = deferredStore,
    options: { alreadyGuarded?: boolean } = {},
): void {
    gatedProductionStore = options.alreadyGuarded
        ? authoritative
        : createGatedBlobStore(authoritative, gate)
}

export function getLegacyBlobStore(): BlobStore {
    return deferredStore
}

export function getBlobStore(): BlobStore {
    return gatedProductionStore ?? deferredStore
}

export async function resolveBlobStore(): Promise<BlobStore> {
    return getBlobStore()
}
