import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => {
    const initialDatabase = { coldstorage: true, characters: [] as any[] }
    const state = {
        authorityEpoch: 4,
        databaseState: { db: initialDatabase as any },
        revision: 7,
    }
    const runtime = {
        get revision() {
            return state.revision
        },
        capturePersistentMutationToken: vi.fn(async () => ({
            revision: state.revision,
            mutationGeneration: 17,
        })),
        getStorageAuthorityEpoch: vi.fn(() => state.authorityEpoch),
        replacePersistentDatabase: vi.fn(async () => undefined),
    }
    return {
        compactColdStorageDatabase: vi.fn(),
        initialDatabase,
        runtime,
        state,
    }
})

vi.mock('../sionyw', () => ({ fetchProtectedResource: vi.fn() }))
vi.mock('../globalApi.svelte', () => ({ forageStorage: { isAccount: false } }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('../stores.svelte', () => ({
    DBState: mocks.state.databaseState,
    selectedCharID: {
        subscribe(run: (value: number) => void) {
            run(0)
            return () => undefined
        },
    },
}))
vi.mock('../alert', () => ({
    alertClear: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertWait: vi.fn(),
}))
vi.mock('src/lang', () => ({ language: { errors: {} } }))
vi.mock('../storage/coldStorageCompaction', () => ({
    compactColdStorageDatabase: mocks.compactColdStorageDatabase,
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    getActiveConversationSession: vi.fn(() => null),
    getPersistentDataRuntime: () => mocks.runtime,
    getPersistentNavigationGeneration: vi.fn(() => 0),
    replacePersistentDatabase: vi.fn(),
}))

describe('makeColdData persistent replacement authority', () => {
    beforeEach(() => {
        mocks.state.authorityEpoch = 4
        mocks.state.databaseState.db = mocks.initialDatabase
        mocks.state.revision = 7
        mocks.runtime.capturePersistentMutationToken.mockReset().mockImplementation(async () => ({
            revision: mocks.state.revision,
            mutationGeneration: 17,
        }))
        mocks.runtime.getStorageAuthorityEpoch.mockClear()
        mocks.runtime.replacePersistentDatabase.mockReset().mockResolvedValue(undefined)
        mocks.compactColdStorageDatabase.mockReset()
    })

    it('skips runtime coordination when cold storage is disabled', async () => {
        mocks.state.databaseState.db = { coldstorage: false, characters: [] }
        const { makeColdData } = await import('./coldstorage.svelte')

        await expect(makeColdData()).resolves.toBe(false)

        expect(mocks.runtime.capturePersistentMutationToken).not.toHaveBeenCalled()
        expect(mocks.compactColdStorageDatabase).not.toHaveBeenCalled()
    })

    it('captures the source after settling pending mutations and tolerates a storage-only revision advance', async () => {
        const settledDatabase = { coldstorage: true, characters: [{ chaId: 'settled' }] } as any
        const candidate = { coldstorage: true, characters: [{ chaId: 'candidate' }] } as any
        mocks.runtime.capturePersistentMutationToken.mockImplementation(async () => {
            mocks.state.databaseState.db = settledDatabase
            mocks.state.authorityEpoch = 8
            return { revision: 9, mutationGeneration: 17 }
        })
        mocks.compactColdStorageDatabase.mockImplementation(async (database, dependencies) => {
            expect(database).toBe(settledDatabase)
            mocks.state.revision = 10
            await dependencies.replaceDatabase(candidate, 'cold-storage-compaction')
            return true
        })
        const { makeColdData } = await import('./coldstorage.svelte')

        await expect(makeColdData()).resolves.toBe(true)

        expect(mocks.runtime.capturePersistentMutationToken).toHaveBeenCalledWith(
            'cold-storage-compaction',
        )
        expect(mocks.runtime.replacePersistentDatabase).toHaveBeenCalledWith(
            candidate,
            'cold-storage-compaction',
            { expectedMutationGeneration: 17, expectedRevision: 10 },
        )
    })

    it('rejects publication when the database identity changes during compaction', async () => {
        mocks.compactColdStorageDatabase.mockImplementation(async (_database, dependencies) => {
            mocks.state.databaseState.db = { coldstorage: true, characters: [] }
            await dependencies.replaceDatabase({} as any, 'cold-storage-compaction')
            return true
        })
        const { makeColdData } = await import('./coldstorage.svelte')

        await expect(makeColdData()).rejects.toThrow(
            'Database changed during cold storage compaction',
        )
        expect(mocks.runtime.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('rejects publication when the storage authority epoch changes during compaction', async () => {
        mocks.compactColdStorageDatabase.mockImplementation(async (_database, dependencies) => {
            mocks.state.authorityEpoch += 1
            await dependencies.replaceDatabase({} as any, 'cold-storage-compaction')
            return true
        })
        const { makeColdData } = await import('./coldstorage.svelte')

        await expect(makeColdData()).rejects.toThrow(
            'Database changed during cold storage compaction',
        )
        expect(mocks.runtime.replacePersistentDatabase).not.toHaveBeenCalled()
    })

    it('forwards the captured mutation generation while source authority remains unchanged', async () => {
        const candidate = { coldstorage: true, characters: [] } as any
        mocks.runtime.capturePersistentMutationToken.mockResolvedValue({
            revision: 11,
            mutationGeneration: 23,
        })
        mocks.compactColdStorageDatabase.mockImplementation(async (_database, dependencies) => {
            await dependencies.replaceDatabase(candidate, 'cold-storage-compaction')
            return true
        })
        const { makeColdData } = await import('./coldstorage.svelte')

        await makeColdData()

        expect(mocks.runtime.replacePersistentDatabase).toHaveBeenCalledWith(
            candidate,
            'cold-storage-compaction',
            { expectedMutationGeneration: 23, expectedRevision: 7 },
        )
    })
})
