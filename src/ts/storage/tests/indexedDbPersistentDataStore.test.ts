import { IDBFactory } from 'fake-indexeddb'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { persistentDataStoreContract } from './persistentDataStoreContract'

let databaseSequence = 0

persistentDataStoreContract(async () => {
    const indexedDB = new IDBFactory()
    const databaseName = `persistent-store-contract-${databaseSequence++}`
    const store = new IndexedDbPersistentDataStore(databaseName, indexedDB)
    await store.open()

    return {
        store,
        async reopen() {
            const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB)
            await reopened.open()
            return reopened
        },
    }
})
