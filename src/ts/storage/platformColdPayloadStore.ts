import type { BlobKeyValueBackend } from './blobStore'
import type { ColdPayloadStore, RootedColdPayloadStoreFactory } from './coldPayloadStore'
import { assertGeneratedStorageRootId, type BlobStorageRoot } from './storageRoot'

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

function utf8Hex(value: string): string {
    return Buffer.from(value, 'utf-8').toString('hex')
}

export function generatedColdPayloadKey(generation: string, logicalKey: string): string {
    assertGeneratedStorageRootId(generation)
    return `blobstore/generations/${generation}/coldstorage/${utf8Hex(logicalKey)}.bin`
}

function generatedMapper(generation: string): ColdPayloadKeyMapper {
    assertGeneratedStorageRootId(generation)
    const prefix = `blobstore/generations/${generation}/coldstorage/`
    return {
        key: (logicalKey) => generatedColdPayloadKey(generation, logicalKey),
        prefix,
        suffix: '.bin',
    }
}

function decodeGeneratedKey(physicalKey: string, generation: string): string | null {
    const { prefix, suffix } = generatedMapper(generation)
    if (!physicalKey.startsWith(prefix) || !physicalKey.endsWith(suffix)) return null
    const hex = physicalKey.slice(prefix.length, -suffix.length)
    if (!hex || hex.length % 2 !== 0 || !/^[0-9a-f]+$/.test(hex)) return null
    const decoded = Buffer.from(hex, 'hex').toString('utf-8')
    return utf8Hex(decoded) === hex ? decoded : null
}

function createGeneratedColdPayloadStore(backend: BlobKeyValueBackend, generation: string): ColdPayloadStore {
    const mapper = generatedMapper(generation)
    return {
        async read(key) {
            const value = await backend.read(mapper.key(key))
            return value === null ? null : value.slice()
        },
        async write(key, data) { await backend.write(mapper.key(key), data.slice()) },
        async list() {
            return (await backend.keys())
                .map((key) => decodeGeneratedKey(key, generation))
                .filter((key): key is string => key !== null)
                .sort()
        },
        async remove(key) { await backend.remove(mapper.key(key)) },
    }
}

export function createRootedColdPayloadStoreFactory(input: {
    legacy: ColdPayloadStore
    generatedBackend: BlobKeyValueBackend
}): RootedColdPayloadStoreFactory {
    const generated = new Map<string, ColdPayloadStore>()
    return {
        open(root: BlobStorageRoot) {
            if (root.kind === 'legacy') return input.legacy
            assertGeneratedStorageRootId(root.id)
            let store = generated.get(root.id)
            if (!store) {
                store = createGeneratedColdPayloadStore(input.generatedBackend, root.id)
                generated.set(root.id, store)
            }
            return store
        },
    }
}
