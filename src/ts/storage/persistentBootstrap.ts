import type { Database } from './database.svelte'
import type { LegacyLocalStorage } from './legacyLocalStorage'
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

export function listLegacyDatabaseBackups(keys: string[]): number[] {
    return keys
        .map((key) => /^database\/dbbackup-(\d+)\.bin$/.exec(key)?.[1])
        .filter((timestamp): timestamp is string => timestamp !== undefined)
        .map(Number)
        .sort((a, b) => b - a)
}

export async function readLegacyDatabaseCandidate(
    storage: LegacyLocalStorage,
    decode: (bytes: Uint8Array) => Promise<Database>,
    onError: (error: unknown) => void = () => undefined,
): Promise<LegacyDatabaseCandidate> {
    try {
        const primary = await storage.getItem('database/database.bin')
        if (primary) return { database: await decode(primary), source: 'primary' }
    } catch (error) {
        onError(error)
    }
    for (const backup of listLegacyDatabaseBackups(await storage.keys())) {
        try {
            const bytes = await storage.getItem(`database/dbbackup-${backup}.bin`)
            if (bytes) return { database: await decode(bytes), source: 'fallback' }
        } catch (error) {
            onError(error)
        }
    }
    return { database: {} as Database, source: 'default' }
}

export async function replaceExplicitBootstrapCandidate(dependencies: {
    loadCandidate(): Promise<Uint8Array | null>
    decodeCandidate(bytes: Uint8Array): Promise<Database>
    replaceCandidate(database: Database): Promise<void>
    onError?(error: unknown): void
}): Promise<boolean> {
    try {
        const bytes = await dependencies.loadCandidate()
        if (!bytes) return false
        await dependencies.replaceCandidate(await dependencies.decodeCandidate(bytes))
        return true
    } catch (error) {
        dependencies.onError?.(error)
        return false
    }
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
