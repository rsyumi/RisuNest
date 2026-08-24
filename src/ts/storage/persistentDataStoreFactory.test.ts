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
        localStorage.clear()
        vi.clearAllMocks()
        Object.assign(globalThis, {
            indexedDB: new IDBFactory(),
            IDBKeyRange,
        })
    })

    afterEach(() => {
        vi.resetModules()
    })

    test('selects SQLite by default in Tauri', async () => {
        expect((await createStore(true)).store).toBeInstanceOf(sqlite.SqlitePersistentDataStore)
    })

    test.each(['', 'indexeddb', 'IndexedDB', 'sqlite', ' IndexedDB '])(
        'keeps SQLite in Tauri regardless of the obsolete backend override %j',
        async (value) => {
            localStorage.setItem('risuForcePersistentBackend', value)

            expect((await createStore(true)).store).toBeInstanceOf(sqlite.SqlitePersistentDataStore)
        },
    )

    test('selects IndexedDB outside Tauri', async () => {
        localStorage.setItem('risuForcePersistentBackend', 'indexeddb')

        const { store, IndexedDbPersistentDataStore } = await createStore(false)
        expect(store).toBeInstanceOf(IndexedDbPersistentDataStore)
    })
})
