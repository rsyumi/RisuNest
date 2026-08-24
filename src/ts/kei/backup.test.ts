import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Database } from '../storage/database.svelte'

let database: Partial<Database>

vi.mock('../storage/database.svelte', () => ({
    getDatabase: () => database,
}))
vi.mock('./kei', () => ({
    keiServerURL: () => 'https://kei.example',
}))

const fetchMock = vi.fn(async () => new Response(''))
vi.stubGlobal('fetch', fetchMock)

async function loadSaveDbKei() {
    vi.resetModules()
    return (await import('./backup')).saveDbKei
}

describe('saveDbKei', () => {
    beforeEach(() => {
        vi.useFakeTimers()
        vi.setSystemTime(1_000_000)
        fetchMock.mockClear()
        fetchMock.mockResolvedValue(new Response(''))
        database = {
            account: { id: 'acc', token: 'secret-token', data: {}, kei: true },
        } as Partial<Database>
    })

    afterEach(() => {
        vi.useRealTimers()
    })

    it('posts the full database as JSON to the kei autobackup route', async () => {
        const saveDbKei = await loadSaveDbKei()

        saveDbKei()

        expect(fetchMock).toHaveBeenCalledTimes(1)
        const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
        expect(url).toBe('https://kei.example/autobackup/save')
        expect(init.method).toBe('POST')
        expect(init.headers).toEqual({ 'Content-Type': 'application/json' })
        expect(JSON.parse(init.body as string)).toEqual({
            token: 'secret-token',
            database,
        })
    })

    it('does nothing without an account or with the kei flag off', async () => {
        const saveDbKei = await loadSaveDbKei()

        database = {}
        saveDbKei()
        database = { account: { id: 'acc', token: 'secret-token', data: {} } } as Partial<Database>
        saveDbKei()

        expect(fetchMock).not.toHaveBeenCalled()
    })

    it('sends at most one backup per five minutes', async () => {
        const saveDbKei = await loadSaveDbKei()

        saveDbKei()
        vi.advanceTimersByTime(5 * 60000 - 1)
        saveDbKei()
        expect(fetchMock).toHaveBeenCalledTimes(1)

        vi.advanceTimersByTime(1)
        saveDbKei()
        expect(fetchMock).toHaveBeenCalledTimes(2)
    })

    it('swallows network failures instead of surfacing an unhandled rejection', async () => {
        const saveDbKei = await loadSaveDbKei()
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        fetchMock.mockRejectedValueOnce(new Error('offline'))

        expect(() => saveDbKei()).not.toThrow()
        await vi.waitFor(() => expect(consoleError).toHaveBeenCalled())
        consoleError.mockRestore()
    })
})
