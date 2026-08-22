import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import type { PersistentDataStore } from './persistentDataStore'
import { createPersistentStorageAuthority, type PersistentStorageAuthority } from './persistentStorageAuthority'
import { createStorageMutationGate } from './storageMutationGate'

const PERSISTENT_DATA_DATABASE_NAME = 'risuai-persistent-data'

export function createPersistentDataStore(): PersistentDataStore {
    return new IndexedDbPersistentDataStore(PERSISTENT_DATA_DATABASE_NAME, indexedDB, IDBKeyRange)
}

let persistentStorageAuthority: PersistentStorageAuthority | null = null

export function getPersistentStorageAuthority(): PersistentStorageAuthority {
    persistentStorageAuthority ??= createPersistentStorageAuthority(
        createPersistentDataStore(),
        createStorageMutationGate(),
    )
    return persistentStorageAuthority
}

export function getRawPersistentDataStore(): PersistentDataStore {
    return getPersistentStorageAuthority().rawStore
}

export function getPersistentDataStore(): PersistentDataStore {
    return getPersistentStorageAuthority().store
}
