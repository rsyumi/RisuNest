import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => {
    const cache = new Map<string, unknown>()
    const assets = new Map<string, unknown>()
    return {
        cache,
        assets,
        fetchProtectedResource: vi.fn(),
        alertLogin: vi.fn(async () => 'new-token'),
        alertNormalWait: vi.fn(async () => undefined),
        sleep: vi.fn((milliseconds: number) => new Promise<void>((resolve) => {
            setTimeout(resolve, milliseconds)
        })),
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
    mocks.sleep.mockReset().mockImplementation((milliseconds: number) => new Promise<void>((resolve) => {
        setTimeout(resolve, milliseconds)
    }))
    mocks.cachedForage.getItem.mockReset().mockImplementation(async (key: string) => (
        mocks.cache.get(key) ?? null
    ))
    mocks.cachedForage.setItem.mockReset().mockImplementation(async (key: string, value: unknown) => {
        mocks.cache.set(key, value)
        return value
    })
    mocks.localforage.getItem.mockReset().mockImplementation(async (key: string) => (
        mocks.assets.get(key) ?? null
    ))
    mocks.localforage.setItem.mockReset().mockImplementation(async (key: string, value: unknown) => {
        mocks.assets.set(key, value)
        return value
    })
    mocks.cache.clear()
    mocks.assets.clear()
    mocks.localforage.createInstance.mockReturnValue(mocks.cachedForage)
    localStorage.clear()
    vi.spyOn(Date, 'now').mockReturnValue(1_725_000_000_123)
})

afterEach(() => {
    vi.useRealTimers()
})

function cancellableResponse(
    status: number,
    headers?: HeadersInit,
    cancel: () => void = vi.fn(),
): { response: Response; cancel: () => void } {
    return {
        response: new Response(new ReadableStream({ cancel }), { status, headers }),
        cancel,
    }
}

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
        await storage.readItem('assets/database-icon.png')

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
            '/api/account/read/6173736574732f64617461626173652d69636f6e2e706e67',
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

    it('does not parse malformed JSON bodies for body-independent statuses', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 70 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(null, 304, {
                'content-type': 'application/json; charset=utf-8',
            }))
            .mockResolvedValueOnce(response('not-json', 403, {
                'content-type': 'application/json; charset=utf-8',
                'x-risu-status': 'warn',
            }))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await expect(storage.writeItem('assets/a.png', new Uint8Array())).resolves.toEqual({
            kind: 'not-modified',
            replacementKey: 'assets/a.png',
        })
        await expect(storage.writeItem('assets/b.png', new Uint8Array())).resolves.toEqual({
            kind: 'auth-warning',
        })
    })

    it('retries an ordinary 403 after login and exposes a warning 403', async () => {
        const retryBody = cancellableResponse(403)
        const warnCancel = vi.fn(() => {
            throw new Error('cancel failed')
        })
        const warnBody = cancellableResponse(403, { 'x-risu-status': 'warn' }, warnCancel)
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 8 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(retryBody.response)
            .mockResolvedValueOnce(response('assets/retried.png'))
            .mockResolvedValueOnce(warnBody.response)
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
        expect(retryBody.cancel).toHaveBeenCalledOnce()
        expect(warnBody.cancel).toHaveBeenCalledOnce()
    })

    it('does not mutate the database cache for warning or failed writes', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 80 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('warn', 403, { 'x-risu-status': 'warn' }))
            .mockResolvedValueOnce(response('failed', 500))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()
        const bytes = new Uint8Array([8, 0])

        await expect(storage.writeItem('database/database.bin', bytes)).resolves.toEqual({
            kind: 'auth-warning',
        })
        await expect(storage.writeItem('database/database.bin', bytes)).rejects.toBe('failed')

        expect(mocks.cachedForage.setItem).not.toHaveBeenCalled()
        expect(mocks.cache.size).toBe(0)
    })

    it('awaits both database cache updates after a successful write', async () => {
        const pending: Array<() => void> = []
        mocks.cachedForage.setItem.mockImplementation((key: string, value: unknown) => (
            new Promise((resolve) => pending.push(() => {
                mocks.cache.set(key, value)
                resolve(value)
            }))
        ))
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 81 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('retry', 403))
            .mockResolvedValueOnce(response('database/database.bin'))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()
        const bytes = new Uint8Array([8, 1])
        let settled = false

        const write = storage.writeItem('database/database.bin', bytes).finally(() => {
            settled = true
        })
        await vi.waitFor(() => expect(pending).toHaveLength(1))
        expect(mocks.alertLogin).toHaveBeenCalledOnce()
        expect(settled).toBe(false)
        pending.shift()!()
        await vi.waitFor(() => expect(pending).toHaveLength(1))
        expect(settled).toBe(false)
        pending.shift()!()

        await expect(write).resolves.toEqual({
            kind: 'written',
            replacementKey: 'database/database.bin',
        })
        expect(mocks.cache.get('database/database.bin')).toEqual(bytes)
        expect(mocks.cache.get('database/database.bin__date')).toBe('1725000000123')
    })

    it('publishes each successful JSON warning once without turning it into a failure', async () => {
        const warningBody = JSON.stringify({ warning: 'quota nearing limit' })
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 9 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(warningBody, 200, {
                'content-type': 'Application/JSON; Charset=UTF-8',
            }))
            .mockResolvedValueOnce(response(warningBody, 200, {
                'content-type': 'application/json; charset=utf-8',
            }))
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
        vi.useFakeTimers()
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 10 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(JSON.stringify({ reloadSession: true }), 200, {
                'content-type': 'application/json; charset=utf-8',
            }))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()
        let settled = false

        void storage.writeItem('database/database.bin', new Uint8Array([1])).finally(() => {
            settled = true
        })
        await vi.waitFor(() => expect(mocks.alertNormalWait).toHaveBeenCalledOnce())
        await vi.advanceTimersByTimeAsync(100_000_001)
        expect(settled).toBe(false)
        expect(mocks.sleep).not.toHaveBeenCalled()
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

    it('fails a 303 cache match when the cached bytes are missing', async () => {
        mocks.fetchProtectedResource.mockResolvedValueOnce(response(
            JSON.stringify({ match: true }),
            303,
            { 'content-type': 'application/json' },
        ))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await expect(storage.readItem('database/database.bin')).rejects.toThrow(
            'Cached account bytes are missing for database/database.bin',
        )
    })

    it('cancels an ignored read 403 body before retrying', async () => {
        const forbidden = cancellableResponse(403)
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(forbidden.response)
            .mockResolvedValueOnce(response(new Uint8Array([9])))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage()

        await expect(storage.readItem('plain-key')).resolves.toEqual({
            kind: 'value',
            bytes: new Uint8Array([9]),
        })
        expect(forbidden.cancel).toHaveBeenCalledOnce()
    })
})
