import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    fetchProtectedResource: vi.fn(),
    database: { characters: [] as any[] },
}))

vi.mock('../sionyw', () => ({ fetchProtectedResource: mocks.fetchProtectedResource }))
vi.mock('../globalApi.svelte', () => ({ forageStorage: { isAccount: true } }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('../stores.svelte', () => ({ DBState: { db: mocks.database } }))
vi.mock('../alert', () => ({
    alertClear: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertWait: vi.fn(),
}))
vi.mock('src/lang', () => ({ language: {} }))

beforeEach(() => {
    mocks.fetchProtectedResource.mockReset()
    mocks.database.characters = []
})

describe('official account cold storage read', () => {
    it('reads the exact remote key and decompresses a successful payload', async () => {
        const { compressSync } = await import('fflate')
        const value = { character: { chaId: 'synthetic-character' } }
        const signal = new AbortController().signal
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(
            compressSync(new TextEncoder().encode(JSON.stringify(value))).buffer as ArrayBuffer,
            { status: 200 },
        ))
        const { getAccountColdStorageItem } = await import('./coldstorage.svelte')

        await expect(getAccountColdStorageItem('cold-a', signal)).resolves.toEqual(value)
        expect(mocks.fetchProtectedResource).toHaveBeenCalledWith('/hub/account/coldstorage', {
            method: 'GET',
            headers: { 'x-risu-key': 'cold-a' },
            signal,
        })
    })

    it('returns missing for every non-200 remote read', async () => {
        const cancel = vi.fn()
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(
            new ReadableStream({ cancel }),
            { status: 404 },
        ))
        const { getAccountColdStorageItem } = await import('./coldstorage.svelte')

        await expect(getAccountColdStorageItem('cold-missing')).resolves.toBeNull()
        expect(cancel).toHaveBeenCalledOnce()
    })

    it('preserves read AbortError identity', async () => {
        const readAbort = new DOMException('read cancelled', 'AbortError')
        const { getAccountColdStorageItem } = await import('./coldstorage.svelte')

        mocks.fetchProtectedResource.mockRejectedValueOnce(readAbort)
        await expect(getAccountColdStorageItem('cold-read-abort')).rejects.toBe(readAbort)
    })

    it('never issues a cold storage write request', async () => {
        const transport = await import('./coldstorage.svelte')

        expect(Object.keys(transport).filter((name) => /^set/.test(name))).toEqual([])
    })
})
