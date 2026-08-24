import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

const platform = vi.hoisted(() => ({ isTauri: false }))
const sqlite = vi.hoisted(() => ({
    SqlitePersistentDataStore: class SqlitePersistentDataStore {},
}))

vi.mock('../platform', () => platform)
vi.mock('./sqlitePersistentDataStore', () => sqlite)

async function createStore(isTauri: boolean) {
    platform.isTauri = isTauri
    vi.resetModules()
    const [{ createPersistentDataStore }, { IndexedDbPersistentDataStore }] = await Promise.all([
        import('./persistentDataStoreFactory'),
        import('./indexedDbPersistentDataStore'),
    ])
    return { store: createPersistentDataStore(), IndexedDbPersistentDataStore }
}

describe('createPersistentDataStore', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        Object.assign(globalThis, {
            indexedDB: new IDBFactory(),
            IDBKeyRange,
        })
    })

    afterEach(() => {
        vi.resetModules()
    })

    test('Tauri always selects SQLite', async () => {
        expect((await createStore(true)).store).toBeInstanceOf(sqlite.SqlitePersistentDataStore)
    })

    test('non-Tauri selects IndexedDB', async () => {
        const { store, IndexedDbPersistentDataStore } = await createStore(false)
        expect(store).toBeInstanceOf(IndexedDbPersistentDataStore)
    })
})
