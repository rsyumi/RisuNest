import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import { RevisionConflictError, type PersistentDataStore } from '../persistentDataStore'
import { bootstrapPersistentDatabase } from '../persistentBootstrap'
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
    it('starts a blank store from one prepared empty database', async () => {
        const store = createStore({ revision: 0, replacementRevision: 1 })
        const prepared = structuredClone(fixtureDatabase)
        const prepareDatabase = vi.fn(async () => structuredClone(prepared))

        const result = await bootstrapPersistentDatabase({ store, prepareDatabase })

        expect(prepareDatabase).toHaveBeenCalledTimes(1)
        expect(prepareDatabase).toHaveBeenCalledWith({})
        expect(store.materializeDatabase).not.toHaveBeenCalled()
        expect(store.replaceFromDatabase).toHaveBeenCalledWith(prepared, 0)
        expect(result).toEqual({ database: prepared, revision: 1 })
    })

    it('reads a nonblank persistent revision without rewriting it', async () => {
        const persistent = structuredClone(fixtureDatabase)
        persistent.username = 'Persistent user'
        const store = createStore({ revision: 7, database: persistent })

        const result = await bootstrapPersistentDatabase({
            store,
            prepareDatabase: async (database) => structuredClone(database),
        })

        expect(store.materializeDatabase).toHaveBeenCalledWith(7)
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(result).toEqual({ database: persistent, revision: 7 })
    })

    it('stores exactly one preparation change to persistent data', async () => {
        const persistent = structuredClone(fixtureDatabase)
        const changed = structuredClone(persistent)
        changed.username = 'Normalized user'
        const store = createStore({ revision: 4, database: persistent, replacementRevision: 5 })

        const result = await bootstrapPersistentDatabase({
            store,
            prepareDatabase: async () => structuredClone(changed),
        })

        expect(store.replaceFromDatabase).toHaveBeenCalledTimes(1)
        expect(store.replaceFromDatabase).toHaveBeenCalledWith(changed, 4)
        expect(result).toEqual({ database: changed, revision: 5 })
    })

    it('reports a conflict when another connection writes the blank store first', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `bootstrap-blank-conflict-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const rival = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await rival.open()

        await expect(bootstrapPersistentDatabase({
            store,
            prepareDatabase: async () => {
                await rival.replaceFromDatabase(structuredClone(fixtureDatabase))
                return structuredClone(fixtureDatabase)
            },
        })).rejects.toBeInstanceOf(RevisionConflictError)
    })

    it('reports a conflict when another connection writes during normalization', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = `bootstrap-normalize-conflict-${crypto.randomUUID()}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const rival = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        await rival.open()
        await store.replaceFromDatabase(structuredClone(fixtureDatabase))

        await expect(bootstrapPersistentDatabase({
            store,
            prepareDatabase: async (database) => {
                const normalized = structuredClone(database)
                normalized.username = 'Normalized user'
                await rival.replaceFromDatabase(structuredClone(fixtureDatabase))
                return normalized
            },
        })).rejects.toBeInstanceOf(RevisionConflictError)
    })
})
