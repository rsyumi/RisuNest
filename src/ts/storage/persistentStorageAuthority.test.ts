import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { createPersistentStorageAuthority } from './persistentStorageAuthority'
import { fixtureDatabase } from './tests/persistentDataFixtures'

describe('persistent storage authority', () => {
    it('shares one raw store, gate, and coherent payload controller', async () => {
        const rawStore = new IndexedDbPersistentDataStore(
            `authority-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        const events: string[] = []
        const gate = {
            async runWrite<T>(operation: () => Promise<T>) {
                events.push('write')
                return operation()
            },
            async runMigration<T>(operation: () => Promise<T>) {
                events.push('migration')
                return operation()
            },
        }
        const authority = createPersistentStorageAuthority(rawStore, gate)
        await authority.store.open()
        await authority.store.replaceFromDatabase(fixtureDatabase)
        authority.activePayloadRoot.install(await rawStore.readActiveTuple())
        await authority.store.replaceFromDatabase(fixtureDatabase)
        const normalized = await authority.activePayloadRoot.refresh()

        expect(events).toEqual(['write', 'write'])
        expect(authority.rawStore).toBe(rawStore)
        expect(authority.gate).toBe(gate)
        expect(normalized).toMatchObject({ revision: 2, payloadGeneration: 'legacy' })
        expect(await authority.activePayloadRoot.getActiveRoot()).toEqual({ kind: 'legacy' })
    })

    it('keeps prepared replacement methods raw while ordinary replacement is gated', async () => {
        const rawStore = new IndexedDbPersistentDataStore(
            `authority-raw-${crypto.randomUUID()}`,
            indexedDB,
            IDBKeyRange,
        )
        const runWrite = vi.fn()
        const authority = createPersistentStorageAuthority(rawStore, {
            runWrite: async <T,>(operation: () => Promise<T>) => {
                runWrite()
                return operation()
            },
            runMigration: async (operation) => operation(),
        })
        await authority.store.open()
        await authority.store.replaceFromDatabase(fixtureDatabase)
        const prepared = await authority.rawStore.prepareReplacement(
            fixtureDatabase,
            'manifest',
            'payload_1',
        )

        expect(runWrite).toHaveBeenCalledOnce()
        expect(await authority.rawStore.listPreparedReplacements()).toEqual([prepared])
    })
})
