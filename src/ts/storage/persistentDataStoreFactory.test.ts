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
    const [{ createPersistentDataStore }, { IndexedDbPersistentDataStore }] = await Promise.all([
        import('./persistentDataStoreFactory'),
        import('./indexedDbPersistentDataStore'),
    ])
    return { store: createPersistentDataStore(), IndexedDbPersistentDataStore }
}

describe('createPersistentDataStore', () => {
    let originalGlobals: Map<string, PropertyDescriptor | undefined>

    beforeEach(() => {
        vi.resetModules()
        vi.clearAllMocks()
        originalGlobals = new Map(['indexedDB', 'IDBKeyRange'].map((key) =>
            [key, Object.getOwnPropertyDescriptor(globalThis, key)]))
        Object.defineProperties(globalThis, {
            indexedDB: { configurable: true, writable: true, value: new IDBFactory() },
            IDBKeyRange: { configurable: true, writable: true, value: IDBKeyRange },
        })
    })

    afterEach(() => {
        try {
            vi.restoreAllMocks()
        } finally {
            for (const [key, descriptor] of originalGlobals) {
                if (descriptor) Object.defineProperty(globalThis, key, descriptor)
                else Reflect.deleteProperty(globalThis, key)
            }
            vi.resetModules()
        }
    })

    test('Tauri always selects SQLite', async () => {
        expect((await createStore(true)).store).toBeInstanceOf(sqlite.SqlitePersistentDataStore)
    })

    test('native selection never reads browser persistence or constructs its backend', async () => {
        const forbiddenAccess = vi.fn(() => { throw new Error('Unexpected browser persistence access') })
        for (const key of ['indexedDB', 'IDBKeyRange']) {
            Object.defineProperty(globalThis, key, { configurable: true, get: forbiddenAccess })
        }
        const browser = await import('./indexedDbPersistentDataStore')
        const constructor = vi.spyOn(browser, 'IndexedDbPersistentDataStore')
        expect((await createStore(true)).store).toBeInstanceOf(sqlite.SqlitePersistentDataStore)
        expect(forbiddenAccess).not.toHaveBeenCalled()
        expect(constructor).not.toHaveBeenCalled()
    })

    test('non-Tauri selects IndexedDB', async () => {
        const { store, IndexedDbPersistentDataStore } = await createStore(false)
        expect(store).toBeInstanceOf(IndexedDbPersistentDataStore)
    })
})
