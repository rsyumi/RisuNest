import type { BlobKeyValueBackend } from './blobStore'
import type { ColdPayloadStore } from './coldPayloadStore'
import type { StorageMutationGate } from './storageMutationGate'

export interface ColdPayloadKeyMapper {
    key(logicalKey: string): string
    prefix: string
    suffix: string
}

export function createKeyValueColdPayloadStore(
    backend: BlobKeyValueBackend,
    mapper: ColdPayloadKeyMapper,
): ColdPayloadStore {
    const logicalKey = (physicalKey: string): string | null => {
        if (!physicalKey.startsWith(mapper.prefix) || !physicalKey.endsWith(mapper.suffix)) return null
        return physicalKey.slice(mapper.prefix.length, physicalKey.length - mapper.suffix.length)
    }
    return {
        async read(key) {
            const value = await backend.read(mapper.key(key))
            return value === null ? null : value.slice()
        },
        async write(key, data) {
            await backend.write(mapper.key(key), data.slice())
        },
        async list() {
            return (await backend.keys()).map(logicalKey).filter((key): key is string => key !== null).sort()
        },
        async remove(key) {
            await backend.remove(mapper.key(key))
        },
    }
}

export function createLegacyTauriColdPayloadStore(backend: BlobKeyValueBackend): ColdPayloadStore {
    return createKeyValueColdPayloadStore(backend, {
        key: (id) => `coldstorage/${id}.json`,
        prefix: 'coldstorage/',
        suffix: '.json',
    })
}

export function createLegacyNodeColdPayloadStore(backend: BlobKeyValueBackend): ColdPayloadStore {
    return createKeyValueColdPayloadStore(backend, {
        key: (id) => `coldstorage/${id}`,
        prefix: 'coldstorage/',
        suffix: '',
    })
}

export function createLegacyOpfsColdPayloadStore(backend: BlobKeyValueBackend): ColdPayloadStore {
    return createKeyValueColdPayloadStore(backend, {
        key: (id) => `coldstorage_${id}.json`,
        prefix: 'coldstorage_',
        suffix: '.json',
    })
}

/** Resolves the OPFS root lazily so browsers without it still boot, as they did before cold payloads were rooted. */
export function createLegacyBrowserOpfsColdPayloadStore(
    source: FileSystemDirectoryHandle | (() => Promise<FileSystemDirectoryHandle>),
): ColdPayloadStore {
    const fileName = (key: string) => `coldstorage_${key}.json`
    let pending: Promise<FileSystemDirectoryHandle> | undefined
    const resolveDirectory = () => {
        if (typeof source !== 'function') return Promise.resolve(source)
        return pending ??= source().catch((error) => {
            pending = undefined
            throw error
        })
    }
    return {
        async read(key) {
            try {
                const directory = await resolveDirectory()
                const file = await (await directory.getFileHandle(fileName(key))).getFile()
                return new Uint8Array(await file.arrayBuffer())
            } catch (error) {
                if (error instanceof DOMException && error.name === 'NotFoundError') return null
                throw error
            }
        },
        async write(key, data) {
            const directory = await resolveDirectory()
            const writable = await (await directory.getFileHandle(fileName(key), { create: true })).createWritable()
            try {
                await writable.write(data.slice().buffer as ArrayBuffer)
            } catch (error) {
                try {
                    await writable.abort(error)
                } catch (abortError) {
                    console.error('OPFS cold payload write abort failed', abortError)
                }
                throw error
            }
            await writable.close()
        },
        async list() {
            const keys: string[] = []
            for await (const [name] of (await resolveDirectory()).entries()) {
                if (name.startsWith('coldstorage_') && name.endsWith('.json')) {
                    keys.push(name.slice('coldstorage_'.length, -'.json'.length))
                }
            }
            return keys.sort()
        },
        async remove(key) {
            try {
                await (await resolveDirectory()).removeEntry(fileName(key))
            } catch (error) {
                if (!(error instanceof DOMException) || error.name !== 'NotFoundError') throw error
            }
        },
    }
}

export function createGatedColdPayloadStore(
    store: ColdPayloadStore,
    gate: StorageMutationGate,
): ColdPayloadStore {
    return {
        read: (key) => store.read(key),
        async write(key, data) {
            const ownedData = data.slice()
            return gate.runWrite(() => store.write(key, ownedData))
        },
        list: () => store.list(),
        remove: (key) => gate.runWrite(() => store.remove(key)),
    }
}
