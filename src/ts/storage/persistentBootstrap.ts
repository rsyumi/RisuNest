import type { Database } from './database.svelte'
import type { DataRevision, PersistentDataStore } from './persistentDataStore'
import { canonicalJson } from './saveCoordinator'

export type PersistentBootstrapSource = 'persistent' | 'primary' | 'fallback' | 'default' | 'account'

export interface LegacyDatabaseCandidate {
    database: Database
    source: Exclude<PersistentBootstrapSource, 'persistent' | 'account'>
}

export interface PersistentBootstrapDependencies {
    store: PersistentDataStore
    loadLegacyCandidate(): Promise<LegacyDatabaseCandidate>
    prepareDatabase(database: Database): Promise<Database>
    explicitCandidate?: Database | null
    explicitSource?: 'account'
}

export interface PersistentBootstrapResult {
    database: Database
    revision: DataRevision
    source: PersistentBootstrapSource
}

export async function bootstrapPersistentDatabase(
    dependencies: PersistentBootstrapDependencies,
): Promise<PersistentBootstrapResult> {
    await dependencies.store.open()
    const active = await dependencies.store.readRoot()

    let database: Database
    let revision = active.revision
    let source: PersistentBootstrapSource

    if (revision > 0) {
        const persistent = await dependencies.store.materializeDatabase(revision)
        database = await dependencies.prepareDatabase(persistent)
        source = 'persistent'
        if (canonicalJson(database) !== canonicalJson(persistent)) {
            revision = (await dependencies.store.replaceFromDatabase(database, revision)).revision
        }
    } else {
        const legacy = await dependencies.loadLegacyCandidate()
        database = await dependencies.prepareDatabase(legacy.database)
        revision = (await dependencies.store.replaceFromDatabase(database, revision)).revision
        source = legacy.source
    }

    if (dependencies.explicitCandidate !== undefined && dependencies.explicitCandidate !== null) {
        database = await dependencies.prepareDatabase(dependencies.explicitCandidate)
        revision = (await dependencies.store.replaceFromDatabase(database, revision)).revision
        source = dependencies.explicitSource ?? 'account'
    }

    return { database, revision, source }
}
