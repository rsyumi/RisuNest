import localforage from 'localforage'
import {
    BaseDirectory, SeekMode, exists, mkdir, open, readDir, readFile, remove, stat, writeFile,
} from '@tauri-apps/plugin-fs'
import { convertFileSrc } from '@tauri-apps/api/core'
import { appDataDir, join } from '@tauri-apps/api/path'
import { isTauri } from '../platform'
import {
    createKeyValueBlobStore,
    type BlobKeyValueBackend,
    type BlobPhysicalKeyMapper,
    type BlobReadRange,
    type BlobStore,
} from './blobStore'
import { assertGeneratedStorageRootId, type BlobStorageRoot } from './storageRoot'
import type { StorageMutationGate } from './storageMutationGate'
export type { BlobStorageRoot } from './storageRoot'

export interface RootedBlobStoreFactory {
    open(root: BlobStorageRoot): BlobStore
}

export interface ActiveBlobRootResolver {
    getActiveRoot(): Promise<BlobStorageRoot>
}

function rootPrefix(root: BlobStorageRoot): string {
    if (root.kind === 'legacy') return ''
    assertGeneratedStorageRootId(root.id)
    return `blobstore/generations/${root.id}/`
}

function logicalKeyHex(key: string): string {
    return Buffer.from(key, 'utf-8').toString('hex')
}

export function physicalBlobKeys(root: BlobStorageRoot, logicalKey: string) {
    const prefix = rootPrefix(root)
    return {
        payload: logicalKey.startsWith('assets/')
            ? `${prefix}${logicalKey}`
            : `${prefix}blobstore/inlays/${logicalKeyHex(logicalKey)}.bin`,
        metadata: `${prefix}blobstore/metadata/${logicalKeyHex(logicalKey)}.json`,
    }
}

function mapperFor(root: BlobStorageRoot): BlobPhysicalKeyMapper {
    const prefix = rootPrefix(root)
    return {
        payload: (key) => physicalBlobKeys(root, key).payload,
        metadata: (key) => physicalBlobKeys(root, key).metadata,
        metadataPrefix: `${prefix}blobstore/metadata/`,
        legacyAssetPrefix: `${prefix}assets/`,
    }
}

export function createKeyValueRootedBlobStoreFactory(backend: BlobKeyValueBackend): RootedBlobStoreFactory {
    const stores = new Map<string, BlobStore>()
    return {
        open(root) {
            const identity = root.kind === 'legacy' ? 'legacy' : `generation:${root.id}`
            let store = stores.get(identity)
            if (!store) {
                store = createKeyValueBlobStore(backend, mapperFor(root))
                stores.set(identity, store)
            }
            return store
        },
    }
}

export function createResolvingBlobStore(factory: RootedBlobStoreFactory, resolver: ActiveBlobRootResolver): BlobStore {
    const resolve = async () => factory.open(await resolver.getActiveRoot())
    return {
        async put(key, data, metadata) { return (await resolve()).put(key, data, metadata) },
        async read(key, range) { return (await resolve()).read(key, range) },
        async stat(key) { return (await resolve()).stat(key) },
        async list(query) { return (await resolve()).list(query) },
        async remove(key) { return (await resolve()).remove(key) },
        async resolveUrl(key) { return (await resolve()).resolveUrl(key) },
    }
}

export interface RefreshingActiveBlobRootResolver extends ActiveBlobRootResolver {
    refresh(): Promise<unknown>
}

export function createGatedResolvingBlobStore(
    factory: RootedBlobStoreFactory,
    resolver: RefreshingActiveBlobRootResolver,
    gate: StorageMutationGate,
): BlobStore {
    const resolve = async () => factory.open(await resolver.getActiveRoot())
    const resolveAfterRefresh = async () => {
        await resolver.refresh()
        return resolve()
    }
    return {
        async put(key, data, metadata) {
            const ownedData = data.slice()
            const ownedMetadata = { ...metadata }
            return gate.runWrite(async () => (await resolveAfterRefresh()).put(key, ownedData, ownedMetadata))
        },
        async read(key, range) { return (await resolve()).read(key, range) },
        async stat(key) { return (await resolve()).stat(key) },
        async list(query) { return (await resolve()).list(query) },
        async remove(key) {
            return gate.runWrite(async () => (await resolveAfterRefresh()).remove(key))
        },
        async resolveUrl(key) { return (await resolve()).resolveUrl(key) },
    }
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
            } finally {
                await stream.close()
            }
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
            for await (const entry of directory.values()) keys.push(Buffer.from(entry.name, 'hex').toString('utf-8'))
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
        resolveUrl: async (key: string) => convertFileSrc(await join(await appDataDir(), key)),
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
            return [...await deps.list('assets'), ...await deps.list('blobstore')]
        },
        async remove(key) {
            if (await deps.exists(key)) await deps.remove(key)
        },
        async resolveUrl(key) { return deps.resolveUrl(key) },
    }
}

const browserLocalStorage = localforage.createInstance({ name: 'risuai' }) as unknown as KeyValueStorage
const legacyRootResolver: ActiveBlobRootResolver = { async getActiveRoot() { return { kind: 'legacy' } } }
let productionFactory: Promise<RootedBlobStoreFactory> | undefined
let productionBackend: Promise<BlobKeyValueBackend> | undefined
let storageProvider: () => Promise<KeyValueStorage | null> = async () => browserLocalStorage

export function configureBlobStoreStorageProvider(provider: () => Promise<KeyValueStorage | null>): void {
    storageProvider = provider
    productionFactory = undefined
    productionBackend = undefined
}

export function createStorageRootedBlobStoreFactory(
    selected: { storage: KeyValueStorage; isAccount: boolean },
): RootedBlobStoreFactory {
    if (selected.isAccount) throw new TypeError('AccountStorage cannot be used as a BlobStore backend')
    return createKeyValueRootedBlobStoreFactory(createStorageBlobKeyValueBackend(selected.storage))
}

export async function createBrowserBlobBackend(
    selected: KeyValueStorage | null,
    getOpfsDirectory: () => Promise<FileSystemDirectoryHandle> = () => navigator.storage.getDirectory(),
): Promise<BlobKeyValueBackend> {
    if (selected?.blobStorageKind === 'opfs') return createOpfsBlobBackend(await getOpfsDirectory())
    return createStorageBlobKeyValueBackend(selected ?? browserLocalStorage)
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

async function createProductionFactory(): Promise<RootedBlobStoreFactory> {
    return createKeyValueRootedBlobStoreFactory(await getPlatformBlobKeyValueBackend())
}

const deferredFactory: RootedBlobStoreFactory = {
    open(root) {
        const openStore = async () => (await (productionFactory ??= createProductionFactory())).open(root)
        return {
            async put(key, data, metadata) { return (await openStore()).put(key, data, metadata) },
            async read(key, range) { return (await openStore()).read(key, range) },
            async stat(key) { return (await openStore()).stat(key) },
            async list(query) { return (await openStore()).list(query) },
            async remove(key) { return (await openStore()).remove(key) },
            async resolveUrl(key) { return (await openStore()).resolveUrl(key) },
        }
    },
}

let activeResolver: ActiveBlobRootResolver = legacyRootResolver

export function setActiveBlobRootResolver(resolver: ActiveBlobRootResolver): void {
    activeResolver = resolver
}

export function getRootedBlobStoreFactory(): RootedBlobStoreFactory {
    return deferredFactory
}

export function getBlobStore(): BlobStore {
    return createResolvingBlobStore(deferredFactory, { getActiveRoot: () => activeResolver.getActiveRoot() })
}

export async function resolveBlobStore(): Promise<BlobStore> {
    return deferredFactory.open(await activeResolver.getActiveRoot())
}
