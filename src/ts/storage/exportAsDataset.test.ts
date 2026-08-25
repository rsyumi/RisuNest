import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    materialize: vi.fn(),
    downloadFile: vi.fn(),
    alertNormal: vi.fn(),
}))

vi.mock('./database.svelte', () => ({
    getDatabase: () => ({
        characters: [{
            type: 'character',
            chaId: 'catalog-only',
            name: 'Catalog only',
            chats: [],
        }],
    }),
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    materializePersistentDatabaseSnapshot: mocks.materialize,
}))
vi.mock('../globalApi.svelte', () => ({ downloadFile: mocks.downloadFile }))
vi.mock('../alert', () => ({ alertNormal: mocks.alertNormal }))
vi.mock('src/lang', () => ({ language: { successExport: 'exported' } }))

describe('exportAsDataset', () => {
    beforeEach(() => {
        mocks.materialize.mockReset().mockResolvedValue({
            characters: [
                {
                    type: 'character',
                    chaId: 'complete-character',
                    name: 'Complete character',
                    desc: 'Description',
                    globalLore: [{ key: 'lore' }],
                    chats: [{ id: 'chat', message: [{ role: 'user', data: 'hello' }] }],
                },
                {
                    type: 'group',
                    chaId: 'group',
                    name: 'Skipped group',
                    chats: [{ id: 'group-chat', message: [] }],
                },
            ],
        })
        mocks.downloadFile.mockReset().mockResolvedValue(undefined)
        mocks.alertNormal.mockReset()
    })

    it('exports every conversation from an authoritative detached snapshot', async () => {
        const { exportAsDataset } = await import('./exportAsDataset')

        await exportAsDataset()

        expect(mocks.materialize).toHaveBeenCalledWith('dataset-export')
        expect(mocks.downloadFile).toHaveBeenCalledOnce()
        const [name, bytes] = mocks.downloadFile.mock.calls[0] as [string, Uint8Array]
        expect(name).toBe('dataset.json')
        expect(JSON.parse(Buffer.from(bytes).toString('utf8'))).toEqual([{
            name: 'Complete character',
            description: 'Description',
            chats: [{ role: 'user', data: 'hello' }],
            lorebook: [{ key: 'lore' }],
        }])
        expect(mocks.alertNormal).toHaveBeenCalledWith('exported')
    })
})
