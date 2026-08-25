import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    invoke: vi.fn(),
    isTauri: true,
}))

vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('../platform', () => ({
    get isTauri() {
        return mocks.isTauri
    },
}))

import { createNativeAppKv, createNativeAppKvStringStorage } from './nativeAppKv'

describe('native app key-value storage', () => {
    beforeEach(() => {
        mocks.invoke.mockReset()
        mocks.isTauri = true
    })

    it('maps get, set, and remove to the persistent store commands', async () => {
        mocks.invoke.mockResolvedValueOnce({ token: 'legacy-token' })
            .mockResolvedValueOnce(undefined)
            .mockResolvedValueOnce(undefined)
        const storage = createNativeAppKv()

        await expect(storage.get('official-account.credential.v1')).resolves.toEqual({
            token: 'legacy-token',
        })
        await storage.set('official-account.credential.v1', { token: 'updated-token' })
        await storage.remove('official-account.credential.v1')

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_get_app_kv', { key: 'official-account.credential.v1' }],
            ['pds_set_app_kv', {
                key: 'official-account.credential.v1',
                value: { token: 'updated-token' },
            }],
            ['pds_remove_app_kv', { key: 'official-account.credential.v1' }],
        ])
    })

    it('rejects every operation outside Tauri', async () => {
        mocks.isTauri = false
        const storage = createNativeAppKv()

        await expect(storage.get('key')).rejects.toThrow('Native app KV requires Tauri')
        await expect(storage.set('key', 'value')).rejects.toThrow('Native app KV requires Tauri')
        await expect(storage.remove('key')).rejects.toThrow('Native app KV requires Tauri')
        expect(mocks.invoke).not.toHaveBeenCalled()
    })

    it('buffers synchronous account metadata writes and flushes them to one versioned key', async () => {
        const appKv = {
            get: vi.fn(async () => ({ existing: 'value' })),
            set: vi.fn(async () => undefined),
            remove: vi.fn(async () => undefined),
        }
        const persisted = await createNativeAppKvStringStorage(appKv, 'account-metadata.v1')

        expect(persisted.storage.getItem('existing')).toBe('value')
        persisted.storage.setItem('new', 'record')
        persisted.storage.removeItem('existing')
        await persisted.flush()

        expect(appKv.set).toHaveBeenLastCalledWith('account-metadata.v1', {
            new: 'record',
        })
    })
})
