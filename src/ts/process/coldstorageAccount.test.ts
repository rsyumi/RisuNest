import { beforeEach, describe, expect, it, vi } from 'vitest'
import { decompressSync } from 'fflate'

const mocks = vi.hoisted(() => ({
    fetchProtectedResource: vi.fn(),
}))

vi.mock('../sionyw', () => ({ fetchProtectedResource: mocks.fetchProtectedResource }))
vi.mock('../globalApi.svelte', () => ({ forageStorage: { isAccount: true } }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('../stores.svelte', () => ({ DBState: { db: { characters: [] } } }))
vi.mock('../alert', () => ({
    alertClear: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertWait: vi.fn(),
}))
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('../storage/coldStorageCompaction', () => ({ compactColdStorageDatabase: vi.fn() }))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({ replacePersistentDatabase: vi.fn() }))

beforeEach(() => {
    mocks.fetchProtectedResource.mockReset()
})

describe('official account cold storage transport', () => {
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
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(null, { status: 204 }))
        const { getAccountColdStorageItem } = await import('./coldstorage.svelte')

        await expect(getAccountColdStorageItem('cold-missing')).resolves.toBeNull()
    })

    it('writes compressed JSON with exact headers and an abort signal', async () => {
        const signal = new AbortController().signal
        const value = { message: [{ role: 'user', data: 'synthetic' }] }
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(null, { status: 200 }))
        const { setAccountColdStorageItem } = await import('./coldstorage.svelte')

        await expect(setAccountColdStorageItem('cold-b', value, signal)).resolves.toBe(true)
        const [url, request] = mocks.fetchProtectedResource.mock.calls[0]
        expect(url).toBe('/hub/account/coldstorage')
        expect(request).toMatchObject({
            method: 'POST',
            headers: {
                'x-risu-key': 'cold-b',
                'content-type': 'application/octet-stream',
            },
            signal,
        })
        expect(JSON.parse(new TextDecoder().decode(decompressSync(request.body)))).toEqual(value)
    })

    it('accepts only status 200 as a successful write', async () => {
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(null, { status: 201 }))
        const { setAccountColdStorageItem } = await import('./coldstorage.svelte')

        await expect(setAccountColdStorageItem('cold-c', {})).resolves.toBe(false)
    })
})
