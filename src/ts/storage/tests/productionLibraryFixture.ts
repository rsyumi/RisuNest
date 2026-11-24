import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { vi } from 'vitest'
import type { Database } from '../database.svelte'
import type { PersistentDataStore } from '../persistentDataStore'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'

/**
 * Stores the fixture library, after `prepare` edits it, in a new IndexedDB store and starts the
 * production runtime on it with nothing selected. The calling test file mocks
 * `persistentDataStoreFactory` to return the store `install` receives. Each call loads a fresh
 * module graph because the production runtime is a module singleton bound to the first store it
 * sees, so the caller imports the modules it drives after this resolves.
 */
export async function openProductionLibrary(
    name: string,
    install: (store: PersistentDataStore) => void,
    prepare: (initial: Database) => void = () => {},
) {
    vi.resetModules()
    // stores.svelte must load before characters: the reverse order lets the
    // stores module effect read database.svelte before it is initialized.
    const stores = await import('../../stores.svelte')
    const database = await import('../database.svelte')
    const { getPersistentDataRuntime } = await import('../persistentDataRuntime.svelte')
    const { projectCompleteScalableWorkingSet } = await import('../workingSetCatalog')

    const initial = database.normalizeDatabaseDefaults(structuredClone(fixtureDatabase) as Database)
    prepare(initial)
    const store = new IndexedDbPersistentDataStore(name, new IDBFactory(), IDBKeyRange)
    await store.open()
    install(store)
    const { revision } = await store.replaceFromDatabase(initial)
    database.setDatabaseLite(projectCompleteScalableWorkingSet(initial, null, revision, new Set()))
    stores.selectedCharID.set(-1)
    await getPersistentDataRuntime().initializeActiveWorkingSet(database.getDatabase())
    return { initial, store, stores, database }
}
