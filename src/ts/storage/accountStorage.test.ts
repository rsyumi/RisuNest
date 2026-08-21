import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => {
    const cache = new Map<string, unknown>()
    const assets = new Map<string, unknown>()
    return {
        cache,
        assets,
        fetchProtectedResource: vi.fn(),
        alertLogin: vi.fn(async () => 'new-token'),
        alertNormalWait: vi.fn(async () => undefined),
        sleep: vi.fn(() => new Promise<void>(() => undefined)),
        cachedForage: {
            getItem: vi.fn(async (key: string) => cache.get(key) ?? null),
            setItem: vi.fn(async (key: string, value: unknown) => {
                cache.set(key, value)
                return value
            }),
        },
        localforage: {
            getItem: vi.fn(async (key: string) => assets.get(key) ?? null),
            setItem: vi.fn(async (key: string, value: unknown) => {
                assets.set(key, value)
                return value
            }),
            createInstance: vi.fn(),
        },
    }
})

mocks.localforage.createInstance.mockReturnValue(mocks.cachedForage)

vi.mock('localforage', () => ({ default: mocks.localforage }))
vi.mock('uuid', () => ({ v4: () => 'fixed-uuid' }))
vi.mock('./database.svelte', () => ({
    getDatabase: () => ({ account: { token: 'account-token', useSync: true } }),
}))
vi.mock('../alert', () => ({
    alertLogin: mocks.alertLogin,
    alertNormalWait: mocks.alertNormalWait,
    alertStore: { set: vi.fn() },
}))
vi.mock('../globalApi.svelte', () => ({
    forageStorage: { keys: vi.fn() },
    getUncleanables: vi.fn(),
    getUncleanablesSync: vi.fn(() => []),
}))
vi.mock('../sionyw', () => ({ fetchProtectedResource: mocks.fetchProtectedResource }))
vi.mock('../util', () => ({ sleep: mocks.sleep }))
vi.mock('src/lang', () => ({ language: { activeTabChange: 'active tab changed' } }))
vi.mock('./databaseRestore', () => ({ completeAccountUnmigration: vi.fn() }))
vi.mock('./persistentDataRuntime.svelte', () => ({ replacePersistentDatabase: vi.fn() }))

function response(body: BodyInit | null, status = 200, headers?: HeadersInit): Response {
    return new Response(body, { status, headers })
}

async function loadStorage() {
    return await import('./accountStorage')
}

beforeEach(() => {
    vi.resetModules()
    mocks.fetchProtectedResource.mockReset()
    mocks.alertLogin.mockReset().mockResolvedValue('new-token')
    mocks.alertNormalWait.mockReset().mockResolvedValue(undefined)
    mocks.sleep.mockReset().mockImplementation(() => new Promise<void>(() => undefined))
    mocks.cachedForage.getItem.mockClear()
    mocks.cachedForage.setItem.mockClear()
    mocks.localforage.getItem.mockClear()
    mocks.localforage.setItem.mockClear()
    mocks.cache.clear()
    mocks.assets.clear()
    mocks.localforage.createInstance.mockReturnValue(mocks.cachedForage)
    localStorage.clear()
    vi.spyOn(Date, 'now').mockReturnValue(1_725_000_000_123)
})

describe('AccountStorage structured wire contract', () => {
    it('writes with the exact session and save-date headers', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 42 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('assets/replaced.png'))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()
        const bytes = new Uint8Array([1, 2, 3])
        const signal = new AbortController().signal

        await expect(storage.writeItem('assets/original.png', bytes, { signal })).resolves.toEqual({
            kind: 'written',
            replacementKey: 'assets/replaced.png',
        })
        expect(mocks.fetchProtectedResource.mock.calls).toEqual([
            ['/api/account/getsessionnumber', { method: 'GET', signal }],
            ['/api/account/write', {
                method: 'POST',
                body: bytes,
                headers: {
                    'content-type': 'application/octet-stream',
                    'x-risu-key': 'assets/original.png',
                    'X-Format': 'nocheck',
                    'x-risu-session': '42',
                    'x-risu-save-date': '1725000000123',
                },
                signal,
            }],
        ])
    })

    it('uses a UUID only for database reads and preserves the cached save date', async () => {
        mocks.cache.set('database/database.bin__date', '1725000000000')
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(new Uint8Array([4, 5])))
            .mockResolvedValueOnce(response(new Uint8Array([6])))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await storage.readItem('database/database.bin')
        await storage.readItem('assets/a.png')

        expect(mocks.fetchProtectedResource.mock.calls[0]).toEqual([
            '/api/account/read/64617461626173652f64617461626173652e62696e|fixed-uuid',
            {
                method: 'GET',
                headers: {
                    'x-risu-key': 'database/database.bin',
                    'x-risu-save-date': '1725000000000',
                },
            },
        ])
        expect(mocks.fetchProtectedResource.mock.calls[1][0]).toBe(
            '/api/account/read/6173736574732f612e706e67',
        )
    })

    it('distinguishes missing and cached read results', async () => {
        mocks.cache.set('database/database.bin', new Uint8Array([7, 8]))
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(null, 204))
            .mockResolvedValueOnce(response(JSON.stringify({ match: true }), 303, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(JSON.stringify({ match: false }), 303, {
                'content-type': 'application/json',
            }))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await expect(storage.readItem('missing')).resolves.toEqual({ kind: 'missing' })
        await expect(storage.readItem('database/database.bin')).resolves.toEqual({
            kind: 'not-modified',
            bytes: new Uint8Array([7, 8]),
        })
        await expect(storage.readItem('database/database.bin')).resolves.toEqual({ kind: 'missing' })
    })

    it('maps 304 writes to the original key and preserves wrapper compatibility', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 7 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(null, 304))
            .mockResolvedValueOnce(response(null, 304))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await expect(storage.writeItem('assets/a.png', new Uint8Array())).resolves.toEqual({
            kind: 'not-modified',
            replacementKey: 'assets/a.png',
        })
        await expect(storage.setItem('assets/a.png', new Uint8Array())).resolves.toBe('assets/a.png')
    })

    it('retries an ordinary 403 after login and exposes a warning 403', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 8 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('forbidden', 403))
            .mockResolvedValueOnce(response('assets/retried.png'))
            .mockResolvedValueOnce(response('warn', 403, { 'x-risu-status': 'warn' }))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await expect(storage.writeItem('assets/a.png', new Uint8Array([1]))).resolves.toEqual({
            kind: 'written',
            replacementKey: 'assets/retried.png',
        })
        expect(mocks.alertLogin).toHaveBeenCalledOnce()
        expect(localStorage.getItem('fallbackRisuToken')).toBe('new-token')
        await expect(storage.writeItem('assets/b.png', new Uint8Array([2]))).resolves.toEqual({
            kind: 'auth-warning',
        })
    })

    it('publishes each successful JSON warning once without turning it into a failure', async () => {
        const warningBody = JSON.stringify({ warning: 'quota nearing limit' })
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 9 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(warningBody, 200, { 'content-type': 'application/json' }))
            .mockResolvedValueOnce(response(warningBody, 200, { 'content-type': 'application/json' }))
        const { AccountStorage, AccountWarning } = await loadStorage()
        const seen: string[] = []
        const unsubscribe = AccountWarning.subscribe((value) => seen.push(value))
        const storage = new AccountStorage()

        await expect(storage.writeItem('database/database.bin', new Uint8Array([1]))).resolves.toEqual({
            kind: 'written',
            replacementKey: warningBody,
        })
        await storage.writeItem('database/database.bin', new Uint8Array([1]))
        unsubscribe()

        expect(seen).toEqual(['', 'quota nearing limit'])
    })

    it('keeps reload-session writes pending after scheduling the existing alert', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 10 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(JSON.stringify({ reloadSession: true }), 200, {
                'content-type': 'application/json',
            }))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()
        let settled = false

        void storage.writeItem('database/database.bin', new Uint8Array([1])).finally(() => {
            settled = true
        })
        await vi.waitFor(() => expect(mocks.alertNormalWait).toHaveBeenCalledOnce())

        expect(mocks.sleep).toHaveBeenCalledOnce()
        expect(settled).toBe(false)
    })

    it('reports cumulative progress and preserves compatibility buffers', async () => {
        const signal = new AbortController().signal
        const stream = new ReadableStream<Uint8Array>({
            start(controller) {
                controller.enqueue(new Uint8Array([1, 2]))
                controller.enqueue(new Uint8Array([3, 4]))
                controller.close()
            },
        })
        mocks.fetchProtectedResource.mockResolvedValueOnce(new Response(stream, {
            status: 200,
            headers: { 'x-body-size': '4' },
        }))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()
        const progress: number[] = []

        await expect(storage.readItem('database/database.bin', {
            signal,
            progress: (ratio) => progress.push(ratio),
        })).resolves.toEqual({ kind: 'value', bytes: new Uint8Array([1, 2, 3, 4]) })
        expect(mocks.fetchProtectedResource.mock.calls[0][1].signal).toBe(signal)
        expect(progress).toEqual([0.5, 1])

        mocks.fetchProtectedResource.mockResolvedValueOnce(response(new Uint8Array([5, 6])))
        await expect(storage.getItem('database/database.bin')).resolves.toEqual(Buffer.from([5, 6]))
    })

    it('maps structured missing and auth-warning results through compatibility wrappers', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(null, 204))
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 11 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('warn', 403, { 'x-risu-status': 'warn' }))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await expect(storage.getItem('missing')).resolves.toBeNull()
        await expect(storage.setItem('database/database.bin', new Uint8Array([1]))).resolves.toBeUndefined()
    })
})
