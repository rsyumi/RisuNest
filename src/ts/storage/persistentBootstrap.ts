import type { Database } from './database.svelte'
import type { DataRevision, PersistentDataStore } from './persistentDataStore'
import { canonicalJson } from './saveCoordinator'

export interface PersistentBootstrapDependencies {
    store: PersistentDataStore
    prepareDatabase(database: Database): Promise<Database>
}

export interface PersistentBootstrapResult {
    database: Database
    revision: DataRevision
}

/**
 * Opens the authoritative local store. A store that has never been written starts from an empty
 * database, because existing RisuAI data only enters the app through backup import.
 */
export async function bootstrapPersistentDatabase(
    dependencies: PersistentBootstrapDependencies,
): Promise<PersistentBootstrapResult> {
    await dependencies.store.open()
    const active = await dependencies.store.readRoot()

    if (active.revision === 0) {
        const database = await dependencies.prepareDatabase({} as Database)
        const { revision } = await dependencies.store.replaceFromDatabase(database, 0)
        return { database, revision }
    }

    const persistent = await dependencies.store.materializeDatabase(active.revision)
    const database = await dependencies.prepareDatabase(persistent)
    if (canonicalJson(database) === canonicalJson(persistent)) {
        return { database, revision: active.revision }
    }
    const { revision } = await dependencies.store.replaceFromDatabase(database, active.revision)
    return { database, revision }
}
