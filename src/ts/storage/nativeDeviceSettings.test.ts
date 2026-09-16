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

import {
    createNativeDeviceSettings,
    createNativeDeviceSettingsBag,
    type NativeDeviceSettings,
} from './nativeDeviceSettings'

function pendingSettings() {
    const commits: (() => void)[] = []
    const failures: ((error: Error) => void)[] = []
    const settings: NativeDeviceSettings = {
        get: vi.fn(async () => null),
        readMany: vi.fn(async (keys: readonly string[]) => keys.map(() => null)),
        set: vi.fn(() => new Promise<void>((resolve, reject) => {
            commits.push(() => resolve())
            failures.push(reject)
        })),
        patch: vi.fn(() => new Promise<void>((resolve, reject) => {
            commits.push(() => resolve())
            failures.push(reject)
        })),
    }
    return { commits, failures, settings }
}

describe('native device settings', () => {
    beforeEach(() => {
        mocks.invoke.mockReset()
        mocks.isTauri = true
    })

    it('maps get, set, and patch to the persistent store commands', async () => {
        mocks.invoke.mockResolvedValueOnce([{ id: 'backup-1' }])
            .mockResolvedValue(undefined)
        const settings = createNativeDeviceSettings()

        await expect(settings.get('sync-conflict-backups.index.v1')).resolves.toEqual([
            { id: 'backup-1' },
        ])
        await settings.set('sync-conflict-backups.index.v1', [])
        await settings.set('sync-conflict-backups.index.v1', null)
        await settings.patch('official-account.association.v1', { 'marker:one': 'value' })

        expect(mocks.invoke.mock.calls).toEqual([
            ['pds_get_device_setting', { key: 'sync-conflict-backups.index.v1' }],
            ['pds_set_device_setting', {
                key: 'sync-conflict-backups.index.v1',
                value: [],
            }],
            ['pds_set_device_setting', {
                key: 'sync-conflict-backups.index.v1',
                value: null,
            }],
            ['pds_patch_device_setting', {
                key: 'official-account.association.v1',
                entries: { 'marker:one': 'value' },
            }],
        ])
    })

    it('rejects every operation outside Tauri', async () => {
        mocks.isTauri = false
        const settings = createNativeDeviceSettings()

        await expect(settings.get('key')).rejects.toThrow('Device settings require Tauri')
        await expect(settings.set('key', 'value')).rejects.toThrow('Device settings require Tauri')
        await expect(settings.patch('key', {})).rejects.toThrow('Device settings require Tauri')
        expect(mocks.invoke).not.toHaveBeenCalled()
    })

    it('loads stored entries and reads back what it just wrote', async () => {
        const settings: NativeDeviceSettings = {
            get: vi.fn(async () => ({ 'marker:one': 'stored' })),
            readMany: vi.fn(async (keys: readonly string[]) => keys.map(() => null)),
            set: vi.fn(async () => undefined),
            patch: vi.fn(async () => undefined),
        }
        const bag = await createNativeDeviceSettingsBag(
            settings,
            'official-account.association.v1',
        )

        expect(bag.storage.getItem('marker:one')).toBe('stored')
        bag.storage.setItem('marker:two', 'written')
        expect(bag.storage.getItem('marker:two')).toBe('written')
        bag.storage.removeItem('marker:one')
        expect(bag.storage.getItem('marker:one')).toBeNull()
        await bag.flush()

        expect(settings.set).not.toHaveBeenCalled()
        expect(settings.patch).toHaveBeenCalledTimes(2)
        expect(settings.patch).toHaveBeenNthCalledWith(
            1,
            'official-account.association.v1',
            { 'marker:two': 'written' },
        )
        expect(settings.patch).toHaveBeenNthCalledWith(
            2,
            'official-account.association.v1',
            { 'marker:one': null },
        )
    })

    it('reports a change as flushed only after its native transaction commits', async () => {
        const { commits, settings } = pendingSettings()
        const bag = await createNativeDeviceSettingsBag(settings, 'official-account.association.v1')
        let flushed = false

        bag.storage.setItem('marker:one', 'written')
        const flush = bag.flush().then(() => { flushed = true })
        await vi.waitFor(() => expect(settings.patch).toHaveBeenCalledOnce())
        expect(flushed).toBe(false)

        commits.shift()?.()
        await flush
        expect(flushed).toBe(true)
    })

    it('reports a failed write at the next flush and not before', async () => {
        const { failures, settings } = pendingSettings()
        const bag = await createNativeDeviceSettingsBag(settings, 'official-account.association.v1')

        bag.storage.setItem('marker:one', 'written')
        const flush = bag.flush()
        await vi.waitFor(() => expect(settings.patch).toHaveBeenCalledOnce())
        failures.shift()?.(new Error('device settings write failed'))

        await expect(flush).rejects.toThrow('device settings write failed')
        await expect(bag.flush()).resolves.toBeUndefined()
    })

    it('skips a write that changes nothing', async () => {
        const settings: NativeDeviceSettings = {
            get: vi.fn(async () => ({ 'marker:one': 'stored' })),
            readMany: vi.fn(async (keys: readonly string[]) => keys.map(() => null)),
            set: vi.fn(async () => undefined),
            patch: vi.fn(async () => undefined),
        }
        const bag = await createNativeDeviceSettingsBag(
            settings,
            'official-account.association.v1',
        )

        bag.storage.setItem('marker:one', 'stored')
        bag.storage.removeItem('marker:absent')
        await bag.flush()

        expect(settings.patch).not.toHaveBeenCalled()
    })

    it('clears the stored value after the queued changes commit', async () => {
        const order: string[] = []
        const settings: NativeDeviceSettings = {
            get: vi.fn(async () => ({ 'marker:one': 'stored' })),
            readMany: vi.fn(async (keys: readonly string[]) => keys.map(() => null)),
            set: vi.fn(async () => void order.push('set')),
            patch: vi.fn(async () => void order.push('patch')),
        }
        const bag = await createNativeDeviceSettingsBag(
            settings,
            'official-account.association.v1',
        )

        bag.storage.setItem('marker:two', 'written')
        await bag.clear()

        expect(order).toEqual(['patch', 'set'])
        expect(settings.set).toHaveBeenCalledWith('official-account.association.v1', null)
        expect(bag.storage.getItem('marker:one')).toBeNull()
        expect(bag.storage.getItem('marker:two')).toBeNull()
    })

    it('reports a stored value that does not hold entries at the first flush', async () => {
        const settings: NativeDeviceSettings = {
            get: vi.fn(async () => ['not', 'entries']),
            readMany: vi.fn(async (keys: readonly string[]) => keys.map(() => null)),
            set: vi.fn(async () => undefined),
            patch: vi.fn(async () => undefined),
        }

        const bag = await createNativeDeviceSettingsBag(
            settings,
            'official-account.association.v1',
        )

        expect(bag.storage.getItem('marker:one')).toBeNull()
        await expect(bag.flush()).rejects.toThrow('does not hold entries')
    })

    it('starts empty and reports an unreadable device file without failing to open', async () => {
        const settings: NativeDeviceSettings = {
            get: vi.fn(async () => {
                throw new Error('device store is unavailable')
            }),
            readMany: vi.fn(async (keys: readonly string[]) => keys.map(() => null)),
            set: vi.fn(async () => undefined),
            patch: vi.fn(async () => undefined),
        }

        const bag = await createNativeDeviceSettingsBag(
            settings,
            'official-account.asset-ledger.v1',
        )

        expect(bag.storage.getItem('officialPublishedAssets:account-1')).toBeNull()
        await expect(bag.flush()).rejects.toThrow('device store is unavailable')
    })
})
