import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { RevisionConflictError, type PersistentDataStore } from '../persistentDataStore'
import { bootstrapPersistentDatabase, listLegacyDatabaseBackups } from '../persistentBootstrap'
import { fixtureDatabase } from './persistentDataFixtures'

function createStore(input?: {
    revision?: number
    database?: Database
    replacementRevision?: number
}): PersistentDataStore {
    const revision = input?.revision ?? 0
    const database = structuredClone(input?.database ?? fixtureDatabase)
    return {
        open: vi.fn(async () => undefined),
        readRoot: vi.fn(async () => ({
            revision,
            value: Object.fromEntries(
                Object.entries(database).filter(([key]) => key !== 'characters'),
            ) as Omit<Database, 'characters'>,
        })),
        materializeDatabase: vi.fn(async () => structuredClone(database)),
        replaceFromDatabase: vi.fn(async () => ({
            revision: input?.replacementRevision ?? revision + 1,
        })),
        queryCharacters: vi.fn(),
        readCharacter: vi.fn(),
        queryConversations: vi.fn(),
        readConversation: vi.fn(),
        readConversationWindow: vi.fn(),
        commit: vi.fn(),
        acquireRevision: vi.fn(),
    }
}

describe('bootstrapPersistentDatabase', () => {
    it('lists legacy backups newest first without including unrelated keys', () => {
        expect(listLegacyDatabaseBackups([
            'database/dbbackup-12.bin',
            'database/database.bin',
            'database/dbbackup-105.bin',
            'database/dbbackup-invalid.bin',
            'assets/dbbackup-999.bin',
        ])).toEqual([105, 12])
    })

    it('imports one prepared legacy candidate when the store is blank', async () => {
        const store = createStore({ revision: 0, replacementRevision: 1 })
        const legacy = structuredClone(fixtureDatabase)
        legacy.username = 'Legacy user'
        const prepared = structuredClone(legacy)
        prepared.username = 'Prepared user'
        const loadLegacyCandidate = vi.fn(async () => ({ database: legacy, source: 'primary' as const }))
        const prepareDatabase = vi.fn(async () => structuredClone(prepared))

        const result = await bootstrapPersistentDatabase({
            store,
            loadLegacyCandidate,
            prepareDatabase,
        })

        expect(loadLegacyCandidate).toHaveBeenCalledTimes(1)
        expect(prepareDatabase).toHaveBeenCalledTimes(1)
        expect(store.replaceFromDatabase).toHaveBeenCalledTimes(1)
        expect(store.replaceFromDatabase).toHaveBeenCalledWith(prepared, 0)
        expect(result).toEqual({ database: prepared, revision: 1, source: 'primary' })
    })

    it.each(['fallback', 'default'] as const)(
        'imports exactly one prepared %s candidate when the store is blank',
        async (source) => {
            const store = createStore({ revision: 0, replacementRevision: 1 })
            const candidate = structuredClone(fixtureDatabase)
            const loadLegacyCandidate = vi.fn(async () => ({ database: candidate, source }))

            const result = await bootstrapPersistentDatabase({
                store,
                loadLegacyCandidate,
                prepareDatabase: async (database) => structuredClone(database),
            })

            expect(loadLegacyCandidate).toHaveBeenCalledTimes(1)
            expect(store.replaceFromDatabase).toHaveBeenCalledWith(candidate, 0)
            expect(result.source).toBe(source)
        },
    )

    it('uses a nonblank persistent revision without invoking the legacy loader', async () => {
        const persistent = structuredClone(fixtureDatabase)
        persistent.username = 'Persistent user'
        const store = createStore({ revision: 7, database: persistent })
        const loadLegacyCandidate = vi.fn()

        const result = await bootstrapPersistentDatabase({
            store,
            loadLegacyCandidate,
            prepareDatabase: async (database) => structuredClone(database),
        })

        expect(loadLegacyCandidate).not.toHaveBeenCalled()
        expect(store.materializeDatabase).toHaveBeenCalledWith(7)
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(result).toEqual({ database: persistent, revision: 7, source: 'persistent' })
    })

    it('stores one preparation change to persistent data and skips a canonical no-op', async () => {
        const persistent = structuredClone(fixtureDatabase)
        const changed = structuredClone(persistent)
        changed.formatversion = 5
        const changedStore = createStore({ revision: 4, database: persistent, replacementRevision: 5 })

        const changedResult = await bootstrapPersistentDatabase({
            store: changedStore,
            loadLegacyCandidate: vi.fn(),
            prepareDatabase: async () => structuredClone(changed),
        })

        expect(changedStore.replaceFromDatabase).toHaveBeenCalledTimes(1)
        expect(changedStore.replaceFromDatabase).toHaveBeenCalledWith(changed, 4)
        expect(changedResult).toEqual({ database: changed, revision: 5, source: 'persistent' })

        const reordered = Object.fromEntries(Object.entries(persistent).reverse()) as unknown as Database
        const unchangedStore = createStore({ revision: 4, database: persistent })
        const unchangedResult = await bootstrapPersistentDatabase({
            store: unchangedStore,
            loadLegacyCandidate: vi.fn(),
            prepareDatabase: async () => reordered,
        })

        expect(unchangedStore.replaceFromDatabase).not.toHaveBeenCalled()
        expect(unchangedResult.revision).toBe(4)
        expect(unchangedResult.database).toBe(reordered)
    })

    it('replaces a nonblank local revision only for an injected explicit candidate', async () => {
        const local = structuredClone(fixtureDatabase)
        const remote = structuredClone(fixtureDatabase)
        remote.username = 'Remote user'
        const store = createStore({ revision: 3, database: local, replacementRevision: 4 })

        const result = await bootstrapPersistentDatabase({
            store,
            loadLegacyCandidate: vi.fn(),
            prepareDatabase: async (database) => structuredClone(database),
            explicitCandidate: remote,
            explicitSource: 'account',
        })

        expect(store.replaceFromDatabase).toHaveBeenCalledTimes(1)
        expect(store.replaceFromDatabase).toHaveBeenCalledWith(remote, 3)
        expect(result).toEqual({ database: remote, revision: 4, source: 'account' })
    })

    it('rejects persistent normalization when another connection commits first', async () => {
        const indexedDB = new IDBFactory()
        const first = new IndexedDbPersistentDataStore('bootstrap-normalization-cas', indexedDB, IDBKeyRange)
        const second = new IndexedDbPersistentDataStore('bootstrap-normalization-cas', indexedDB, IDBKeyRange)
        await Promise.all([first.open(), second.open()])
        await first.replaceFromDatabase(structuredClone(fixtureDatabase))

        const prepared = structuredClone(fixtureDatabase)
        prepared.formatversion = 5
        const concurrent = structuredClone(fixtureDatabase)
        concurrent.username = 'Concurrent user'
        const { characters: _characters, ...concurrentRoot } = concurrent

        await expect(
            bootstrapPersistentDatabase({
                store: first,
                loadLegacyCandidate: vi.fn(),
                prepareDatabase: async () => {
                    await second.commit({ expectedRevision: 1, root: concurrentRoot })
                    return prepared
                },
            }),
        ).rejects.toBeInstanceOf(RevisionConflictError)

        expect((await second.readRoot()).revision).toBe(2)
        expect((await second.materializeDatabase()).username).toBe('Concurrent user')
    })

    it('rejects a revision-zero import when another connection imports first', async () => {
        const indexedDB = new IDBFactory()
        const first = new IndexedDbPersistentDataStore('bootstrap-blank-cas', indexedDB, IDBKeyRange)
        const second = new IndexedDbPersistentDataStore('bootstrap-blank-cas', indexedDB, IDBKeyRange)
        await Promise.all([first.open(), second.open()])
        const candidate = structuredClone(fixtureDatabase)
        candidate.username = 'Stale starter'
        const concurrent = structuredClone(fixtureDatabase)
        concurrent.username = 'First importer'

        await expect(
            bootstrapPersistentDatabase({
                store: first,
                loadLegacyCandidate: async () => ({ database: candidate, source: 'primary' }),
                prepareDatabase: async (database) => {
                    await second.replaceFromDatabase(concurrent)
                    return structuredClone(database)
                },
            }),
        ).rejects.toBeInstanceOf(RevisionConflictError)

        expect((await second.readRoot()).revision).toBe(1)
        expect((await second.materializeDatabase()).username).toBe('First importer')
    })
})
