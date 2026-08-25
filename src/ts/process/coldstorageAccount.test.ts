import { beforeEach, describe, expect, it, vi } from 'vitest'
import { decompressSync } from 'fflate'

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
vi.mock('../storage/coldStorageCompaction', () => ({ compactColdStorageDatabase: vi.fn() }))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({ replacePersistentDatabase: vi.fn() }))

beforeEach(() => {
    mocks.fetchProtectedResource.mockReset()
    mocks.database.characters = []
})

describe('official account cold storage transport', () => {
    it('collects decoded cold backup payloads without retaining encoded bytes', async () => {
        const key = '12345678-1234-1234-1234-123456789abc'
        const value = { message: [{ role: 'user', data: 'cold backup' }] }
        mocks.database.characters = [{ coldstorage: key }]
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(
            (await import('fflate')).compressSync(
                new TextEncoder().encode(JSON.stringify(value)),
            ).buffer as ArrayBuffer,
            { status: 200 },
        ))
        const { collectColdStorageBackupPayloads } = await import('./coldstorage.svelte')

        await expect(collectColdStorageBackupPayloads(mocks.database)).resolves.toEqual({
            payloads: [{
                key,
                backupName: `coldstorage_${key}.json`,
                value,
            }],
            missingKeys: [],
            invalidKeys: [],
        })
    })

    it('skips public cleanup for an incomplete catalog working set', async () => {
        const { createCatalogCharacterStub } = await import('../storage/workingSetCatalog')
        mocks.database.characters = [createCatalogCharacterStub({
            id: 'char-a',
            name: 'Alpha',
            configuredIndex: 0,
            recentAt: 0,
            trashed: false,
            conversationCount: 1,
            type: 'character',
        })]
        const list = vi.fn(async () => ['unused'])
        const remove = vi.fn(async () => undefined)
        const { cleanColdStorage, configureLocalColdStorageRuntime } = await import(
            './coldstorage.svelte'
        )
        configureLocalColdStorageRuntime({
            list,
            remove,
        } as any)

        await cleanColdStorage()

        expect(mocks.fetchProtectedResource).not.toHaveBeenCalled()
        expect(list).not.toHaveBeenCalled()
        expect(remove).not.toHaveBeenCalled()
    })

    it('keeps an account-mode startup cold payload in the local authoritative store', async () => {
        const files = new Map<string, Uint8Array>()
        const directory = {
            getFileHandle: vi.fn(async (name: string) => ({
                createWritable: async () => ({
                    write: async (value: ArrayBuffer | Uint8Array) => {
                        files.set(name, value instanceof Uint8Array ? value.slice() : new Uint8Array(value.slice(0)))
                    },
                    close: async () => undefined,
                }),
                getFile: async () => ({
                    arrayBuffer: async () => files.get(name)!.slice().buffer,
                }),
            })),
        }
        const value = { message: [{ role: 'user', data: 'local cold payload' }] }
        const { getColdStorageItem, setLocalColdStorageItem, configureLocalColdStorageRuntime } = await import('./coldstorage.svelte')
        const { createLocalColdStorageRuntime } = await import('../storage/localColdStorageRuntime')
        const { createLegacyBrowserOpfsColdPayloadStore } = await import('../storage/platformColdPayloadStore')
        configureLocalColdStorageRuntime(createLocalColdStorageRuntime(
            createLegacyBrowserOpfsColdPayloadStore(async () => directory as unknown as FileSystemDirectoryHandle),
        ))

        await expect(setLocalColdStorageItem('cold-local', value)).resolves.toBe(true)
        await expect(getColdStorageItem('cold-local', { accountFallback: true })).resolves.toEqual(value)

        expect(mocks.fetchProtectedResource).not.toHaveBeenCalled()
        expect(directory.getFileHandle).toHaveBeenCalledWith(
            'coldstorage_cold-local.json',
            { create: true },
        )
    })

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

    it('cancels the ignored successful write body', async () => {
        const cancel = vi.fn()
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(
            new ReadableStream({ cancel }),
            { status: 200 },
        ))
        const { setAccountColdStorageItem } = await import('./coldstorage.svelte')

        await expect(setAccountColdStorageItem('cold-d', {})).resolves.toBe(true)
        expect(cancel).toHaveBeenCalledOnce()
    })

    it('preserves read and write AbortError identity', async () => {
        const readAbort = new DOMException('read cancelled', 'AbortError')
        const writeAbort = new DOMException('write cancelled', 'AbortError')
        const { getAccountColdStorageItem, setAccountColdStorageItem } = await import(
            './coldstorage.svelte'
        )

        mocks.fetchProtectedResource.mockRejectedValueOnce(readAbort)
        await expect(getAccountColdStorageItem('cold-read-abort')).rejects.toBe(readAbort)

        mocks.fetchProtectedResource.mockRejectedValueOnce(writeAbort)
        await expect(setAccountColdStorageItem('cold-write-abort', {})).rejects.toBe(writeAbort)
    })
})
