import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import type { PersistentDataStore } from './persistentDataStore'

const PERSISTENT_DATA_DATABASE_NAME = 'risuai-persistent-data'

export function createPersistentDataStore(): PersistentDataStore {
    return new IndexedDbPersistentDataStore(PERSISTENT_DATA_DATABASE_NAME, indexedDB)
}
