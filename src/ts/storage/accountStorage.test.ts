import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { AccountNativeOfficialWriteAttemptContext } from './accountStorage'
import type { NativeOfficialAccountFlow } from './sync/nativeOfficialAccountFlow'
import {
    SaveCoordinator,
    makeDatabase,
    makeStore,
    captureRoot,
} from './saveCoordinator.testSupport'

const mocks = vi.hoisted(() => {
    const cache = new Map<string, unknown>()
    const assets = new Map<string, unknown>()
    const blobAssets = new Map<string, Uint8Array>()
    const database = { account: { token: 'account-token', useSync: true }, characters: [] as any[] }
    return {
        cache,
        assets,
        blobAssets,
        database,
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
        completeAccountUnmigration: vi.fn(),
        getUncleanablesSync: vi.fn(),
        getColdStorageItem: vi.fn(),
        getAccountColdStorageItem: vi.fn(),
        setLocalColdStorageItem: vi.fn(),
        materializePersistentDatabaseSnapshotWithRevision: vi.fn(),
        replacePersistentDatabase: vi.fn(),
        runtime: { revision: 11 },
        isTauri: false,
        alertSet: vi.fn(),
    }
})

mocks.localforage.createInstance.mockReturnValue(mocks.cachedForage)

vi.mock('localforage', () => ({ default: mocks.localforage }))
vi.mock('uuid', () => ({ v4: () => 'fixed-uuid' }))
vi.mock('./database.svelte', () => ({
    getDatabase: () => mocks.database,
}))
vi.mock('../alert', () => ({
    alertLogin: mocks.alertLogin,
    alertNormalWait: mocks.alertNormalWait,
    alertStore: { set: mocks.alertSet },
}))
vi.mock('../globalApi.svelte', () => ({
    forageStorage: { keys: vi.fn() },
    getUncleanables: vi.fn(),
    getUncleanablesSync: mocks.getUncleanablesSync,
}))
vi.mock('../sionyw', () => ({ fetchProtectedResource: mocks.fetchProtectedResource }))
vi.mock('../util', () => ({ sleep: mocks.sleep }))
vi.mock('src/lang', () => ({
    language: {
        activeTabChange: 'active tab changed',
        accountUnmigration: {
            preparing: 'Preparing local data',
            cold: 'Checking cold data',
            assets: 'Checking assets',
            finishing: 'Disabling account sync',
        },
    },
}))
vi.mock('./databaseRestore', async (importOriginal) => ({
    ...await importOriginal<typeof import('./databaseRestore')>(),
    completeAccountUnmigration: mocks.completeAccountUnmigration,
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => mocks.runtime,
    replacePersistentDatabase: mocks.replacePersistentDatabase,
    materializePersistentDatabaseSnapshotWithRevision:
        mocks.materializePersistentDatabaseSnapshotWithRevision,
}))
vi.mock('../platform', () => ({
    get isTauri() {
        return mocks.isTauri
    },
}))
vi.mock('./platformBlobStore', () => ({
    resolveBlobStore: async () => ({
        read: async (key: string) => mocks.blobAssets.get(key) ?? null,
    }),
}))
vi.mock('../drive/backupAssets', () => ({
    selectLegacyBackupAssetKeys: (keys: string[]) => keys.filter((key) => key.startsWith('assets/')),
}))
vi.mock('./accountAssetAccess', () => ({
    storeActiveAsset: async (_store: unknown, key: string, bytes: Uint8Array) => {
        mocks.blobAssets.set(key, bytes.slice())
    },
}))
vi.mock('../process/coldstorage.svelte', () => ({
    getColdStorageItem: mocks.getColdStorageItem,
    getAccountColdStorageItem: mocks.getAccountColdStorageItem,
    isColdStorageBackupData: (value: unknown) => typeof value === 'object' && value !== null,
    listColdDataKeys: async () => ['cold-character-key'],
    setLocalColdStorageItem: mocks.setLocalColdStorageItem,
}))

function response(body: BodyInit | null, status = 200, headers?: HeadersInit): Response {
    return new Response(body, { status, headers })
}

async function loadStorage() {
    return await import('./accountStorage')
}

beforeEach(() => {
    mocks.alertSet.mockClear()
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
    mocks.blobAssets.clear()
    mocks.database.account = { token: 'account-token', useSync: true }
    mocks.database.characters = []
    mocks.completeAccountUnmigration.mockReset().mockImplementation(async (_database, dependencies) => {
        await dependencies.prepareResources()
    })
    mocks.getUncleanablesSync.mockReset().mockReturnValue([])
    mocks.getColdStorageItem.mockReset().mockResolvedValue(null)
    mocks.getAccountColdStorageItem.mockReset().mockResolvedValue(null)
    mocks.setLocalColdStorageItem.mockReset().mockResolvedValue(true)
    mocks.materializePersistentDatabaseSnapshotWithRevision.mockReset().mockResolvedValue({
        database: mocks.database,
        revision: 11,
        mutationGeneration: 17,
    })
    mocks.replacePersistentDatabase.mockReset().mockResolvedValue(undefined)
    mocks.runtime.revision = 12
    mocks.isTauri = false
    mocks.localforage.createInstance.mockReset().mockReturnValue(mocks.cachedForage)
    localStorage.clear()
    vi.spyOn(Date, 'now').mockReset().mockReturnValue(1_725_000_000_123)
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
    it('keeps a newer edit and account access when an unmigration download outlives its snapshot', async () => {
        vi.useFakeTimers()
        const database = makeDatabase()
        database.account = mocks.database.account as typeof database.account
        const store = makeStore()
        const coordinator = new SaveCoordinator({
            store,
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: vi.fn(),
        })
        coordinator.initialize(11)
        mocks.materializePersistentDatabaseSnapshotWithRevision.mockResolvedValue({
            database: structuredClone(database),
            revision: 11,
            mutationGeneration: 0,
        })
        const actual =
            await vi.importActual<typeof import('./databaseRestore')>('./databaseRestore')
        mocks.completeAccountUnmigration.mockImplementation(actual.completeAccountUnmigration)
        mocks.replacePersistentDatabase.mockImplementation((candidate, reason, options) =>
            coordinator.replacePersistentDatabase(candidate, reason, options),
        )
        mocks.getAccountColdStorageItem.mockResolvedValue({ character: database.characters[0] })
        mocks.getUncleanablesSync.mockReturnValue(['assets/remote.png'])
        let finishDownload!: (value: Response) => void
        mocks.fetchProtectedResource.mockImplementation(
            () =>
                new Promise((resolve) => {
                    finishDownload = resolve
                }),
        )
        localStorage.setItem('accountst', 'able')
        localStorage.setItem('fallbackRisuToken', JSON.stringify(database.account))
        const { unMigrationAccount } = await loadStorage()
        const operation = unMigrationAccount()
        const rejection = expect(operation).rejects.toThrow('mutation generation')
        await vi.waitFor(() => expect(mocks.fetchProtectedResource).toHaveBeenCalledOnce())
        database.username = 'Newer local edit'
        coordinator.markPersistentDataDirty(1)
        finishDownload(response(Uint8Array.of(7)))
        await rejection
        expect(database.username).toBe('Newer local edit')
        expect(database.account?.useSync).toBe(true)
        expect(mocks.database.account.useSync).toBe(true)
        expect(store.replaceFromDatabase).not.toHaveBeenCalled()
        expect(localStorage.getItem('accountst')).toBe('able')
        expect(localStorage.getItem('fallbackRisuToken')).not.toBeNull()
        expect(mocks.blobAssets.get('assets/remote.png')).toEqual(Uint8Array.of(7))
    })

    it('can retry a failed download and clears markers only after successful replacement', async () => {
        const actual =
            await vi.importActual<typeof import('./databaseRestore')>('./databaseRestore')
        mocks.completeAccountUnmigration.mockImplementation(actual.completeAccountUnmigration)
        mocks.getAccountColdStorageItem.mockResolvedValue({ character: {} })
        mocks.getUncleanablesSync.mockReturnValue(['assets/retry.png'])
        mocks.fetchProtectedResource
            .mockRejectedValueOnce(new Error('offline'))
            .mockResolvedValueOnce(response(Uint8Array.of(8)))
        localStorage.setItem('accountst', 'able')
        localStorage.setItem('dosync', 'sync')
        localStorage.setItem('fallbackRisuToken', JSON.stringify(mocks.database.account))
        const reload = vi.spyOn(location, 'reload').mockImplementation(() => undefined)
        mocks.replacePersistentDatabase.mockImplementation(async (candidate) => {
            expect(candidate.account).toBeNull()
            expect(localStorage.getItem('accountst')).toBe('able')
            expect(mocks.blobAssets.get('assets/retry.png')).toEqual(Uint8Array.of(8))
        })
        const { unMigrationAccount } = await loadStorage()
        await expect(unMigrationAccount()).rejects.toThrow('offline')
        expect(localStorage.getItem('accountst')).toBe('able')
        expect(reload).not.toHaveBeenCalled()
        await unMigrationAccount()
        expect(localStorage.getItem('accountst')).toBeNull()
        expect(localStorage.getItem('dosync')).toBe('avoid')
        expect(localStorage.getItem('fallbackRisuToken')).toBeNull()
        expect(reload).toHaveBeenCalledOnce()
        reload.mockRestore()
    })
    it.each([true, false])(
        'publishes JSON warnings on 403 before auth handling (warn header: %s)',
        async (warn) => {
            mocks.fetchProtectedResource
                .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 1 })))
                .mockResolvedValueOnce(
                    response(
                        JSON.stringify({ warning: 'account quota', reloadSession: true }),
                        403,
                        {
                            'content-type': 'application/json; charset=utf-8',
                            ...(warn ? { 'x-risu-status': 'warn' } : {}),
                        },
                    ),
                )
                .mockResolvedValueOnce(response('database/database.bin'))
            const { AccountStorage, AccountWarning } = await loadStorage()
            const warnings: string[] = []
            const unsubscribe = AccountWarning.subscribe((value) => warnings.push(value))
            const result = await new AccountStorage({
                databaseCache: mocks.cachedForage,
            }).writeItem(
                'database/database.bin',
                Uint8Array.of(1),
            )
            unsubscribe()
            expect(warnings).toEqual(['', 'account quota'])
            expect(result.kind).toBe(warn ? 'auth-warning' : 'written')
            expect(mocks.alertLogin).toHaveBeenCalledTimes(warn ? 0 : 1)
            expect(mocks.cachedForage.setItem).toHaveBeenCalledTimes(warn ? 0 : 2)
            expect(mocks.alertNormalWait).not.toHaveBeenCalled()
        },
    )

    it.each(['auth-warning', 'reauthentication-needed'] as const)(
        'publishes a native %s warning before returning or retrying',
        async (kind) => {
            const { AccountStorage, AccountWarning } = await loadStorage()
            const warnings: string[] = []
            const unsubscribe = AccountWarning.subscribe((value) => warnings.push(value))
            const attempt = vi
                .fn()
                .mockResolvedValueOnce({ kind, session: 'session', warning: 'native quota' })
                .mockResolvedValueOnce({
                    kind: 'written',
                    session: 'session',
                    replacementKey: 'database/database.bin',
                    receipt: {},
                })
            await new AccountStorage().writeOfficialDatabaseFromNative(attempt)
            unsubscribe()
            expect(warnings).toEqual(['', 'native quota'])
            expect(attempt).toHaveBeenCalledTimes(kind === 'auth-warning' ? 1 : 2)
        },
    )

    it('shares an in-flight unmigration, shows progress, and preserves account markers after a conflict', async () => {
        let fail!: (error: Error) => void
        mocks.completeAccountUnmigration.mockImplementation(
            () =>
                new Promise((_resolve, reject) => {
                    fail = reject
                }),
        )
        localStorage.setItem('accountst', 'able')
        localStorage.setItem('dosync', 'sync')
        localStorage.setItem('fallbackRisuToken', JSON.stringify(mocks.database.account))
        const { unMigrationAccount, accountUnmigrationBusy } = await loadStorage()
        const busy: boolean[] = []
        const unsubscribe = accountUnmigrationBusy.subscribe((value) => busy.push(value))
        const first = unMigrationAccount()
        const second = unMigrationAccount()
        expect(first).toBe(second)
        expect(mocks.alertSet).toHaveBeenCalledWith({ type: 'wait', msg: 'Preparing local data' })
        const rejection = expect(first).rejects.toThrow('Expected mutation generation')
        await vi.waitFor(() => expect(mocks.completeAccountUnmigration).toHaveBeenCalledOnce())
        fail(new Error('Expected mutation generation'))
        await rejection
        expect(localStorage.getItem('accountst')).toBe('able')
        expect(localStorage.getItem('dosync')).toBe('sync')
        expect(localStorage.getItem('fallbackRisuToken')).not.toBeNull()
        expect(mocks.database.account.useSync).toBe(true)
        expect(mocks.alertSet).toHaveBeenLastCalledWith({ type: 'none', msg: '' })
        expect(busy).toEqual([false, true, false])
        unsubscribe()
    })
    it('passes a safe credential, save date, session, and signal to a native database attempt', async () => {
        const signal = new AbortController().signal
        const attempt = vi.fn(async () => ({
            kind: 'written' as const,
            session: 'session-42',
            replacementKey: 'database/database.bin',
            warning: null,
            reloadSession: false,
            receipt: { jobId: 'job-1' },
        }))
        const credentialRouting = {
            getToken: vi.fn(() => 'native-token'),
            reauthenticate: vi.fn(async () => undefined),
        }
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            databaseCache: mocks.cachedForage,
            assetCache: mocks.localforage,
            credentialRouting,
        })

        const result = await storage.writeOfficialDatabaseFromNative(attempt, { signal })

        expect(attempt).toHaveBeenCalledWith({
            credential: { kind: 'risu-auth', token: 'native-token' },
            session: null,
            saveDate: '1725000000123',
            signal,
        })
        expect(result).toEqual({
            kind: 'written',
            replacementKey: 'database/database.bin',
            receipt: { jobId: 'job-1' },
            completeReload: expect.any(Function),
        })
        await expect(result?.kind === 'auth-warning' ? undefined : result?.completeReload())
            .resolves.toBeUndefined()
        expect(mocks.alertNormalWait).not.toHaveBeenCalled()
    })

    it('declines a native database attempt before invocation when legacy authentication is unsafe', async () => {
        const attempt = vi.fn()
        const { AccountStorage } = await loadStorage()
        localStorage.setItem('ignoreRisuAuth', 'true')
        const ignoredLegacyAuth = new AccountStorage({
            credentialRouting: {
                getToken: () => 'legacy-token',
                reauthenticate: vi.fn(),
            },
        })

        await expect(ignoredLegacyAuth.writeOfficialDatabaseFromNative(attempt))
            .resolves.toBeNull()

        localStorage.removeItem('ignoreRisuAuth')
        const missingLegacyAuth = new AccountStorage({
            credentialRouting: {
                getToken: () => null,
                reauthenticate: vi.fn(),
            },
        })
        await expect(missingLegacyAuth.writeOfficialDatabaseFromNative(attempt))
            .resolves.toBeNull()
        expect(attempt).not.toHaveBeenCalled()
    })

    it('shares a native attempt session with following JavaScript account writes', async () => {
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            credentialRouting: {
                getToken: () => 'native-token',
                reauthenticate: vi.fn(),
            },
        })
        await storage.writeOfficialDatabaseFromNative(async () => ({
            kind: 'written',
            session: 'session-from-native',
            replacementKey: 'database/database.bin',
            receipt: undefined,
        }))
        mocks.fetchProtectedResource.mockResolvedValueOnce(response('assets/next.png'))

        await storage.writeItem('assets/next.png', Uint8Array.of(1))

        expect(mocks.fetchProtectedResource).toHaveBeenCalledTimes(1)
        expect(mocks.fetchProtectedResource.mock.calls[0][1].headers['x-risu-session'])
            .toBe('session-from-native')
    })

    it('passes a JavaScript-acquired session to the next native database attempt', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 'session-from-js' }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('assets/first.png'))
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            credentialRouting: {
                getToken: () => 'native-token',
                reauthenticate: vi.fn(),
            },
        })
        await storage.writeItem('assets/first.png', Uint8Array.of(1))
        const attempt = vi.fn(async () => ({
            kind: 'written' as const,
            session: 'session-from-js',
            replacementKey: 'database/database.bin',
            receipt: undefined,
        }))

        await storage.writeOfficialDatabaseFromNative(attempt)

        expect(attempt).toHaveBeenCalledWith(expect.objectContaining({
            session: 'session-from-js',
        }))
    })

    it('reauthenticates an ordinary native 403 and retries with fresh credentials and save date', async () => {
        let token = 'old-token'
        vi.spyOn(Date, 'now')
            .mockReturnValueOnce(1000)
            .mockReturnValueOnce(1001)
        const attempt = vi.fn()
            .mockResolvedValueOnce({
                kind: 'reauthentication-needed',
                session: 'session-42',
            })
            .mockResolvedValueOnce({
                kind: 'written',
                session: 'session-42',
                replacementKey: 'database/database.bin',
                receipt: { jobId: 'job-2' },
            })
        const credentialRouting = {
            getToken: vi.fn(() => token),
            reauthenticate: vi.fn(async () => {
                token = 'new-token'
            }),
        }
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({ credentialRouting })

        await expect(storage.writeOfficialDatabaseFromNative(attempt)).resolves.toEqual({
            kind: 'written',
            replacementKey: 'database/database.bin',
            receipt: { jobId: 'job-2' },
            completeReload: expect.any(Function),
        })

        expect(mocks.alertLogin).toHaveBeenCalledOnce()
        expect(credentialRouting.reauthenticate).toHaveBeenCalledWith('new-token')
        expect(attempt.mock.calls).toEqual([
            [{
                credential: { kind: 'risu-auth', token: 'old-token' },
                session: null,
                saveDate: '1000',
                signal: undefined,
            }],
            [{
                credential: { kind: 'risu-auth', token: 'new-token' },
                session: 'session-42',
                saveDate: '1001',
                signal: undefined,
            }],
        ])
    })

    it('stops native publication after three rejected reauthentication attempts', async () => {
        const credentialRouting = {
            getToken: vi.fn(() => 'native-token'),
            reauthenticate: vi.fn(async () => undefined),
        }
        const attempt = vi.fn(async () => ({
            kind: 'reauthentication-needed' as const,
            session: 'session-42',
        }))
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({ credentialRouting })

        await expect(storage.writeOfficialDatabaseFromNative(attempt)).rejects.toThrow(
            'Official account reauthentication was rejected too many times',
        )

        expect(attempt).toHaveBeenCalledTimes(3)
        expect(mocks.alertLogin).toHaveBeenCalledTimes(2)
        expect(credentialRouting.reauthenticate).toHaveBeenCalledTimes(2)
    })

    it('aborts while waiting for native reauthentication without retrying after login resolves', async () => {
        let resolveLogin!: (value: string) => void
        mocks.alertLogin.mockReturnValueOnce(new Promise<string>((resolve) => {
            resolveLogin = resolve
        }))
        const credentialRouting = {
            getToken: vi.fn(() => 'old-token'),
            reauthenticate: vi.fn(async () => undefined),
        }
        const attempt = vi.fn(async () => ({
            kind: 'reauthentication-needed' as const,
            session: 'session-42',
        }))
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({ credentialRouting })
        const controller = new AbortController()

        const publication = storage.writeOfficialDatabaseFromNative(attempt, {
            signal: controller.signal,
        })
        await vi.waitFor(() => expect(mocks.alertLogin).toHaveBeenCalledOnce())
        controller.abort(new DOMException('Publication cancelled', 'AbortError'))

        await expect(publication).rejects.toMatchObject({ name: 'AbortError' })
        resolveLogin('new-token')
        await Promise.resolve()
        await Promise.resolve()

        expect(attempt).toHaveBeenCalledOnce()
        expect(credentialRouting.reauthenticate).not.toHaveBeenCalled()
    })

    it('preserves the shared session when a native auth outcome has no acquired session', async () => {
        let token = 'old-token'
        const credentialRouting = {
            getToken: () => token,
            reauthenticate: vi.fn(async () => {
                token = 'new-token'
            }),
        }
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({ credentialRouting })
        await storage.writeOfficialDatabaseFromNative(async () => ({
            kind: 'written',
            session: 'shared-session',
            replacementKey: 'database/database.bin',
            receipt: undefined,
        }))
        const attempt = vi.fn()
            .mockResolvedValueOnce({
                kind: 'reauthentication-needed',
                session: null,
            })
            .mockResolvedValueOnce({
                kind: 'written',
                session: 'shared-session',
                replacementKey: 'database/database.bin',
                receipt: undefined,
            })

        await storage.writeOfficialDatabaseFromNative(attempt)

        expect(attempt.mock.calls[1][0].session).toBe('shared-session')
    })

    it('returns a native auth warning without reauthentication', async () => {
        const credentialRouting = {
            getToken: vi.fn(() => 'native-token'),
            reauthenticate: vi.fn(async () => undefined),
        }
        const attempt = vi.fn(async () => ({
            kind: 'auth-warning' as const,
            session: 'warning-session',
        }))
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({ credentialRouting })

        await expect(storage.writeOfficialDatabaseFromNative(attempt)).resolves.toEqual({
            kind: 'auth-warning',
        })

        expect(attempt).toHaveBeenCalledOnce()
        expect(mocks.alertLogin).not.toHaveBeenCalled()
        expect(credentialRouting.reauthenticate).not.toHaveBeenCalled()
    })

    it('returns native not-modified success with its opaque receipt', async () => {
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            credentialRouting: {
                getToken: () => 'native-token',
                reauthenticate: vi.fn(),
            },
        })

        const result = await storage.writeOfficialDatabaseFromNative(async () => ({
            kind: 'not-modified',
            session: 'session-304',
            replacementKey: 'database/database.bin',
            receipt: { jobId: 'job-304' },
        }))

        expect(result).toEqual({
            kind: 'not-modified',
            replacementKey: 'database/database.bin',
            receipt: { jobId: 'job-304' },
            completeReload: expect.any(Function),
        })
    })

    it('publishes each native success warning through the shared deduplicated warning store', async () => {
        const { AccountStorage, AccountWarning, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            credentialRouting: {
                getToken: () => 'native-token',
                reauthenticate: vi.fn(),
            },
        })
        const seen: string[] = []
        const unsubscribe = AccountWarning.subscribe((value) => seen.push(value))
        const attempt = async () => ({
            kind: 'written' as const,
            session: 'warning-session',
            replacementKey: 'database/database.bin',
            warning: 'quota nearing limit',
            receipt: undefined,
        })

        await storage.writeOfficialDatabaseFromNative(attempt)
        await storage.writeOfficialDatabaseFromNative(attempt)
        unsubscribe()

        expect(seen).toEqual(['', 'quota nearing limit'])
    })

    it('adopts recovered session and warning metadata without exposing raw session accessors', async () => {
        const { AccountStorage, AccountWarning, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            credentialRouting: {
                getToken: () => 'native-token',
                reauthenticate: vi.fn(),
            },
        })
        const seen: string[] = []
        const unsubscribe = AccountWarning.subscribe((value) => seen.push(value))

        const recovered = storage.adoptRecoveredOfficialWrite({
            session: 'recovered-session',
            warning: 'recovered warning',
            reloadSession: false,
        })
        const attempt = vi.fn(async (_context: AccountNativeOfficialWriteAttemptContext) => ({
            kind: 'not-modified' as const,
            session: 'recovered-session',
            replacementKey: 'database/database.bin',
            receipt: undefined,
        }))
        await storage.writeOfficialDatabaseFromNative(attempt)
        await recovered.completeReload()
        unsubscribe()

        expect(attempt.mock.calls[0][0].session).toBe('recovered-session')
        expect(seen).toEqual(['', 'recovered warning'])
        expect(mocks.alertNormalWait).not.toHaveBeenCalled()
    })

    it('defers native reload-session handling until durable finalization calls the callback', async () => {
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            credentialRouting: {
                getToken: () => 'native-token',
                reauthenticate: vi.fn(),
            },
        })
        const result = await storage.writeOfficialDatabaseFromNative(async () => ({
            kind: 'written',
            session: 'reload-session',
            replacementKey: 'database/database.bin',
            reloadSession: true,
            receipt: { jobId: 'job-reload' },
        }))
        if (!result || result.kind === 'auth-warning') throw new Error('Expected native write success')
        expect(mocks.alertNormalWait).not.toHaveBeenCalled()
        let settled = false

        void result.completeReload().finally(() => {
            settled = true
        })

        await vi.waitFor(() => expect(mocks.alertNormalWait).toHaveBeenCalledOnce())
        expect(settled).toBe(false)
    })

    it('returns native capability unavailability without retrying or changing session state', async () => {
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({
            credentialRouting: {
                getToken: () => 'native-token',
                reauthenticate: vi.fn(),
            },
        })
        const attempt = vi.fn(async () => null)

        await expect(storage.writeOfficialDatabaseFromNative(attempt)).resolves.toBeNull()

        expect(attempt).toHaveBeenCalledOnce()
    })

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
        const storage = new AccountStorage({ databaseCache: mocks.cachedForage })

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
        const storage = new AccountStorage({ databaseCache: mocks.cachedForage })

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

    it('aborts an asset upload while its ordinary 403 reauthentication is waiting', async () => {
        let resolveLogin!: (value: string) => void
        mocks.alertLogin.mockReturnValueOnce(new Promise<string>((resolve) => {
            resolveLogin = resolve
        }))
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 8 }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('retry', 403))
            .mockResolvedValueOnce(response('assets/retried.png'))
        const credentialRouting = {
            getToken: vi.fn(() => 'old-token'),
            reauthenticate: vi.fn(async () => undefined),
        }
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const storage = new AccountStorage({ credentialRouting })
        const controller = new AbortController()
        const aborted = new DOMException('Publication disposed', 'AbortError')

        const write = storage.writeItem('assets/a.png', Uint8Array.of(1), {
            signal: controller.signal,
        })
        await vi.waitFor(() => expect(mocks.alertLogin).toHaveBeenCalledOnce())
        controller.abort(aborted)
        resolveLogin('new-token')

        await expect(write).rejects.toBe(aborted)
        expect(credentialRouting.reauthenticate).not.toHaveBeenCalled()
        expect(mocks.fetchProtectedResource).toHaveBeenCalledTimes(2)
    })

    it('aborts an account read while its 403 reauthentication is waiting', async () => {
        let resolveLogin!: (value: string) => void
        mocks.alertLogin.mockReturnValueOnce(new Promise<string>((resolve) => {
            resolveLogin = resolve
        }))
        mocks.fetchProtectedResource.mockResolvedValueOnce(response('retry', 403))
        const credentialRouting = {
            getToken: vi.fn(() => 'old-token'),
            reauthenticate: vi.fn(async () => undefined),
        }
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage({ credentialRouting })
        const controller = new AbortController()

        const read = storage.readItem('database/database.bin', { signal: controller.signal })
        await vi.waitFor(() => expect(mocks.alertLogin).toHaveBeenCalledOnce())
        controller.abort(new DOMException('Read cancelled', 'AbortError'))

        await expect(read).rejects.toMatchObject({ name: 'AbortError' })
        resolveLogin('new-token')
        await Promise.resolve()
        await Promise.resolve()

        expect(credentialRouting.reauthenticate).not.toHaveBeenCalled()
        expect(mocks.fetchProtectedResource).toHaveBeenCalledOnce()
    })

    it('routes native reauthentication without reading or writing the legacy fallback token', async () => {
        localStorage.setItem('fallbackRisuToken', JSON.stringify({ token: 'legacy-token' }))
        const getItem = vi.spyOn(Storage.prototype, 'getItem')
        const setItem = vi.spyOn(Storage.prototype, 'setItem')
        const credentialRouting = {
            getToken: vi.fn(() => 'native-token'),
            reauthenticate: vi.fn(async () => undefined),
        }
        mocks.alertLogin.mockResolvedValueOnce(JSON.stringify({
            id: 'account-1',
            token: 'refreshed-token',
            data: { dpop_private_key: 'must-not-persist' },
        }))
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(null, 403))
            .mockResolvedValueOnce(response(new Uint8Array([9, 1])))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage({
            databaseCache: mocks.cachedForage,
            assetCache: mocks.localforage,
            credentialRouting,
        })

        await expect(storage.readItem('native-resource')).resolves.toEqual({
            kind: 'value',
            bytes: new Uint8Array([9, 1]),
        })

        expect(credentialRouting.getToken).toHaveBeenCalledTimes(2)
        expect(credentialRouting.reauthenticate).toHaveBeenCalledWith(JSON.stringify({
            id: 'account-1',
            token: 'refreshed-token',
            data: { dpop_private_key: 'must-not-persist' },
        }))
        expect(getItem).not.toHaveBeenCalledWith('fallbackRisuToken')
        expect(setItem).not.toHaveBeenCalledWith('fallbackRisuToken', expect.anything())
        expect(localStorage.getItem('fallbackRisuToken')).toBe(JSON.stringify({
            token: 'legacy-token',
        }))
    })

    it('completes restore after AccountStorage reauthenticates a 403 inside the flow queue', async () => {
        const vault = {
            stored: null as unknown,
            read: vi.fn(async () => vault.stored ?? null),
            write: vi.fn(async (credential: unknown) => void (vault.stored = credential)),
            clear: vi.fn(async () => void (vault.stored = null)),
        }
        const setRouting = vi.fn()
        let flow!: NativeOfficialAccountFlow
        let reauthenticateSnapshotRequest!: (loginResult: string) => Promise<void>
        mocks.alertLogin.mockResolvedValueOnce(JSON.stringify({
            id: 'account-1',
            token: 'refreshed-token',
            data: { dpop_private_key: 'must-not-persist' },
        }))
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(null, 403))
            .mockResolvedValueOnce(response(null, 204))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage({
            databaseCache: mocks.cachedForage,
            assetCache: mocks.localforage,
            credentialRouting: {
                getToken: () => flow.getToken(),
                reauthenticate: (loginResult) => reauthenticateSnapshotRequest(loginResult),
            },
        })
        const { createNativeOfficialAccountFlowService } =
            await import('./sync/nativeOfficialAccountFlow')
        const service = createNativeOfficialAccountFlowService({
            credentialVault: vault,
            adapter: {
                pull: vi.fn(async () => {
                    await storage.readItem('database/database.bin')
                    return { kind: 'missing' as const }
                }),
                pin: vi.fn(),
                resetAccountAssociation: vi.fn(),
            },
            initialCredential: { id: 'account-1', token: 'legacy-token', data: {} },
            flushPendingData: vi.fn(async () => undefined),
            getRevision: () => 1,
            restart: vi.fn(async () => undefined),
            setRouting,
            clearLegacyFallback: vi.fn(),
            flushMetadata: vi.fn(async () => undefined),
            clearMetadata: vi.fn(async () => undefined),
            resetAccountSession: vi.fn(),
        })
        flow = service.flow
        reauthenticateSnapshotRequest = (loginResult) =>
            service.snapshotRequestReauthentication.reauthenticate(loginResult).then(() => undefined)

        await expect(flow.restore()).resolves.toEqual({ kind: 'missing' })

        expect(mocks.fetchProtectedResource).toHaveBeenCalledTimes(2)
        expect(flow.getToken()).toBe('refreshed-token')
        expect(vault.stored).toEqual({
            id: 'account-1',
            token: 'refreshed-token',
            data: {},
        })
        expect(setRouting).toHaveBeenCalledOnce()
    }, 1_000)

    it('aborts snapshot retry when a 403 returns credentials for another account', async () => {
        let flow!: NativeOfficialAccountFlow
        let reauthenticateSnapshotRequest!: (loginResult: string) => Promise<void>
        mocks.alertLogin.mockResolvedValueOnce(JSON.stringify({
            id: 'account-2',
            token: 'other-token',
            data: {},
        }))
        mocks.fetchProtectedResource.mockResolvedValueOnce(response(null, 403))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage({
            databaseCache: mocks.cachedForage,
            assetCache: mocks.localforage,
            credentialRouting: {
                getToken: () => flow.getToken(),
                reauthenticate: (loginResult) => reauthenticateSnapshotRequest(loginResult),
            },
        })
        const { createNativeOfficialAccountFlowService } =
            await import('./sync/nativeOfficialAccountFlow')
        const vault = {
            read: vi.fn(async () => null),
            write: vi.fn(async () => undefined),
            clear: vi.fn(async () => undefined),
        }
        const setRouting = vi.fn()
        const service = createNativeOfficialAccountFlowService({
            credentialVault: vault,
            adapter: {
                pull: vi.fn(async () => {
                    await storage.readItem('database/database.bin')
                    return { kind: 'missing' as const }
                }),
                pin: vi.fn(),
                resetAccountAssociation: vi.fn(),
            },
            initialCredential: { id: 'account-1', token: 'legacy-token', data: {} },
            flushPendingData: vi.fn(async () => undefined),
            getRevision: () => 1,
            restart: vi.fn(async () => undefined),
            setRouting,
            clearLegacyFallback: vi.fn(),
            flushMetadata: vi.fn(async () => undefined),
            clearMetadata: vi.fn(async () => undefined),
            resetAccountSession: vi.fn(),
        })
        flow = service.flow
        reauthenticateSnapshotRequest = (loginResult) =>
            service.snapshotRequestReauthentication.reauthenticate(loginResult).then(() => undefined)

        await expect(flow.restore()).rejects.toThrow(
            'Native official account changed during snapshot reauthentication',
        )

        expect(mocks.fetchProtectedResource).toHaveBeenCalledOnce()
        expect(flow.getToken()).toBe('legacy-token')
        expect(vault.write).not.toHaveBeenCalled()
        expect(setRouting).not.toHaveBeenCalled()
    })

    it('does not publish an account A pin after a 403 authenticates account B', async () => {
        let flow!: NativeOfficialAccountFlow
        let reauthenticateSnapshotRequest!: (loginResult: string) => Promise<void>
        mocks.alertLogin.mockResolvedValueOnce(JSON.stringify({
            id: 'account-b',
            token: 'other-token',
            data: {},
        }))
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 'session-a' }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response(null, 403))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage({
            databaseCache: mocks.cachedForage,
            assetCache: mocks.localforage,
            credentialRouting: {
                getToken: () => flow.getToken(),
                reauthenticate: (loginResult) => reauthenticateSnapshotRequest(loginResult),
            },
        })
        const { createNativeOfficialAccountFlowService } =
            await import('./sync/nativeOfficialAccountFlow')
        const vault = {
            read: vi.fn(async () => null),
            write: vi.fn(async () => undefined),
            clear: vi.fn(async () => undefined),
        }
        const setRouting = vi.fn()
        const publication = {
            publish: vi.fn(async () => {
                await storage.writeItem('database/database.bin', Uint8Array.of(1))
            }),
            dispose: vi.fn(async () => undefined),
        }
        const service = createNativeOfficialAccountFlowService({
            credentialVault: vault,
            adapter: {
                pull: vi.fn(),
                pin: vi.fn(async () => publication),
                resetAccountAssociation: vi.fn(),
            },
            initialCredential: { id: 'account-a', token: 'legacy-token', data: {} },
            flushPendingData: vi.fn(async () => undefined),
            getRevision: () => 1,
            restart: vi.fn(async () => undefined),
            setRouting,
            clearLegacyFallback: vi.fn(),
            flushMetadata: vi.fn(async () => undefined),
            clearMetadata: vi.fn(async () => undefined),
            resetAccountSession: vi.fn(),
        })
        flow = service.flow
        reauthenticateSnapshotRequest = (loginResult) =>
            service.snapshotRequestReauthentication.reauthenticate(loginResult).then(() => undefined)

        await expect(flow.publish()).rejects.toThrow(
            'Native official account changed during snapshot reauthentication',
        )

        expect(publication.publish).toHaveBeenCalledOnce()
        expect(publication.dispose).toHaveBeenCalledOnce()
        expect(mocks.fetchProtectedResource).toHaveBeenCalledTimes(2)
        expect(flow.getToken()).toBe('legacy-token')
        expect(vault.write).not.toHaveBeenCalled()
        expect(setRouting).not.toHaveBeenCalled()
    })

    it('does not let an external asset 403 resurrect credentials after queued logout', async () => {
        let flow!: NativeOfficialAccountFlow
        mocks.alertLogin.mockResolvedValueOnce(JSON.stringify({
            id: 'account-1',
            token: 'refreshed-token',
            data: {},
        }))
        mocks.fetchProtectedResource.mockResolvedValueOnce(response(null, 403))
        const { AccountStorage } = await loadStorage()
        const liveStorage = new AccountStorage({
            databaseCache: mocks.cachedForage,
            assetCache: mocks.localforage,
            credentialRouting: {
                getToken: () => flow.getToken(),
                reauthenticate: (loginResult) =>
                    flow.reauthenticate(loginResult).then(() => undefined),
            },
        })
        const { createNativeOfficialAccountFlowService } =
            await import('./sync/nativeOfficialAccountFlow')
        const vault = {
            stored: { id: 'account-1', token: 'legacy-token', data: {} } as unknown,
            read: vi.fn(async () => vault.stored ?? null),
            write: vi.fn(async (credential: unknown) => void (vault.stored = credential)),
            clear: vi.fn(async () => void (vault.stored = null)),
        }
        let resolvePull: (result: { kind: 'missing' }) => void = () => undefined
        const pull = vi.fn(async () => new Promise<{ kind: 'missing' }>((resolve) => {
            resolvePull = resolve
        }))
        const setRouting = vi.fn()
        const service = createNativeOfficialAccountFlowService({
            credentialVault: vault,
            adapter: {
                pull,
                pin: vi.fn(),
                resetAccountAssociation: vi.fn(),
            },
            initialCredential: { id: 'account-1', token: 'legacy-token', data: {} },
            flushPendingData: vi.fn(async () => undefined),
            getRevision: () => 1,
            restart: vi.fn(async () => undefined),
            setRouting,
            clearLegacyFallback: vi.fn(),
            flushMetadata: vi.fn(async () => undefined),
            clearMetadata: vi.fn(async () => undefined),
            resetAccountSession: vi.fn(),
        })
        flow = service.flow

        const restore = flow.restore()
        await vi.waitFor(() => expect(pull).toHaveBeenCalledOnce())
        const logout = flow.logout()
        const liveRead = liveStorage.readItem('assets/live.png')
        await vi.waitFor(() => expect(mocks.alertLogin).toHaveBeenCalledOnce())
        resolvePull({ kind: 'missing' })

        await expect(restore).resolves.toEqual({ kind: 'missing' })
        await expect(logout).resolves.toBeUndefined()
        await expect(liveRead).rejects.toThrow(
            'Native official account session changed during reauthentication',
        )
        expect(flow.getToken()).toBeNull()
        expect(vault.stored).toBeNull()
        expect(setRouting).toHaveBeenCalledTimes(1)
        expect(setRouting).toHaveBeenCalledWith(null)
    }, 1_000)

    it('does not reuse account A session headers after logout and account B login', async () => {
        mocks.fetchProtectedResource
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 'session-a' }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('assets/a.png'))
            .mockResolvedValueOnce(response(JSON.stringify({ sessionNumber: 'session-b' }), 200, {
                'content-type': 'application/json',
            }))
            .mockResolvedValueOnce(response('assets/b.png'))
        const { AccountStorage, resetAccountStorageSession } = await loadStorage()
        resetAccountStorageSession()
        const { createNativeOfficialAccountFlow } = await import('./sync/nativeOfficialAccountFlow')
        const flow = createNativeOfficialAccountFlow({
            credentialVault: {
                read: vi.fn(async () => null),
                write: vi.fn(async () => undefined),
                clear: vi.fn(async () => undefined),
            },
            adapter: {
                pull: vi.fn(),
                pin: vi.fn(),
                resetAccountAssociation: vi.fn(),
            },
            initialCredential: { id: 'account-a', token: 'token-a', data: {} },
            flushPendingData: vi.fn(async () => undefined),
            getRevision: () => 1,
            restart: vi.fn(async () => undefined),
            setRouting: vi.fn(),
            clearLegacyFallback: vi.fn(),
            flushMetadata: vi.fn(async () => undefined),
            clearMetadata: vi.fn(async () => undefined),
            resetAccountSession: resetAccountStorageSession,
        })
        const storage = new AccountStorage({
            databaseCache: mocks.cachedForage,
            assetCache: mocks.localforage,
            credentialRouting: {
                getToken: () => flow.getToken(),
                reauthenticate: (loginResult) =>
                    flow.reauthenticate(loginResult).then(() => undefined),
            },
        })

        await storage.writeItem('assets/a.png', Uint8Array.of(1))
        await flow.logout()
        await flow.login({ id: 'account-b', token: 'token-b', data: {} })
        await storage.writeItem('assets/b.png', Uint8Array.of(2))

        expect(mocks.fetchProtectedResource).toHaveBeenCalledTimes(4)
        expect(mocks.fetchProtectedResource.mock.calls[1][1].headers['x-risu-session'])
            .toBe('session-a')
        expect(mocks.fetchProtectedResource.mock.calls[3][1].headers['x-risu-session'])
            .toBe('session-b')
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
        const storage = new AccountStorage({ databaseCache: mocks.cachedForage })
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

    it('uses injected native caches without creating or accessing LocalForage', async () => {
        mocks.localforage.createInstance.mockClear()
        const databaseCache = {
            getItem: vi.fn(async () => null),
            setItem: vi.fn(async () => undefined),
        }
        const assetCache = {
            getItem: vi.fn(async () => null),
            setItem: vi.fn(async () => undefined),
        }
        mocks.fetchProtectedResource.mockResolvedValueOnce(response(new Uint8Array([4, 2])))
        const { AccountStorage } = await loadStorage()
        const storage = new AccountStorage({ databaseCache, assetCache })

        await expect(storage.readItem('assets/native.png')).resolves.toEqual({
            kind: 'value',
            bytes: new Uint8Array([4, 2]),
        })

        expect(assetCache.getItem).toHaveBeenCalledWith('assets/native.png')
        expect(assetCache.setItem).toHaveBeenCalledOnce()
        expect(mocks.localforage.createInstance).not.toHaveBeenCalled()
        expect(mocks.localforage.getItem).not.toHaveBeenCalled()
        expect(mocks.localforage.setItem).not.toHaveBeenCalled()
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

    it('unmigrates assets referenced by the account cold character before disabling account', async () => {
        const officialCold = {
            character: {
                type: 'character',
                chaId: 'cold-character',
                additionalAssets: [['official', 'assets/account-cold-only.png']],
            },
        }
        mocks.database.characters = [{
            type: 'character',
            chaId: 'catalog-only',
            chats: [],
        }]
        const authoritativeDatabase = {
            account: mocks.database.account,
            characters: [{
            type: 'character',
            chaId: 'cold-character',
            coldstorage: 'cold-character-key',
            chats: [],
        }],
        }
        mocks.materializePersistentDatabaseSnapshotWithRevision.mockResolvedValue({
            database: authoritativeDatabase,
            revision: 11,
            mutationGeneration: 17,
        })
        mocks.getAccountColdStorageItem.mockResolvedValue(officialCold)
        mocks.getUncleanablesSync.mockImplementation((_database, _mode, options) => (
            options.chars.flatMap((character: any) => (
                character.additionalAssets?.map((asset: string[]) => asset[1]) ?? []
            ))
        ))
        mocks.fetchProtectedResource.mockResolvedValue(response(new Uint8Array([7, 6, 5])))
        const events: string[] = []
        mocks.completeAccountUnmigration.mockImplementation(async (_database, dependencies) => {
            const candidate = structuredClone({ ..._database, account: null })
            await dependencies.prepareResources(candidate)
            await dependencies.replaceDatabase(candidate, 'account-unmigration')
            events.push('disable-account')
        })
        const { unMigrationAccount } = await loadStorage()

        await unMigrationAccount()

        expect(mocks.materializePersistentDatabaseSnapshotWithRevision).toHaveBeenCalledWith(
            'account-unmigration',
        )
        expect(mocks.completeAccountUnmigration).toHaveBeenCalledWith(
            authoritativeDatabase,
            expect.any(Object),
        )
        expect(mocks.replacePersistentDatabase).toHaveBeenCalledWith(
            expect.objectContaining({ account: null }),
            'account-unmigration',
            {
                authoritative: true,
                expectedRevision: 11,
                expectedMutationGeneration: 17,
            },
        )
        expect(mocks.getAccountColdStorageItem).toHaveBeenCalledWith('cold-character-key')
        expect(mocks.blobAssets.get('assets/account-cold-only.png')).toEqual(
            new Uint8Array([7, 6, 5]),
        )
        const replaced = mocks.replacePersistentDatabase.mock.calls[0][0]
        expect(replaced.characters[0].coldstorage).toBeUndefined()
        expect(replaced.characters[0].additionalAssets).toEqual(
            officialCold.character.additionalAssets,
        )
        expect(events).toEqual(['disable-account'])
    })

    it('rejects legacy account unmigration on native before reading any legacy storage', async () => {
        mocks.isTauri = true
        const { unMigrationAccount } = await loadStorage()

        await expect(unMigrationAccount()).rejects.toThrow(
            'Account unmigration is only available on the web',
        )

        expect(mocks.materializePersistentDatabaseSnapshotWithRevision).not.toHaveBeenCalled()
        expect(mocks.localforage.createInstance).not.toHaveBeenCalled()
    })
})
