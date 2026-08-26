import { describe, expect, it, vi } from 'vitest'

import { nativePersistentRevisionLease } from '../storage/nativePersistentExport'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'
import type { PersistentRevisionLease } from '../storage/persistentDataStore'
import { tryNativeKeiBackup } from './nativeBackup'

function nativeRuntime() {
    const release = vi.fn(async () => undefined)
    const acquireRevision = vi.fn(
        async (revision: number): Promise<PersistentRevisionLease> => ({
            revision,
            [nativePersistentRevisionLease]: 'lease-7',
            readRoot: vi.fn(),
            queryPresets: vi.fn(),
            readPreset: vi.fn(),
            queryCharacters: vi.fn(),
            readCharacter: vi.fn(),
            queryConversations: vi.fn(),
            readConversation: vi.fn(),
            readConversationWindow: vi.fn(),
            queryPluginStorage: vi.fn(),
            readPluginStorage: vi.fn(),
            readAssetAlias: vi.fn(),
            readAssetOwnerHead: vi.fn(),
            release,
        }) as PersistentRevisionLease,
    )
    const flushPendingData = vi.fn(async () => undefined)
    const runtime = {
        revision: 7,
        store: { acquireRevision },
        flushPendingData,
    } as unknown as PersistentDataRuntime
    return { runtime, acquireRevision, flushPendingData, release }
}

describe('tryNativeKeiBackup', () => {
    it('uploads a pinned revision without passing database or body bytes over IPC', async () => {
        const harness = nativeRuntime()
        const invoke = vi.fn(async (_command: string, _args: Record<string, unknown>) => ({
            revision: 7,
            bytes: 1234,
            status: 503,
        }))

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => true,
                    invoke,
                },
            ),
        ).resolves.toBe(true)

        expect(harness.flushPendingData).toHaveBeenCalledWith('kei-auto-backup')
        expect(harness.acquireRevision).toHaveBeenCalledWith(7)
        expect(invoke).toHaveBeenCalledWith('pds_kei_backup_upload', {
            lease: 'lease-7',
            url: 'https://kei.example/autobackup/save',
            expectedAccountId: 'account-1',
            token: 'secret-token',
        })
        expect(invoke.mock.calls[0][1]).not.toHaveProperty('database')
        expect(invoke.mock.calls[0][1]).not.toHaveProperty('body')
        expect(harness.release).toHaveBeenCalledOnce()
    })

    it('keeps the existing path when native persistence is unavailable', async () => {
        const harness = nativeRuntime()
        const invoke = vi.fn()

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => false,
                    invoke,
                },
            ),
        ).resolves.toBe(false)

        expect(harness.flushPendingData).not.toHaveBeenCalled()
        expect(harness.acquireRevision).not.toHaveBeenCalled()
        expect(invoke).not.toHaveBeenCalled()
    })

    it('falls back after releasing a non-native revision lease', async () => {
        const harness = nativeRuntime()
        const release = vi.fn(async () => undefined)
        harness.acquireRevision.mockResolvedValueOnce({
            revision: 7,
            readRoot: vi.fn(),
            queryPresets: vi.fn(),
            readPreset: vi.fn(),
            queryCharacters: vi.fn(),
            readCharacter: vi.fn(),
            queryConversations: vi.fn(),
            readConversation: vi.fn(),
            readConversationWindow: vi.fn(),
            queryPluginStorage: vi.fn(),
            readPluginStorage: vi.fn(),
            readAssetAlias: vi.fn(),
            readAssetOwnerHead: vi.fn(),
            release,
        })
        const invoke = vi.fn()

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => true,
                    invoke,
                },
            ),
        ).resolves.toBe(false)

        expect(release).toHaveBeenCalledOnce()
        expect(invoke).not.toHaveBeenCalled()
    })

    it('releases the lease and preserves the upload error', async () => {
        const harness = nativeRuntime()
        const uploadError = new Error('offline')
        const invoke = vi.fn(async () => {
            throw uploadError
        })

        await expect(
            tryNativeKeiBackup(
                {
                    runtime: harness.runtime,
                    url: 'https://kei.example/autobackup/save',
                    accountId: 'account-1',
                    token: 'secret-token',
                },
                {
                    isTauri: () => true,
                    invoke,
                },
            ),
        ).rejects.toBe(uploadError)

        expect(harness.release).toHaveBeenCalledOnce()
    })
})
