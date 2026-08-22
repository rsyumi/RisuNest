import { createMutationGatedPersistentDataStore } from './mutationGatedPersistentDataStore'
import type { PersistentDataStore } from './persistentDataStore'
import type { StorageMutationGate } from './storageMutationGate'

export interface PersistentStorageAuthority {
    rawStore: PersistentDataStore
    store: PersistentDataStore
    gate: StorageMutationGate
}

export function createPersistentStorageAuthority(
    rawStore: PersistentDataStore,
    gate: StorageMutationGate,
): PersistentStorageAuthority {
    return {
        rawStore,
        store: createMutationGatedPersistentDataStore(rawStore, gate),
        gate,
    }
}
