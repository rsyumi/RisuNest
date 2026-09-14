import { describe, expect, it, vi } from 'vitest'
import {
    buildExternalDeviceCaptureRequest,
    prepareExternalDeviceCapture,
    pendingExternalDeviceCaptureGroups,
    resolveExternalDeviceSections,
    selectExternalDeviceRestoreSections,
} from './deviceSections'
import type { ExternalStorageScope } from './types'
import type { DeviceNativeInvoke } from '../../deviceBackup/nativeSpool'

const scope = (
    deviceSettings: boolean,
    devicePlugins: boolean,
): ExternalStorageScope => ({
    library: true,
    referencedAssets: true,
    deviceSettings,
    devicePlugins,
})

function databases(...names: string[]): IDBFactory {
    return {
        databases: vi.fn(async () => names.map(name => ({ name, version: 1 }))),
    } as unknown as IDBFactory
}

describe('external backup device sections', () => {
    it('selects exact settings and plugin sources without app databases', async () => {
        const factory = databases('safe_plugin_z', 'app-main', 'safe_plugin_a')
        await expect(
            resolveExternalDeviceSections('backup', scope(true, true), factory),
        ).resolves.toEqual([
            'device-settings',
            'local-storage',
            'localforage',
            'indexed-db:0073006100660065005f0070006c007500670069006e005f0061',
            'indexed-db:0073006100660065005f0070006c007500670069006e005f007a',
        ])
    })

    it('never enumerates plugin databases for a settings-only capture', async () => {
        const factory = databases('safe_plugin_unused')
        await expect(
            resolveExternalDeviceSections('backup', scope(true, false), factory),
        ).resolves.toEqual(['device-settings'])
        expect(factory.databases).not.toHaveBeenCalled()
    })

    it('rejects device data in synchronization repositories', async () => {
        await expect(
            resolveExternalDeviceSections('sync', scope(true, false), databases()),
        ).rejects.toThrow('cannot include device-only')
    })

    it('batches overlapping destinations only when their immutable device scope matches', async () => {
        await expect(
            buildExternalDeviceCaptureRequest(
                [
                    { jobId: 'job-b', purpose: 'backup', scope: scope(false, true) },
                    { jobId: 'job-a', purpose: 'backup', scope: scope(false, true) },
                ],
                databases(),
            ),
        ).resolves.toMatchObject({
            consumerIds: ['job-a', 'job-b'],
            deviceSections: ['local-storage', 'localforage'],
        })
        await expect(
            buildExternalDeviceCaptureRequest(
                [
                    { jobId: 'job-a', purpose: 'backup', scope: scope(true, false) },
                    { jobId: 'job-b', purpose: 'backup', scope: scope(false, true) },
                ],
                databases(),
            ),
        ).rejects.toThrow('different device scopes')
    })

    it('uses a cached native capture without entering maintenance', async () => {
        const native = vi.fn(async (command: string) => {
            expect(command).toBe('external_storage_prepare_device_capture')
            return { state: 'ready', captureId: 'a'.repeat(64) }
        })
        const invoke = native as unknown as DeviceNativeInvoke
        await expect(
            prepareExternalDeviceCapture(
                [{ jobId: 'job-a', purpose: 'backup', scope: scope(true, false) }],
                invoke,
                { factory: databases() },
            ),
        ).resolves.toEqual({ state: 'ready', captureId: 'a'.repeat(64) })
        expect(native).toHaveBeenCalledWith('external_storage_prepare_device_capture', {
            request: {
                consumerIds: ['job-a'],
                deviceSections: ['device-settings'],
            },
        })
    })

    it('reconstructs same-scope waiting groups without connection credentials', () => {
        const deviceScope = scope(true, false)
        const groups = pendingExternalDeviceCaptureGroups({
            supported: true,
            selection: {
                kind: 'none',
                selectionEpoch: '1',
                paused: false,
                decisionRequired: false,
            },
            connections: [
                {
                    id: 'connection-a',
                    providerId: 'webdav',
                    purpose: 'backup',
                    strategy: 'backup-only',
                    mode: 'existing',
                    displayName: 'Synthetic',
                    endpoint: {
                        providerId: 'webdav',
                        authority: 'synthetic.invalid',
                        repositoryHint: 'backup',
                        warnings: [],
                        remoteVerified: true,
                    },
                    scope: deviceScope,
                    capabilities: {
                        cas: false,
                        sequential: false,
                        backupOnly: true,
                        resumableUpload: false,
                        rangeDownload: false,
                        snapshotDiscovery: true,
                        evidence: 'synthetic',
                    },
                    status: 'ready',
                },
            ],
            jobs: [
                {
                    id: 'job-a',
                    connectionId: 'connection-a',
                    kind: 'backup',
                    state: 'waiting',
                    phase: 'device-capture',
                    completedBytes: '0',
                    completedItems: '0',
                    startedAtMs: '1',
                    updatedAtMs: '1',
                },
            ],
        })
        expect(groups).toEqual([
            [{ jobId: 'job-a', purpose: 'backup', scope: deviceScope }],
        ])
        expect(groups[0][0]).not.toHaveProperty('config')
    })

    it('maps restore areas to exact included sections and preserves unselected areas', () => {
        expect(
            selectExternalDeviceRestoreSections(
                'backup',
                scope(true, true),
                ['device-settings', 'local-storage', 'localforage'],
                ['devicePlugins'],
            ),
        ).toEqual({
            library: false,
            deviceSections: ['local-storage', 'localforage'],
        })
        expect(() =>
            selectExternalDeviceRestoreSections(
                'backup',
                scope(true, true),
                ['device-settings', 'local-storage'],
                ['devicePlugins'],
            ),
        ).toThrow('incomplete device plugin scope')
        expect(() =>
            selectExternalDeviceRestoreSections(
                'backup',
                scope(false, true),
                ['device-settings', 'local-storage', 'localforage'],
                ['devicePlugins'],
            ),
        ).toThrow('differ from its repository scope')
    })
})
