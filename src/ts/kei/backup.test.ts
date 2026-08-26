import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Database } from '../storage/database.svelte'

let database: Partial<Database>
let persistentDatabase: Partial<Database>
const materializePersistentDatabaseSnapshot = vi.fn()
const getPersistentDataRuntime = vi.fn()
const runNativeKeiBackupJob = vi.fn()

vi.mock('../storage/database.svelte', () => ({
    getDatabase: () => database,
}))
vi.mock('./kei', () => ({
    keiServerURL: () => 'https://kei.example',
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    materializePersistentDatabaseSnapshot,
    getPersistentDataRuntime,
}))
vi.mock('./nativeBackup', () => ({
    runNativeKeiBackupJob,
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
        persistentDatabase = {
            account: { id: 'acc', token: 'secret-token', data: {}, kei: true },
            characters: [{
                type: 'character',
                chaId: 'complete-character',
                chats: [{ id: 'complete-chat', message: [{ role: 'user', data: 'complete' }] }],
            }],
        } as Partial<Database>
        materializePersistentDatabaseSnapshot.mockReset().mockResolvedValue(persistentDatabase)
        getPersistentDataRuntime.mockReset().mockReturnValue({ revision: 7 })
        runNativeKeiBackupJob.mockReset().mockResolvedValue(false)
    })

    afterEach(() => {
        vi.useRealTimers()
    })

    it('posts the full database as JSON to the kei autobackup route', async () => {
        const saveDbKei = await loadSaveDbKei()

        await saveDbKei()

        expect(materializePersistentDatabaseSnapshot).toHaveBeenCalledWith('kei-auto-backup')
        expect(fetchMock).toHaveBeenCalledTimes(1)
        const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit]
        expect(url).toBe('https://kei.example/autobackup/save')
        expect(init.method).toBe('POST')
        expect(init.headers).toEqual({ 'Content-Type': 'application/json' })
        expect(JSON.parse(init.body as string)).toEqual({
            token: 'secret-token',
            database: persistentDatabase,
        })
    })

    it('uses the native job without materializing the database when it is available', async () => {
        const saveDbKei = await loadSaveDbKei()
        runNativeKeiBackupJob.mockResolvedValueOnce(true)

        await saveDbKei()

        expect(runNativeKeiBackupJob).toHaveBeenCalledWith({
            runtime: { revision: 7 },
            url: 'https://kei.example/autobackup/save',
            accountId: 'acc',
            token: 'secret-token',
        })
        expect(materializePersistentDatabaseSnapshot).not.toHaveBeenCalled()
        expect(fetchMock).not.toHaveBeenCalled()
    })

    it('keeps the JavaScript backup as the pre-request capability fallback', async () => {
        const saveDbKei = await loadSaveDbKei()

        await saveDbKei()

        expect(runNativeKeiBackupJob).toHaveBeenCalledOnce()
        expect(materializePersistentDatabaseSnapshot).toHaveBeenCalledWith('kei-auto-backup')
        expect(fetchMock).toHaveBeenCalledTimes(1)
    })

    it('does not retry in JavaScript after an accepted native job fails', async () => {
        const saveDbKei = await loadSaveDbKei()
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        runNativeKeiBackupJob.mockRejectedValueOnce(new Error('native request failed'))

        await saveDbKei()

        expect(materializePersistentDatabaseSnapshot).not.toHaveBeenCalled()
        expect(fetchMock).not.toHaveBeenCalled()
        expect(consoleError).toHaveBeenCalledWith(
            'Kei auto backup failed:',
            expect.objectContaining({ message: 'native request failed' }),
        )
        consoleError.mockRestore()
    })

    it('does nothing without an account or with the kei flag off', async () => {
        const saveDbKei = await loadSaveDbKei()

        database = {}
        await saveDbKei()
        database = { account: { id: 'acc', token: 'secret-token', data: {} } } as Partial<Database>
        await saveDbKei()

        expect(fetchMock).not.toHaveBeenCalled()
        expect(materializePersistentDatabaseSnapshot).not.toHaveBeenCalled()
    })

    it('sends at most one backup per five minutes', async () => {
        const saveDbKei = await loadSaveDbKei()

        await saveDbKei()
        vi.advanceTimersByTime(5 * 60000 - 1)
        await saveDbKei()
        expect(fetchMock).toHaveBeenCalledTimes(1)

        vi.advanceTimersByTime(1)
        await saveDbKei()
        expect(fetchMock).toHaveBeenCalledTimes(2)
    })

    it('swallows network failures instead of surfacing an unhandled rejection', async () => {
        const saveDbKei = await loadSaveDbKei()
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        fetchMock.mockRejectedValueOnce(new Error('offline'))

        await expect(saveDbKei()).resolves.toBeUndefined()
        await vi.waitFor(() => expect(consoleError).toHaveBeenCalled())
        consoleError.mockRestore()
    })

    it('does not publish when the live account and materialized snapshot disagree', async () => {
        const saveDbKei = await loadSaveDbKei()
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        persistentDatabase.account = {
            id: 'different-account',
            token: 'different-token',
            data: {},
            kei: true,
        }

        await saveDbKei()

        expect(fetchMock).not.toHaveBeenCalled()
        expect(consoleError).toHaveBeenCalledWith(
            'Kei auto backup failed:',
            expect.objectContaining({ message: 'Kei account changed during backup materialization' }),
        )
        consoleError.mockRestore()
    })
})
